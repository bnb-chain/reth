//! Pre-warming PathDB MokaCache for fullnode block validation.
//!
//! # Motivation
//!
//! During `validate_block_with_state_with_triedb_sync_validate`, the trie root computation
//! (`intermediate_and_commit_hashed_post_state`) suffers cold MokaCache reads for all intermediate
//! trie nodes (branch/extension nodes). The existing `TrieDBPrefetchHandle` only warms leaf nodes
//! (accounts/storage values), leaving intermediate nodes cold and causing latency spikes.
//!
//! This module warms ALL intermediate trie nodes by speculatively executing the block's exact
//! transaction list before the main execution, then running `intermediate_hashed_post_state`
//! (read-only, no DiffLayer commit) in background threads to populate the global PathDB MokaCache.
//!
//! # Two-phase design
//!
//! ## Phase 1 – EVM speculative execution (blocking, scoped)
//!
//! Transactions are distributed round-robin across `PREWARM_WORKERS` threads.  Each thread:
//!   1. Opens its own read-only `StateProvider` (concurrent readers supported).
//!   2. Builds an EVM backed by `State` (bundle tracking enabled).
//!   3. Executes its tx slice with `disable_nonce_check = true` / `disable_base_fee = true`.
//!   4. Extracts `TrieDBHashedPostState` from the bundle (speculative writes discarded).
//!
//! ## Phase 2 – Trie traversal (background, concurrent with main validation)
//!
//! Each worker's `TrieDBHashedPostState` is handed to a background thread that calls
//! `intermediate_hashed_post_state` (no DiffLayer commit) to warm the global PathDB MokaCache.
//!
//! The caller receives a `Vec<JoinHandle<()>>` and **must join all handles before**
//! calling `intermediate_and_commit_hashed_post_state` to guarantee the MokaCache is fully warm.
//!
//! # Correctness
//!
//! - No external state mutations: only `StateProvider` reads.
//! - Speculative bundle states are discarded after extracting `TrieDBHashedPostState`.
//! - Main execution uses a fresh `State`; prewarm does not affect it.

use alloy_consensus::transaction::Recovered;
use alloy_evm::{Evm as EvmTrait, IntoTxEnv};
use alloy_primitives::B256;
use reth_evm::{ConfigureEvm, TxEnvFor};
use reth_primitives_traits::{HeaderTy, NodePrimitives};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use revm::database::{states::bundle_state::BundleRetention, State};
use rust_eth_triedb::TrieDBHashedPostState;
use rust_eth_triedb_common::DiffLayers;
use tracing::{debug, warn};

/// Number of parallel worker threads for prewarm trie traversal.
const PREWARM_WORKERS: usize = 4;

/// Pre-warms the global PathDB MokaCache for the trie root computation of a validated block.
///
/// **Phase 1** (blocking): speculative EVM execution across `PREWARM_WORKERS` threads discovers
/// state changes for the exact block tx list and extracts per-worker `TrieDBHashedPostState`.
///
/// **Phase 2** (background): returns `Vec<JoinHandle<()>>` for trie traversal threads that warm
/// the PathDB MokaCache concurrently with the ongoing block validation.
///
/// # Caller contract
///
/// The caller **must join all returned handles before calling
/// `intermediate_and_commit_hashed_post_state()`** to guarantee the MokaCache is warm.
///
/// Call this BEFORE `spawn_cache_with_triedb_prefetcher` / `execute_block` to maximize the
/// concurrent window available for MokaCache warming.
///
/// Skips gracefully (returns empty Vec) if triedb is inactive or `txs` is empty.
pub(crate) fn prewarm_block_trie_moka_cache<N, P, Evm>(
    provider: &P,
    evm_config: &Evm,
    parent_header: &HeaderTy<N>,
    parent_hash: B256,
    parent_state_root: B256,
    difflayers: Option<DiffLayers>,
    txs: Vec<Recovered<N::SignedTx>>,
) -> Vec<std::thread::JoinHandle<()>>
where
    N: NodePrimitives,
    P: StateProviderFactory + Sync,
    Evm: ConfigureEvm<Primitives = N> + Sync,
    Recovered<N::SignedTx>: IntoTxEnv<TxEnvFor<Evm>>,
    N::SignedTx: Clone + Send,
{
    if !rust_eth_triedb::triedb_manager::is_triedb_active() {
        return Vec::new();
    }
    if txs.is_empty() {
        return Vec::new();
    }

    let tx_count = txs.len();

    // ── 1. Distribute txs round-robin into PREWARM_WORKERS buckets ────────────────────────────
    let mut buckets: [Vec<Recovered<N::SignedTx>>; PREWARM_WORKERS] =
        std::array::from_fn(|_| Vec::new());
    for (i, tx) in txs.into_iter().enumerate() {
        buckets[i % PREWARM_WORKERS].push(tx);
    }

    // ── 2. Phase 1: EVM speculative execution (scoped, blocks until trie states ready) ─────────
    let trie_states: Vec<TrieDBHashedPostState> = std::thread::scope(|s| {
        let handles: Vec<_> = buckets
            .into_iter()
            .enumerate()
            .map(|(worker_id, worker_txs)| {
                s.spawn(move || -> Option<TrieDBHashedPostState> {
                    let sp = match provider.state_by_block_hash(parent_hash) {
                        Ok(sp) => sp,
                        Err(e) => {
                            warn!(
                                target: "engine::tree::prewarm",
                                worker = worker_id, err = %e,
                                "Prewarm worker failed to open state provider"
                            );
                            return None;
                        }
                    };

                    // Build EVM state with bundle tracking.
                    // Keep `sp` alive (borrowed by sp_db) for hashed_post_state call below.
                    let trie_state = {
                        let sp_db = StateProviderDatabase::new(&*sp);
                        let mut state_db = State::builder()
                            .with_database(sp_db)
                            .with_bundle_update()
                            .build();

                        let mut evm_env = evm_config.evm_env(parent_header);
                        evm_env.cfg_env.disable_nonce_check = true;
                        evm_env.cfg_env.disable_base_fee = true;

                        {
                            let mut evm = evm_config.evm_with_env(&mut state_db, evm_env);
                            for tx in worker_txs {
                                let _ = evm.transact(tx);
                            }
                            // evm drops → releases &mut state_db
                        }

                        // Extract trie state for Phase 2 background warming.
                        // Speculative writes are discarded; only the hashed representation is kept.
                        state_db.merge_transitions(BundleRetention::PlainState);
                        let hashed_state = sp.hashed_post_state(&state_db.bundle_state);
                        hashed_state.to_triedb_hashed_post_state()
                        // state_db drops; sp released for hashed_post_state use above
                    };

                    Some(trie_state)
                })
            })
            .collect();

        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    });

    debug!(
        target: "engine::tree::prewarm",
        tx_count,
        trie_workers = trie_states.len(),
        "Phase 1 (EVM prewarm) complete; spawning Phase 2 (trie MokaCache) background threads"
    );

    // ── 3. Phase 2: Trie traversal (background, concurrent with block validation) ───────────────
    //
    // Each thread calls intermediate_hashed_post_state (read-only, no DiffLayer commit) to
    // warm the global PathDB MokaCache (Arc-shared across all get_global_triedb() clones).
    // The caller MUST join these handles before intermediate_and_commit_hashed_post_state()
    // to guarantee the MokaCache is fully warm.
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
