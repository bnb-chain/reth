//! Bsc Consensus implementation.

// TODO: doc
#![allow(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
// The `bsc` feature must be enabled to use this crate.
#![cfg(feature = "bsc")]

use alloy_json_abi::JsonAbi;
use alloy_rlp::Decodable;
use lazy_static::lazy_static;
use lru::LruCache;
use parking_lot::RwLock;
use reth_chainspec::ChainSpec;
use reth_consensus::{Consensus, ConsensusError, PostExecutionInput};
use reth_primitives::{
    constants::EMPTY_MIX_HASH,
    parlia::{ParliaConfig, Snapshot, VoteAddress, VoteAttestation},
    Address, BlockWithSenders, GotExpected, Header, SealedBlock, SealedHeader, B256,
    EMPTY_OMMER_ROOT_HASH, U256,
};
use secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, SECP256K1,
};
use sha3::{Digest, Keccak256};
use std::{
    clone::Clone,
    collections::{HashMap, VecDeque},
    fmt::{Debug, Formatter},
    num::NonZeroUsize,
    sync::Arc,
    time::SystemTime,
};

use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    Mutex, RwLockReadGuard, RwLockWriteGuard,
};
use tracing::{log::debug, trace};

use reth_beacon_consensus::BeaconEngineMessage;
use reth_engine_primitives::EngineTypes;
use reth_network::{fetch::FetchClient, message::EngineMessage};
use reth_primitives::{BlockBody, BlockHash, BlockHashOrNumber, BlockNumber};
use reth_provider::BlockReaderIdExt;

mod util;
pub use util::*;
mod constants;
pub use constants::*;
mod feynman_fork;
pub use feynman_fork::*;
mod error;
pub use error::ParliaConsensusError;
mod go_rng;
pub use go_rng::{RngSource, Shuffle};
mod abi;
pub use abi::*;
use reth_consensus_common::validation::{
    validate_against_parent_4844, validate_against_parent_eip1559_base_fee,
    validate_against_parent_hash_number, validate_against_parent_timestamp,
    validate_header_base_fee, validate_header_gas,
};

mod validation;
pub use validation::{validate_4844_header_of_bsc, validate_block_post_execution};
mod client;
mod system_tx;
use client::*;
mod task;
use task::*;

const RECOVERED_PROPOSER_CACHE_NUM: usize = 4096;
const STORAGE_CACHE_NUM: usize = 1000;

lazy_static! {
    // recovered proposer cache map by block_number: proposer_address
    static ref RECOVERED_PROPOSER_CACHE: RwLock<LruCache<B256, Address>> = RwLock::new(LruCache::new(NonZeroUsize::new(RECOVERED_PROPOSER_CACHE_NUM).unwrap()));
}

/// BSC parlia consensus implementation
#[derive(Clone)]
pub struct Parlia {
    chain_spec: Arc<ChainSpec>,
    epoch: u64,
    period: u64,
    validator_abi: JsonAbi,
    validator_abi_before_luban: JsonAbi,
    slash_abi: JsonAbi,
    stake_hub_abi: JsonAbi,
}

/// Helper type of the validators info in header
#[derive(Clone, Debug, Default)]
pub struct ValidatorsInfo {
    pub consensus_addrs: Vec<Address>,
    pub vote_addrs: Option<Vec<VoteAddress>>,
}

impl Default for Parlia {
    fn default() -> Self {
        Self::new(Arc::new(ChainSpec::default()), ParliaConfig::default())
    }
}

impl Parlia {
    pub fn new(chain_spec: Arc<ChainSpec>, cfg: ParliaConfig) -> Self {
        let validator_abi = serde_json::from_str(*VALIDATOR_SET_ABI).unwrap();
        let validator_abi_before_luban =
            serde_json::from_str(*VALIDATOR_SET_ABI_BEFORE_LUBAN).unwrap();
        let slash_abi = serde_json::from_str(*SLASH_INDICATOR_ABI).unwrap();
        let stake_hub_abi = serde_json::from_str(*STAKE_HUB_ABI).unwrap();

        Self {
            chain_spec,
            epoch: cfg.epoch,
            period: cfg.period,
            validator_abi,
            validator_abi_before_luban,
            slash_abi,
            stake_hub_abi,
        }
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn period(&self) -> u64 {
        self.period
    }

    #[inline]
    pub fn chain_spec(&self) -> &ChainSpec {
        &self.chain_spec
    }

    pub fn recover_proposer(&self, header: &Header) -> Result<Address, ParliaConsensusError> {
        let mut cache = RECOVERED_PROPOSER_CACHE.write();

        let hash = header.hash_slow();
        if let Some(&proposer) = cache.get(&hash) {
            return Ok(proposer);
        }

        let extra_data = &header.extra_data;

        if extra_data.len() < EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Err(ParliaConsensusError::ExtraSignatureMissing);
        }

        let signature_offset = extra_data.len() - EXTRA_SEAL_LEN;
        let recovery_byte = extra_data[signature_offset + EXTRA_SEAL_LEN - 1] as i32;
        let signature_bytes = &extra_data[signature_offset..signature_offset + EXTRA_SEAL_LEN - 1];

        let recovery_id = RecoveryId::from_i32(recovery_byte)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;
        let signature = RecoverableSignature::from_compact(signature_bytes, recovery_id)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;

        let message = Message::from_digest_slice(
            hash_with_chain_id(header, self.chain_spec.chain.id()).as_slice(),
        )
        .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;
        let public = &SECP256K1
            .recover_ecdsa(&message, &signature)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;

        let proposer =
            Address::from_slice(&Keccak256::digest(&public.serialize_uncompressed()[1..])[12..]);

        cache.put(hash, proposer);
        Ok(proposer)
    }

    pub fn parse_validators_from_header(
        &self,
        header: &Header,
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let val_bytes = self.get_validator_bytes_from_header(header).ok_or_else(|| {
            ParliaConsensusError::InvalidHeaderExtraLen {
                header_extra_len: header.extra_data.len() as u64,
            }
        })?;

        if val_bytes.is_empty() {
            return Err(ParliaConsensusError::InvalidHeaderExtraValidatorBytesLen {
                is_epoch: true,
                validator_bytes_len: 0,
            })
        }

        if self.chain_spec.is_luban_active_at_block(header.number) {
            self.parse_validators_after_luban(&val_bytes)
        } else {
            self.parse_validators_before_luban(&val_bytes)
        }
    }

    pub fn get_vote_attestation_from_header(
        &self,
        header: &Header,
    ) -> Result<Option<VoteAttestation>, ParliaConsensusError> {
        let extra_len = header.extra_data.len();

        if extra_len <= EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Ok(None);
        }

        if !self.chain_spec.is_luban_active_at_block(header.number) {
            return Ok(None);
        }

        let mut raw_attestation_data = if header.number % self.epoch != 0 {
            &header.extra_data[EXTRA_VANITY_LEN..extra_len - EXTRA_SEAL_LEN]
        } else {
            let validator_count =
                header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
            let start = EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM + validator_count * EXTRA_VALIDATOR_LEN;
            let end = extra_len - EXTRA_SEAL_LEN;
            &header.extra_data[start..end]
        };
        if raw_attestation_data.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            Decodable::decode(&mut raw_attestation_data)
                .map_err(|_| ParliaConsensusError::ABIDecodeInnerError)?,
        ))
    }

    pub fn get_validator_bytes_from_header(&self, header: &Header) -> Option<Vec<u8>> {
        let extra_len = header.extra_data.len();
        if extra_len <= EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return None;
        }

        let is_luban_active = self.chain_spec.is_luban_active_at_block(header.number);
        let is_epoch = header.number % self.epoch == 0;

        if !is_luban_active {
            if is_epoch &&
                (extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN) %
                    EXTRA_VALIDATOR_LEN_BEFORE_LUBAN !=
                    0
            {
                return None;
            }

            Some(header.extra_data[EXTRA_VANITY_LEN..extra_len - EXTRA_SEAL_LEN].to_vec())
        } else {
            if !is_epoch {
                return None;
            }

            let count = header.extra_data[EXTRA_VANITY_LEN] as usize;
            if count == 0 ||
                extra_len <= EXTRA_VANITY_LEN + EXTRA_SEAL_LEN + count * EXTRA_VALIDATOR_LEN
            {
                return None;
            }

            let start = EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM;
            let end = start + count * EXTRA_VALIDATOR_LEN;

            Some(header.extra_data[start..end].to_vec())
        }
    }

    pub fn back_off_time(&self, snap: &Snapshot, header: &Header) -> u64 {
        let validator = header.beneficiary;
        if snap.is_inturn(validator) {
            return 0;
        }

        let mut delay = BACKOFF_TIME_OF_INITIAL;
        let mut validators = snap.validators.clone();

        if self.chain_spec.is_planck_active_at_block(header.number) {
            let validator_count = validators.len() as u64;

            let mut recents = HashMap::with_capacity(snap.recent_proposers.len());
            let bound = header.number.saturating_sub(validator_count / 2 + 1);
            for (&seen, &proposer) in &snap.recent_proposers {
                if seen <= bound {
                    continue
                };
                recents.insert(proposer, seen);
            }

            if recents.contains_key(&validator) {
                // The backOffTime does not matter when a validator has signed recently.
                return 0;
            }

            let inturn_addr = snap.inturn_validator();
            if recents.contains_key(&inturn_addr) {
                trace!(
                    "in turn validator({:?}) has recently signed, skip initialBackOffTime",
                    inturn_addr
                );
                delay = 0
            }

            // Exclude the recently signed validators
            validators.retain(|addr| !recents.contains_key(addr));
        }

        let mut rng = RngSource::new(snap.block_number as i64);
        let mut back_off_steps: Vec<u64> = (0..validators.len() as u64).collect();
        back_off_steps.shuffle(&mut rng);

        // get the index of the current validator and its shuffled backoff time.
        for (idx, val) in validators.iter().enumerate() {
            if *val == validator {
                return delay + back_off_steps[idx] * BACKOFF_TIME_OF_WIGGLE
            }
        }

        debug!("the validator is not authorized");
        0
    }

    fn parse_validators_before_luban(
        &self,
        validator_bytes: &[u8],
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let count = validator_bytes.len() / EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
        let mut consensus_addrs = Vec::with_capacity(count);

        for i in 0..count {
            let start = i * EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
            let end = start + EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
            let address = Address::from_slice(&validator_bytes[start..end]);
            consensus_addrs.push(address);
        }

        Ok(ValidatorsInfo { consensus_addrs, vote_addrs: None })
    }

    fn parse_validators_after_luban(
        &self,
        validator_bytes: &[u8],
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let count = validator_bytes.len() / EXTRA_VALIDATOR_LEN;
        let mut consensus_addrs = Vec::with_capacity(count);
        let mut vote_addrs = Vec::with_capacity(count);

        for i in 0..count {
            let consensus_start = i * EXTRA_VALIDATOR_LEN;
            let consensus_end = consensus_start + ADDRESS_LENGTH;
            let consensus_address =
                Address::from_slice(&validator_bytes[consensus_start..consensus_end]);
            consensus_addrs.push(consensus_address);

            let vote_start = consensus_start + ADDRESS_LENGTH;
            let vote_end = consensus_start + EXTRA_VALIDATOR_LEN;
            let vote_address = VoteAddress::from_slice(&validator_bytes[vote_start..vote_end]);
            vote_addrs.push(vote_address);
        }

        Ok(ValidatorsInfo { consensus_addrs, vote_addrs: Some(vote_addrs) })
    }

    fn check_header_extra_len(&self, header: &Header) -> Result<(), ParliaConsensusError> {
        let extra_len = header.extra_data.len();
        if extra_len < EXTRA_VANITY_LEN {
            return Err(ParliaConsensusError::ExtraVanityMissing);
        }
        if extra_len < EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Err(ParliaConsensusError::ExtraSignatureMissing);
        }

        if header.number % self.epoch != 0 {
            return Ok(());
        }

        if !self.chain_spec.is_luban_active_at_block(header.number) {
            let validator_bytes_len = extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN;
            if validator_bytes_len / EXTRA_VALIDATOR_LEN_BEFORE_LUBAN == 0 ||
                validator_bytes_len % EXTRA_VALIDATOR_LEN_BEFORE_LUBAN != 0
            {
                return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                    header_extra_len: extra_len as u64,
                });
            }
        } else {
            let count = header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
            let expect =
                EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM + EXTRA_SEAL_LEN + count * EXTRA_VALIDATOR_LEN;
            if count == 0 || extra_len < expect {
                return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                    header_extra_len: extra_len as u64,
                });
            }
        }

        Ok(())
    }

    fn check_header_extra(&self, header: &Header) -> Result<(), ParliaConsensusError> {
        self.check_header_extra_len(header)?;

        let is_epoch = header.number % self.epoch == 0;
        let validator_bytes_len = self.get_validator_len_from_header(header)?;
        if (!is_epoch && validator_bytes_len != 0) || (is_epoch && validator_bytes_len == 0) {
            return Err(ParliaConsensusError::InvalidHeaderExtraValidatorBytesLen {
                is_epoch,
                validator_bytes_len,
            });
        }

        Ok(())
    }

    fn get_validator_len_from_header(
        &self,
        header: &Header,
    ) -> Result<usize, ParliaConsensusError> {
        if header.number % self.epoch != 0 {
            return Ok(0);
        }

        let extra_len = header.extra_data.len();

        if !self.chain_spec.is_luban_active_at_block(header.number) {
            return Ok(extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN);
        }

        let count = header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
        Ok(count * EXTRA_VALIDATOR_LEN)
    }
}

/// Header and Block validation
impl Parlia {
    fn present_timestamp(&self) -> u64 {
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
    }

    fn validate_header_with_predicted_timestamp(
        &self,
        header: &SealedHeader,
        predicted_timestamp: u64,
    ) -> Result<(), ConsensusError> {
        if header.timestamp < predicted_timestamp {
            return Err(ConsensusError::TimestampNotExpected {
                timestamp: header.timestamp,
                predicted_timestamp,
            });
        }
        let present_timestamp = self.present_timestamp();
        if predicted_timestamp > present_timestamp {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp: predicted_timestamp,
                present_timestamp,
            });
        }
        self.validate_header(header)
    }

    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // Don't waste time checking blocks from the future
        let present_timestamp = self.present_timestamp();
        if header.timestamp > present_timestamp {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp: header.timestamp,
                present_timestamp,
            });
        }

        // Check extra data
        self.check_header_extra(header).map_err(|_| ConsensusError::InvalidHeaderExtra)?;

        // Ensure that the mix digest is zero as we don't have fork protection currently
        if header.mix_hash != EMPTY_MIX_HASH {
            return Err(ConsensusError::InvalidMixHash);
        }

        // Ensure that the block with no uncles
        if header.ommers_hash != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::BodyOmmersHashDiff(
                GotExpected { got: header.ommers_hash, expected: EMPTY_OMMER_ROOT_HASH }.into(),
            ));
        }

        validate_header_gas(header)?;
        validate_header_base_fee(header, &self.chain_spec)?;

        // Ensures that EIP-4844 fields are valid once cancun is active.
        if self.chain_spec.is_cancun_active_at_timestamp(header.timestamp) {
            validate_4844_header_of_bsc(header)?;
        } else if header.blob_gas_used.is_some() {
            return Err(ConsensusError::BlobGasUsedUnexpected)
        } else if header.excess_blob_gas.is_some() {
            return Err(ConsensusError::ExcessBlobGasUnexpected)
        } else if header.parent_beacon_block_root.is_some() {
            return Err(ConsensusError::ParentBeaconBlockRootUnexpected)
        }

        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header, parent)?;
        validate_against_parent_timestamp(header, parent)?;
        validate_against_parent_eip1559_base_fee(header, parent, &self.chain_spec)?;

        // ensure that the blob gas fields for this block
        if self.chain_spec.is_cancun_active_at_timestamp(header.timestamp) {
            validate_against_parent_4844(header, parent)?;
        }

        Ok(())
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock) -> Result<(), ConsensusError> {
        // Check transaction root
        if let Err(error) = block.ensure_transaction_root_valid() {
            return Err(ConsensusError::BodyTransactionRootDiff(error.into()));
        }

        // EIP-4844: Shard Blob Transactions
        if self.chain_spec.is_cancun_active_at_timestamp(block.timestamp) {
            // Check that the blob gas used in the header matches the sum of the blob gas used by
            // each blob tx
            let header_blob_gas_used =
                block.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
            let total_blob_gas = block.blob_gas_used();
            if total_blob_gas != header_blob_gas_used {
                return Err(ConsensusError::BlobGasUsedDiff(GotExpected {
                    got: header_blob_gas_used,
                    expected: total_blob_gas,
                }));
            }
        }

        Ok(())
    }
}

impl Consensus for Parlia {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        self.validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        self.validate_header_against_parent(header, parent)
    }

    // No total difficulty check for Parlia
    fn validate_header_with_total_difficulty(
        &self,
        _header: &Header,
        _total_difficulty: U256,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock) -> Result<(), ConsensusError> {
        self.validate_block_pre_execution(block)
    }

    fn validate_block_post_execution(
        &self,
        block: &BlockWithSenders,
        input: PostExecutionInput<'_>,
    ) -> Result<(), ConsensusError> {
        validate_block_post_execution(block, &self.chain_spec, input.receipts)
    }
}

impl Debug for Parlia {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parlia")
            .field("chain_spec", &self.chain_spec)
            .field("epoch", &self.epoch)
            .field("period", &self.period)
            .finish()
    }
}

/// Builder type for configuring the setup
#[derive(Debug)]
pub struct ParliaEngineBuilder<Client, Engine: EngineTypes> {
    chain_spec: Arc<ChainSpec>,
    cfg: ParliaConfig,
    storage: Storage,
    to_engine: UnboundedSender<BeaconEngineMessage<Engine>>,
    network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
    fetch_client: FetchClient,
    client: Client,
}

// === impl ParliaEngineBuilder ===

impl<Client, Engine> ParliaEngineBuilder<Client, Engine>
where
    Client: BlockReaderIdExt + Clone + 'static,
    Engine: EngineTypes + 'static,
{
    /// Creates a new builder instance to configure all parts.
    pub fn new(
        chain_spec: Arc<ChainSpec>,
        cfg: ParliaConfig,
        client: Client,
        to_engine: UnboundedSender<BeaconEngineMessage<Engine>>,
        network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
        fetch_client: FetchClient,
    ) -> Self {
        let latest_header = client
            .latest_header()
            .ok()
            .flatten()
            .unwrap_or_else(|| chain_spec.sealed_genesis_header());

        Self {
            chain_spec,
            cfg,
            client,
            storage: Storage::new(latest_header),
            to_engine,
            network_block_event_rx,
            fetch_client,
        }
    }

    /// Consumes the type and returns all components
    #[track_caller]
    pub fn build(self) -> ParliaClient {
        let Self {
            chain_spec,
            cfg,
            storage,
            to_engine,
            network_block_event_rx,
            fetch_client,
            client,
        } = self;
        let parlia_client = ParliaClient::new(storage.clone(), fetch_client);
        ParliaEngineTask::start(
            chain_spec.clone(),
            Parlia::new(chain_spec, cfg.clone()),
            client,
            to_engine,
            network_block_event_rx,
            storage,
            parlia_client.clone(),
            cfg.period,
        );
        parlia_client
    }
}

/// In memory storage
#[derive(Debug, Clone)]
pub(crate) struct Storage {
    inner: Arc<tokio::sync::RwLock<StorageInner>>,
}

// == impl Storage ===

impl Storage {
    /// Initializes the [Storage] with the given best block. This should be initialized with the
    /// highest block in the chain, if there is a chain already stored on-disk.
    fn new(best_block: SealedHeader) -> Self {
        let mut storage = StorageInner {
            best_hash: best_block.hash(),
            best_block: best_block.number,
            best_header: best_block.clone(),
            headers: LimitedHashSet::new(STORAGE_CACHE_NUM),
            hash_to_number: LimitedHashSet::new(STORAGE_CACHE_NUM),
            bodies: LimitedHashSet::new(STORAGE_CACHE_NUM),
        };
        storage.headers.put(best_block.number, best_block.clone());
        storage.hash_to_number.put(best_block.hash(), best_block.number);
        Self { inner: Arc::new(tokio::sync::RwLock::new(storage)) }
    }

    /// Returns the write lock of the storage
    pub(crate) async fn write(&self) -> RwLockWriteGuard<'_, StorageInner> {
        self.inner.write().await
    }

    /// Returns the read lock of the storage
    pub(crate) async fn read(&self) -> RwLockReadGuard<'_, StorageInner> {
        self.inner.read().await
    }
}

/// In-memory storage for the chain the parlia engine task cache.
#[derive(Debug)]
pub(crate) struct StorageInner {
    /// Headers buffered for download.
    pub(crate) headers: LimitedHashSet<BlockNumber, SealedHeader>,
    /// A mapping between block hash and number.
    pub(crate) hash_to_number: LimitedHashSet<BlockHash, BlockNumber>,
    /// Bodies buffered for download.
    pub(crate) bodies: LimitedHashSet<BlockHash, BlockBody>,
    /// Tracks best block
    pub(crate) best_block: u64,
    /// Tracks hash of best block
    pub(crate) best_hash: B256,
    /// The best header in the chain
    pub(crate) best_header: SealedHeader,
}

// === impl StorageInner ===

impl StorageInner {
    /// Returns the matching header if it exists.
    pub(crate) fn header_by_hash_or_number(
        &self,
        hash_or_num: BlockHashOrNumber,
    ) -> Option<SealedHeader> {
        let num = match hash_or_num {
            BlockHashOrNumber::Hash(hash) => self.hash_to_number.get(&hash).copied()?,
            BlockHashOrNumber::Number(num) => num,
        };
        self.headers.get(&num).cloned()
    }

    /// Inserts a new header+body pair
    pub(crate) fn insert_new_block(&mut self, header: SealedHeader, body: BlockBody) {
        self.best_hash = header.hash();
        self.best_block = header.number;
        self.best_header = header.clone();

        trace!(target: "parlia::client", num=self.best_block, hash=?self.best_hash, "inserting new block");
        self.headers.put(header.number, header);
        self.bodies.put(self.best_hash, body);
        self.hash_to_number.put(self.best_hash, self.best_block);
    }

    /// Inserts a new header
    pub(crate) fn insert_new_header(&mut self, header: SealedHeader) {
        self.best_hash = header.hash();
        self.best_block = header.number;
        self.best_header = header.clone();

        trace!(target: "parlia::client", num=self.best_block, hash=?self.best_hash, "inserting new header");
        self.headers.put(header.number, header);
        self.hash_to_number.put(self.best_hash, self.best_block);
    }
}

#[derive(Debug)]
struct LimitedHashSet<K, V> {
    map: HashMap<K, V>,
    queue: VecDeque<K>,
    capacity: usize,
}

impl<K, V> LimitedHashSet<K, V>
where
    K: std::hash::Hash + Eq + Clone,
{
    fn new(capacity: usize) -> Self {
        Self { map: HashMap::new(), queue: VecDeque::new(), capacity }
    }

    fn put(&mut self, key: K, value: V) {
        if self.map.len() >= self.capacity {
            if let Some(old_key) = self.queue.pop_front() {
                self.map.remove(&old_key);
            }
        }
        self.map.insert(key.clone(), value);
        self.queue.push_back(key);
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // To make sure the abi is correct
    #[test]
    fn test_new_parlia() {
        let parlia = Parlia::new(Arc::new(ChainSpec::default()), ParliaConfig::default());
        assert_eq!(parlia.epoch(), 200);
        assert_eq!(parlia.period(), 3);
    }

    #[test]
    fn test_inner_storage() {
        let default_block = Header::default().seal_slow();
        let mut storage = StorageInner {
            best_hash: default_block.hash(),
            best_block: default_block.number,
            best_header: default_block.clone(),
            headers: LimitedHashSet::new(10),
            hash_to_number: LimitedHashSet::new(10),
            bodies: LimitedHashSet::new(10),
        };
        storage.headers.put(default_block.number, default_block.clone());
        storage.hash_to_number.put(default_block.hash(), default_block.number);

        let block = Header::default().seal_slow();
        storage.insert_new_block(block.clone(), BlockBody::default());
        assert_eq!(storage.best_block, block.number);
        assert_eq!(storage.best_hash, block.hash());
        assert_eq!(storage.best_header, block);
        assert_eq!(storage.headers.get(&block.number), Some(&block));
        assert_eq!(storage.hash_to_number.get(&block.hash()), Some(&block.number));
        assert_eq!(storage.bodies.get(&block.hash()), Some(&BlockBody::default()));
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Hash(block.hash())),
            Some(block.clone())
        );
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Number(block.number)),
            Some(block.clone())
        );
        assert_eq!(storage.best_block, block.number);
        assert_eq!(storage.best_hash, block.hash());
        assert_eq!(storage.best_header, block);

        let header = Header::default().seal_slow();
        storage.insert_new_header(header.clone());
        assert_eq!(storage.best_block, header.number);
        assert_eq!(storage.best_hash, header.hash());
        assert_eq!(storage.best_header, header);
        assert_eq!(storage.headers.get(&header.number), Some(&header));
        assert_eq!(storage.hash_to_number.get(&header.hash()), Some(&header.number));
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Hash(header.hash())),
            Some(header.clone())
        );
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Number(header.number)),
            Some(header.clone())
        );
        assert_eq!(storage.best_block, header.number);
        assert_eq!(storage.best_hash, header.hash());
        assert_eq!(storage.best_header, header);
    }

    #[test]
    fn test_limited_hash_set() {
        let mut set = LimitedHashSet::new(2);
        set.put(1, 1);
        set.put(2, 2);
        set.put(3, 3);
        assert_eq!(set.get(&1), None);
        assert_eq!(set.get(&2), Some(&2));
        assert_eq!(set.get(&3), Some(&3));
    }
}
