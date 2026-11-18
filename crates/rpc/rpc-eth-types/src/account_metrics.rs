//! Timings for nonce and balance RPCs, including queue time and exact fallbacks.

use metrics::{counter, histogram, Histogram};
use std::time::Instant;

/// Request timer. Drop records total time, including error and cancellation paths.
#[derive(Debug)]
pub struct AccountRpcTimer {
    started: Instant,
    total: Histogram,
    method: &'static str,
    mode: &'static str,
}

impl AccountRpcTimer {
    /// Start a nonce or balance request, before dispatching blocking IO.
    pub fn new(method: &'static str, relaxed: bool) -> Self {
        let mode = if relaxed { "ready" } else { "exact" };
        Self {
            started: Instant::now(),
            total: histogram!("rpc.account.duration_seconds", "method" => method, "mode" => mode, "phase" => "total"),
            method,
            mode,
        }
    }

    /// Record elapsed time for a fixed processing phase, in seconds.
    /// Ready-mode requests emit `queue`, `select` and `read`; both modes emit `total` on drop.
    /// A slow `read` after `exact_fallback` can include overlay computation or waiting.
    pub fn record(&self, phase: &'static str, start: Instant) {
        histogram!("rpc.account.duration_seconds", "method" => self.method, "mode" => self.mode, "phase" => phase).record(start.elapsed().as_secs_f64());
    }

    /// Count a completed request, including provider and account-read errors.
    pub fn finish(&self, success: bool) {
        counter!("rpc.account.requests_total", "method" => self.method, "mode" => self.mode, "result" => if success { "ok" } else { "error" }).increment(1);
    }
}

impl Drop for AccountRpcTimer {
    fn drop(&mut self) {
        self.total.record(self.started.elapsed().as_secs_f64());
    }
}
