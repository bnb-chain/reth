use std::sync::{
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use alloy_primitives::{B256, U256};
use rust_eth_triedb_common::TrieDatabase;
use rust_eth_triedb_state_trie::{SecureTrieTrait, StateTrie};

#[derive(Clone, Debug, Default)]
pub(super) struct UpdatePreheatStats {
    pub(super) updates_applied: u64,
    pub(super) deletes_applied: u64,
    pub(super) update_errors: u64,
    pub(super) elapsed: Duration,
}

/// Best-effort preheat that mimics the access pattern of `trie.update(..)`.
///
/// - Runs on a *clone* of the trie (caller-owned), so correctness is preserved.
/// - Cancelable: checks `cancel_flag` between slot updates.
pub(super) fn update_shaped_preheat_storage_trie<DB>(
    storage_trie: &mut StateTrie<DB>,
    hashed_address: B256,
    changed_slots: &[(B256, U256)],
    cancel_flag: &AtomicBool,
) -> UpdatePreheatStats
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    let started = Instant::now();
    let mut stats = UpdatePreheatStats::default();

    // Update-shaped phase: apply a bounded subset of slot updates. This triggers resolve patterns
    // similar to the real trie update path (splits/collapses/adjacent resolves) without committing.
    for (hashed_slot, value) in changed_slots.iter() {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        let res = if value.is_zero() {
            storage_trie.delete_storage_with_hash_state(hashed_address, *hashed_slot)
        } else {
            storage_trie.update_storage_u256_with_hash_state(hashed_address, *hashed_slot, *value)
        };

        match res {
            Ok(()) => {
                if value.is_zero() {
                    stats.deletes_applied = stats.deletes_applied.saturating_add(1);
                } else {
                    stats.updates_applied = stats.updates_applied.saturating_add(1);
                }
            }
            Err(_) => {
                stats.update_errors = stats.update_errors.saturating_add(1);
            }
        }
    }

    stats.elapsed = started.elapsed();
    stats
}

