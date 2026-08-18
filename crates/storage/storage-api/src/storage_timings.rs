//! Per-request accounting of storage read latency.
//!
//! Historical state reads fan out into several backends (history index, changesets, plain state)
//! that are reached through deeply nested provider calls. Threading an accumulator through those
//! signatures would touch dozens of trait methods, so the counters live in thread-local storage
//! instead: a caller opens a [`StorageTimingsScope`], runs synchronous work, and reads the totals
//! back out.
//!
//! Accounting is off unless a scope is active, and [`record_storage_read`] checks that flag before
//! reading the clock, so uninstrumented callers such as the executor pay nothing.

use core::{cell::Cell, time::Duration};
use std::{cell::RefCell, time::Instant};

/// A storage read bucket.
///
/// [`Account`](Self::Account), [`Slot`](Self::Slot) and [`Bytecode`](Self::Bytecode) are the
/// top-level reads issued by the EVM and never overlap each other. [`History`](Self::History) and
/// [`Changeset`](Self::Changeset) are nested inside the first two, so summing all five would double
/// count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageBucket {
    /// Account reads, including the history lookup and value fetch they trigger.
    Account,
    /// Storage slot reads, including the history lookup and value fetch they trigger.
    Slot,
    /// Bytecode reads.
    Bytecode,
    /// History index lookups, nested inside [`Account`](Self::Account) and [`Slot`](Self::Slot).
    History,
    /// Changeset reads, nested inside [`Account`](Self::Account) and [`Slot`](Self::Slot).
    Changeset,
}

impl StorageBucket {
    /// Number of buckets.
    const COUNT: usize = 5;
}

/// Storage read latency accumulated by a [`StorageTimingsScope`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StorageTimings {
    buckets: [(Duration, u64); StorageBucket::COUNT],
}

impl StorageTimings {
    /// Time spent in the given bucket.
    pub const fn elapsed(&self, bucket: StorageBucket) -> Duration {
        self.buckets[bucket as usize].0
    }

    /// Number of calls recorded for the given bucket.
    pub const fn calls(&self, bucket: StorageBucket) -> u64 {
        self.buckets[bucket as usize].1
    }

    /// Total time spent in storage reads.
    ///
    /// Only the top-level buckets are summed; see [`StorageBucket`] for the nesting.
    pub fn total(&self) -> Duration {
        self.elapsed(StorageBucket::Account) +
            self.elapsed(StorageBucket::Slot) +
            self.elapsed(StorageBucket::Bytecode)
    }

    /// Time spent resolving reads that fell through to plain state, derived as account + slot minus
    /// the nested buckets.
    ///
    /// Also absorbs the per-read bookkeeping outside the nested buckets, such as key hashing.
    /// Saturates at zero: the subtraction only holds while every history and changeset read happens
    /// inside an account or slot read.
    pub fn plain(&self) -> Duration {
        (self.elapsed(StorageBucket::Account) + self.elapsed(StorageBucket::Slot))
            .saturating_sub(self.elapsed(StorageBucket::History))
            .saturating_sub(self.elapsed(StorageBucket::Changeset))
    }

    /// Share of `total_request_time` that was not spent in storage reads.
    pub fn non_storage(&self, total_request_time: Duration) -> Duration {
        total_request_time.saturating_sub(self.total())
    }
}

thread_local! {
    /// Whether a [`StorageTimingsScope`] is accounting on this thread.
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    /// Counters for the active scope. Meaningless unless `ENABLED` is set.
    static TIMINGS: RefCell<StorageTimings> = RefCell::new(StorageTimings::default());
}

/// Accounts storage reads on the current thread until dropped.
///
/// Only meaningful around synchronous code. The counters are thread-local, so a future that
/// resumes on a different thread after an await point stops being accounted for.
#[derive(Debug)]
pub struct StorageTimingsScope {
    /// Whether this scope owns the accounting. False when disabled, or when another scope on this
    /// thread is already accounting.
    owns_accounting: bool,
}

impl StorageTimingsScope {
    /// Starts accounting, unless `enabled` is false or a scope is already active on this thread.
    pub fn new(enabled: bool) -> Self {
        let owns_accounting = enabled && !ENABLED.get();
        if owns_accounting {
            TIMINGS.with_borrow_mut(|timings| *timings = StorageTimings::default());
            ENABLED.set(true);
        }
        Self { owns_accounting }
    }

    /// Returns the counters collected so far, or `None` if this scope isn't accounting.
    pub fn timings(&self) -> Option<StorageTimings> {
        self.owns_accounting.then(|| TIMINGS.with_borrow(|timings| *timings))
    }
}

impl Drop for StorageTimingsScope {
    fn drop(&mut self) {
        // Cleared on drop rather than in an explicit finish, so an early return can't leak
        // accounting into the next task that reuses this pool thread.
        if self.owns_accounting {
            ENABLED.set(false);
        }
    }
}

/// Runs `f`, attributing its duration to `bucket` if a [`StorageTimingsScope`] is active.
#[inline]
pub fn record_storage_read<R>(bucket: StorageBucket, f: impl FnOnce() -> R) -> R {
    if !ENABLED.get() {
        return f();
    }

    let started_at = Instant::now();
    let result = f();
    let elapsed = started_at.elapsed();

    TIMINGS.with_borrow_mut(|timings| {
        let bucket = &mut timings.buckets[bucket as usize];
        bucket.0 += elapsed;
        bucket.1 += 1;
    });

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_scope_records_nothing() {
        let scope = StorageTimingsScope::new(false);
        record_storage_read(StorageBucket::Account, || ());
        assert_eq!(scope.timings(), None);
    }

    #[test]
    fn records_calls_and_nesting() {
        let scope = StorageTimingsScope::new(true);
        record_storage_read(StorageBucket::Account, || {
            record_storage_read(StorageBucket::History, || ())
        });
        record_storage_read(StorageBucket::Slot, || ());

        let timings = scope.timings().unwrap();
        assert_eq!(timings.calls(StorageBucket::Account), 1);
        assert_eq!(timings.calls(StorageBucket::Slot), 1);
        assert_eq!(timings.calls(StorageBucket::History), 1);
        assert_eq!(timings.calls(StorageBucket::Changeset), 0);
        // account + slot, with bytecode unused
        assert_eq!(
            timings.total(),
            timings.elapsed(StorageBucket::Account) + timings.elapsed(StorageBucket::Slot)
        );
    }

    #[test]
    fn nested_scope_does_not_reset_outer() {
        let outer = StorageTimingsScope::new(true);
        record_storage_read(StorageBucket::Account, || ());
        {
            let inner = StorageTimingsScope::new(true);
            assert_eq!(inner.timings(), None);
            record_storage_read(StorageBucket::Account, || ());
        }
        assert_eq!(outer.timings().unwrap().calls(StorageBucket::Account), 2);
    }

    #[test]
    fn accounting_stops_after_drop() {
        drop(StorageTimingsScope::new(true));
        assert!(!ENABLED.get());
    }

    /// Guards `StorageBucket::COUNT` against a variant being added without widening the array,
    /// which would otherwise only surface as an out-of-bounds panic at runtime.
    #[test]
    fn every_bucket_is_indexable() {
        let buckets = [
            StorageBucket::Account,
            StorageBucket::Slot,
            StorageBucket::Bytecode,
            StorageBucket::History,
            StorageBucket::Changeset,
        ];
        assert_eq!(buckets.len(), StorageBucket::COUNT);

        let scope = StorageTimingsScope::new(true);
        for bucket in buckets {
            record_storage_read(bucket, || ());
        }
        let timings = scope.timings().unwrap();
        for bucket in buckets {
            assert_eq!(timings.calls(bucket), 1);
        }
    }
}
