//! Pre-warming PathDB MokaCache and EVM state cache for fullnode block validation.
//!
//! # Motivation
//!
//! During `validate_block_with_state_with_triedb_sync_validate`, the trie root computation
//! (`intermediate_and_commit_hashed_post_state`) suffers cold MokaCache reads for all intermediate
//! trie nodes (branch/extension nodes). The existing `TrieDBPrefetchHandle` only warms leaf nodes
//! (accounts/storage values), leaving intermediate nodes cold and causing latency spikes.
//!
//! Additionally, `execute_block` suffers cold EVM state reads for all accounts and storage slots
//! touched by the block's transactions.
//!
//! This module warms BOTH:
//! 1. `CachedReads` (EVM state cache): populated by reading all accounts/storage touched during
//!    speculative EVM execution, reducing cold DB reads in the real `execute_block`.
//! 2. PathDB MokaCache (trie node cache): warmed by running `intermediate_hashed_post_state`
//!    (read-only, no DiffLayer commit) in background threads.
//!
//! # Two-phase design
//!
//! ## Phase 1 – EVM speculative execution (blocking, scoped)
//!
//! Transactions are distributed round-robin across `PREWARM_WORKERS` threads.  Each thread:
//!   1. Opens its own read-only `StateProvider` (concurrent readers supported).
//!   2. Builds an EVM backed by `State<CachedReadsDbMut>` (bundle tracking enabled).
//!   3. Executes its tx slice with `disable_nonce_check = true`.
//!   4. Commits each result to the State (populates bundle for trie warming).
//!   5. Returns `(worker_cached_reads, TrieDBHashedPostState)`.
//!
//! After Phase 1, all `CachedReads` are merged into the caller-supplied `cached_reads`.
//!
//! ## Phase 2 – Trie traversal (background, concurrent with main validation)
//!
//! Each worker's `TrieDBHashedPostState` is handed to a background thread that calls
//! `intermediate_hashed_post_state` (no DiffLayer commit) to warm the global PathDB MokaCache.
//!
//! The caller should **join all handles before calling
//! `intermediate_and_commit_hashed_post_state`** to guarantee the MokaCache is fully warm.
//! The threads run concurrently with `execute_block` (which dominates wall-clock time), so
//! the join is usually a no-op — identical contract to miner joining before
//! `finish_with_difflayer()`.
//!
//! # Correctness
//!
//! - No external state mutations: only `StateProvider` reads, `CachedReads` writes.
//! - Speculative bundle states are discarded after extracting `TrieDBHashedPostState`.
//! - Main execution uses a fresh `State`; `CachedReads` is a read-only cache.

use alloy_consensus::transaction::Recovered;
use alloy_evm::{Evm as EvmTrait, IntoTxEnv};
use alloy_primitives::B256;
use reth_evm::{ConfigureEvm, TxEnvFor};
use reth_primitives_traits::{HeaderTy, NodePrimitives};
use reth_provider::StateProviderFactory;
use reth_revm::cached::CachedReads;
use reth_revm::database::StateProviderDatabase;
use revm::database::{states::bundle_state::BundleRetention, State};
use revm::DatabaseCommit;
use rust_eth_triedb::TrieDBHashedPostState;
use rust_eth_triedb_common::DiffLayers;
use tracing::{debug, warn};

/// Number of parallel worker threads for prewarm — matches miner's PREWARM_WORKERS.
const PREWARM_WORKERS: usize = 5;

/// Pre-warms the EVM state cache (`CachedReads`) and PathDB MokaCache for block validation.
///
/// Mirrors `prewarm_miner_evm_cache` interface: takes `&mut CachedReads` (caller owns it)
/// and returns `Vec<JoinHandle<()>>` for Phase 2 background trie threads.
///
/// **Phase 1** (blocking): speculative EVM execution across `PREWARM_WORKERS` threads populates
/// `cached_reads` with all touched accounts/storage and extracts per-worker
/// `TrieDBHashedPostState`.
///
/// **Phase 2** (background): returns `Vec<JoinHandle<()>>` for trie traversal threads that warm
/// the PathDB MokaCache concurrently with the ongoing block validation.
///
/// # Caller contract
///
/// The caller **must join all returned handles before root computation** to guarantee the
/// MokaCache is fully warm.  Phase 2 threads run concurrently with `execute_block`,
/// so by the time execution finishes they are typically already done.
///
/// Call this BEFORE `execute_block` to maximize the concurrent warming window.
///
/// Skips gracefully (no-op on `cached_reads`, returns empty Vec) if `txs` is empty.
pub(crate) fn prewarm_block<N, P, Evm>(
    provider: &P,
    evm_config: &Evm,
    parent_header: &HeaderTy<N>,
    parent_hash: B256,
    parent_state_root: B256,
    difflayers: Option<DiffLayers>,
    txs: Vec<Recovered<N::SignedTx>>,
    cached_reads: &mut CachedReads,
) -> Vec<std::thread::JoinHandle<()>>
where
    N: NodePrimitives,
    P: StateProviderFactory + Sync,
    Evm: ConfigureEvm<Primitives = N> + Sync,
    Recovered<N::SignedTx>: IntoTxEnv<TxEnvFor<Evm>>,
    N::SignedTx: Clone + Send,
{
    if txs.is_empty() {
        return Vec::new();
    }

    let tx_count = txs.len();
    let trie_active = rust_eth_triedb::triedb_manager::is_triedb_active();

    // ── 1. Distribute txs round-robin into PREWARM_WORKERS buckets ────────────────────────────
    let mut buckets: [Vec<Recovered<N::SignedTx>>; PREWARM_WORKERS] =
        std::array::from_fn(|_| Vec::new());
    for (i, tx) in txs.into_iter().enumerate() {
        buckets[i % PREWARM_WORKERS].push(tx);
    }

    // ── 2. Phase 1: EVM speculative execution (scoped, blocks until caches ready) ─────────────
    //
    // Each worker returns (worker_cached_reads, Option<TrieDBHashedPostState>).
    // Reads populate worker_cached_reads; committed state populates bundle for trie warming.
    let pairs: Vec<(CachedReads, Option<TrieDBHashedPostState>)> = std::thread::scope(|s| {
        let handles: Vec<_> = buckets
            .into_iter()
            .enumerate()
            .map(|(worker_id, worker_txs)| {
                s.spawn(move || -> (CachedReads, Option<TrieDBHashedPostState>) {
                    let sp = match provider.state_by_block_hash(parent_hash) {
                        Ok(sp) => sp,
                        Err(e) => {
                            warn!(
                                target: "engine::tree::prewarm",
                                worker = worker_id, err = %e,
                                "Prewarm worker failed to open state provider"
                            );
                            return (CachedReads::default(), None);
                        }
                    };

                    let mut worker_cached = CachedReads::default();

                    let trie_state = {
                        let sp_db = StateProviderDatabase::new(&*sp);
                        let cached_db = worker_cached.as_db_mut(sp_db);
                        let mut state_db = State::builder()
                            .with_database(cached_db)
                            .with_bundle_update()
                            .build();

                        let mut evm_env = evm_config.evm_env(parent_header);
                        evm_env.cfg_env.disable_nonce_check = true;

                        {
                            let mut evm = evm_config.evm_with_env(&mut state_db, evm_env);
                            for tx in worker_txs {
                                // Commit each result so bundle_state accumulates changes
                                // for trie warming (Phase 2).  Reads are captured in
                                // worker_cached via CachedReadsDbMut regardless of commit.
                                if let Ok(result) = evm.transact(tx) {
                                    evm.db_mut().commit(result.state);
                                }
                            }
                            // evm drops → releases &mut state_db
                        }

                        // Extract trie state for Phase 2 background warming.
                        if trie_active {
                            state_db.merge_transitions(BundleRetention::PlainState);
                            let hashed_state = sp.hashed_post_state(&state_db.bundle_state);
                            Some(hashed_state.to_triedb_hashed_post_state())
                        } else {
                            None
                        }
                        // state_db drops → releases worker_cached
                    };

                    (worker_cached, trie_state)
                })
            })
            .collect();

        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    // ── 3. Merge per-worker CachedReads into caller-supplied cached_reads ─────────────────────
    let mut trie_states: Vec<TrieDBHashedPostState> = Vec::new();
    let mut accounts_warmed = 0usize;
    let mut contracts_warmed = 0usize;

    for (partial, trie_state) in pairs {
        accounts_warmed += partial.accounts.len();
        contracts_warmed += partial.contracts.len();
        for (addr, acc) in partial.accounts {
            cached_reads.accounts.entry(addr).or_insert(acc);
        }
        for (hash, code) in partial.contracts {
            cached_reads.contracts.entry(hash).or_insert(code);
        }
        if let Some(ts) = trie_state {
            trie_states.push(ts);
        }
    }

    debug!(
        target: "engine::tree::prewarm",
        tx_count,
        accounts_warmed,
        contracts_warmed,
        trie_workers = trie_states.len(),
        "Phase 1 (EVM prewarm) complete; spawning Phase 2 (trie MokaCache) background threads"
    );

    if !trie_active {
        return Vec::new();
    }

    // ── 4. Phase 2: Trie traversal (background, concurrent with block validation) ───────────────
    //
    // Each thread calls intermediate_hashed_post_state (read-only, no DiffLayer commit) to
    // warm the global PathDB MokaCache (Arc-shared across all get_global_triedb() clones).
    trie_states
        .into_iter()
        .map(|trie_state| {
            let difflayers = difflayers.clone();
            std::thread::spawn(move || {
                let mut triedb = rust_eth_triedb::get_global_triedb();
                let _ = triedb.intermediate_hashed_post_state(
                    parent_state_root,
                    difflayers.as_ref(),
                    &trie_state,
                    None, // no prefetcher for speculative prewarm
                );
            })
        })
        .collect()
}
