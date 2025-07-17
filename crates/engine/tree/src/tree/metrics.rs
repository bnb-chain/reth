use reth_evm::metrics::ExecutorMetrics;
use reth_metrics::{
    metrics::{Counter, Gauge, Histogram},
    Metrics,
};
use reth_trie::updates::TrieUpdates;

/// Metrics for the `EngineApi`.
#[derive(Debug, Default)]
pub(crate) struct EngineApiMetrics {
    /// Engine API-specific metrics.
    pub(crate) engine: EngineMetrics,
    /// Block executor metrics.
    pub(crate) executor: ExecutorMetrics,
    /// Metrics for block validation
    pub(crate) block_validation: BlockValidationMetrics,
    /// A copy of legacy blockchain tree metrics, to be replaced when we replace the old tree
    pub(crate) tree: TreeMetrics,
}

/// Metrics for the entire blockchain tree
#[derive(Metrics)]
#[metrics(scope = "blockchain_tree")]
pub(super) struct TreeMetrics {
    /// The highest block number in the canonical chain
    pub canonical_chain_height: Gauge,
    /// The number of reorgs
    pub reorgs: Counter,
    /// The latest reorg depth
    pub latest_reorg_depth: Gauge,
}

/// Metrics for the `EngineApi`.
#[derive(Metrics)]
#[metrics(scope = "consensus.engine.beacon")]
pub(crate) struct EngineMetrics {
    /// How many executed blocks are currently stored.
    pub(crate) executed_blocks: Gauge,
    /// How many already executed blocks were directly inserted into the tree.
    pub(crate) inserted_already_executed_blocks: Counter,
    /// The number of times the pipeline was run.
    pub(crate) pipeline_runs: Counter,
    /// The total count of forkchoice updated messages received.
    pub(crate) forkchoice_updated_messages: Counter,
    /// The total count of new payload messages received.
    pub(crate) new_payload_messages: Counter,
    /// Histogram of persistence operation durations (in seconds)
    pub(crate) persistence_duration: Histogram,
    /// Tracks the how often we failed to deliver a newPayload response.
    ///
    /// This effectively tracks how often the message sender dropped the channel and indicates a CL
    /// request timeout (e.g. it took more than 8s to send the response and the CL terminated the
    /// request which resulted in a closed channel).
    pub(crate) failed_new_payload_response_deliveries: Counter,
    /// Tracks the how often we failed to deliver a forkchoice update response.
    pub(crate) failed_forkchoice_updated_response_deliveries: Counter,
    
    /// Overall block processing duration (from start to finish)
    pub(crate) block_total_duration: Histogram,
    /// Overall Block execution duration during live sync
    pub(crate) block_execution_duration: Histogram,
    /// Overall validation duration (from start to finish, excluding execution)
    pub(crate) block_validation_duration: Histogram,
}

/// Metrics for non-execution related block validation.
#[derive(Metrics)]
#[metrics(scope = "sync.block_validation")]
pub(crate) struct BlockValidationMetrics {
    /// Total number of storage tries updated in the state root calculation
    pub(crate) state_root_storage_tries_updated_total: Counter,
    /// Total number of times the parallel state root computation fell back to regular.
    pub(crate) state_root_parallel_fallback_total: Counter,
    /// Trie input computation duration
    pub(crate) trie_input_duration: Histogram,

    /// Total number of background parallel state root tasks (historical cumulative)
    #[allow(dead_code)]
    pub background_parallel_state_root_tasks: Counter,
    /// Total number of foreground parallel state root tasks (historical cumulative)
    #[allow(dead_code)]
    pub foreground_parallel_state_root_tasks: Counter,
    /// Total number of background cache tasks (historical cumulative)
    #[allow(dead_code)]
    pub background_cache_tasks: Counter,
    /// Total number of foreground sync state root computations (historical cumulative)
    #[allow(dead_code)]
    pub faillback_sync_state_root_tasks: Counter,

    /// State root computation duration (parallel)
    pub state_root_parallel_duration: Histogram,
    /// State root computation duration (serial)
    pub state_root_serial_duration: Histogram,
}

impl BlockValidationMetrics {
    /// Records comprehensive state root computation metrics for live sync
    /// This includes both the original metrics and the new live sync performance metrics
    pub(crate) fn record_state_root(
        &self,
        trie_output: &TrieUpdates,
        elapsed_as_secs: f64,
        is_parallel: bool,
    ) {
        // Record storage tries count
        self.state_root_storage_tries_updated_total
            .increment(trie_output.storage_tries_ref().len() as u64);

        // Record live sync specific metrics
        if is_parallel {
            self.state_root_parallel_duration.record(elapsed_as_secs);
        } else {
            self.state_root_serial_duration.record(elapsed_as_secs);
        }
    }
}

/// Metrics for the blockchain tree block buffer
#[derive(Metrics)]
#[metrics(scope = "blockchain_tree.block_buffer")]
pub(crate) struct BlockBufferMetrics {
    /// Total blocks in the block buffer
    pub blocks: Gauge,
}
