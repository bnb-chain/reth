//! TrieDBPrefetchTask is a task that is responsible for prefetching the triedb from the database.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvError, Sender, TryRecvError},
    Arc, Mutex,
};
use std::sync::RwLock;
use std::time::Duration;
use std::sync::OnceLock;

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::{hex, keccak256, map::B256Set, B256};
use rust_eth_triedb_common::{DiffLayers, TrieDatabase};
use rust_eth_triedb_pathdb::PathDB;
use rust_eth_triedb_state_trie::{SecureTrieId, SecureTrieBuilder, SecureTrieError, SecureTrieTrait, StateTrie};
use rust_eth_triedb::{triedb_reth::TrieDBPrefetchState};
use reth_revm::state::EvmState;
use reth_trie::MultiProofTargets;
use tracing::{error, warn, info, trace};
use rayon::prelude::*;

use crate::tree::payload_processor::executor::WorkloadExecutor;
use crate::tree::payload_processor::multiproof::MultiProofMessage;

/// A best-effort, non-blocking snapshot of the current triedb prefetch state.
///
/// This is used to pass a *partial* `prefetch_state` into trie-root computation without waiting
/// for the prefetcher to finish. Missing entries simply fall back to DB/difflayer reads, so this
/// only affects performance.
#[derive(Debug, Default, Clone)]
pub(crate) struct TrieDBPrefetchSnapshot {
    inner: Arc<RwLock<Option<PublishedPrefetchSnapshot>>>,
}

#[derive(Debug, Clone)]
struct PublishedPrefetchSnapshot {
    root_hash: B256,
    state: Arc<TrieDBPrefetchState<PathDB>>,
}

impl TrieDBPrefetchSnapshot {
    /// Loads the latest published snapshot (without validating root hash).
    pub(crate) fn load(&self) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.inner.read().ok().and_then(|g| g.as_ref().map(|s| s.state.clone()))
    }

    /// Loads the latest snapshot only if it matches the expected parent root hash.
    pub(crate) fn load_for_root(&self, expected_root: B256) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.inner
            .read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|s| (s.root_hash == expected_root).then(|| s.state.clone())))
    }

    fn store(&self, root_hash: B256, state: Arc<TrieDBPrefetchState<PathDB>>) {
        if let Ok(mut g) = self.inner.write() {
            *g = Some(PublishedPrefetchSnapshot { root_hash, state });
        }
    }
}

/// Best-effort storage root lookup for a hashed address.
///
/// This is performance-critical in the prefetcher and must be safe to call from multiple threads.
/// It never mutates state and falls back to `EMPTY_ROOT_HASH` when the account has no storage.
fn lookup_storage_root(
    path_db: &PathDB,
    difflayers: Option<&DiffLayers>,
    hashed_address: B256,
) -> Option<B256> {
    if let Some(difflayers) = difflayers {
        if let Some(storage_root) = difflayers.get_storage_root(hashed_address) {
            return Some(storage_root)
        }
    }

    match path_db.get_storage_root(hashed_address) {
        Ok(Some(storage_root)) => Some(storage_root),
        Ok(None) => Some(EMPTY_ROOT_HASH),
        Err(e) => {
            error!(
                target: "engine::trie_db_prefetch",
                "Failed to get storage root for hashed_address: 0x{}, error: {:?}",
                hex::encode(hashed_address),
                e
            );
            None
        }
    }
}

/// Dedicated rayon thread-pool for triedb prefetch work.
///
/// We keep this separate from any other global rayon usage so cache-warming doesn't contend with
/// trie-root calculation or other parallel subsystems.
fn triedb_prefetch_rayon_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        // Conservative default: a few threads are usually enough to saturate RocksDB reads.
        let num_threads = std::env::var("TRIEDB_PREFETCH_RAYON_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(4);
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("triedb-prefetch-{i}"))
            .build()
            .expect("failed to build triedb prefetch rayon thread pool")
    })
}

/// Error type for TrieDB prefetch operations.
#[derive(Debug, thiserror::Error)]
pub enum TrieDBPrefetchError {
    /// Secure trie error.
    #[error("Secure trie error: {0}")]
    SecureTrie(#[from] SecureTrieError),
    /// Failed to build account trie.
    #[error("Failed to build account trie: {0}")]
    BuildAccountTrie(String),
}


/// Message type for TrieDB prefetch operations.
pub(super) enum TrieDBPrefetchMessage {
    PrefetchState(MultiProofTargets),
    PrefetchSlots(B256Set),
    PrefetchFinished(),
}

/// Result type for TrieDB prefetch operations.
pub(crate) enum TrieDBPrefetchResult {
    #[allow(dead_code)]
    PrefetchAccountResult(Arc<TrieDBPrefetchState<PathDB>>),
    PrefetchStorageResult((B256, StateTrie<PathDB>, usize)),
}

/// Convert EVM state to TrieDB prefetch state.
pub fn evm_state_to_trie_db_prefetch_state(evm_state: &EvmState) -> MultiProofTargets {
    let mut state = MultiProofTargets::default();
    for (address, account) in evm_state {
        let hashed_address = keccak256(address.as_slice());
        let mut slots = B256Set::default();
        for (slot, _) in account.storage.iter() {
            let hashed_key = keccak256(B256::from(*slot));
            slots.insert(hashed_key);
        }
        state.insert(hashed_address, slots);
    }
    state
}

/// A direct TrieDB prefetcher that can be driven from `EvmState` updates (e.g. miner-side).
///
/// This is a small public wrapper around the internal triedb prefetch tasks used by the engine.
/// Unlike [`TrieDBPrefetchHandle`], this does **not** require wiring the multiproof channel; users
/// can call [`TrieDBStatePrefetcher::on_state_update`] directly.
#[derive(Debug, Clone)]
pub struct TrieDBStatePrefetcher {
    inner: Arc<TrieDBStatePrefetcherInner>,
}

#[derive(Debug)]
struct TrieDBStatePrefetcherInner {
    state_tx: Sender<TrieDBPrefetchMessage>,
    result_rx: Mutex<Option<Receiver<TrieDBPrefetchResult>>>,
    cancel_flag: Arc<AtomicBool>,
    snapshot: TrieDBPrefetchSnapshot,
    root_hash: B256,
}

impl TrieDBStatePrefetcher {
    /// Creates and starts the prefetcher task(s).
    pub fn new(
        root_hash: B256,
        path_db: PathDB,
        difflayers: Option<DiffLayers>,
    ) -> Result<Self, TrieDBPrefetchError> {
        let executor = WorkloadExecutor::default();
        let spawn_exec = executor.clone();

        let (state_tx, state_rx) = mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let snapshot = TrieDBPrefetchSnapshot::default();

        let (account_task, prefetch_result_rx) = TrieDBPrefetchAccountTask::new(
            root_hash,
            path_db,
            difflayers,
            executor,
            state_rx,
            cancel_flag.clone(),
            snapshot.clone(),
        )?;

        // Spawn the account task (which in turn spawns per-account storage tasks).
        // This uses the internal workload executor's tokio runtime.
        spawn_exec.spawn_blocking(move || {
            account_task.run();
        });

        Ok(Self {
            inner: Arc::new(TrieDBStatePrefetcherInner {
                state_tx,
                result_rx: Mutex::new(Some(prefetch_result_rx)),
                cancel_flag,
                snapshot,
                root_hash,
            }),
        })
    }

    /// Feed a state update into the prefetcher (typically called from an `OnStateHook`).
    pub fn on_state_update(&self, update: &EvmState) {
        if self.inner.cancel_flag.load(Ordering::Relaxed) {
            return;
        }

        let targets = evm_state_to_trie_db_prefetch_state(update);
        if let Err(e) = self.inner.state_tx.send(TrieDBPrefetchMessage::PrefetchState(targets)) {
            warn!(
                target: "engine::trie_db_prefetch",
                "TrieDBStatePrefetcher failed to send prefetch targets: {e:?}"
            );
        }
    }

    /// Feed one-shot proof targets into the prefetcher.
    ///
    /// This is useful when the full set of account/storage targets is known (e.g. final
    /// `TrieDBHashedPostState`) and we want to ensure prefetch coverage beyond streaming state
    /// updates.
    pub fn prefetch_targets(&self, targets: MultiProofTargets) {
        if self.inner.cancel_flag.load(Ordering::Relaxed) {
            return;
        }

        if let Err(e) = self
            .inner
            .state_tx
            .send(TrieDBPrefetchMessage::PrefetchState(targets))
        {
            warn!(
                target: "engine::trie_db_prefetch",
                "TrieDBStatePrefetcher failed to send one-shot prefetch targets: {e:?}"
            );
        }
    }

    /// Returns the most recent published prefetch snapshot, if any.
    ///
    /// This is non-blocking and may return `None` if the background task hasn't published yet.
    pub fn try_snapshot(&self) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.inner.snapshot.load()
    }

    /// Returns the most recent snapshot, only if it matches the given `root_hash`.
    pub fn try_snapshot_for_root(&self, root_hash: B256) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.inner.snapshot.load_for_root(root_hash)
    }

    /// Finishes the prefetcher and returns the produced prefetch state, if available.
    ///
    /// This will signal all tasks to stop and then block until the final `PrefetchAccountResult`
    /// is received (or the channel is dropped).
    pub fn finish(self) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        // Do not set `cancel_flag` before finishing: the account/storage tasks consult this flag
        // and may exit early, dropping queued prefetch targets. We rely on `PrefetchFinished` to
        // shut down cleanly, and only mark cancelled after we receive the result.
        let _ = self.inner.state_tx.send(TrieDBPrefetchMessage::PrefetchFinished());

        let rx = self.inner.result_rx.lock().ok()?.take()?;
        // Never block forever in miner/block-production paths: if the background task fails to
        // respond, we fall back to "no prefetch".
        let res = match rx.recv_timeout(Duration::from_secs(2)).ok()? {
            TrieDBPrefetchResult::PrefetchAccountResult(state) => Some(state),
            TrieDBPrefetchResult::PrefetchStorageResult((_, _, _)) => None,
        };
        self.inner.cancel_flag.store(true, Ordering::Relaxed);
        // Publish the final state as well, in case callers want to reuse it.
        if let Some(state) = res.as_ref() {
            // Note: `TrieDBStatePrefetcher` is created for a single parent root.
            // Use the root passed at construction time to tag the snapshot.
            self.inner.snapshot.store(self.inner.root_hash, state.clone());
        }
        res
    }

    /// Stop the prefetcher without waiting for the final `TrieDBPrefetchState`.
    ///
    /// This is intended for miner / validation hot paths where we only want best-effort cache
    /// warming during trie-root calculation, and don't want to block on collecting results.
    pub fn stop_no_wait(&self) {
        // Best-effort signal to shut down tasks.
        let _ = self.inner.state_tx.send(TrieDBPrefetchMessage::PrefetchFinished());
        self.inner.cancel_flag.store(true, Ordering::Relaxed);
    }
}

/// Handle for TrieDB prefetch operations.
#[allow(dead_code)]
pub(super) struct TrieDBPrefetchHandle {
    /// Executor for the task.
    executor: WorkloadExecutor,
    /// Receiver for the multi proof messages from executer evm.
    message_rx: Receiver<MultiProofMessage>,
    /// Sender for the trie db prefetch messages to account task.
    state_message_tx: Sender<TrieDBPrefetchMessage>,
    /// Cancellation flag shared across all prefetch tasks.
    cancel_flag: Arc<AtomicBool>,
    /// Best-effort snapshot published by the account task.
    snapshot: TrieDBPrefetchSnapshot,
}

impl TrieDBPrefetchHandle {
    #[allow(dead_code)]
    pub(super) fn new(
        root_hash: B256,
        path_db: PathDB,
        difflayers: Option<DiffLayers>,
        executor: WorkloadExecutor,
        message_rx: Receiver<MultiProofMessage>) ->
        Result<(Self, Receiver<TrieDBPrefetchResult>), TrieDBPrefetchError> {

        // Create the channel for the trie db prefetch messages to account task.
        let (state_message_tx, state_message_rx) = mpsc::channel();

        // Create a shared cancellation flag for all prefetch tasks.
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let snapshot = TrieDBPrefetchSnapshot::default();

        // Create the account task.
        let (account_task, prefetch_result_rx) = TrieDBPrefetchAccountTask::new(
            root_hash,
            path_db,
            difflayers,
            executor.clone(),
            state_message_rx,
            cancel_flag.clone(),
            snapshot.clone(),
        )?;

        // Create the handle for the trie db prefetch task.
        let handle = Self {
            executor,
            message_rx,
            state_message_tx,
            cancel_flag,
            snapshot,
        };

        // Spawn the account task.
        handle.executor.spawn_blocking(move || {
            account_task.run();
        });

        return Ok((handle, prefetch_result_rx));
    }

    /// Returns the most recently published prefetch snapshot, if any.
    pub(super) fn try_snapshot(&self) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.snapshot.load()
    }

    /// Returns the most recent snapshot if it matches the expected root.
    pub(super) fn try_snapshot_for_root(&self, root_hash: B256) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.snapshot.load_for_root(root_hash)
    }

    /// Returns a clone of the snapshot handle.
    pub(super) fn snapshot_handle(&self) -> TrieDBPrefetchSnapshot {
        self.snapshot.clone()
    }

    #[allow(dead_code)]
    pub(super) fn run(self) {
        loop {
            match self.message_rx.recv() {
                Ok(message) => {
                    match message {
                        MultiProofMessage::PrefetchProofs(targets) => {
                            if let Err(e) = self.state_message_tx.send(TrieDBPrefetchMessage::PrefetchState(targets)) {
                                error!(
                                    target: "engine::trie_db_prefetch",
                                    "Triedb prefetch han failed to send prefetch state message(prefetch proofs) to account task: {:?}", e.to_string()
                                );
                            }
                        }
                        MultiProofMessage::TriedbPrefetchFinished => {
                            // Explicit finalization signal from the caller. This allows the caller
                            // to send an additional one-shot `PrefetchProofs` (derived from the
                            // final hashed post state) after EVM execution, but before we stop the
                            // triedb prefetch tasks.
                            if let Err(e) = self
                                .state_message_tx
                                .send(TrieDBPrefetchMessage::PrefetchFinished())
                            {
                                trace!(
                                    target: "engine::trie_db_prefetch",
                                    "Triedb prefetch handle failed to send prefetch state finished message to account task: {:?}", e.to_string()
                                );
                            }
                            break;
                        }
                        MultiProofMessage::StateUpdate(_, update) => {
                            let state = evm_state_to_trie_db_prefetch_state(&update);
                            if let Err(e) = self.state_message_tx.send(TrieDBPrefetchMessage::PrefetchState(state)) {
                                error!(
                                    target: "engine::trie_db_prefetch",
                                    "Triedb prefetch handle failed to send prefetch state message(state update) to account task: {:?}", e.to_string()
                                );
                            }
                        }
                        MultiProofMessage::FinishedStateUpdates => {
                            // Execution finished (state hook dropped). We keep the prefetch task
                            // alive until we receive an explicit `TriedbPrefetchFinished` signal,
                            // so callers can still send one-shot `PrefetchProofs` after execution.
                            trace!(
                                target: "engine::trie_db_prefetch",
                                "Received FinishedStateUpdates; awaiting TriedbPrefetchFinished"
                            );
                        }
                        _ => {
                            warn!(
                                target: "engine::trie_db_prefetch",
                                "Triedb prefetch task received unexpected message type: {:?}",
                                std::any::type_name_of_val(&message)
                            );
                        }
                    }
                }
                Err(RecvError) => {
                    // Channel closed - this happens when all Senders are dropped
                    // This is expected when the sender is closed intentionally
                    error!(
                        target: "engine::trie_db_prefetch",
                        "Triedb prefetch task message channel closed, ending task"
                    );
                    break;
                }
            }
        }
    }
}

/// Task for prefetching the account trie.
#[derive(Debug)]
pub(super) struct TrieDBPrefetchAccountTask {
    #[allow(dead_code)]
    root_hash: B256,
    path_db: PathDB,
    difflayers: Option<DiffLayers>,
    executor: WorkloadExecutor,

    /// Receiver for the trie db prefetch messages from account task.
    state_message_rx: Receiver<TrieDBPrefetchMessage>,
    /// Sender for the trie db prefetch results to account task.
    prefetch_result_tx: Sender<TrieDBPrefetchResult>,

    /// HashMap for the storage tasks sender.
    storage_tasks: HashMap<B256, Sender<TrieDBPrefetchMessage>>,
    /// HashMap for the storage results.
    storage_results: HashMap<B256, Receiver<TrieDBPrefetchResult>>,

    /// Prefetch state for the account trie.
    prefetch_state: Box<TrieDBPrefetchState<PathDB>>,

    /// Cancellation flag shared across all prefetch tasks.
    cancel_flag: Arc<AtomicBool>,

    /// Best-effort snapshot publisher.
    snapshot: TrieDBPrefetchSnapshot,

    /// Throttle snapshot publishing to avoid cloning too frequently.
    last_snapshot_at: std::time::Instant,
}

impl TrieDBPrefetchAccountTask {
    pub(super) fn new(
        root_hash: B256,
        path_db: PathDB,
        difflayers: Option<DiffLayers>,
        executor: WorkloadExecutor,
        state_message_rx: Receiver<TrieDBPrefetchMessage>,
        cancel_flag: Arc<AtomicBool>,
        snapshot: TrieDBPrefetchSnapshot)
        -> Result<(Self, Receiver<TrieDBPrefetchResult>), TrieDBPrefetchError> {

        let id = SecureTrieId::new(root_hash);
        let account_trie = SecureTrieBuilder::new(path_db.clone())
            .with_id(id)
            .build_with_difflayer(difflayers.as_ref())
            .map_err(|e| TrieDBPrefetchError::BuildAccountTrie(format!("Failed to build account trie for root hash: 0x{}, error: {}", hex::encode(root_hash), e)))?;

        let prefetch_state = Box::new(TrieDBPrefetchState {
            account_trie: account_trie,
            storage_roots: HashMap::new(),
            storage_tries: HashMap::new(),
        });

        let (prefetch_result_tx, prefetch_result_rx) = mpsc::channel();
        let task = Self {
            root_hash,
            path_db,
            difflayers,
            executor,
            state_message_rx,
            prefetch_result_tx,
            storage_tasks: HashMap::new(),
            storage_results: HashMap::new(),
            prefetch_state,
            cancel_flag,
            snapshot,
            last_snapshot_at: std::time::Instant::now(),
        };

        Ok((task, prefetch_result_rx))
    }

    /// Concurrently send PrefetchFinished message to all storage tasks.
    /// Returns (successful_addresses, failed_addresses) and removes failed ones from storage_tasks.
    pub(super) fn send_prefetch_finished_to_all_storage_tasks(&mut self) {
        let results: Vec<(B256, Result<(), mpsc::SendError<TrieDBPrefetchMessage>>)> =
            triedb_prefetch_rayon_pool().install(|| {
                self.storage_tasks
                    .par_iter()
                    .map(|(hashed_address, storage_task)| {
                        let result = storage_task.send(TrieDBPrefetchMessage::PrefetchFinished());
                        (*hashed_address, result)
                    })
                    .collect()
            });

        for (hashed_address, result) in results {
            match result {
                Ok(()) => {},
                Err(e) => {
                    self.storage_tasks.remove(&hashed_address);
                    self.storage_results.remove(&hashed_address);
                    trace!(
                        target: "engine::trie_db_prefetch",
                        "Failed to send prefetch finished message to storage trie for address 0x{:x}: {:?}", hashed_address, e
                    );
                }
            }
        }
    }

    /// Wait for all storage_results and write them to storage_tries.
    /// Returns (successful_count, failed_addresses).
    #[allow(dead_code)]
    pub(super) fn receive_prefetch_storage_results(&mut self) -> usize {
        if self.storage_results.is_empty() {
            return 0;
        }

        let start = std::time::Instant::now();
        // Iterate over all storage_results and receive results serially
        let receivers: Vec<(B256, Receiver<TrieDBPrefetchResult>)> = self.storage_results.drain().collect();
        let mut slot_count = 0;
        for (hashed_address, receiver) in receivers {
            match receiver.recv() {
                Ok(TrieDBPrefetchResult::PrefetchStorageResult((addr, storage_trie, touched_slot_count))) => {
                    if addr == hashed_address {
                        slot_count += touched_slot_count;
                        self.prefetch_state.storage_tries.insert(hashed_address, storage_trie);
                    } else {
                        warn!(
                            target: "engine::trie_db_prefetch",
                            "Address mismatch in storage result: expected 0x{:x}, got 0x{:x}", hashed_address, addr
                        );
                    }
                }
                Ok(_) => {
                    error!(
                        target: "engine::trie_db_prefetch",
                        "Unexpected result type for address 0x{:x}, prefetch account result", hashed_address
                    );
                }
                Err(e) => {
                    error!(
                        target: "engine::trie_db_prefetch",
                        "Failed to receive prefetch storage result for address 0x{:x}: {:?}", hashed_address, e
                    );
                }
            }
        }
        let duration = start.elapsed();
        info!(
            target: "engine::trie_db_prefetch",
            "Completed prefetch storage results, accounts: {}, storage tries: {}, slots: {}, finished duration: {:?}", self.prefetch_state.storage_roots.len(), self.prefetch_state.storage_tries.len(), slot_count, duration
        );
        slot_count
    }

    /// Non-blocking version that quickly collects available results without waiting.
    #[allow(dead_code)]
    fn receive_prefetch_storage_results_non_blocking(&mut self) {
        if self.storage_results.is_empty() {
            return;
        }

        // Collect available results without blocking
        let mut to_remove = Vec::new();
        for (hashed_address, receiver) in &self.storage_results {
            match receiver.try_recv() {
                Ok(TrieDBPrefetchResult::PrefetchStorageResult((addr, storage_trie, _))) => {
                    if addr == *hashed_address {
                        self.prefetch_state.storage_tries.insert(*hashed_address, storage_trie);
                    }
                    to_remove.push(*hashed_address);
                }
                Ok(_) => {
                    to_remove.push(*hashed_address);
                }
                Err(TryRecvError::Empty) => {
                    // Result not ready yet, skip it
                }
                Err(TryRecvError::Disconnected) => {
                    to_remove.push(*hashed_address);
                }
            }
        }

        // Remove processed receivers
        for addr in to_remove {
            self.storage_results.remove(&addr);
        }
    }

    pub(super) fn terminate_all_tasks(&mut self) {
        self.send_prefetch_finished_to_all_storage_tasks();
        self.receive_prefetch_storage_results();
        let state: Arc<TrieDBPrefetchState<PathDB>> = Arc::from(self.prefetch_state.clone());
        // Publish final snapshot.
        self.snapshot.store(self.root_hash, state.clone());

        if let Err(e) =
            self.prefetch_result_tx.send(TrieDBPrefetchResult::PrefetchAccountResult(state))
        {
            error!(
                target: "engine::trie_db_prefetch",
                "Failed to send prefetch account result: {:?}", e
            );
        }
    }

    pub(super) fn run(mut self) {
        // Publish interval (ms) for snapshot cloning. Keep this modest: we only need occasional
        // snapshots to be useful for the upcoming trie-root calculation.
        let publish_interval = std::env::var("TRIEDB_PREFETCH_SNAPSHOT_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(50));

        loop {
            match self.state_message_rx.recv() {
                Ok(message) => {
                    match message {
                        TrieDBPrefetchMessage::PrefetchState(targets) => {
                            // Check cancellation before processing
                            if self.cancel_flag.load(Ordering::Relaxed) {
                                self.terminate_all_tasks();
                                return;
                            }

                            // Fast path: split into (already-known roots) and (need root lookup).
                            //
                            // Root lookup can be expensive (RocksDB), so we do it in parallel.
                            let mut known: Vec<(B256, B256, B256Set)> = Vec::new();
                            let mut need_lookup: Vec<(B256, B256Set)> = Vec::new();

                            for (hashed_address, slots) in targets.into_iter() {
                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate_all_tasks();
                                    return;
                                }

                                // If we already know the storage root (from previous messages),
                                // avoid doing any DB work.
                                if let Some(root) = self.prefetch_state.storage_roots.get(&hashed_address).copied() {
                                    known.push((hashed_address, root, slots));
                                    continue;
                                }

                                // If difflayers can answer, also avoid DB hits (cheap, in-memory).
                                if let Some(root) = self
                                    .difflayers
                                    .as_ref()
                                    .and_then(|dl| dl.get_storage_root(hashed_address))
                                {
                                    known.push((hashed_address, root, slots));
                                    continue;
                                }

                                need_lookup.push((hashed_address, slots));
                            }

                            // Parallel storage-root lookup for remaining accounts.
                            if !need_lookup.is_empty() {
                                let path_db = self.path_db.clone();
                                let difflayers = self.difflayers.clone();
                                let cancel_flag = self.cancel_flag.clone();

                                let mut looked_up: Vec<(B256, B256, B256Set)> =
                                    triedb_prefetch_rayon_pool().install(|| {
                                        need_lookup
                                            .into_par_iter()
                                            .filter_map(|(hashed_address, slots)| {
                                                if cancel_flag.load(Ordering::Relaxed) {
                                                    return None
                                                }

                                                let root = lookup_storage_root(
                                                    &path_db,
                                                    difflayers.as_ref(),
                                                    hashed_address,
                                                )?;
                                                Some((hashed_address, root, slots))
                                            })
                                            .collect()
                                    });

                                known.append(&mut looked_up);
                            }

                            // Apply results sequentially:
                            // - update our local root cache
                            // - touch account paths (best-effort cache warming)
                            // - enqueue storage slot prefetching
                            for (hashed_address, storage_root, slots) in known {
                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate_all_tasks();
                                    return;
                                }

                                // Spawn/drive per-account storage prefetch task (best-effort).
                                if !slots.is_empty() {
                                    self.prefetch_slots(storage_root, hashed_address, slots);
                                }

                                // Only touch account trie + cache the root once per address.
                                if !self.prefetch_state.storage_roots.contains_key(&hashed_address) {
                                    self.prefetch_state
                                        .storage_roots
                                        .insert(hashed_address, storage_root);
                                    if let Err(e) = self
                                        .prefetch_state
                                        .account_trie
                                        .touch_account_with_hash_state(hashed_address)
                                    {
                                        warn!(
                                            target: "engine::trie_db_prefetch",
                                            "Failed to touch account trie for address 0x{:x}: {:?}",
                                            hashed_address,
                                            e
                                        );
                                    }
                                }
                            }

                            // Opportunistically collect any completed storage tries.
                            self.receive_prefetch_storage_results_non_blocking();

                            // Best-effort publish snapshot (throttled).
                            let should_publish = self.snapshot.load().is_none()
                                || self.last_snapshot_at.elapsed() >= publish_interval;
                            if should_publish {
                                self.last_snapshot_at = std::time::Instant::now();
                                self.snapshot
                                    .store(self.root_hash, Arc::from(self.prefetch_state.clone()));
                            }
                        }
                        TrieDBPrefetchMessage::PrefetchSlots(_) => {
                            error!(
                                target: "engine::trie_db_prefetch",
                                "Triedb prefetch account task received unexpected message, prefetch slots"
                            );
                        }
                        TrieDBPrefetchMessage::PrefetchFinished() => {
                            self.terminate_all_tasks();
                            return;
                        }
                    }
                }
                Err(RecvError) => {
                    // Channel closed - this happens when all Senders are dropped
                    // This is expected when the sender is closed intentionally
                    error!(
                        target: "engine::trie_db_prefetch",
                        "Triedb prefetch account task message channel closed, ending task"
                    );
                    break;
                }
            }
        }
    }

    pub(super) fn prefetch_slots(&mut self, storage_root: B256, hashed_address: B256, slots: B256Set) {
        if !self.storage_tasks.contains_key(&hashed_address) {
            let id = SecureTrieId::new(storage_root)
                .with_owner(hashed_address);
            let storage_trie = match SecureTrieBuilder::new(self.path_db.clone())
                .with_id(id)
                .build_with_difflayer(self.difflayers.as_ref())
            {
                Ok(trie) => trie,
                Err(e) => {
                    error!(
                        target: "engine::trie_db_prefetch",
                        "Failed to build storage trie for hashed_address: 0x{}, storage_root: 0x{}, error: {:?}",
                        hex::encode(hashed_address),
                        hex::encode(storage_root),
                        e
                    );
                    return;
                }
            };

            let (state_message_tx, state_message_rx) = mpsc::channel();
            let (storage_task, storage_result_rx) = TrieDBPrefetchStorageTask::new(
                hashed_address,
                storage_trie,
                state_message_rx,
                self.cancel_flag.clone(),
            );

            self.storage_results.insert(hashed_address, storage_result_rx);

            self.executor.spawn_blocking(move || {
                storage_task.run();
            });

            self.storage_tasks.insert(hashed_address, state_message_tx);
        };

        let storage_task = self.storage_tasks.get(&hashed_address).unwrap();
        if let Err(e) = storage_task.send(TrieDBPrefetchMessage::PrefetchSlots(slots)) {
            error!(
                target: "engine::trie_db_prefetch",
                "Failed to send prefetch slot message to storage trie: {:?}", e
            );
        }
        return;
    }
}

/// Task for prefetching the storage trie.
#[derive(Debug)]
pub(super) struct TrieDBPrefetchStorageTask {
    hashed_address: B256,
    storage_trie: StateTrie<PathDB>,
    touched_slots: B256Set,

    state_message_rx: Receiver<TrieDBPrefetchMessage>,
    prefetch_result_tx: Sender<TrieDBPrefetchResult>,

    /// Cancellation flag shared across all prefetch tasks.
    cancel_flag: Arc<AtomicBool>,
}

impl TrieDBPrefetchStorageTask {
    pub(super) fn new(
        hashed_address: B256,
        storage_trie: StateTrie<PathDB>,
        state_message_rx: Receiver<TrieDBPrefetchMessage>,
        cancel_flag: Arc<AtomicBool>)
        -> (Self, Receiver<TrieDBPrefetchResult>) {
        let (prefetch_result_tx, prefetch_result_rx) = mpsc::channel();
        let task = Self {
            hashed_address,
            storage_trie,
            touched_slots: B256Set::default(),
            state_message_rx,
            prefetch_result_tx,
            cancel_flag,
        };
        (task, prefetch_result_rx)
    }

    pub(super) fn terminate(&mut self) {
        if let Err(e) = self.prefetch_result_tx.send(TrieDBPrefetchResult::PrefetchStorageResult((
            self.hashed_address,
            self.storage_trie.clone(),
            self.touched_slots.len()
        ))) {
            error!(
                target: "engine::trie_db_prefetch",
                "Failed to send PrefetchStorageResult for address 0x{:x}: {:?}",
                self.hashed_address, e
            );
        }
    }

    pub(super) fn run(mut self) {
        loop {
            match self.state_message_rx.recv() {
                Ok(message) => {
                    match message {
                        TrieDBPrefetchMessage::PrefetchSlots(slots) => {
                            // Check cancellation before processing
                            if self.cancel_flag.load(Ordering::Relaxed) {
                                self.terminate();
                                return;
                            }
                            // Dedup and materialize the new slots to be fetched.
                            let mut new_slots: Vec<B256> = Vec::with_capacity(slots.len());
                            for slot in slots.iter() {
                                if self.touched_slots.contains(slot) {
                                    continue;
                                }
                                self.touched_slots.insert(*slot);
                                new_slots.push(*slot);
                            }
                            if new_slots.is_empty() {
                                continue;
                            }

                            // For large batches, split work across a few cloned tries in parallel.
                            // This improves cache-warming efficiency for accounts with many slots.
                            const PARALLEL_SLOT_THRESHOLD: usize = 64;
                            if new_slots.len() >= PARALLEL_SLOT_THRESHOLD {
                                let workers = (new_slots.len() / PARALLEL_SLOT_THRESHOLD)
                                    .clamp(2, 8);
                                let chunk_size = (new_slots.len() + workers - 1) / workers;
                                let base_trie = self.storage_trie.clone();

                                triedb_prefetch_rayon_pool().install(|| {
                                    new_slots.par_chunks(chunk_size).for_each(|chunk| {
                                        let mut trie = base_trie.clone();
                                        for slot in chunk {
                                            // Best-effort: errors don't affect correctness.
                                            let _ = trie.touch_storage_with_hash_state(*slot);
                                        }
                                    });
                                });
                            } else {
                                for slot in new_slots {
                                    if self.cancel_flag.load(Ordering::Relaxed) {
                                        self.terminate();
                                        return;
                                    }
                                    if let Err(e) = self.storage_trie.touch_storage_with_hash_state(slot) {
                                        error!(
                                            target: "engine::trie_db_prefetch",
                                            "Failed to touch storage trie for slot 0x{:x}: {:?}", slot, e
                                        );
                                    }
                                }
                            }
                        }
                        TrieDBPrefetchMessage::PrefetchFinished() => {
                            self.terminate();
                            return;
                        }
                        _ => {
                            error!(
                                target: "engine::trie_db_prefetch",
                                "Triedb prefetch storage task received unexpected message, prefetch account"
                            );
                        }
                    }
                }
                Err(RecvError) => {
                    // Channel closed - this happens when all Senders are dropped
                    // This is expected when the sender is closed intentionally
                    error!(
                        target: "engine::trie_db_prefetch",
                        "Triedb prefetch storage task message channel closed, ending task (address: 0x{:x})",
                        self.hashed_address
                    );
                    break;
                }
            }
        }
    }
}
