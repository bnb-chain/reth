use std::collections::HashMap;

use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use thiserror::Error;
use tracing::trace;

use reth_db::database::Database;
use reth_interfaces::trie::StorageRootError;
use reth_primitives::{Address, B256, U256};
use reth_primitives::revm_primitives::{AccountInfo, Bytecode};
use reth_provider::{DatabaseProviderFactory, ProviderError, providers::ConsistentDbView};
use reth_trie::{HashedPostState, StorageRoot};
use reth_trie::hashed_cursor::{HashedCursorFactory, HashedPostStateCursorFactory};
use reth_trie::metrics::TrieRootMetrics;
use reth_trie::node_iter::{AccountNode, AccountNodeIter};
use reth_trie::stats::TrieTracker;
use reth_trie::trie_cursor::TrieCursorFactory;
use reth_trie::walker::TrieWalker;
use reth_trie_parallel::StorageRootTargets;

pub trait Prefetch {
    type Error;

    fn prefetch(self, hashed_state: HashedPostState) -> Result<usize, Self::Error>;
}

#[derive(Debug, Clone)]
pub struct TriePrefetcher<DB, Provider> {
    /// Consistent view of the database.
    view: ConsistentDbView<DB, Provider>,
    /// State trie metrics.
    #[cfg(feature = "metrics")]
    metrics: TrieRootMetrics,
}

impl<DB, Provider> TriePrefetcher<DB, Provider> {
    /// Create new trie prefetcher.
    pub fn new(view: ConsistentDbView<DB, Provider>) -> Self {
        Self {
            view,
            #[cfg(feature = "metrics")]
            metrics: TrieRootMetrics::default(),
        }
    }
}

impl<DB, Provider> TriePrefetcher<DB, Provider>
where
    DB: Database,
    Provider: DatabaseProviderFactory<DB> + Send + Sync,
{
    pub fn prefetch(self, hashed_state: HashedPostState) -> Result<usize, TriePrefetchError> {
        let mut tracker = TrieTracker::default();
        let prefix_sets = hashed_state.construct_prefix_sets();
        let storage_root_targets = StorageRootTargets::new(
            hashed_state.accounts.keys().copied(),
            prefix_sets.storage_prefix_sets,
        );
        let hashed_state_sorted = hashed_state.into_sorted();

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

        trace!(target: "trie::trie_prefetcher", "prefetching trie root");

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

        let mut storage_nodes_walked = 0usize;
        while let Some(node) = account_node_iter.try_next().map_err(ProviderError::Database)? {
            match node {
                AccountNode::Branch(node) => {
                    tracker.inc_branch();
                }
                AccountNode::Leaf(hashed_address, account) => {
                    let entries_number = match storage_roots.remove(&hashed_address) {
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
                    storage_nodes_walked += entries_number;
                    tracker.inc_leaf();
                }
            }
        }

        let stats = tracker.finish();

        trace!(
            target: "trie::trie_prefetcher",
            duration = ?stats.duration(),
            branches_added = stats.branches_added(),
            leaves_added = stats.leaves_added(),
            "prefetched trie root"
        );

        let trie_nodes_walked =
            (stats.leaves_added() + stats.branches_added()) as usize + storage_nodes_walked;
        Ok(trie_nodes_walked)
    }
}

/// Error during trie prefetch.
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
