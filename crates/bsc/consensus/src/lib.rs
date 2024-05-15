//! Bsc Consensus implementation.

// TODO: doc
#![allow(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
// The `bsc` feature must be enabled to use this crate.
#![cfg(feature = "bsc")]

use alloy_dyn_abi::{DynSolValue, FunctionExt, JsonAbiExt};
use alloy_json_abi::JsonAbi;
use alloy_rlp::Decodable;
use lazy_static::lazy_static;
use lru::LruCache;
use parking_lot::RwLock;
use secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, SECP256K1,
};
use sha3::{Digest, Keccak256};
use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    num::NonZeroUsize,
    sync::Arc,
    time::SystemTime,
};

use reth_consensus::{Consensus, ConsensusError};
use reth_consensus_common::validation::validate_4844_header_standalone;
use reth_db::models::parlia::{Snapshot, ValidatorInfo, VoteAddress, VoteAttestation};
use reth_primitives::{
    constants::EMPTY_MIX_HASH, Address, BlockNumber, Bytes, ChainSpec, GotExpected, Hardfork,
    Header, SealedBlock, SealedHeader, Transaction, TxKind, TxLegacy, B256, EMPTY_OMMER_ROOT_HASH,
    U256,
};

mod util;
pub use util::*;
mod constants;
pub use constants::*;

pub mod contract_upgrade;
mod feynman_fork;
pub use feynman_fork::*;
mod error;
pub use error::ParliaConsensusError;
mod go_rng;
pub use go_rng::{RngSource, Shuffle};

const RECOVERED_PROPOSER_CACHE_NUM: usize = 4096;

lazy_static! {
    // recovered proposer cache map by block_number: proposer_address
    static ref RECOVERED_PROPOSER_CACHE: RwLock<LruCache<B256, Address>> = RwLock::new(LruCache::new(NonZeroUsize::new(RECOVERED_PROPOSER_CACHE_NUM).unwrap()));
}

#[derive(Clone, Debug)]
pub struct ParliaConfig {
    epoch: u64,
    period: u64,
}

impl Default for ParliaConfig {
    fn default() -> Self {
        Self { epoch: 300, period: 15 }
    }
}

/// BSC parlia consensus implementation
pub struct Parlia {
    chain_spec: Arc<ChainSpec>,
    epoch: u64,
    period: u64,
    validator_abi: JsonAbi,
    validator_abi_before_luban: JsonAbi,
    slash_abi: JsonAbi,
    stake_hub_abi: JsonAbi,
}

impl Default for Parlia {
    fn default() -> Self {
        Self::new(Arc::new(ChainSpec::default()), ParliaConfig::default())
    }
}

impl Parlia {
    pub fn new(chain_spec: Arc<ChainSpec>, cfg: ParliaConfig) -> Self {
        let validator_abi = load_abi_from_file("./res/validator_set.json").unwrap();
        let validator_abi_before_luban =
            load_abi_from_file("./res/validator_set_before_luban.json").unwrap();
        let slash_abi = load_abi_from_file("./res/slash.json").unwrap();
        let stake_hub_abi = load_abi_from_file("./res/stake_hub.json").unwrap();

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

    pub fn is_on_feynman(&self, timestamp: u64, parent_timestamp: u64) -> bool {
        self.chain_spec.fork(Hardfork::Feynman).active_at_timestamp(timestamp) &&
            !self.chain_spec.fork(Hardfork::Feynman).active_at_timestamp(parent_timestamp)
    }

    pub fn is_on_luban(&self, block_number: BlockNumber) -> bool {
        self.chain_spec.fork(Hardfork::Luban).active_at_block(block_number) &&
            !self.chain_spec.fork(Hardfork::Luban).active_at_block(block_number - 1)
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
        let signature_offset = header.extra_data.len() - EXTRA_SEAL_LEN;

        let sig = &header.extra_data[signature_offset..signature_offset + EXTRA_SEAL_LEN - 1];
        let rec =
            RecoveryId::from_i32(header.extra_data[signature_offset + EXTRA_SEAL_LEN - 1] as i32)
                .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;
        let signature = RecoverableSignature::from_compact(sig, rec)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;

        let mut sig_hash_header = header.clone();
        sig_hash_header.extra_data =
            Bytes::copy_from_slice(&header.extra_data[..header.extra_data.len() - EXTRA_SEAL_LEN]);
        let message = Message::from_digest_slice(
            hash_with_chain_id(&sig_hash_header, self.chain_spec.chain.id()).as_slice(),
        )
        .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;

        let public = &SECP256K1
            .recover_ecdsa(&message, &signature)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;
        let address_slice = &Keccak256::digest(&public.serialize_uncompressed()[1..])[12..];
        let proposer = Address::from_slice(address_slice);

        cache.put(hash, proposer);
        Ok(proposer)
    }

    pub fn parse_validators_from_header(
        &self,
        header: &Header,
    ) -> Result<(Vec<Address>, Option<HashMap<Address, ValidatorInfo>>), ParliaConsensusError> {
        let val_bytes = self.get_validator_bytes_from_header(header)?;

        if !self.chain_spec.fork(Hardfork::Luban).active_at_block(header.number) {
            let count = val_bytes.len() / EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
            let mut vals = Vec::with_capacity(count);
            for i in 0..count {
                let start = i * EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
                let end = start + EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
                vals.push(Address::from_slice(&val_bytes[start..end]));
            }

            return Ok((vals, None));
        }

        let count = val_bytes.len() / EXTRA_VALIDATOR_LEN;
        let mut vals = Vec::with_capacity(count);
        let mut val_info_map = HashMap::with_capacity(count);
        for i in 0..count {
            let start = i * EXTRA_VALIDATOR_LEN;
            let end = start + ADDRESS_LENGTH;
            let addr = Address::from_slice(&val_bytes[start..end]);
            vals.push(addr);

            let start = i * EXTRA_VALIDATOR_LEN + ADDRESS_LENGTH;
            let end = i * EXTRA_VALIDATOR_LEN + EXTRA_VALIDATOR_LEN;
            val_info_map.insert(
                addr,
                ValidatorInfo {
                    index: (i + 1) as u64,
                    vote_addr: VoteAddress::from_slice(&val_bytes[start..end]),
                },
            );
        }

        Ok((vals, Some(val_info_map)))
    }

    pub fn get_vote_attestation_from_header(
        &self,
        header: &Header,
    ) -> Result<Option<VoteAttestation>, ParliaConsensusError> {
        self.check_header_extra_len(header)?;

        let mut raw;
        let extra_len = header.extra_data.len();
        if header.number % self.epoch != 0 {
            raw = &header.extra_data[EXTRA_VANITY_LEN..extra_len - EXTRA_SEAL_LEN]
        } else {
            let count = header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
            let start = EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM + count * EXTRA_VALIDATOR_LEN;
            let end = extra_len - EXTRA_SEAL_LEN;
            raw = &header.extra_data[start..end];
        }
        if raw.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            Decodable::decode(&mut raw).map_err(|_| ParliaConsensusError::ABIDecodeInnerError)?,
        ))
    }

    pub fn get_validator_bytes_from_header(
        &self,
        header: &Header,
    ) -> Result<Vec<u8>, ParliaConsensusError> {
        self.check_header_extra_len(header)?;

        if header.number % self.epoch != 0 {
            return Err(ParliaConsensusError::NotInEpoch { block_number: header.number });
        }

        let extra_len = header.extra_data.len();

        if !self.chain_spec.fork(Hardfork::Luban).active_at_block(header.number) {
            return Ok(header.extra_data[EXTRA_VANITY_LEN..extra_len - EXTRA_SEAL_LEN].to_vec());
        }

        let count = header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
        let start = EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM;
        let end = start + count * EXTRA_VALIDATOR_LEN;

        Ok(header.extra_data[start..end].to_vec())
    }

    pub fn back_off_time(&self, snap: &Snapshot, header: &Header) -> u64 {
        let validator = header.beneficiary;
        if snap.is_inturn(validator) {
            return 0;
        }
        let idx = match snap.index_of(validator) {
            Some(i) => i,
            None => {
                // The backOffTime does not matter when a validator is not authorized.
                return 0;
            }
        };

        let mut rng = RngSource::new(snap.block_number as i64);
        let validator_count = snap.validators.len();

        if !self.chain_spec.fork(Hardfork::Luban).active_at_block(header.number) {
            // select a random step for delay, range 0~(proposer_count-1)
            let mut backoff_steps = Vec::new();
            for i in 0..validator_count {
                backoff_steps.push(i);
            }
            backoff_steps.shuffle(&mut rng);
            return BACKOFF_TIME_OF_INITIAL + (backoff_steps[idx] as u64) * BACKOFF_TIME_OF_WIGGLE;
        }

        // Exclude the recently signed validators first
        let mut recents = HashMap::new();
        let limit = self.get_recently_proposal_limit(header, validator_count as u64);
        let block_number = header.number;
        for (seen, proposer) in snap.recent_proposers.iter() {
            if block_number < limit || *seen > block_number - limit {
                if validator == *proposer {
                    // The backOffTime does not matter when a validator has signed recently.
                    return 0;
                }
                recents.insert(*proposer, true);
            }
        }
        let mut index = idx;
        let mut backoff_steps = Vec::new();
        for i in 0..validator_count {
            if recents.get(&snap.validators[i]).is_some() {
                if i < idx {
                    index -= 1;
                }
                continue;
            }
            backoff_steps.push(backoff_steps.len())
        }

        // select a random step for delay in left validators
        backoff_steps.shuffle(&mut rng);
        let mut delay =
            BACKOFF_TIME_OF_INITIAL + (backoff_steps[index] as u64) * BACKOFF_TIME_OF_WIGGLE;
        // If the current validator has recently signed, reduce initial delay.
        if recents.get(&snap.inturn_validator()).is_some() {
            delay -= BACKOFF_TIME_OF_INITIAL;
        }
        delay
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

        if !self.chain_spec.fork(Hardfork::Luban).active_at_block(header.number) {
            if (extra_len - EXTRA_SEAL_LEN - EXTRA_VANITY_LEN) / EXTRA_VALIDATOR_LEN_BEFORE_LUBAN ==
                0
            {
                return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                    header_extra_len: extra_len as u64,
                });
            }
            if (extra_len - EXTRA_SEAL_LEN - EXTRA_VANITY_LEN) % EXTRA_VALIDATOR_LEN_BEFORE_LUBAN !=
                0
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
        if !is_epoch && validator_bytes_len != 0 {
            return Err(ParliaConsensusError::InvalidHeaderExtraValidatorBytesLen {
                is_epoch,
                validator_bytes_len,
            });
        }
        if is_epoch && validator_bytes_len == 0 {
            return Err(ParliaConsensusError::InvalidHeaderExtraValidatorBytesLen {
                is_epoch,
                validator_bytes_len,
            });
        }

        Ok(())
    }

    pub fn get_recently_proposal_limit(&self, header: &Header, validator_count: u64) -> u64 {
        if self.chain_spec.fork(Hardfork::Luban).active_at_block(header.number) {
            validator_count * 2 / 3 + 1
        } else {
            validator_count / 2 + 1
        }
    }

    fn get_validator_len_from_header(
        &self,
        header: &Header,
    ) -> Result<usize, ParliaConsensusError> {
        self.check_header_extra_len(header)?;

        if header.number % self.epoch != 0 {
            return Ok(0);
        }

        let extra_len = header.extra_data.len();

        if !self.chain_spec.fork(Hardfork::Luban).active_at_block(header.number) {
            return Ok(extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN);
        }

        let count = header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
        Ok(count * EXTRA_VALIDATOR_LEN)
    }
}

// Assemble system tx
impl Parlia {
    pub fn init_genesis_contracts(&self) -> Vec<Transaction> {
        let function = self.validator_abi.function("init").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[]).unwrap();

        let contracts = vec![
            *VALIDATOR_CONTRACT,
            *SLASH_CONTRACT,
            *LIGHT_CLIENT_CONTRACT,
            *RELAYER_HUB_CONTRACT,
            *TOKEN_HUB_CONTRACT,
            *RELAYER_INCENTIVIZE_CONTRACT,
            *CROSS_CHAIN_CONTRACT,
        ];

        contracts
            .into_iter()
            .map(|contract| {
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(self.chain_spec.chain.id()),
                    nonce: 0,
                    gas_limit: u64::MAX / 2,
                    gas_price: 0,
                    value: U256::ZERO,
                    input: Bytes::from(input.clone()),
                    to: TxKind::Call(contract),
                })
            })
            .collect()
    }

    pub fn init_feynman_contracts(&self) -> Vec<Transaction> {
        let function = self.stake_hub_abi.function("initialize").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[]).unwrap();

        let contracts = vec![
            *STAKE_HUB_CONTRACT,
            *BSC_GOVERNOR_CONTRACT,
            *GOV_TOKEN_CONTRACT,
            *BSC_TIMELOCK_CONTRACT,
            *TOKEN_RECOVER_PORTAL_CONTRACT,
        ];

        contracts
            .into_iter()
            .map(|contract| {
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(self.chain_spec.chain.id()),
                    nonce: 0,
                    gas_limit: u64::MAX / 2,
                    gas_price: 0,
                    value: U256::ZERO,
                    input: Bytes::from(input.clone()),
                    to: TxKind::Call(contract),
                })
            })
            .collect()
    }

    pub fn slash(&self, address: Address) -> Transaction {
        let function = self.slash_abi.function("slash").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[DynSolValue::from(address)]).unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::ZERO,
            input: Bytes::from(input.clone()),
            to: TxKind::Call(*SLASH_CONTRACT),
        })
    }

    pub fn distribute_to_system(&self, system_reward: u128) -> Transaction {
        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::from(system_reward),
            input: Bytes::default(),
            to: TxKind::Call(*SYSTEM_REWARD_CONTRACT),
        })
    }

    pub fn distribute_to_validator(&self, address: Address, block_reward: u128) -> Transaction {
        let function = self.validator_abi.function("deposit").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[DynSolValue::from(address)]).unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::from(block_reward),
            input: Bytes::from(input),
            to: TxKind::Call(*VALIDATOR_CONTRACT),
        })
    }

    pub fn distribute_finality_reward(
        &self,
        validators: Vec<Address>,
        weights: Vec<U256>,
    ) -> Transaction {
        let function =
            self.validator_abi.function("distributeFinalityReward").unwrap().first().unwrap();

        let validators = validators.into_iter().map(|val| DynSolValue::from(val)).collect();
        let weights = weights.into_iter().map(|weight| DynSolValue::from(weight)).collect();
        let input = function
            .abi_encode_input(&[DynSolValue::Array(validators), DynSolValue::Array(weights)])
            .unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::ZERO,
            input: Bytes::from(input),
            to: TxKind::Call(*VALIDATOR_CONTRACT),
        })
    }

    pub fn update_validator_set_v2(
        &self,
        validators: Vec<Address>,
        voting_powers: Vec<U256>,
        vote_addresses: Vec<Vec<u8>>,
    ) -> Transaction {
        let function =
            self.validator_abi.function("updateValidatorSetV2").unwrap().first().unwrap();

        let validators = validators.into_iter().map(|val| DynSolValue::from(val)).collect();
        let voting_powers = voting_powers.into_iter().map(|val| DynSolValue::from(val)).collect();
        let vote_addresses = vote_addresses.into_iter().map(|val| DynSolValue::from(val)).collect();
        let input = function
            .abi_encode_input(&[
                DynSolValue::Array(validators),
                DynSolValue::Array(voting_powers),
                DynSolValue::Array(vote_addresses),
            ])
            .unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::ZERO,
            input: Bytes::from(input),
            to: TxKind::Call(*VALIDATOR_CONTRACT),
        })
    }
}

// ABI encode/decode
impl Parlia {
    pub fn get_current_validators_before_luban(
        &self,
        block_number: BlockNumber,
    ) -> (Address, Bytes) {
        let function = if self.chain_spec.fork(Hardfork::Euler).active_at_block(block_number) {
            self.validator_abi_before_luban
                .function("getMiningValidators")
                .unwrap()
                .first()
                .unwrap()
        } else {
            self.validator_abi_before_luban.function("getValidators").unwrap().first().unwrap()
        };

        (*VALIDATOR_CONTRACT, Bytes::from(function.abi_encode_input(&[]).unwrap()))
    }

    pub fn unpack_data_into_validator_set_before_luban(&self, data: &[u8]) -> Vec<Address> {
        let function =
            self.validator_abi_before_luban.function("getValidators").unwrap().first().unwrap();
        let output = function.abi_decode_output(data, true).unwrap();

        output.into_iter().map(|val| val.as_address().unwrap()).collect()
    }

    pub fn get_current_validators(&self) -> (Address, Bytes) {
        let function = self.validator_abi.function("getMiningValidators").unwrap().first().unwrap();

        (*VALIDATOR_CONTRACT, Bytes::from(function.abi_encode_input(&[]).unwrap()))
    }

    pub fn unpack_data_into_validator_set(&self, data: &[u8]) -> (Vec<Address>, Vec<VoteAddress>) {
        let function = self.validator_abi.function("getMiningValidators").unwrap().first().unwrap();
        let output = function.abi_decode_output(data, true).unwrap();

        let consensus_addresses = output[0]
            .as_array()
            .unwrap()
            .into_iter()
            .map(|val| val.as_address().unwrap())
            .collect();
        let vote_address = output[1]
            .as_array()
            .unwrap()
            .into_iter()
            .map(|val| VoteAddress::from_slice(val.as_bytes().unwrap()))
            .collect();

        (consensus_addresses, vote_address)
    }

    pub fn get_validator_election_info(&self) -> (Address, Bytes) {
        let function =
            self.stake_hub_abi.function("getValidatorElectionInfo").unwrap().first().unwrap();

        (
            *STAKE_HUB_CONTRACT,
            Bytes::from(
                function
                    .abi_encode_input(&[
                        DynSolValue::from(U256::from(0)),
                        DynSolValue::from(U256::from(0)),
                    ])
                    .unwrap(),
            ),
        )
    }

    pub fn unpack_data_into_validator_election_info(
        &self,
        data: &[u8],
    ) -> (Vec<Address>, Vec<U256>, Vec<Vec<u8>>, U256) {
        let function =
            self.stake_hub_abi.function("getValidatorElectionInfo").unwrap().first().unwrap();
        let output = function.abi_decode_output(data, true).unwrap();

        let consensus_address = output[0]
            .as_array()
            .unwrap()
            .into_iter()
            .map(|val| val.as_address().unwrap())
            .collect();
        let voting_powers =
            output[1].as_array().unwrap().into_iter().map(|val| val.as_uint().unwrap().0).collect();
        let vote_addresses = output[2]
            .as_array()
            .unwrap()
            .into_iter()
            .map(|val| val.as_bytes().unwrap().to_vec())
            .collect();
        let total_length = output[3].as_uint().unwrap().0;

        (consensus_address, voting_powers, vote_addresses, total_length)
    }

    pub fn get_max_elected_validators(&self) -> (Address, Bytes) {
        let function =
            self.stake_hub_abi.function("maxElectedValidators").unwrap().first().unwrap();

        (*STAKE_HUB_CONTRACT, Bytes::from(function.abi_encode_input(&[]).unwrap()))
    }

    pub fn unpack_data_into_max_elected_validators(&self, data: &[u8]) -> U256 {
        let function =
            self.stake_hub_abi.function("maxElectedValidators").unwrap().first().unwrap();
        let output = function.abi_decode_output(data, true).unwrap();

        output[0].as_uint().unwrap().0
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

impl Consensus for Parlia {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // Don't waste time checking blocks from the future
        let present_timestamp =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        if header.timestamp > present_timestamp {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp: header.timestamp,
                present_timestamp,
            });
        }

        // Ensure that the block's difficulty is DIFF_INTURN or DIFF_NOTURN
        if header.difficulty != DIFF_INTURN && header.difficulty != DIFF_NOTURN {
            return Err(ConsensusError::InvalidDifficulty { difficulty: header.difficulty });
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

        // Gas used needs to be less than gas limit. Gas used is going to be checked after
        // execution.
        if header.gas_used > header.gas_limit {
            return Err(ConsensusError::HeaderGasUsedExceedsGasLimit {
                gas_used: header.gas_used,
                gas_limit: header.gas_limit,
            });
        }

        // Check if base fee is set.
        if self.chain_spec.fork(Hardfork::London).active_at_block(header.number) &&
            header.base_fee_per_gas.is_none()
        {
            return Err(ConsensusError::BaseFeeMissing);
        }

        // Ensures that EIP-4844 fields are valid once cancun is active.
        if self.chain_spec.is_cancun_active_at_timestamp(header.timestamp) {
            validate_4844_header_standalone(header)?;
        } else if header.blob_gas_used.is_some() {
            return Err(ConsensusError::BlobGasUsedUnexpected);
        } else if header.excess_blob_gas.is_some() {
            return Err(ConsensusError::ExcessBlobGasUnexpected);
        } else if header.parent_beacon_block_root.is_some() {
            return Err(ConsensusError::ParentBeaconBlockRootUnexpected);
        }

        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        header.validate_against_parent(parent, &self.chain_spec).map_err(ConsensusError::from)?;
        Ok(())
    }

    // No total difficulty check for Parlia
    fn validate_header_with_total_difficulty(
        &self,
        _header: &Header,
        _total_difficulty: U256,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }

    fn validate_block(&self, block: &SealedBlock) -> Result<(), ConsensusError> {
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
                })
                .into());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_primitives::{address, hex};

    #[test]
    fn abi_encode() {
        let expected = "63a036b500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

        let stake_hub_abi = load_abi_from_file("./res/stake_hub.json").unwrap();
        let function = stake_hub_abi.function("getValidatorElectionInfo").unwrap().first().unwrap();
        let input = function
            .abi_encode_input(&[DynSolValue::from(U256::from(0)), DynSolValue::from(U256::from(0))])
            .unwrap();

        let input_str = hex::encode(&input);
        println!("encoded data: {:?}", &input_str);
        assert_eq!(input_str, expected);
    }

    #[test]
    fn abi_decode() {
        let expected_consensus_addr = address!("C08B5542D177ac6686946920409741463a15dDdB");
        let expected_voting_power = U256::from(1);
        let expected_vote_addr = hex::decode("3c2438a4113804bf99e3849ef31887c0f880a0feb92f356f58fbd023a82f5311fc87a5883a662e9ebbbefc90bf13aa53").unwrap();
        let expected_total_length = U256::from(1);

        let output_str = "000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000c08b5542d177ac6686946920409741463a15dddb000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000303c2438a4113804bf99e3849ef31887c0f880a0feb92f356f58fbd023a82f5311fc87a5883a662e9ebbbefc90bf13aa5300000000000000000000000000000000";
        let output = hex::decode(output_str).unwrap();

        let stake_hub_abi = load_abi_from_file("./res/stake_hub.json").unwrap();
        let function = stake_hub_abi.function("getValidatorElectionInfo").unwrap().first().unwrap();
        let output = function.abi_decode_output(&output, true).unwrap();

        let consensus_address: Vec<Address> = output[0]
            .as_array()
            .unwrap()
            .into_iter()
            .map(|val| val.as_address().unwrap())
            .collect();
        let voting_powers: Vec<U256> =
            output[1].as_array().unwrap().into_iter().map(|val| val.as_uint().unwrap().0).collect();
        let vote_addresses: Vec<Vec<u8>> = output[2]
            .as_array()
            .unwrap()
            .into_iter()
            .map(|val| val.as_bytes().unwrap().to_vec())
            .collect();
        let total_length = output[3].as_uint().unwrap().0;

        println!("consensus address: {:?}", consensus_address[0]);
        println!("voting power: {:?}", voting_powers[0]);
        println!("vote address: {:?}", hex::encode(&vote_addresses[0]));
        println!("total length: {:?}", total_length);

        assert_eq!(consensus_address[0], expected_consensus_addr);
        assert_eq!(voting_powers[0], expected_voting_power);
        assert_eq!(vote_addresses[0], expected_vote_addr);
        assert_eq!(total_length, expected_total_length);
    }
}
