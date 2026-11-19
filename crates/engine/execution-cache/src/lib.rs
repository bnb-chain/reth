//! Cross-block execution cache for payload processing.
//!
//! This crate provides the core caching infrastructure used during block execution:
//! - [`ExecutionCache`]: Fixed-size concurrent caches for accounts, storage, and bytecode
//! - [`SavedCache`]: An execution cache snapshot associated with a specific block hash
//! - [`PayloadExecutionCache`]: Thread-safe wrapper for sharing cached state across payload
//!   processing tasks

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cached_state;
pub use cached_state::*;

mod txpool;
pub use txpool::*;

use alloy_primitives::B256;
use metrics::{Counter, Histogram};
use parking_lot::Mutex;
use reth_metrics::Metrics;
use reth_primitives_traits::FastInstant as Instant;
use std::{sync::Arc, time::Duration};
use tracing::{debug, instrument, warn};

/// A guarded, thread-safe cache of execution state that tracks the most recent block's caches.
///
/// This is the cross-block cache used to accelerate sequential payload processing.
/// When a new block arrives, its parent's cached state can be reused to avoid
/// redundant database lookups.
///
/// This process assumes that payloads are received sequentially.
///
/// ## Cache Safety
///
/// **CRITICAL**: Cache update operations require exclusive access. All concurrent cache users
/// (such as prewarming tasks) must be terminated before calling
/// [`PayloadExecutionCache::update_with_guard`], otherwise the cache may be corrupted or cleared.
#[derive(Clone, Debug, Default)]
pub struct PayloadExecutionCache {
    /// Guarded cloneable cache identified by a block hash.
    inner: Arc<Mutex<ExecutionCacheSlot>>,
    /// Metrics for cache operations.
    metrics: PayloadExecutionCacheMetrics,
}

impl PayloadExecutionCache {
    /// Returns the cache for `parent_hash` if it's available for use.
    ///
    /// A cache is considered available when:
    /// - It exists and matches the requested parent hash
    /// - No other tasks are currently using it (checked via Arc reference count)
    #[instrument(level = "debug", target = "engine::tree::payload_processor", skip(self))]
    pub fn get_cache_for(&self, parent_hash: B256) -> Option<SavedCache> {
        let start = Instant::now();
        let mut slot = self.inner.lock();
        // An execution that falls back to a fresh cache supersedes a pending update too.
        if slot.in_flight {
            slot.generation = Arc::new(());
        }
        let cache = &mut slot.cache;

        let elapsed = start.elapsed();
        self.metrics.execution_cache_wait_duration.record(elapsed.as_secs_f64());
        if elapsed.as_millis() > 5 {
            warn!(blocked_for=?elapsed, "Blocked waiting for execution cache mutex");
        }

        if let Some(c) = cache.as_mut() {
            let cached_hash = c.executed_block_hash();
            // Check that the cache hash matches the parent hash of the current block. It won't
            // match in case it's a fork block.
            let hash_matches = cached_hash == parent_hash;
            // Check `is_available()` to ensure no other tasks (e.g., prewarming) currently hold
            // a reference to this cache. We can only reuse it when we have exclusive access.
            let available = c.is_available();
            let usage_count = c.usage_count();

            debug!(
                target: "engine::caching",
                %cached_hash,
                %parent_hash,
                hash_matches,
                available,
                usage_count,
                "Existing cache found"
            );

            if available {
                if !hash_matches {
                    // Fork block: clear and update the hash on the ORIGINAL before cloning.
                    // This prevents the canonical chain from matching on the stale hash
                    // and picking up polluted data if the fork block fails.
                    c.clear_with_hash(parent_hash);
                }
                return Some(c.clone())
            } else if hash_matches {
                self.metrics.execution_cache_in_use.increment(1);
            }
        } else {
            debug!(target: "engine::caching", %parent_hash, "No cache found");
        }

        None
    }

    /// Takes an exclusively owned matching cache for a background update without waiting.
    ///
    /// While detached, readers fall back to their provider. At most one update is in flight;
    /// a later checkout, update or local insertion invalidates the returned publication token.
    pub fn take_for_update(&self, parent_hash: B256) -> Option<ExecutionCacheUpdate> {
        let mut slot = self.inner.try_lock()?;
        slot.generation = Arc::new(());
        if slot.in_flight ||
            slot.cache.as_ref().is_some_and(|cache| {
                cache.executed_block_hash() != parent_hash || !cache.is_available()
            })
        {
            return None;
        }
        slot.in_flight = true;
        Some(ExecutionCacheUpdate {
            owner: self.clone(),
            generation: slot.generation.clone(),
            cache: slot.cache.take(),
            active: true,
        })
    }

    /// Waits for the mutex protecting the stored `Option<SavedCache>` to be released.
    ///
    /// This does not wait for other users to drop their [`ExecutionCache`] clones or for removed
    /// caches to finish dropping after unlocking. A subsequent [`Self::get_cache_for`] can still
    /// return `None`, causing its caller to allocate a new cache while those drops run.
    ///
    /// Returns only the time spent waiting for the mutex, excluding post-unlock cleanup.
    pub fn wait_for_availability(&self) -> Duration {
        let start = Instant::now();
        // Acquire lock to wait for any current holders to finish
        let _guard = self.inner.lock();
        let elapsed = start.elapsed();
        if elapsed.as_millis() > 5 {
            debug!(
                target: "engine::tree::payload_processor",
                blocked_for=?elapsed,
                "Waited for execution cache to become available"
            );
        }
        elapsed
    }

    /// Runs `update_fn` with mutable access to the stored `Option<SavedCache>` under the mutex.
    /// Returns the closure's result after releasing the mutex, allowing removed caches to be
    /// dropped outside the lock.
    ///
    /// Drop extra [`SavedCache`] or [`ExecutionCache`] clones of the stored cache before the
    /// closure returns: [`Self::get_cache_for`] requires that cache's Arc strong reference
    /// count to be one.
    ///
    /// ## CRITICAL SAFETY REQUIREMENT
    ///
    /// **Before calling this method, you MUST ensure there are no other active cache users.**
    /// This includes:
    /// - No running prewarming task instances that could write to the cache
    /// - No concurrent transactions that might access the cached state
    /// - All prewarming operations must be completed or cancelled
    ///
    /// Violating this requirement can result in cache corruption, incorrect state data,
    /// and potential consensus failures.
    pub fn update_with_guard<F, R>(&self, update_fn: F) -> R
    where
        F: FnOnce(&mut Option<SavedCache>) -> R,
    {
        let mut guard = self.inner.lock();
        guard.generation = Arc::new(());
        update_fn(&mut guard.cache)
    }
}

/// Cache publication state; the generation token cannot wrap or be reused while retained.
#[derive(Debug, Default)]
struct ExecutionCacheSlot {
    cache: Option<SavedCache>,
    generation: Arc<()>,
    in_flight: bool,
}

/// Exclusive cache ownership and conditional publication for one background update.
///
/// Dropping an unfinished update releases its reservation. Detached data is dropped outside
/// the cache mutex, including when an update panics or has become obsolete.
#[derive(Debug)]
pub struct ExecutionCacheUpdate {
    owner: PayloadExecutionCache,
    generation: Arc<()>,
    cache: Option<SavedCache>,
    active: bool,
}

impl ExecutionCacheUpdate {
    /// Takes the detached cache, if a matching parent cache existed.
    pub const fn take_cache(&mut self) -> Option<SavedCache> {
        self.cache.take()
    }

    /// Returns whether work is still useful before opening or populating detached caches.
    pub fn is_current(&self) -> bool {
        let slot = self.owner.inner.lock();
        Arc::ptr_eq(&slot.generation, &self.generation) && slot.cache.is_none()
    }

    /// Publishes only if no newer execution has touched the cache slot.
    pub fn publish(mut self, cache: SavedCache) -> bool {
        let mut slot = self.owner.inner.lock();
        slot.in_flight = false;
        self.active = false;
        if !Arc::ptr_eq(&slot.generation, &self.generation) || slot.cache.is_some() {
            return false;
        }
        slot.cache = Some(cache);
        true
    }
}

impl Drop for ExecutionCacheUpdate {
    fn drop(&mut self) {
        if self.active {
            self.owner.inner.lock().in_flight = false;
        }
    }
}

/// Metrics for [`PayloadExecutionCache`] operations.
#[derive(Metrics, Clone)]
#[metrics(scope = "consensus.engine.beacon")]
struct PayloadExecutionCacheMetrics {
    /// Counter for when the execution cache was unavailable because other threads
    /// (e.g., prewarming) are still using it.
    execution_cache_in_use: Counter,
    /// Time spent waiting for execution cache mutex to become available.
    execution_cache_wait_duration: Histogram,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_checkout_blocks_second() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::repeat_byte(1u8);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let first = cache.get_cache_for(hash);
        assert!(first.is_some());

        let second = cache.get_cache_for(hash);
        assert!(second.is_none());
    }

    #[test]
    fn checkout_available_after_drop() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::repeat_byte(2u8);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash);
        assert!(checked_out.is_some());
        drop(checked_out);

        let second = cache.get_cache_for(hash);
        assert!(second.is_some());
    }

    #[test]
    fn raw_cache_handle_blocks_checkout_until_drop() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::repeat_byte(3u8);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash).expect("checkout should succeed");
        let cache_handle = checked_out.cache().clone();
        drop(checked_out);

        let blocked = cache.get_cache_for(hash);
        assert!(blocked.is_none(), "raw ExecutionCache handle should keep slot in use");

        drop(cache_handle);

        let available = cache.get_cache_for(hash);
        assert!(available.is_some(), "checkout should succeed after raw handle is dropped");
    }

    #[test]
    fn hash_mismatch_clears_and_retags() {
        let cache = PayloadExecutionCache::default();
        let hash_a = B256::repeat_byte(0xAA);
        let hash_b = B256::repeat_byte(0xBB);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash_a, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash_b);
        assert!(checked_out.is_some());
        assert_eq!(checked_out.unwrap().executed_block_hash(), hash_b);
    }

    #[test]
    fn empty_cache_returns_none() {
        let cache = PayloadExecutionCache::default();
        assert!(cache.get_cache_for(B256::ZERO).is_none());
    }

    fn saved(hash: B256) -> SavedCache {
        SavedCache::new(hash, ExecutionCache::new(1_000))
    }

    #[test]
    fn background_update_detaches_then_publishes_matching_cache() {
        let cache = PayloadExecutionCache::default();
        let parent = B256::repeat_byte(1);
        let child = B256::repeat_byte(2);
        cache.update_with_guard(|slot| *slot = Some(saved(parent)));
        let mut update = cache.take_for_update(parent).unwrap();
        assert_eq!(update.take_cache().unwrap().executed_block_hash(), parent);
        // The cache mutex remains available for FCU or another reader during the update.
        assert!(cache.inner.try_lock().is_some());
        assert!(update.publish(saved(child)));
        assert_eq!(cache.get_cache_for(child).unwrap().executed_block_hash(), child);
    }

    #[test]
    fn checkout_during_background_update_invalidates_publication() {
        let cache = PayloadExecutionCache::default();
        let update = cache.take_for_update(B256::ZERO).unwrap();
        assert!(cache.get_cache_for(B256::repeat_byte(1)).is_none());
        assert!(!update.publish(saved(B256::repeat_byte(2))));
        assert!(cache.get_cache_for(B256::repeat_byte(2)).is_none());
    }

    #[test]
    fn background_update_cannot_overwrite_newer_cache() {
        let cache = PayloadExecutionCache::default();
        let update = cache.take_for_update(B256::ZERO).unwrap();
        let newer = B256::repeat_byte(3);
        cache.update_with_guard(|slot| *slot = Some(saved(newer)));
        assert!(!update.publish(saved(B256::repeat_byte(2))));
        assert_eq!(cache.get_cache_for(newer).unwrap().executed_block_hash(), newer);
    }

    #[test]
    fn background_updates_are_bounded_and_new_insert_invalidates_old_result() {
        let cache = PayloadExecutionCache::default();
        let update = cache.take_for_update(B256::ZERO).unwrap();
        assert!(cache.take_for_update(B256::repeat_byte(1)).is_none());
        assert!(!update.publish(saved(B256::repeat_byte(2))));
        assert!(cache.take_for_update(B256::ZERO).is_some());
    }

    #[test]
    fn dropped_or_panicking_update_releases_reservation() {
        let cache = PayloadExecutionCache::default();
        let update = cache.take_for_update(B256::ZERO).unwrap();
        drop(update);
        let update = cache.take_for_update(B256::ZERO).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _update = update;
            panic!("cache update test panic");
        }));
        assert!(panic.is_err());
        assert!(cache.take_for_update(B256::ZERO).is_some());
    }

    #[test]
    fn background_update_skips_locked_in_use_and_mismatched_caches() {
        let cache = PayloadExecutionCache::default();
        let parent = B256::repeat_byte(1);
        cache.update_with_guard(|slot| *slot = Some(saved(parent)));
        let guard = cache.inner.lock();
        assert!(cache.take_for_update(parent).is_none());
        drop(guard);
        let checked_out = cache.get_cache_for(parent).unwrap();
        assert!(cache.take_for_update(parent).is_none());
        drop(checked_out);
        assert!(cache.take_for_update(B256::repeat_byte(2)).is_none());
        assert_eq!(cache.get_cache_for(parent).unwrap().executed_block_hash(), parent);
    }
}
