use std::collections::HashMap;
use std::sync::Arc;

use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use thiserror::Error;
use tracing::trace;

use reth_db::database::Database;
use reth_interfaces::trie::StorageRootError;
use reth_provider::{DatabaseProviderFactory, ProviderError, providers::ConsistentDbView};
use reth_trie::{HashedPostState, StorageRoot};
use reth_trie::hashed_cursor::{HashedCursorFactory, HashedPostStateCursorFactory};
use reth_trie::metrics::TrieRootMetrics;
use reth_trie::node_iter::{AccountNode, AccountNodeIter};
use reth_trie::stats::TrieTracker;
use reth_trie::trie_cursor::TrieCursorFactory;
use reth_trie::walker::TrieWalker;
use reth_trie_parallel::StorageRootTargets;
use reth_tasks::pool::BlockingTaskPool;
use rayon::ThreadPoolBuilder;

/// Prefetch trie storage when executing transactions.
#[derive(Debug, Clone)]
pub struct TriePrefetch<DB, Provider> {
    /// Consistent view of the database.
    view: ConsistentDbView<DB, Provider>,
    /// Blocking task pool.
    blocking_pool: BlockingTaskPool,
    /// State trie metrics.
    #[cfg(feature = "metrics")]
    metrics: TrieRootMetrics,
}

impl<DB, Provider> TriePrefetch<DB, Provider>
where
    DB: Database + 'static,
    Provider: DatabaseProviderFactory<DB> + Send + Sync + 'static
{
    /// Create new TriePrefetch instance.
    pub fn new(view: ConsistentDbView<DB, Provider>) -> Self {
        let blocking_pool = BlockingTaskPool::new(ThreadPoolBuilder::default().build().unwrap());
        Self {
            view,
            blocking_pool,
            #[cfg(feature = "metrics")]
            metrics: TrieRootMetrics::default(),
        }
    }

    /// Spawn an async task to prefetch trie storage for the given hashed state.
    pub fn prefetch(self: Arc<Self>, hashed_state: HashedPostState) -> Result<(), TriePrefetchError> {
        let self_clone = Arc::clone(&self);
        let _ = self.blocking_pool.spawn_fifo(move || -> Result<(), TriePrefetchError> {
            self_clone.prefetch_once(hashed_state)
        });
        Ok(())
    }

    /// Prefetch trie storage for the given hashed state.
    fn prefetch_once(&self, hashed_state: HashedPostState) -> Result<(), TriePrefetchError> {
        let mut tracker = TrieTracker::default();
        let prefix_sets = hashed_state.construct_prefix_sets();
        let storage_root_targets = StorageRootTargets::new(
            hashed_state.accounts.keys().copied(),
            prefix_sets.storage_prefix_sets,
        );
        let hashed_state_sorted = hashed_state.into_sorted();

        trace!(target: "trie::trie_prefetch", "start prefetching trie storages");
        let mut storage_roots = storage_root_targets
            .into_par_iter()
            .map(|(hashed_address, prefix_set)| {
                let provider_ro = self.view.provider_ro()?;
                let entries_number = StorageRoot::new_hashed(
                    provider_ro.tx_ref(),
                    HashedPostStateCursorFactory::new(provider_ro.tx_ref(), &hashed_state_sorted),
                    hashed_address,
                    #[cfg(feature = "metrics")]
                    self.metrics.clone(),
                )
                .with_prefix_set(prefix_set)
                .prefetch();
                Ok((hashed_address, entries_number?))
            })
            .collect::<Result<HashMap<_, _>, TriePrefetchError>>()?;

        trace!(target: "trie::trie_prefetch", "prefetching account tries");
        let provider_ro = self.view.provider_ro()?;
        let hashed_cursor_factory =
            HashedPostStateCursorFactory::new(provider_ro.tx_ref(), &hashed_state_sorted);
        let trie_cursor_factory = provider_ro.tx_ref();

        let hashed_account_cursor =
            hashed_cursor_factory.hashed_account_cursor().map_err(ProviderError::Database)?;
        let trie_cursor =
            trie_cursor_factory.account_trie_cursor().map_err(ProviderError::Database)?;

        let walker =
            TrieWalker::new(trie_cursor, prefix_sets.account_prefix_set).with_updates(false);
        let mut account_node_iter = AccountNodeIter::new(walker, hashed_account_cursor);

        while let Some(node) = account_node_iter.try_next().map_err(ProviderError::Database)? {
            match node {
                AccountNode::Branch(_) => {
                    tracker.inc_branch();
                }
                AccountNode::Leaf(hashed_address, _) => {
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
                        .prefetch()?,
                    };
                    tracker.inc_leaf();
                }
            }
        }

        let stats = tracker.finish();

        trace!(
            target: "trie::trie_prefetch",
            duration = ?stats.duration(),
            branches_added = stats.branches_added(),
            leaves_added = stats.leaves_added(),
            "prefetched trie storages"
        );

        Ok(())
    }
}

/// Error during prefetching trie storage.
#[derive(Error, Debug)]
pub enum TriePrefetchError {
    /// Error while calculating storage root.
    #[error(transparent)]
    StorageRoot(#[from] StorageRootError),
    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

impl From<TriePrefetchError> for ProviderError {
    fn from(error: TriePrefetchError) -> Self {
        match error {
            TriePrefetchError::Provider(error) => error,
            TriePrefetchError::StorageRoot(StorageRootError::DB(error)) => {
                ProviderError::Database(error)
            }
        }
    }
}