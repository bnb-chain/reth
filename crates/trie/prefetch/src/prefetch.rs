use rayon::prelude::*;
use reth_db::database::Database;
use reth_execution_errors::StorageRootError;
use reth_primitives::B256;
use reth_provider::{providers::ConsistentDbView, ProviderError, ProviderFactory};
use reth_trie::{
    hashed_cursor::{HashedCursorFactory, HashedPostStateCursorFactory},
    metrics::TrieRootMetrics,
    node_iter::{TrieElement, TrieNodeIter},
    stats::TrieTracker,
    trie_cursor::TrieCursorFactory,
    walker::TrieWalker,
    HashedPostState, HashedStorage, StorageRoot,
};
use reth_trie_parallel::{parallel_root::ParallelStateRootError, StorageRootTargets};
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot::Receiver, watch};
use tracing::{debug, trace};

/// Prefetch trie storage when executing transactions.
#[derive(Debug, Clone)]
pub struct TriePrefetch {
    /// Cached accounts.
    cached_accounts: HashMap<B256, bool>,
    /// Cached storages.
    cached_storages: HashMap<B256, HashMap<B256, bool>>,
    /// State trie metrics.
    #[cfg(feature = "metrics")]
    metrics: TrieRootMetrics,
}

impl Default for TriePrefetch {
    fn default() -> Self {
        Self::new()
    }
}

impl TriePrefetch {
    /// Create new `TriePrefetch` instance.
    pub fn new() -> Self {
        Self {
            cached_accounts: HashMap::new(),
            cached_storages: HashMap::new(),
            #[cfg(feature = "metrics")]
            metrics: TrieRootMetrics::default(),
        }
    }

    /// Run the prefetching task.
    pub async fn run<DB>(
        &mut self,
        consistent_view: Arc<ConsistentDbView<DB, ProviderFactory<DB>>>,
        mut prefetch_rx: UnboundedReceiver<HashedPostState>,
        mut interrupt_rx: Receiver<()>,
    ) where
        DB: Database + 'static,
    {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut count = 0u64;
        loop {
            tokio::select! {
                hashed_state = prefetch_rx.recv() => {
                    if let Some(hashed_state) = hashed_state {
                        count += 1;

                        let consistent_view = Arc::clone(&consistent_view);
                        let hashed_state = self.deduplicate_and_update_cached(&hashed_state);

                        let self_clone = Arc::new(self.clone());
                        let mut shutdown_rx = shutdown_rx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = self_clone.prefetch_once::<DB>(consistent_view, hashed_state, count, &mut shutdown_rx).await {
                                debug!(target: "trie::trie_prefetch", ?e, "Error while prefetching trie storage");
                            };
                        });
                    }
                }
                _ = &mut interrupt_rx => {
                    debug!(target: "trie::trie_prefetch", "Interrupted trie prefetch task. Processed {:?}, left {:?}", count, prefetch_rx.len());
                    let _ = shutdown_tx.send(true);
                    return
                }
            }
        }
    }

    /// Deduplicate `hashed_state` based on `cached` and update `cached`.
    fn deduplicate_and_update_cached(&mut self, hashed_state: &HashedPostState) -> HashedPostState {
        let mut new_hashed_state = HashedPostState::default();

        // deduplicate accounts if their keys are not present in storages
        for (address, account) in &hashed_state.accounts {
            if !hashed_state.storages.contains_key(address) &&
                !self.cached_accounts.contains_key(address)
            {
                self.cached_accounts.insert(*address, true);
                new_hashed_state.accounts.insert(*address, *account);
            }
        }

        // deduplicate storages
        for (address, storage) in &hashed_state.storages {
            let cached_entry = self.cached_storages.entry(*address).or_default();

            // Collect the keys to be added to `new_storage` after filtering
            let keys_to_add: Vec<_> = storage
                .storage
                .iter()
                .filter(|(slot, _)| !cached_entry.contains_key(*slot))
                .map(|(slot, _)| *slot)
                .collect();

            // Iterate over `keys_to_add` to update `cached_entry` and `new_storage`
            let new_storage: HashMap<_, _> = keys_to_add
                .into_iter()
                .map(|slot| {
                    cached_entry.insert(slot, true);
                    (slot, *storage.storage.get(&slot).unwrap())
                })
                .collect();

            if !new_storage.is_empty() {
                new_hashed_state
                    .storages
                    .insert(*address, HashedStorage::from_iter(false, new_storage.into_iter()));

                if let Some(account) = hashed_state.accounts.get(address) {
                    new_hashed_state.accounts.insert(*address, *account);
                }
            }
        }

        new_hashed_state
    }

    /// Prefetch trie storage for the given hashed state.
    pub async fn prefetch_once<DB>(
        self: Arc<Self>,
        consistent_view: Arc<ConsistentDbView<DB, ProviderFactory<DB>>>,
        hashed_state: HashedPostState,
        count: u64,
        shutdown_rx: &mut watch::Receiver<bool>,
    ) -> Result<(), TriePrefetchError>
    where
        DB: Database,
    {
        debug!("prefetch once started {:?}", count);

        let mut tracker = TrieTracker::default();

        let prefix_sets = hashed_state.construct_prefix_sets().freeze();
        let storage_root_targets = StorageRootTargets::new(
            hashed_state.accounts.keys().copied(),
            prefix_sets.storage_prefix_sets,
        );
        let hashed_state_sorted = hashed_state.into_sorted();

        trace!(target: "trie::trie_prefetch", "start prefetching trie storages");
        let mut storage_roots = storage_root_targets
            .into_par_iter()
            .map(|(hashed_address, prefix_set)| {
                if *shutdown_rx.borrow() {
                    return Ok((B256::ZERO, 0)); // return early if shutdown
                }

                let provider_ro = consistent_view.provider_ro()?;

                let storage_root_result = StorageRoot::new_hashed(
                    provider_ro.tx_ref(),
                    HashedPostStateCursorFactory::new(provider_ro.tx_ref(), &hashed_state_sorted),
                    hashed_address,
                    #[cfg(feature = "metrics")]
                    self.metrics.clone(),
                )
                .with_prefix_set(prefix_set)
                .prefetch();

                Ok((hashed_address, storage_root_result?))
            })
            .collect::<Result<HashMap<_, _>, ParallelStateRootError>>()?;

        if *shutdown_rx.borrow() {
            return Ok(()); // return early if shutdown
        }

        trace!(target: "trie::trie_prefetch", "prefetching account tries");
        let provider_ro = consistent_view.provider_ro()?;
        let hashed_cursor_factory =
            HashedPostStateCursorFactory::new(provider_ro.tx_ref(), &hashed_state_sorted);
        let trie_cursor_factory = provider_ro.tx_ref();

        let walker = TrieWalker::new(
            trie_cursor_factory.account_trie_cursor().map_err(ProviderError::Database)?,
            prefix_sets.account_prefix_set,
        )
        .with_deletions_retained(false);
        let mut account_node_iter = TrieNodeIter::new(
            walker,
            hashed_cursor_factory.hashed_account_cursor().map_err(ProviderError::Database)?,
        );

        while let Some(node) = account_node_iter.try_next().map_err(ProviderError::Database)? {
            if *shutdown_rx.borrow() {
                return Ok(()); // return early if shutdown
            }

            match node {
                TrieElement::Branch(_) => {
                    tracker.inc_branch();
                }
                TrieElement::Leaf(hashed_address, _) => {
                    match storage_roots.remove(&hashed_address) {
                        Some(result) => result,
                        // Since we do not store all intermediate nodes in the database, there might
                        // be a possibility of re-adding a non-modified leaf to the hash builder.
                        None => StorageRoot::new_hashed(
                            trie_cursor_factory,
                            hashed_cursor_factory.clone(),
                            hashed_address,
                            #[cfg(feature = "metrics")]
                            self.metrics.clone(),
                        )
                        .prefetch()
                        .ok()
                        .unwrap_or_default(),
                    };
                    tracker.inc_leaf();
                }
            }
        }

        let stats = tracker.finish();

        #[cfg(feature = "metrics")]
        self.metrics.record(stats);

        trace!(
            target: "trie::trie_prefetch",
            duration = ?stats.duration(),
            branches_added = stats.branches_added(),
            leaves_added = stats.leaves_added(),
            "prefetched account trie"
        );

        debug!("prefetch once finished {:?}", count);

        Ok(())
    }
}

/// Error during prefetching trie storage.
#[derive(Error, Debug)]
pub enum TriePrefetchError {
    /// Error while calculating storage root.
    #[error(transparent)]
    StorageRoot(#[from] StorageRootError),
    /// Error while calculating parallel storage root.
    #[error(transparent)]
    ParallelStateRoot(#[from] ParallelStateRootError),
    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

impl From<TriePrefetchError> for ProviderError {
    fn from(error: TriePrefetchError) -> Self {
        match error {
            TriePrefetchError::Provider(error) => error,
            TriePrefetchError::StorageRoot(StorageRootError::DB(error)) => Self::Database(error),
            TriePrefetchError::ParallelStateRoot(error) => error.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time;

    #[tokio::test]
    async fn test_channel() {
        let (prefetch_tx, mut prefetch_rx) = tokio::sync::mpsc::unbounded_channel();
        let (interrupt_tx, mut interrupt_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = prefetch_rx.recv() => {
                        println!("got message");
                        time::sleep(time::Duration::from_secs(3)).await;
                    }
                    _ = &mut interrupt_rx => {
                        println!("left items in channel: {}" ,prefetch_rx.len());
                        break;
                    }
                }
            }
        });

        for _ in 0..10 {
            prefetch_tx.send(()).unwrap();
        }

        time::sleep(time::Duration::from_secs(3)).await;

        interrupt_tx.send(()).unwrap();

        time::sleep(time::Duration::from_secs(10)).await;
    }
}
