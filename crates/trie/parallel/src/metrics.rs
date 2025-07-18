use crate::stats::ParallelTrieStats;
use metrics::{Counter, Histogram};
use reth_metrics::Metrics;
use reth_trie::{metrics::TrieRootMetrics, TrieType};

/// Parallel state root metrics.
#[derive(Debug)]
pub struct ParallelStateRootMetrics {
    /// State trie metrics.
    pub state_trie: TrieRootMetrics,
    /// Parallel trie metrics.
    pub parallel: ParallelTrieMetrics,
    /// Storage trie metrics.
    pub storage_trie: TrieRootMetrics,
}

impl Default for ParallelStateRootMetrics {
    fn default() -> Self {
        Self {
            state_trie: TrieRootMetrics::new(TrieType::State),
            parallel: ParallelTrieMetrics::new_with_labels(&[("type", "root")]),
            storage_trie: TrieRootMetrics::new(TrieType::Storage),
        }
    }
}

impl ParallelStateRootMetrics {
    /// Record state trie metrics
    pub fn record_state_trie(&self, stats: ParallelTrieStats) {
        self.state_trie.record(stats.trie_stats());
        self.parallel.record(stats);
    }
}

/// Parallel state root metrics.
#[derive(Metrics)]
#[metrics(scope = "trie_parallel")]
pub struct ParallelTrieMetrics {
    /// The number of storage roots computed in parallel.
    pub precomputed_storage_roots: Histogram,
    /// The number of leaves for which we did not pre-compute the storage roots.
    pub missed_leaves: Histogram,

    /// The number of parallel storage roots computed.
    pub parallel_storage_count: Counter,
    /// The time it takes to parallel storage root calculation.
    pub parallel_storage_duration: Histogram,
    /// The time it takes to sync account root calculation.
    pub sync_account_duration: Histogram,
    /// The number of storage roots recalculated.
    pub storage_recalc: Counter,
    /// The number of storage roots recalculated.
    pub storage_recalc_duration: Histogram,
    /// The time it takes to sync account root calculation.
    pub account_rlp_duration: Histogram,
}

impl ParallelTrieMetrics {
    /// Record parallel trie metrics.
    pub fn record(&self, stats: ParallelTrieStats) {
        self.precomputed_storage_roots.record(stats.precomputed_storage_roots() as f64);
        self.missed_leaves.record(stats.missed_leaves() as f64);
    }
}
