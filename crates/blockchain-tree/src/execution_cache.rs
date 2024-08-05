use std::{collections::HashMap, num::NonZeroUsize};

use lazy_static::lazy_static;
use lru::LruCache;
use metrics::Counter;
use parking_lot::RwLock;
use tracing::debug;

use quick_cache::sync::Cache;
use reth_metrics::Metrics;
use reth_primitives::{Account, Address, BlockNumber, Bytecode, StorageKey, StorageValue, B256};
use reth_provider::{
    AccountReader, BlockHashReader, ExecutionDataProvider, StateProofProvider, StateProvider,
    StateRootProvider,
};
use reth_revm::db::BundleState;
use reth_storage_errors::provider::ProviderResult;
use reth_trie::{updates::TrieUpdates, AccountProof};
/// The size of cache, counted by the number of accounts.
const CACHE_SIZE: usize = 10240;

lazy_static! {
    /// Account cache
    static ref ACCOUNT_CACHE: Cache<Address, Account> = Cache::new(CACHE_SIZE);

    /// Storage cache
    static ref STORAGE_CACHE: Cache<Address, HashMap<StorageKey, StorageValue>> = Cache::new(CACHE_SIZE);
}

/// Metrics for cache.
#[derive(Metrics)]
#[metrics(scope = "blockchain_tree.cache")]
pub(crate) struct CacheMetrics {
    /// Total account access count
    pub(crate) account_access_total: Counter,
    /// Total account access cache hit count
    pub(crate) account_cache_hit_total: Counter,
    /// Total storage access count
    pub(crate) storage_access_total: Counter,
    /// Total storage access cache hit count
    pub(crate) storage_cache_hit_total: Counter,
}

pub(crate) fn apply_bundle_state_to_cache(bundle: BundleState) {
    let change_set = bundle.into_plain_state(reth_provider::OriginalValuesKnown::Yes);

    for (address, account_info) in change_set.accounts.iter() {
        match account_info {
            None => {
                ACCOUNT_CACHE.remove(address);
                STORAGE_CACHE.remove(address);
            }
            Some(acc) => {
                ACCOUNT_CACHE.insert(
                    *address,
                    Account {
                        nonce: acc.nonce,
                        balance: acc.balance,
                        bytecode_hash: Some(acc.code_hash),
                    },
                );
            }
        }
    }

    for storage in change_set.storage.iter() {
        if storage.wipe_storage {
            STORAGE_CACHE.remove(&storage.address);
        } else {
            let mut map = HashMap::new();
            for (k, v) in storage.storage.clone() {
                map.insert(k.into(), v);
            }
            STORAGE_CACHE.insert(storage.address, map);
        }
    }
}

#[derive(Debug)]
pub(crate) struct CachedBundleStateProvider<SP: StateProvider, EDP: ExecutionDataProvider> {
    /// The inner state provider.
    pub state_provider: SP,
    /// Block execution data.
    pub block_execution_data_provider: EDP,
    /// Cache metrics.
    pub cache_metrics: CacheMetrics,
}

impl<SP: StateProvider, EDP: ExecutionDataProvider> CachedBundleStateProvider<SP, EDP> {
    /// Create new cached bundle state provider
    pub(crate) fn new(state_provider: SP, block_execution_data_provider: EDP) -> Self {
        Self {
            state_provider,
            block_execution_data_provider,
            cache_metrics: CacheMetrics::default(),
        }
    }
}

impl<SP: StateProvider, EDP: ExecutionDataProvider> BlockHashReader
    for CachedBundleStateProvider<SP, EDP>
{
    fn block_hash(&self, block_number: BlockNumber) -> ProviderResult<Option<B256>> {
        let block_hash = self.block_execution_data_provider.block_hash(block_number);
        if block_hash.is_some() {
            return Ok(block_hash)
        }
        self.state_provider.block_hash(block_number)
    }

    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        unimplemented!()
    }
}

impl<SP: StateProvider, EDP: ExecutionDataProvider> AccountReader
    for CachedBundleStateProvider<SP, EDP>
{
    fn basic_account(&self, address: Address) -> ProviderResult<Option<Account>> {
        if let Some(account) =
            self.block_execution_data_provider.execution_outcome().account(&address)
        {
            Ok(account)
        } else {
            self.cache_metrics.account_access_total.increment(1);

            let cached = ACCOUNT_CACHE.get(&address);
            return match cached {
                Some(account) => {
                    debug!(target: "blockchain_tree", address = ?address.to_string(), "Hit blockchain tree account cache");
                    self.cache_metrics.account_cache_hit_total.increment(1);
                    Ok(Some(account))
                }
                None => {
                    let db_value = AccountReader::basic_account(&self.state_provider, address);
                    match db_value {
                        Ok(account) => {
                            if let Some(_) = account {
                                ACCOUNT_CACHE.insert(address, account.unwrap());
                                debug!(target: "blockchain_tree", address = ?address.to_string(), "Add blockchain tree account cache");
                            }
                            Ok(account)
                        }
                        Err(err) => Err(err.into()),
                    }
                }
            }
        }
    }
}

impl<SP: StateProvider, EDP: ExecutionDataProvider> StateRootProvider
    for CachedBundleStateProvider<SP, EDP>
{
    fn state_root(&self, bundle_state: &BundleState) -> ProviderResult<B256> {
        let mut state = self.block_execution_data_provider.execution_outcome().state().clone();
        state.extend(bundle_state.clone());
        self.state_provider.state_root(&state)
    }

    fn state_root_with_updates(
        &self,
        bundle_state: &BundleState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let mut state = self.block_execution_data_provider.execution_outcome().state().clone();
        state.extend(bundle_state.clone());
        self.state_provider.state_root_with_updates(&state)
    }
}

impl<SP: StateProvider, EDP: ExecutionDataProvider> StateProofProvider
    for CachedBundleStateProvider<SP, EDP>
{
    fn proof(
        &self,
        bundle_state: &BundleState,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        let mut state = self.block_execution_data_provider.execution_outcome().state().clone();
        state.extend(bundle_state.clone());
        self.state_provider.proof(&state, address, slots)
    }
}

impl<SP: StateProvider, EDP: ExecutionDataProvider> StateProvider
    for CachedBundleStateProvider<SP, EDP>
{
    fn storage(
        &self,
        account: Address,
        storage_key: reth_primitives::StorageKey,
    ) -> ProviderResult<Option<reth_primitives::StorageValue>> {
        let u256_storage_key = storage_key.into();
        if let Some(value) = self
            .block_execution_data_provider
            .execution_outcome()
            .storage(&account, u256_storage_key)
        {
            return Ok(Some(value))
        }

        self.cache_metrics.storage_access_total.increment(1);

        let mut cached = STORAGE_CACHE.get(&account).unwrap_or_else(|| HashMap::new());

        if let Some(v) = cached.get(&storage_key) {
            debug!(target: "blockchain_tree", address = ?account.to_string(), storage_key = ?storage_key, "Hit blockchain tree storage cache");
            self.cache_metrics.storage_cache_hit_total.increment(1);
            return Ok(Some(*v))
        }

        if let Some(value) = StateProvider::storage(&self.state_provider, account, storage_key)? {
            cached.insert(storage_key, value);
            STORAGE_CACHE.insert(account, cached);
            debug!(target: "blockchain_tree", address = ?account.to_string(), storage_key = ?storage_key, "Add blockchain tree cache case");
            return Ok(Some(value))
        }
        Ok(None)
    }

    fn bytecode_by_hash(&self, code_hash: B256) -> ProviderResult<Option<Bytecode>> {
        if let Some(bytecode) =
            self.block_execution_data_provider.execution_outcome().bytecode(&code_hash)
        {
            return Ok(Some(bytecode))
        }

        self.state_provider.bytecode_by_hash(code_hash)
    }
}
