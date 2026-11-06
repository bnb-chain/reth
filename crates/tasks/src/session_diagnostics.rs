//! Opt-in named-session tracing. IDs and block numbers appear only in sampled logs,
//! never in metric labels. Observations do not own channels, providers or Runtime handles.

use reth_metrics::{metrics::Histogram, Metrics};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Context for a named session; IDs must be unique within the calling subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedTaskContext {
    /// Process-local root task ID, for log correlation only.
    pub id: u64,
    /// Bounded source label, e.g. import, miner, miner-empty.
    pub origin: &'static str,
    /// Block number, for logs only.
    pub block_number: u64,
}

/// Queue and runtime histograms for an observed session, including unwind.
#[derive(Metrics, Clone)]
#[metrics(scope = "executor.named_session")]
struct SessionMetrics {
    /// Submission to actual named closure start, not task creation to start.
    queue_wait_seconds: Histogram,
    /// Named closure lifetime, including proof-worker broadcast/teardown, not CPU time.
    run_seconds: Histogram,
}

#[derive(Clone)]
struct Running {
    context: NamedTaskContext,
    since: Instant,
    sampled: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    previous: Option<NamedTaskContext>,
    last_log: Option<Instant>,
}

pub(crate) struct SessionTracker {
    worker: &'static str,
    runtime: &'static str,
    state: Mutex<State>,
}

impl SessionTracker {
    pub(crate) fn new(worker: &'static str, runtime: Option<&'static str>) -> Self {
        Self { worker, runtime: runtime.unwrap_or("default"), state: Mutex::new(State::default()) }
    }

    /// Called immediately before submission. The running owner is a snapshot; queued work
    /// can also be ahead, so do not present that owner as the only source of queue delay.
    pub(crate) fn submitted(
        self: &Arc<Self>,
        context: NamedTaskContext,
        pending: usize,
    ) -> SessionObservation {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let sample = pending > 0 && permit_log(&mut state, now);
        if sample {
            if let Some(owner) = &state.running {
                owner.sampled.store(true, Ordering::Relaxed);
            }
            tracing::warn!(target: "reth::tasks::session", runtime = self.runtime, worker = self.worker,
                root_task_id = context.id, origin = context.origin, block_number = context.block_number,
                pending_tasks = pending,
                blocking_root_task_id = ?state.running.as_ref().map(|r| r.context.id),
                blocking_origin = ?state.running.as_ref().map(|r| r.context.origin),
                blocking_block_number = ?state.running.as_ref().map(|r| r.context.block_number),
                blocking_age_ms = ?state.running.as_ref().map(|r| r.since.elapsed().as_millis()),
                "Sampled named session queued");
        }
        SessionObservation {
            tracker: self.clone(),
            context,
            queued: now,
            started: None,
            sampled: Arc::new(AtomicBool::new(sample)),
            metrics: SessionMetrics::new_with_labels(&[
                ("runtime", self.runtime),
                ("worker", self.worker),
                ("origin", context.origin),
            ]),
        }
    }
}

fn permit_log(state: &mut State, now: Instant) -> bool {
    if state.last_log.is_some_and(|last| now.duration_since(last) < Duration::from_secs(5)) {
        return false
    }
    state.last_log = Some(now);
    true
}

pub(crate) struct SessionObservation {
    tracker: Arc<SessionTracker>,
    context: NamedTaskContext,
    queued: Instant,
    started: Option<Instant>,
    sampled: Arc<AtomicBool>,
    metrics: SessionMetrics,
}

impl SessionObservation {
    pub(crate) fn start(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.queued);
        self.metrics.queue_wait_seconds.record(elapsed.as_secs_f64());
        let mut state = self.tracker.state.lock().unwrap_or_else(|e| e.into_inner());
        if elapsed >= Duration::from_millis(20) && permit_log(&mut state, now) {
            self.sampled.store(true, Ordering::Relaxed);
        }
        if self.sampled.load(Ordering::Relaxed) {
            tracing::warn!(target: "reth::tasks::session", runtime = self.tracker.runtime, worker = self.tracker.worker,
                root_task_id = self.context.id, origin = self.context.origin, block_number = self.context.block_number,
                queue_ms = elapsed.as_millis(), previous_root_task_id = ?state.previous.map(|p| p.id),
                previous_block_number = ?state.previous.map(|p| p.block_number),
                "Sampled named session started");
        }
        state.running =
            Some(Running { context: self.context, since: now, sampled: self.sampled.clone() });
        self.started = Some(now);
    }
}

impl Drop for SessionObservation {
    fn drop(&mut self) {
        let Some(started) = self.started else { return };
        let elapsed = started.elapsed();
        self.metrics.run_seconds.record(elapsed.as_secs_f64());
        let mut state = self.tracker.state.lock().unwrap_or_else(|e| e.into_inner());
        state.previous = Some(self.context);
        state.running = None;
        if self.sampled.load(Ordering::Relaxed) {
            tracing::warn!(target: "reth::tasks::session", runtime = self.tracker.runtime, worker = self.tracker.worker,
                root_task_id = self.context.id, origin = self.context.origin, block_number = self.context.block_number,
                run_ms = elapsed.as_millis(), panicking = std::thread::panicking(),
                "Sampled named session exited");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_is_bounded_per_worker() {
        let mut state = State::default();
        let now = Instant::now();
        assert!(permit_log(&mut state, now));
        assert!(!permit_log(&mut state, now + Duration::from_secs(4)));
        assert!(permit_log(&mut state, now + Duration::from_secs(5)));
    }

    #[test]
    fn tracks_owner_predecessor_and_cleans_up_on_unwind_or_unstarted_drop() {
        let tracker = Arc::new(SessionTracker::new("account-workers", Some("miner")));
        let a = NamedTaskContext { id: 1, origin: "miner", block_number: 10 };
        let b = NamedTaskContext { id: 2, ..a };
        let mut first = tracker.submitted(a, 0);
        first.start();
        let mut second = tracker.submitted(b, 1);
        assert_eq!(tracker.state.lock().unwrap().running.as_ref().unwrap().context, a);
        assert!(first.sampled.load(Ordering::Relaxed));
        drop(first);
        assert_eq!(tracker.state.lock().unwrap().previous, Some(a));
        second.start();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = second;
            panic!("test");
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"test"));
        assert!(tracker.state.lock().unwrap().running.is_none());
        assert_eq!(tracker.state.lock().unwrap().previous, Some(b));
        drop(tracker.submitted(a, 0));
        assert_eq!(tracker.state.lock().unwrap().previous, Some(b));
        let isolated = SessionTracker::new("account-workers", Some("import"));
        assert!(isolated.state.lock().unwrap().previous.is_none());
    }
}
