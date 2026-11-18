//! Wall-clock stages of already-executed insertion and canonical handoff.
//! Canonical stages include all sources and may overlap with enclosing FCU scopes.

use std::time::Instant;

pub(super) struct HandoffStage {
    phase: &'static str,
    started: Instant,
}

impl HandoffStage {
    pub(super) fn new(phase: &'static str) -> Self {
        Self { phase, started: Instant::now() }
    }
}

impl Drop for HandoffStage {
    fn drop(&mut self) {
        metrics::histogram!("engine.handoff.stage_seconds", "phase" => self.phase)
            .record(self.started.elapsed().as_secs_f64());
    }
}
