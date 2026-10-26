//! Bounded, per-root progress diagnostics. No channel or provider is retained here.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Instant,
};

/// Fixed progress slots; values are counters, except the explicitly named timestamp slots.
#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum Progress {
    /// Updates accepted by the input channel.
    UpdatesSent,
    /// Updates converted by the hashing task.
    UpdatesHashed,
    /// Hashed updates consumed by the root task.
    UpdatesReceived,
    /// End marker accepted by the input channel, in microseconds since creation plus one.
    FinishSentUs,
    /// End marker read by the hashing task, in microseconds since creation plus one.
    FinishHashedUs,
    /// End marker read by the root task, in microseconds since creation plus one.
    FinishReceivedUs,
    /// Proof batches dispatched.
    ProofBatchesSent,
    /// Proof batches received (including errors).
    ProofBatchesReceived,
    /// Storage worker threads entered for this session.
    StorageWorkersStarted,
    /// Storage worker threads exited for this session.
    StorageWorkersExited,
    /// Account worker threads entered for this session.
    AccountWorkersStarted,
    /// Account worker threads exited for this session.
    AccountWorkersExited,
    /// Storage providers and cursors initialized, ready to receive jobs.
    StorageWorkersReady,
    /// Account providers and cursors initialized, ready to receive jobs.
    AccountWorkersReady,
    /// Storage proof jobs dequeued.
    StorageJobsStarted,
    /// Storage proof jobs exited, including failures.
    StorageJobsExited,
    /// Account proof jobs dequeued.
    AccountJobsStarted,
    /// Account proof jobs exited, including failures.
    AccountJobsExited,
    /// Number of storage workers requested for this session.
    StorageWorkersRequested,
    /// Number of account workers requested for this session.
    AccountWorkersRequested,
}

/// Shared atomic observations, with no effect on task ownership or cancellation.
#[derive(Debug)]
pub struct RootTaskDiagnostics {
    id: u64,
    origin: &'static str,
    block_number: u64,
    parent_hash: alloy_primitives::B256,
    created: Instant,
    progress: [AtomicU64; 20],
    // Stage timestamps allow a timeout snapshot to distinguish queued from running tasks.
    stages: [AtomicU64; 19],
    sampled: AtomicBool,
    result: AtomicU64,
    // submission/start ages for storage and account sessions, microseconds plus one.
    proof_sessions: [AtomicU64; 4],
}

/// Distinguishes an actual root result from cancellation or another failure.
#[derive(Debug, Clone, Copy)]
#[repr(u64)]
pub enum RootTaskResult {
    /// A root was computed successfully.
    Success = 1,
    /// The consumer cancellation channel ended the task.
    Cancelled = 2,
    /// The task returned another error.
    Failed = 3,
}

/// Lifecycle milestones. Timestamps are relative to this root task, not wall-clock time.
#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum Stage {
    /// Named storage session starts (before broadcasting workers).
    StorageSessionStarted,
    /// All workers of the storage session have exited.
    StorageSessionExited,
    /// Named account session starts (before broadcasting workers).
    AccountSessionStarted,
    /// All workers of the account session have exited.
    AccountSessionExited,
    /// Named sparse-trie closure starts.
    RootStarted,
    /// Root computation returns, before publishing or cleaning up the trie.
    RootComputed,
    /// Sparse-trie closure exits, including unwind and early return.
    RootExited,
    /// Hashing closure starts.
    HashingStarted,
    /// Hashing closure exits.
    HashingExited,
    /// Consumer begins waiting for the root after execution.
    WaitStarted,
    /// The bounded wait expires.
    TimedOut,
    /// Serial fallback has been submitted.
    FallbackSubmitted,
    /// Parallel root wins the race after fallback submission.
    ParallelWon,
    /// Serial fallback wins the race.
    FallbackWon,
    /// Consumer cancellation channel has been closed by its guard.
    ConsumerDropped,
    /// Result send failed because the receiver was already dropped.
    ReceiverDropped,
    /// Preparing the serial fallback provider on the consumer thread.
    FallbackPreparing,
    /// Named serial fallback closure starts.
    FallbackStarted,
    /// Named serial fallback closure exits.
    FallbackExited,
}

impl RootTaskDiagnostics {
    /// Creates diagnostics with a bounded origin label supplied by the caller.
    pub fn new(origin: &'static str) -> Arc<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            origin,
            block_number: 0,
            parent_hash: alloy_primitives::B256::ZERO,
            created: Instant::now(),
            progress: std::array::from_fn(|_| AtomicU64::new(0)),
            stages: std::array::from_fn(|_| AtomicU64::new(0)),
            sampled: AtomicBool::new(false),
            result: AtomicU64::new(0),
            proof_sessions: std::array::from_fn(|_| AtomicU64::new(0)),
        })
    }

    /// Adds block context before sharing the observations with workers.
    pub fn for_block(
        origin: &'static str,
        block_number: u64,
        parent_hash: alloy_primitives::B256,
    ) -> Arc<Self> {
        let mut this = Self::new(origin);
        let inner = Arc::get_mut(&mut this).expect("new diagnostics are unshared");
        inner.block_number = block_number;
        inner.parent_hash = parent_hash;
        this
    }

    /// Process-local identifier for correlating existing debug logs.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Returns the fixed origin label.
    pub const fn origin(&self) -> &'static str {
        self.origin
    }

    /// Submit identity to the actual named scheduler. IDs never become metric labels.
    pub fn proof_session_submitted(&self, kind: &'static str) -> reth_tasks::NamedTaskContext {
        let index = if kind == "storage" { 0 } else { 2 };
        self.proof_sessions[index].store(self.elapsed_us(), Ordering::Relaxed);
        reth_tasks::NamedTaskContext {
            id: self.id,
            origin: self.origin,
            block_number: self.block_number,
        }
    }

    /// Record the actual named closure start, before broadcasting into the proof worker pool.
    pub fn proof_session_started(&self, kind: &'static str) {
        let index = if kind == "storage" { 1 } else { 3 };
        self.proof_sessions[index].store(self.elapsed_us(), Ordering::Relaxed);
    }

    /// Starts at worker.run entry: separate session/broadcast dispatch from provider/cursor init.
    pub fn proof_worker_initializing(&self, kind: &'static str) -> DiagnosticPhase {
        let index = if kind == "storage" { 1 } else { 3 };
        let start = self.proof_sessions[index].load(Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if start != 0 {
            metrics::histogram!("trie.diagnostics.proof_worker_dispatch_seconds", "origin" => self.origin, "kind" => kind)
                .record(self.elapsed_us().saturating_sub(start) as f64 / 1_000_000.0);
        }
        let _ = start;
        self.phase(if kind == "storage" { "storage_init" } else { "account_init" })
    }

    /// Foreground-independent phase timer, recording early error/unwind as well as completion.
    pub fn phase(&self, phase: &'static str) -> DiagnosticPhase {
        DiagnosticPhase { origin: self.origin, phase, started: Instant::now(), completed: false }
    }

    /// End marker has been consumed and all outstanding proof/trie work has drained.
    /// This interval excludes the hashing/root-input queue before the marker was consumed.
    pub fn finish_drained(&self) {
        let start = self.progress[Progress::FinishReceivedUs as usize].load(Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if start != 0 {
            metrics::histogram!("trie.diagnostics.finish_drain_seconds", "origin" => self.origin)
                .record(self.elapsed_us().saturating_sub(start) as f64 / 1_000_000.0);
        }
        let _ = start;
    }

    /// Records a counter without formatting logs or allocating.
    pub fn increment(&self, progress: Progress) {
        self.progress[progress as usize].fetch_add(1, Ordering::Relaxed);
    }

    fn elapsed_us(&self) -> u64 {
        self.created.elapsed().as_micros().min(u64::MAX as u128 - 1) as u64 + 1
    }

    /// Records the requested pool sizes once, before worker dispatch.
    pub fn requested_workers(&self, storage: usize, account: usize) {
        self.progress[Progress::StorageWorkersRequested as usize]
            .store(storage as u64, Ordering::Relaxed);
        self.progress[Progress::AccountWorkersRequested as usize]
            .store(account as u64, Ordering::Relaxed);
    }

    /// Records an end-marker timestamp. Zero means not observed.
    pub fn marker(&self, progress: Progress) {
        let now = self.elapsed_us();
        self.progress[progress as usize].store(now, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if matches!(progress, Progress::FinishHashedUs | Progress::FinishReceivedUs) {
            let (previous, phase) = if matches!(progress, Progress::FinishHashedUs) {
                (Progress::FinishSentUs, "hash_input")
            } else {
                (Progress::FinishHashedUs, "root_input")
            };
            let start = self.progress[previous as usize].load(Ordering::Relaxed);
            if start != 0 {
                metrics::histogram!("trie.diagnostics.finish_marker_seconds", "origin" => self.origin, "phase" => phase)
                    .record(now.saturating_sub(start) as f64 / 1_000_000.0);
            }
            if matches!(progress, Progress::FinishReceivedUs) {
                let pending = self.progress[Progress::ProofBatchesSent as usize]
                    .load(Ordering::Relaxed)
                    .saturating_sub(
                        self.progress[Progress::ProofBatchesReceived as usize]
                            .load(Ordering::Relaxed),
                    );
                metrics::histogram!("trie.diagnostics.proofs_pending_at_finish", "origin" => self.origin).record(pending as f64);
            }
        }
    }

    /// Records task return, including its outcome, before publishing the result.
    pub fn returned(&self, result: RootTaskResult) {
        self.result.store(result as u64, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        metrics::counter!("trie.diagnostics.root_results_total", "origin" => self.origin, "result" => self.result_label()).increment(1);
        #[cfg(feature = "metrics")]
        {
            let start = self.progress[Progress::FinishSentUs as usize].load(Ordering::Relaxed);
            if start != 0 {
                metrics::histogram!("trie.diagnostics.post_finish_seconds", "origin" => self.origin, "result" => self.result_label())
                    .record(self.elapsed_us().saturating_sub(start) as f64 / 1_000_000.0);
            }
        }
        self.mark(Stage::RootComputed);
    }

    fn result_label(&self) -> &'static str {
        match self.result.load(Ordering::Relaxed) {
            1 => "success",
            2 => "cancelled",
            3 => "failed",
            _ => "pending",
        }
    }

    /// Records a lifecycle transition; only sampled timed-out tasks emit transition logs.
    pub fn mark(&self, stage: Stage) {
        let age = self.elapsed_us();
        self.stages[stage as usize].store(age, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        {
            let phase = STAGE_NAMES[stage as usize];
            metrics::counter!("trie.diagnostics.transitions_total", "origin" => self.origin, "phase" => phase).increment(1);
            metrics::histogram!("trie.diagnostics.stage_age_seconds", "origin" => self.origin, "phase" => phase).record(age as f64 / 1_000_000.0);
        }
        if self.sampled.load(Ordering::Relaxed) {
            self.log(stage);
        }
    }

    /// Emits at most one initial timeout snapshot per 30 seconds per process.
    /// Subsequent lifecycle events for that task are logged so resource-release order is visible.
    pub fn timeout(&self) {
        self.mark(Stage::TimedOut);
        static CLOCK: OnceLock<Instant> = OnceLock::new();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = CLOCK.get_or_init(Instant::now).elapsed().as_secs();
        if should_sample(now, &NEXT) {
            self.sampled.store(true, Ordering::Relaxed);
            self.log(Stage::TimedOut);
        }
    }

    fn log(&self, stage: Stage) {
        // Bound bursts when many previously sampled tasks resume simultaneously.
        static CLOCK: OnceLock<Instant> = OnceLock::new();
        static BUDGET: AtomicU64 = AtomicU64::new(0);
        if !allow_log(CLOCK.get_or_init(Instant::now).elapsed().as_secs(), &BUDGET) {
            #[cfg(feature = "metrics")]
            metrics::counter!("trie.diagnostics.suppressed_log_events_total").increment(1);
            return;
        }
        // This is a best-effort snapshot across threads, not a synchronized state transition.
        let counters = NamedSlots {
            names: &[
                "updates_sent",
                "updates_hashed",
                "updates_received",
                "finish_sent_us",
                "finish_hashed_us",
                "finish_received_us",
                "proof_batches_sent",
                "proof_batches_received",
                "storage_workers_started",
                "storage_workers_exited",
                "account_workers_started",
                "account_workers_exited",
                "storage_workers_ready",
                "account_workers_ready",
                "storage_jobs_started",
                "storage_jobs_exited",
                "account_jobs_started",
                "account_jobs_exited",
                "storage_workers_requested",
                "account_workers_requested",
            ],
            values: self.progress.each_ref().map(|v| v.load(Ordering::Relaxed)),
        };
        let stages_us = NamedSlots {
            names: &STAGE_NAMES,
            values: self.stages.each_ref().map(|v| v.load(Ordering::Relaxed)),
        };
        tracing::warn!(target: "trie::diagnostics", root_task_id = self.id, origin = self.origin, block_number = self.block_number, parent_hash = ?self.parent_hash,
            ?stage, root_result = self.result_label(), age_us = self.elapsed_us(), ?counters, ?stages_us,
            "Sampled state-root timeout progress");
    }

    /// Counts worker entries and exits without per-worker logs.
    pub fn worker(self: &Arc<Self>, start: Progress, end: Progress) -> WorkerGuard {
        self.increment(start);
        WorkerGuard { diagnostics: self.clone(), end }
    }

    /// Tracks the lifetime of an outer worker closure, including early return and panic.
    pub fn enter(self: &Arc<Self>, start: Stage, end: Stage) -> StageGuard {
        self.mark(start);
        StageGuard { diagnostics: self.clone(), end }
    }
}

/// Records closure exit without retaining task resources.
#[derive(Debug)]
pub struct StageGuard {
    diagnostics: Arc<RootTaskDiagnostics>,
    end: Stage,
}
impl Drop for StageGuard {
    fn drop(&mut self) {
        self.diagnostics.mark(self.end);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn proof_session_submission_and_start_are_distinct_clocks() {
        let d = RootTaskDiagnostics::for_block("miner", 99, alloy_primitives::B256::ZERO);
        let context = d.proof_session_submitted("storage");
        assert_eq!(context.id, d.id());
        assert_eq!(context.block_number, 99);
        assert_ne!(d.proof_sessions[0].load(Ordering::Relaxed), 0);
        assert_eq!(d.proof_sessions[1].load(Ordering::Relaxed), 0);
        d.proof_session_started("storage");
        let mut init = d.proof_worker_initializing("storage");
        init.complete();
        drop(init);
        assert!(
            d.proof_sessions[1].load(Ordering::Relaxed) >=
                d.proof_sessions[0].load(Ordering::Relaxed)
        );
        assert_eq!(d.proof_sessions[2].load(Ordering::Relaxed), 0);
        assert_eq!(d.proof_sessions[3].load(Ordering::Relaxed), 0);
        d.marker(Progress::FinishSentUs);
        d.marker(Progress::FinishHashedUs);
        d.marker(Progress::FinishReceivedUs);
        d.finish_drained();
        let mut finalize = d.phase("final_root");
        finalize.complete();
    }

    use super::*;
    #[test]
    fn resumed_task_log_bursts_are_bounded() {
        let budget = AtomicU64::new(0);
        for _ in 0..32 {
            assert!(allow_log(0, &budget));
        }
        assert!(!allow_log(29, &budget));
        assert!(allow_log(30, &budget));
    }

    #[test]
    fn timeout_sampling_is_bounded_under_concurrency() {
        let next = AtomicU64::new(0);
        assert!(should_sample(0, &next));
        assert!(!should_sample(29, &next));
        let admitted = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    if should_sample(30, &next) {
                        admitted.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(admitted.load(Ordering::Relaxed), 1);
        assert!(!should_sample(59, &next));
        assert!(should_sample(60, &next));
        assert_eq!(Stage::FallbackExited as usize + 1, STAGE_NAMES.len());
    }

    #[test]
    fn milestones_survive_worker_exit_and_are_monotonic() {
        let d = RootTaskDiagnostics::new("test");
        {
            let _guard = d.enter(Stage::RootStarted, Stage::RootExited);
            d.increment(Progress::UpdatesSent);
        }
        assert_eq!(d.progress[Progress::UpdatesSent as usize].load(Ordering::Relaxed), 1);
        let start = d.stages[Stage::RootStarted as usize].load(Ordering::Relaxed);
        let end = d.stages[Stage::RootExited as usize].load(Ordering::Relaxed);
        assert!(start > 0 && end >= start);
        assert_eq!(Arc::strong_count(&d), 1);
    }
}

/// Counts a worker exit, including errors and panics.
#[derive(Debug)]
pub struct WorkerGuard {
    diagnostics: Arc<RootTaskDiagnostics>,
    end: Progress,
}
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.diagnostics.increment(self.end);
    }
}

struct NamedSlots<const N: usize> {
    names: &'static [&'static str; N],
    values: [u64; N],
}
impl<const N: usize> std::fmt::Debug for NamedSlots<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Progress");
        for (name, value) in self.names.iter().zip(&self.values) {
            d.field(name, value);
        }
        d.finish()
    }
}

const STAGE_NAMES: [&str; 19] = [
    "storage_session_started",
    "storage_session_exited",
    "account_session_started",
    "account_session_exited",
    "root_started",
    "root_computed",
    "root_exited",
    "hashing_started",
    "hashing_exited",
    "wait_started",
    "timed_out",
    "fallback_submitted",
    "parallel_won",
    "fallback_won",
    "consumer_dropped",
    "receiver_dropped",
    "fallback_preparing",
    "fallback_started",
    "fallback_exited",
];

fn should_sample(now: u64, next: &AtomicU64) -> bool {
    let deadline = next.load(Ordering::Relaxed);
    now >= deadline &&
        next.compare_exchange(
            deadline,
            now.saturating_add(30),
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_ok()
}

// High bits identify a 30-second window; the low byte counts emitted lines (max 32).
#[allow(deprecated)] // Keep compatibility with the workspace MSRV.
fn allow_log(now: u64, budget: &AtomicU64) -> bool {
    let window = (now / 30) << 8;
    budget
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
            let count = if previous & !255 == window { previous & 255 } else { 0 };
            (count < 32).then_some(window | (count + 1))
        })
        .is_ok()
}

/// Wall-clock phase scoped to one root task. Dropping it before complete records an error
/// (including cancellation/unwind); it does not prove that the phase itself caused the error.
#[derive(Debug)]
pub struct DiagnosticPhase {
    origin: &'static str,
    phase: &'static str,
    started: Instant,
    completed: bool,
}
impl DiagnosticPhase {
    /// Mark that the phase returned normally.
    pub fn complete(&mut self) {
        self.completed = true;
    }
}
impl Drop for DiagnosticPhase {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        #[cfg(feature = "metrics")]
        metrics::histogram!("trie.diagnostics.phase_seconds", "origin" => self.origin,
            "phase" => self.phase, "outcome" => if self.completed { "completed" } else { "interrupted" })
            .record(elapsed.as_secs_f64());
        let _ = (elapsed, self.origin, self.phase, self.completed);
    }
}
