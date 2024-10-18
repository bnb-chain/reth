//! Bsc Tasks implementation.

#![allow(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
// The `bsc` feature must be enabled to use this crate.
#![cfg(feature = "bsc")]

use alloy_primitives::{BlockHash, BlockNumber, B256};
use alloy_rpc_types::engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4,
    ExecutionPayloadV1, PayloadAttributes,
};
use reth_beacon_consensus::{BeaconEngineMessage, EngineNodeTypes};
use reth_bsc_consensus::Parlia;
use reth_bsc_evm::SnapshotReader;
use reth_bsc_payload_builder::{BscBuiltPayload, BscPayloadBuilderAttributes};
use reth_chainspec::EthChainSpec;
use reth_engine_primitives::{
    EngineApiMessageVersion, EngineObjectValidationError, EngineTypes, EngineValidator,
    PayloadOrAttributes, PayloadTypes,
};
use reth_network_api::events::EngineMessage;
use reth_network_p2p::BlockClient;
use reth_primitives::{BlockBody, BlockHashOrNumber, SealedHeader};
use reth_provider::{BlockReaderIdExt, CanonChainTracker, ParliaProvider};
use std::{
    clone::Clone,
    collections::{HashMap, VecDeque},
    fmt::Debug,
    marker::PhantomData,
    sync::Arc,
};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    Mutex, RwLockReadGuard, RwLockWriteGuard,
};
use tracing::trace;

mod client;
use client::*;

mod task;
use task::*;

// === impl BscEngineTypes ===

/// The types used in the default mainnet ethereum beacon consensus engine.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct BscEngineTypes<T: PayloadTypes = BscPayloadTypes> {
    _marker: PhantomData<T>,
}

impl<T: PayloadTypes> PayloadTypes for BscEngineTypes<T> {
    type BuiltPayload = T::BuiltPayload;
    type PayloadAttributes = T::PayloadAttributes;
    type PayloadBuilderAttributes = T::PayloadBuilderAttributes;
}

impl<T: PayloadTypes> EngineTypes for BscEngineTypes<T>
where
    T::BuiltPayload: TryInto<ExecutionPayloadV1>
        + TryInto<ExecutionPayloadEnvelopeV2>
        + TryInto<ExecutionPayloadEnvelopeV3>
        + TryInto<ExecutionPayloadEnvelopeV4>,
{
    type ExecutionPayloadV1 = ExecutionPayloadV1;
    type ExecutionPayloadV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadV3 = ExecutionPayloadEnvelopeV3;
    type ExecutionPayloadV4 = ExecutionPayloadEnvelopeV4;
}

/// A default payload type for [`BscEngineTypes`]
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct BscPayloadTypes;

impl PayloadTypes for BscPayloadTypes {
    type BuiltPayload = BscBuiltPayload;
    type PayloadAttributes = PayloadAttributes;
    type PayloadBuilderAttributes = BscPayloadBuilderAttributes;
}

/// Validator for the bsc engine API.
#[derive(Debug, Clone)]
pub struct BscEngineValidator {}

impl<Types> EngineValidator<Types> for BscEngineValidator
where
    Types: EngineTypes<PayloadAttributes = PayloadAttributes>,
{
    fn validate_version_specific_fields(
        &self,
        _version: EngineApiMessageVersion,
        _payload_or_attrs: PayloadOrAttributes<'_, PayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        Ok(())
    }

    fn ensure_well_formed_attributes(
        &self,
        _version: EngineApiMessageVersion,
        _attributes: &PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        Ok(())
    }
}

const STORAGE_CACHE_NUM: usize = 1000;

/// Builder type for configuring the setup
#[derive(Debug)]
pub struct ParliaEngineBuilder<Client, N, Provider, SnapShotProvider>
where
    N: EngineNodeTypes,
{
    chain_spec: Arc<N::ChainSpec>,
    storage: Storage,
    to_engine: UnboundedSender<BeaconEngineMessage<N::Engine>>,
    network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
    fetch_client: Client,
    provider: Provider,
    parlia: Parlia,
    snapshot_reader: SnapshotReader<SnapShotProvider>,
    _marker: PhantomData<N>,
}

// === impl ParliaEngineBuilder ===

impl<Client, N, Provider, SnapShotProvider>
    ParliaEngineBuilder<Client, N, Provider, SnapShotProvider>
where
    Client: BlockClient + 'static,
    N: EngineNodeTypes + 'static,
    Provider: BlockReaderIdExt + CanonChainTracker + Clone + 'static,
    SnapShotProvider: ParliaProvider + 'static,
{
    /// Creates a new builder instance to configure all parts.
    pub fn new(
        chain_spec: Arc<N::ChainSpec>,
        provider: Provider,
        parlia_provider: SnapShotProvider,
        parlia: Parlia,
        to_engine: UnboundedSender<BeaconEngineMessage<N::Engine>>,
        network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
        fetch_client: Client,
        _marker: PhantomData<N>,
    ) -> Self {
        let latest_header = provider.latest_header().ok().flatten().unwrap_or_else(|| {
            SealedHeader::new(chain_spec.genesis_header().clone(), chain_spec.genesis_hash())
        });

        let mut finalized_hash = None;
        let mut safe_hash = None;
        let snapshot_reader =
            SnapshotReader::new(Arc::new(parlia_provider), Arc::new(parlia.clone()));
        let snapshot_result = snapshot_reader.snapshot(&latest_header, None);
        if snapshot_result.is_ok() {
            let snap = snapshot_result.unwrap();
            finalized_hash = Some(snap.vote_data.source_hash);
            safe_hash = Some(snap.vote_data.target_hash);
        }

        Self {
            chain_spec,
            provider,
            snapshot_reader,
            parlia,
            storage: Storage::new(latest_header, finalized_hash, safe_hash),
            to_engine,
            network_block_event_rx,
            fetch_client,
            _marker,
        }
    }

    /// Consumes the type and returns all components
    #[track_caller]
    pub fn build(self, start_engine_task: bool) -> ParliaClient<Client> {
        let Self {
            chain_spec,
            storage,
            to_engine,
            network_block_event_rx,
            fetch_client,
            provider,
            parlia,
            snapshot_reader,
            _marker,
        } = self;
        let parlia_client = ParliaClient::new(storage.clone(), fetch_client);
        let period = parlia.period();
        if start_engine_task {
            ParliaEngineTask::<N, Provider, SnapShotProvider, Client>::start(
                chain_spec,
                parlia,
                provider,
                snapshot_reader,
                to_engine,
                network_block_event_rx,
                storage,
                parlia_client.clone(),
                period,
            );
        }
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
    fn new(
        best_block: SealedHeader,
        finalized_hash: Option<B256>,
        safe_hash: Option<B256>,
    ) -> Self {
        let best_finalized_hash = finalized_hash.unwrap_or_default();
        let best_safe_hash = safe_hash.unwrap_or_default();

        let mut storage = StorageInner {
            best_hash: best_block.hash(),
            best_block: best_block.number,
            best_header: best_block.clone(),
            headers: LimitedHashSet::new(STORAGE_CACHE_NUM),
            hash_to_number: LimitedHashSet::new(STORAGE_CACHE_NUM),
            bodies: LimitedHashSet::new(STORAGE_CACHE_NUM),
            best_finalized_hash,
            best_safe_hash,
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
    /// Tracks hash of best finalized block
    pub(crate) best_finalized_hash: B256,
    /// Tracks hash of best safe block
    pub(crate) best_safe_hash: B256,
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

    /// Inserts new finalized and safe hash
    pub(crate) fn insert_finalized_and_safe_hash(&mut self, finalized: B256, safe: B256) {
        self.best_finalized_hash = finalized;
        self.best_safe_hash = safe;
    }

    /// Cleans the caches
    pub(crate) fn clean_caches(&mut self) {
        self.headers = LimitedHashSet::new(STORAGE_CACHE_NUM);
        self.hash_to_number = LimitedHashSet::new(STORAGE_CACHE_NUM);
        self.bodies = LimitedHashSet::new(STORAGE_CACHE_NUM);
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
    use alloy_primitives::Sealable;
    use reth_primitives::SealedHeader;

    use super::*;

    #[test]
    fn test_inner_storage() {
        let default_block = SealedHeader::default();
        let mut storage = StorageInner {
            best_hash: default_block.hash_slow(),
            best_block: default_block.number,
            best_header: default_block.clone(),
            headers: LimitedHashSet::new(10),
            hash_to_number: LimitedHashSet::new(10),
            bodies: LimitedHashSet::new(10),
            best_finalized_hash: B256::default(),
            best_safe_hash: B256::default(),
        };
        storage.headers.put(default_block.number, default_block.clone());
        storage.hash_to_number.put(default_block.hash_slow(), default_block.number);

        let block = SealedHeader::default();
        storage.insert_new_block(block.clone(), BlockBody::default());
        assert_eq!(storage.best_block, block.number);
        assert_eq!(storage.best_hash, block.hash_slow());
        assert_eq!(storage.best_header, block);
        assert_eq!(storage.headers.get(&block.number), Some(&block));
        assert_eq!(storage.hash_to_number.get(&block.hash_slow()), Some(&block.number));
        assert_eq!(storage.bodies.get(&block.hash_slow()), Some(&BlockBody::default()));
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Hash(block.hash_slow())),
            Some(block.clone())
        );
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Number(block.number)),
            Some(block.clone())
        );
        assert_eq!(storage.best_block, block.number);
        assert_eq!(storage.best_hash, block.hash_slow());
        assert_eq!(storage.best_header, block);

        let header = SealedHeader::default();
        storage.insert_new_header(header.clone());
        assert_eq!(storage.best_block, header.number);
        assert_eq!(storage.best_hash, header.hash_slow());
        assert_eq!(storage.best_header, header);
        assert_eq!(storage.headers.get(&header.number), Some(&header));
        assert_eq!(storage.hash_to_number.get(&header.hash_slow()), Some(&header.number));
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Hash(header.hash_slow())),
            Some(header.clone())
        );
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Number(header.number)),
            Some(header.clone())
        );
        assert_eq!(storage.best_block, header.number);
        assert_eq!(storage.best_hash, header.hash_slow());
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

    #[test]
    fn test_clean_cache() {
        let default_block = SealedHeader::default();
        let mut storage = StorageInner {
            best_hash: default_block.hash_slow(),
            best_block: default_block.number,
            best_header: default_block.clone(),
            headers: LimitedHashSet::new(10),
            hash_to_number: LimitedHashSet::new(10),
            bodies: LimitedHashSet::new(10),
            best_finalized_hash: B256::default(),
            best_safe_hash: B256::default(),
        };
        storage.headers.put(default_block.number, default_block.clone());
        storage.hash_to_number.put(default_block.hash_slow(), default_block.number);

        let block = SealedHeader::default();
        storage.insert_new_block(block.clone(), BlockBody::default());
        assert_eq!(storage.best_block, block.number);
        assert_eq!(storage.best_hash, block.hash_slow());
        assert_eq!(storage.best_header, block);
        assert_eq!(storage.headers.get(&block.number), Some(&block));
        assert_eq!(storage.hash_to_number.get(&block.hash_slow()), Some(&block.number));
        assert_eq!(storage.bodies.get(&block.hash_slow()), Some(&BlockBody::default()));
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Hash(block.hash_slow())),
            Some(block.clone())
        );
        assert_eq!(
            storage.header_by_hash_or_number(BlockHashOrNumber::Number(block.number)),
            Some(block.clone())
        );
        assert_eq!(storage.best_block, block.number);
        assert_eq!(storage.best_hash, block.hash_slow());
        assert_eq!(storage.best_header, block);

        storage.clean_caches();
        assert_eq!(storage.headers.get(&block.number), None);
        assert_eq!(storage.hash_to_number.get(&block.hash_slow()), None);
        assert_eq!(storage.bodies.get(&block.hash_slow()), None);
    }
}
