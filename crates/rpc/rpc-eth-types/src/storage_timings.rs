//! Reporting for the storage read accounting collected by
//! [`reth_storage_api::StorageTimingsScope`].
//!
//! Emitting is opt-in through the log filter, e.g. `RUST_LOG=storage::timings=debug`. Callers must
//! ask [`storage_timings_enabled`] first and pass the answer to
//! [`StorageTimingsScope::new`](reth_storage_api::StorageTimingsScope::new), so that a node running
//! without the filter never reads the clock on a storage read.

use reth_storage_api::{StorageBucket, StorageTimings};
use std::time::Duration;
use tracing::{Level, debug, enabled, info};

/// Log target for the storage read breakdown.
pub const STORAGE_TIMINGS_TARGET: &str = "storage::timings";

/// Whether the storage read breakdown would be logged.
pub fn storage_timings_enabled() -> bool {
    enabled!(target: STORAGE_TIMINGS_TARGET, Level::DEBUG)
}

/// Logs the storage read breakdown of a single block trace as one event.
///
/// `total` is the wall time of the traced section, so `evm_ms` also covers inspector bookkeeping
/// and trace construction, not just EVM execution.
///
/// Watch the `accounted` field: only the historical state provider is instrumented, so a block
/// whose parent state is served by the latest-state provider reports zeroed buckets rather than
/// cheap storage.
pub fn log_block_storage_timings(
    method: &'static str,
    block_number: u64,
    tx_count: usize,
    total: Duration,
    timings: &StorageTimings,
) {
    let account_calls = timings.calls(StorageBucket::Account);
    let slot_calls = timings.calls(StorageBucket::Slot);
    let history_calls = timings.calls(StorageBucket::History);

    info!(
        target: STORAGE_TIMINGS_TARGET,
        method,
        block = block_number,
        txs = tx_count,
        total_ms = total.as_millis(),
        storage_ms = timings.total().as_millis(),
        evm_ms = timings.non_storage(total).as_millis(),
        // Buckets are in microseconds: bytecode and plain-state reads are routinely sub-millisecond
        // and would otherwise all report zero.
        acct_n = account_calls,
        acct_us = timings.elapsed(StorageBucket::Account).as_micros(),
        slot_n = slot_calls,
        slot_us = timings.elapsed(StorageBucket::Slot).as_micros(),
        code_n = timings.calls(StorageBucket::Bytecode),
        code_us = timings.elapsed(StorageBucket::Bytecode).as_micros(),
        hist_n = history_calls,
        hist_us = timings.elapsed(StorageBucket::History).as_micros(),
        cs_n = timings.calls(StorageBucket::Changeset),
        cs_us = timings.elapsed(StorageBucket::Changeset).as_micros(),
        plain_us = timings.plain().as_micros(),
        cs_hit_pct = changeset_hit_rate(timings),
        accounted = accounted(tx_count, timings),
        "Block trace storage breakdown"
    );
}

/// Whether the buckets can be trusted for this block.
///
/// False means the numbers are not a measurement of cheap storage, they are an absence of
/// measurement. Two causes:
///
/// - The parent state was served by the latest-state provider instead of the historical one, which
///   happens when the parent block is the persisted tip — so for any block still in the in-memory
///   chain. Only the historical provider is instrumented.
/// - A read path reached storage without going through the instrumented account or slot entry
///   points, which would also break the nesting the derived fields rely on.
///
/// Detected via the invariant that every account and slot read performs exactly one history lookup,
/// plus the fact that executing a transaction always reads at least the sender account.
const fn accounted(tx_count: usize, timings: &StorageTimings) -> bool {
    let reads = timings.calls(StorageBucket::Account) + timings.calls(StorageBucket::Slot);
    if tx_count > 0 && reads == 0 {
        return false
    }
    timings.calls(StorageBucket::History) == reads
}

/// Share of account and slot reads that had to fall back to a changeset, in whole percent.
///
/// High values mean the traced block is old enough that most of the state it touches was
/// overwritten since, which is the inherent cost of tracing history.
const fn changeset_hit_rate(timings: &StorageTimings) -> u64 {
    let reads = timings.calls(StorageBucket::Account) + timings.calls(StorageBucket::Slot);
    if reads == 0 {
        return 0
    }
    timings.calls(StorageBucket::Changeset) * 100 / reads
}
