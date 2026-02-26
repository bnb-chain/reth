//! TrieDBPrefetchTask is a task that is responsible for prefetching the triedb from the database.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvError, Sender, TryRecvError},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::{hex, keccak256, map::B256Set, B256};
use rust_eth_triedb_common::{DiffLayers, TrieDatabase};
use rust_eth_triedb_pathdb::PathDB;
use rust_eth_triedb_state_trie::{SecureTrieId, SecureTrieBuilder, SecureTrieError, SecureTrieTrait, StateTrie};
use rust_eth_triedb::{triedb_reth::TrieDBPrefetchState};
use reth_revm::state::EvmState;
use reth_trie::MultiProofTargets;
use tracing::{debug, error, info, trace, warn};
use rayon::prelude::*;

use crate::tree::payload_processor::executor::WorkloadExecutor;
use crate::tree::payload_processor::multiproof::MultiProofMessage;

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
    let mut state = MultiProofTargets::with_capacity(evm_state.len());
    for (address, account) in evm_state {
        let hashed_address = keccak256(address.as_slice());
        let slots: B256Set = account
            .storage
            .iter()
            .map(|(slot, _)| keccak256(B256::from(*slot)))
            .collect();
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

        let (account_task, prefetch_result_rx) = TrieDBPrefetchAccountTask::new(
            root_hash,
            path_db,
            difflayers,
            executor,
            state_rx,
            cancel_flag.clone(),
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

    /// Finishes the prefetcher and returns the produced prefetch state, if available.
    ///
    /// This will signal all tasks to stop and then block until the final `PrefetchAccountResult`
    /// is received (or the channel is dropped).
    pub fn finish(self) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.inner.cancel_flag.store(true, Ordering::Relaxed);
        let _ = self.inner.state_tx.send(TrieDBPrefetchMessage::PrefetchFinished());

        let rx = self.inner.result_rx.lock().ok()?.take()?;
        // Never block forever in miner/block-production paths: if the background task fails to
        // respond, we fall back to "no prefetch".
        match rx.recv_timeout(Duration::from_secs(2)).ok()? {
            TrieDBPrefetchResult::PrefetchAccountResult(state) => Some(state),
            TrieDBPrefetchResult::PrefetchStorageResult((_, _, _)) => None,
        }
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

        // Create the account task.
        let (account_task, prefetch_result_rx) = TrieDBPrefetchAccountTask::new(
            root_hash,
            path_db,
            difflayers,
            executor.clone(),
            state_message_rx,
            cancel_flag.clone(),
        )?;

        // Create the handle for the trie db prefetch task.
        let handle = Self {
            executor,
            message_rx,
            state_message_tx,
            cancel_flag,
        };

        // Spawn the account task.
        handle.executor.spawn_blocking(move || {
            account_task.run();
        });

        return Ok((handle, prefetch_result_rx));
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
                            // Set cancellation flag to immediately stop all tasks
                            self.cancel_flag.store(true, Ordering::Relaxed);
                            // Send PrefetchFinished message to account task
                            if let Err(e) = self.state_message_tx.send(TrieDBPrefetchMessage::PrefetchFinished()) {
                                trace!(
                                    target: "engine::trie_db_prefetch",
                                    "Triedb prefetch handle failed to send prefetch state finished message to account task: {:?}", e.to_string()
                                );
                            }
                            break;
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
}

impl TrieDBPrefetchAccountTask {
    pub(super) fn new(
        root_hash: B256,
        path_db: PathDB,
        difflayers: Option<DiffLayers>,
        executor: WorkloadExecutor,
        state_message_rx: Receiver<TrieDBPrefetchMessage>,
        cancel_flag: Arc<AtomicBool>)
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
        };

        Ok((task, prefetch_result_rx))
    }

    /// Concurrently send PrefetchFinished message to all storage tasks.
    /// Returns (successful_addresses, failed_addresses) and removes failed ones from storage_tasks.
    pub(super) fn send_prefetch_finished_to_all_storage_tasks(&mut self) {
        let results: Vec<(B256, Result<(), mpsc::SendError<TrieDBPrefetchMessage>>)> = self.storage_tasks
            .par_iter()
            .map(|(hashed_address, storage_task)| {
                let result = storage_task.send(TrieDBPrefetchMessage::PrefetchFinished());
                (*hashed_address, result)
            })
            .collect();

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
        if let Err(e) = self.prefetch_result_tx.send(TrieDBPrefetchResult::PrefetchAccountResult(
            Arc::from(self.prefetch_state.clone())
        )) {
            error!(
                target: "engine::trie_db_prefetch",
                "Failed to send prefetch account result: {:?}", e
            );
        }
    }

    pub(super) fn run(mut self) {
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
                            for (address, slots) in targets.iter() {
                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate_all_tasks();
                                    return;
                                }
                                if let Some(storage_root) = self.get_storage_root(*address) {
                                    if !slots.is_empty() {
                                        self.prefetch_slots(storage_root, *address, slots.clone());
                                    }
                                    if !self.prefetch_state.storage_roots.contains_key(address) {
                                        self.prefetch_state.storage_roots.insert(*address, storage_root);
                                        if let Err(e) = self.prefetch_state.account_trie.touch_account_with_hash_state(*address) {
                                            warn!(
                                                target: "engine::trie_db_prefetch",
                                                "Failed to get account trie for address 0x{:x}: {:?}", address, e
                                            );
                                        }
                                    }
                                }
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

    pub(super) fn get_storage_root(&mut self, hashed_address: B256) -> Option<B256> {
        if let Some(storage_root) = self.prefetch_state.storage_roots.get(&hashed_address) {
            return Some(*storage_root);
        }

        if let Some(difflayers) = &self.difflayers {
            if let Some(storage_root) = difflayers.get_storage_root(hashed_address) {
                return Some(storage_root);
            }
        }

        match self.path_db.get_storage_root(hashed_address) {
            Ok(Some(storage_root)) => Some(storage_root),
            Ok(None) => Some (EMPTY_ROOT_HASH),
            Err(e) => {
                error!(
                    target: "engine::trie_db_prefetch",
                    "Failed to get storage root for hashed_address: 0x{}, error: {:?}", hex::encode(hashed_address), e
                );
                None
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

    // Stats for observability/debugging (debug-level logs).
    created_at: Instant,
    first_message_at: Option<Instant>,
    last_message_at: Instant,
    total_slots_received: u64,
    total_slots_already_touched: u64,
    total_slots_touch_ok: u64,
    total_slots_touch_err: u64,
    total_batches: u64,
    total_parallel_batches: u64,
    total_touch_us: u64,
}

impl TrieDBPrefetchStorageTask {
    #[inline]
    fn max_parallel_touch_workers() -> usize {
        // Conservative default. We want to raise prefetch throughput for hotspot accounts without
        // overwhelming RocksDB with random reads.
        8
    }

    #[inline]
    fn min_parallel_touch_slots() -> usize {
        // Only parallelize when a batch is large enough to amortize clone/dispatch overhead.
        64
    }

    pub(super) fn new(
        hashed_address: B256,
        storage_trie: StateTrie<PathDB>,
        state_message_rx: Receiver<TrieDBPrefetchMessage>,
        cancel_flag: Arc<AtomicBool>)
        -> (Self, Receiver<TrieDBPrefetchResult>) {
        let (prefetch_result_tx, prefetch_result_rx) = mpsc::channel();
        let now = Instant::now();
        let task = Self {
            hashed_address,
            storage_trie,
            touched_slots: B256Set::default(),
            state_message_rx,
            prefetch_result_tx,
            cancel_flag,
            created_at: now,
            first_message_at: None,
            last_message_at: now,
            total_slots_received: 0,
            total_slots_already_touched: 0,
            total_slots_touch_ok: 0,
            total_slots_touch_err: 0,
            total_batches: 0,
            total_parallel_batches: 0,
            total_touch_us: 0,
        };
        (task, prefetch_result_rx)
    }

    pub(super) fn terminate(&mut self) {
        let lifetime_ms = self.created_at.elapsed().as_secs_f64() * 1000.0;
        let first_to_last_ms = self
            .first_message_at
            .map(|first| self.last_message_at.saturating_duration_since(first).as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        debug!(
            target: "engine::trie_db_prefetch",
            address = %format!("0x{:x}", self.hashed_address),
            touched_slots = self.touched_slots.len(),
            lifetime_ms,
            first_to_last_ms,
            total_slots_received = self.total_slots_received,
            total_slots_already_touched = self.total_slots_already_touched,
            total_slots_touch_ok = self.total_slots_touch_ok,
            total_slots_touch_err = self.total_slots_touch_err,
            total_batches = self.total_batches,
            total_parallel_batches = self.total_parallel_batches,
            total_touch_ms = (self.total_touch_us as f64) / 1000.0,
            "Triedb prefetch storage task terminating"
        );
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
                    let now = Instant::now();
                    if self.first_message_at.is_none() {
                        self.first_message_at = Some(now);
                    }
                    self.last_message_at = now;
                    match message {
                        TrieDBPrefetchMessage::PrefetchSlots(slots) => {
                            // Check cancellation before processing
                            if self.cancel_flag.load(Ordering::Relaxed) {
                                self.terminate();
                                return;
                            }

                            self.total_batches = self.total_batches.saturating_add(1);
                            self.total_slots_received =
                                self.total_slots_received.saturating_add(slots.len() as u64);

                            // Fast path: filter out already touched slots.
                            let mut new_slots: Vec<B256> = Vec::new();
                            new_slots.reserve(slots.len());
                            for slot in slots.iter() {
                                if self.touched_slots.contains(slot) {
                                    continue;
                                }
                                new_slots.push(*slot);
                            }
                            let slots_already_touched = slots.len().saturating_sub(new_slots.len());
                            self.total_slots_already_touched = self
                                .total_slots_already_touched
                                .saturating_add(slots_already_touched as u64);

                            if new_slots.is_empty() {
                                continue;
                            }

                            let touch_started = Instant::now();

                            // Parallelize within a single account when the incoming batch is large.
                            // This mirrors geth-bsc's "parallel subfetchers" behavior for hotspot accounts,
                            // but we only rely on warming PathDB/RocksDB caches for correctness.
                            let use_parallel = new_slots.len() >= Self::min_parallel_touch_slots();
                            let mut touch_ok = 0u64;
                            let mut touch_err = 0u64;
                            let mut parallel_workers = 1usize;
                            let mut chunk_size = new_slots.len();

                            if use_parallel {
                                self.total_parallel_batches = self.total_parallel_batches.saturating_add(1);
                                parallel_workers = Self::max_parallel_touch_workers().max(1);
                                // Distribute work roughly evenly across workers.
                                chunk_size = (new_slots.len() + parallel_workers - 1) / parallel_workers;
                                if chunk_size == 0 {
                                    chunk_size = 1;
                                }

                                let base_trie = self.storage_trie.clone();
                                let cancel_flag = self.cancel_flag.clone();
                                // Touch slots in parallel using cloned trie instances (not shared mutably).
                                // Note: This does not "merge" expanded nodes back into `self.storage_trie`, but it
                                // warms PathDB's node cache and RocksDB block cache which is what we primarily need.
                                let touched: Vec<Vec<B256>> = new_slots
                                    .par_chunks(chunk_size)
                                    .map(|chunk| {
                                        let mut trie = base_trie.clone();
                                        let mut ok_slots = Vec::with_capacity(chunk.len());
                                        for slot in chunk {
                                            if cancel_flag.load(Ordering::Relaxed) {
                                                break;
                                            }
                                            if let Err(e) = trie.touch_storage_with_hash_state(*slot) {
                                                // Keep errors visible but avoid per-slot spam in common cases.
                                                trace!(
                                                    target: "engine::trie_db_prefetch",
                                                    address = %format!("0x{:x}", self.hashed_address),
                                                    slot = %format!("0x{:x}", slot),
                                                    "touch_storage_with_hash_state failed: {e:?}"
                                                );
                                            } else {
                                                ok_slots.push(*slot);
                                            }
                                        }
                                        ok_slots
                                    })
                                    .collect();

                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate();
                                    return;
                                }

                                for ok_slots in touched {
                                    let ok_len = ok_slots.len() as u64;
                                    touch_ok = touch_ok.saturating_add(ok_len);
                                    for slot in ok_slots {
                                        self.touched_slots.insert(slot);
                                    }
                                }
                                touch_err = (new_slots.len() as u64).saturating_sub(touch_ok);
                            } else {
                                // Sequential touch keeps the mutated/warmed nodes in `self.storage_trie` itself,
                                // which can help when we later return this trie in `PrefetchStorageResult`.
                                for slot in new_slots {
                                    if self.cancel_flag.load(Ordering::Relaxed) {
                                        self.terminate();
                                        return;
                                    }
                                    if let Err(e) = self.storage_trie.touch_storage_with_hash_state(slot) {
                                        trace!(
                                            target: "engine::trie_db_prefetch",
                                            address = %format!("0x{:x}", self.hashed_address),
                                            slot = %format!("0x{:x}", slot),
                                            "touch_storage_with_hash_state failed: {e:?}"
                                        );
                                        touch_err = touch_err.saturating_add(1);
                                    } else {
                                        self.touched_slots.insert(slot);
                                        touch_ok = touch_ok.saturating_add(1);
                                    }
                                }
                            }

                            let touch_elapsed = touch_started.elapsed();
                            let touch_us_u64 =
                                (touch_elapsed.as_micros().min(u64::MAX as u128)) as u64;
                            self.total_touch_us =
                                self.total_touch_us.saturating_add(touch_us_u64);
                            self.total_slots_touch_ok = self.total_slots_touch_ok.saturating_add(touch_ok);
                            self.total_slots_touch_err = self.total_slots_touch_err.saturating_add(touch_err);

                            if touch_err > 0 {
                                warn!(
                                    target: "engine::trie_db_prefetch",
                                    address = %format!("0x{:x}", self.hashed_address),
                                    slots_received = slots.len(),
                                    slots_already_touched,
                                    slots_attempted = touch_ok + touch_err,
                                    touch_ok,
                                    touch_err,
                                    parallel = use_parallel,
                                    parallel_workers,
                                    chunk_size,
                                    touch_ms = touch_elapsed.as_secs_f64() * 1000.0,
                                    "Triedb prefetch had slot touch errors"
                                );
                            }

                            debug!(
                                target: "engine::trie_db_prefetch",
                                address = %format!("0x{:x}", self.hashed_address),
                                slots_received = slots.len(),
                                slots_new = slots.len().saturating_sub(slots_already_touched),
                                slots_already_touched,
                                touch_ok,
                                touch_err,
                                touched_slots_total = self.touched_slots.len(),
                                parallel = use_parallel,
                                parallel_workers,
                                chunk_size,
                                touch_ms = touch_elapsed.as_secs_f64() * 1000.0,
                                avg_touch_us = if touch_ok + touch_err == 0 {
                                    0.0
                                } else {
                                    (touch_elapsed.as_micros() as f64) / ((touch_ok + touch_err) as f64)
                                },
                                "Triedb prefetch touched storage slots"
                            );
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
