//! TrieDBPrefetchTask is a task that is responsible for prefetching the triedb from the database.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, RecvError, Sender, TryRecvError},
    Arc, Mutex,
};
use std::time::Duration;

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::{hex, keccak256, B256, U256};
use rust_eth_triedb_common::{DiffLayers, TrieDatabase};
use rust_eth_triedb_pathdb::PathDB;
use rust_eth_triedb_state_trie::{SecureTrieBuilder, SecureTrieError, SecureTrieId, SecureTrieTrait, StateTrie};
use rust_eth_triedb::{triedb_reth::TrieDBPrefetchState};
use reth_revm::state::EvmState;
use tracing::{error, info, trace, warn};
use rayon::prelude::*;

use crate::tree::payload_processor::executor::WorkloadExecutor;
use crate::tree::payload_processor::multiproof::MultiProofMessage;
// Commented out while update-shaped preheat is disabled:
// use crate::tree::payload_processor::triedb_hash_preheater::{
//     update_shaped_preheat_storage_trie,
// };

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
///
/// Only `PrefetchEvmState` is used; it is sent from miner/fullnode state_hook.
pub(super) enum TrieDBPrefetchMessage {
    /// Full EvmState from state_hook: drives path touch (account + storage) and update-shaped preheating.
    PrefetchEvmState(EvmState),
    /// Storage trie: (hashed_slot, value) pairs. Path-touch is done for each slot; then update-shaped preheat when non-empty.
    /// `value == 0` is treated as a best-effort delete during preheating.
    PrefetchSlotsWithValues(Vec<(B256, U256)>),
    PrefetchFinished(),
}

/// Result type for TrieDB prefetch operations.
pub(crate) enum TrieDBPrefetchResult {
    PrefetchAccountResult(Arc<TrieDBPrefetchState<PathDB>>, u64),
    PrefetchStorageResult((B256, StateTrie<PathDB>, usize)),
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
    /// Number of EvmState updates sent (each typically corresponds to one block tx).
    evm_state_updates_sent: AtomicU64,
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
                evm_state_updates_sent: AtomicU64::new(0),
            }),
        })
    }

    /// Feed a state update into the prefetcher (typically called from miner/fullnode state_hook).
    pub fn on_state_update(&self, update: &EvmState) {
        if self.inner.cancel_flag.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = self
            .inner
            .state_tx
            .send(TrieDBPrefetchMessage::PrefetchEvmState(update.clone()))
        {
            warn!(
                target: "engine::trie_db_prefetch",
                "TrieDBStatePrefetcher failed to send PrefetchEvmState: {e:?}"
            );
        } else {
            self.inner.evm_state_updates_sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Finishes the prefetcher and returns the produced prefetch state, if available.
    ///
    /// This will signal all tasks to stop and then block until the final `PrefetchAccountResult`
    /// is received (or the channel is dropped).
    ///
    /// When `block_tx_count` is provided, logs evm_state sent/processed and block tx count at stop (best case all equal).
    pub fn finish(self, block_tx_count: Option<u32>) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        let evm_state_updates_sent = self.inner.evm_state_updates_sent.load(Ordering::Relaxed);

        self.inner.cancel_flag.store(true, Ordering::Relaxed);
        let _ = self.inner.state_tx.send(TrieDBPrefetchMessage::PrefetchFinished());

        let rx = self.inner.result_rx.lock().ok()?.take()?;
        // Never block forever in miner/block-production paths: if the background task fails to
        // respond, we fall back to "no prefetch".
        match rx.recv_timeout(Duration::from_secs(2)).ok()? {
            TrieDBPrefetchResult::PrefetchAccountResult(state, evm_state_processed) => {
                info!(
                    target: "engine::trie_db_prefetch",
                    evm_state_updates_sent,
                    evm_state_processed,
                    block_tx_count = ?block_tx_count,
                    "triedb prefetcher stop (miner/fullnode): sent vs processed vs block tx (best case all equal)"
                );
                Some(state)
            }
            TrieDBPrefetchResult::PrefetchStorageResult((_, _, _)) => None,
        }
    }
}

/// Handle for TrieDB prefetch operations.
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

    pub(super) fn run(self) {
        loop {
            match self.message_rx.recv() {
                Ok(message) => {
                    match message {
                        MultiProofMessage::StateUpdate(_, update) => {
                            if let Err(e) = self
                                .state_message_tx
                                .send(TrieDBPrefetchMessage::PrefetchEvmState(update))
                            {
                                error!(
                                    target: "engine::trie_db_prefetch",
                                    "Triedb prefetch handle failed to send PrefetchEvmState: {:?}", e
                                );
                            }
                        }
                        MultiProofMessage::FinishedStateUpdates => {
                            self.cancel_flag.store(true, Ordering::Relaxed);
                            let _ = self.state_message_tx.send(TrieDBPrefetchMessage::PrefetchFinished());
                            break;
                        }
                        MultiProofMessage::PrefetchProofs(_)
                        | MultiProofMessage::EmptyProof { .. }
                        | MultiProofMessage::ProofCalculated(_)
                        | MultiProofMessage::ProofCalculationError(_) => {}
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

    /// Number of PrefetchEvmState messages processed (best case equals block tx count).
    evm_state_processed: u64,
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
            evm_state_processed: 0,
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
        let evm_state_processed = self.evm_state_processed;
        if let Err(e) = self.prefetch_result_tx.send(TrieDBPrefetchResult::PrefetchAccountResult(
            Arc::from(self.prefetch_state.clone()),
            evm_state_processed,
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
                        TrieDBPrefetchMessage::PrefetchEvmState(update) => {
                            if self.cancel_flag.load(Ordering::Relaxed) {
                                self.terminate_all_tasks();
                                return;
                            }
                            self.evm_state_processed += 1;
                            for (address, account) in update.iter() {
                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate_all_tasks();
                                    return;
                                }
                                let hashed_address = keccak256(address.as_slice());
                                let slots_with_values: Vec<(B256, U256)> = account
                                    .storage
                                    .iter()
                                    .map(|(slot, value)| {
                                        (keccak256(B256::from(*slot)), value.present_value)
                                    })
                                    .collect();
                                if let Some(storage_root) = self.get_storage_root(hashed_address) {
                                    if !slots_with_values.is_empty() {
                                        self.prefetch_slots_with_values(
                                            storage_root,
                                            hashed_address,
                                            slots_with_values,
                                        );
                                    }
                                    if !self.prefetch_state.storage_roots.contains_key(&hashed_address) {
                                        self.prefetch_state.storage_roots.insert(hashed_address, storage_root);
                                        if let Err(e) = self
                                            .prefetch_state
                                            .account_trie
                                            .touch_account_with_hash_state(hashed_address)
                                        {
                                            warn!(
                                                target: "engine::trie_db_prefetch",
                                                "Failed to touch account 0x{:x}: {:?}", hashed_address, e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        TrieDBPrefetchMessage::PrefetchSlotsWithValues(_) => {
                            error!(
                                target: "engine::trie_db_prefetch",
                                "Account task received PrefetchSlotsWithValues (belongs to storage task)"
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

    pub(super) fn prefetch_slots_with_values(
        &mut self,
        storage_root: B256,
        hashed_address: B256,
        slots_with_values: Vec<(B256, U256)>,
    ) {
        if !self.storage_tasks.contains_key(&hashed_address) {
            let id = SecureTrieId::new(storage_root).with_owner(hashed_address);
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
                self.executor.clone(),
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
        if let Err(e) =
            storage_task.send(TrieDBPrefetchMessage::PrefetchSlotsWithValues(slots_with_values))
        {
            error!(
                target: "engine::trie_db_prefetch",
                "Failed to send PrefetchSlotsWithValues to storage trie: {:?}", e
            );
        }
    }
}

/// Task for prefetching the storage trie.
#[derive(Debug)]
pub(super) struct TrieDBPrefetchStorageTask {
    hashed_address: B256,
    storage_trie: StateTrie<PathDB>,

    #[allow(dead_code)] // used when update-shaped preheat is enabled
    executor: WorkloadExecutor,

    state_message_rx: Receiver<TrieDBPrefetchMessage>,
    prefetch_result_tx: Sender<TrieDBPrefetchResult>,

    /// Cancellation flag shared across all prefetch tasks.
    cancel_flag: Arc<AtomicBool>,

    /// Number of update+hash-shaped preheats performed for this storage trie.
    #[allow(dead_code)] // used when update-shaped preheat is enabled
    storage_update_hash_preheat_runs: u64,
}

impl TrieDBPrefetchStorageTask {
    pub(super) fn new(
        hashed_address: B256,
        storage_trie: StateTrie<PathDB>,
        executor: WorkloadExecutor,
        state_message_rx: Receiver<TrieDBPrefetchMessage>,
        cancel_flag: Arc<AtomicBool>)
        -> (Self, Receiver<TrieDBPrefetchResult>) {
        let (prefetch_result_tx, prefetch_result_rx) = mpsc::channel();
        let task = Self {
            hashed_address,
            storage_trie,
            executor,
            state_message_rx,
            prefetch_result_tx,
            cancel_flag,
            storage_update_hash_preheat_runs: 0,
        };
        (task, prefetch_result_rx)
    }

    pub(super) fn terminate(&mut self) {
        if let Err(e) = self.prefetch_result_tx.send(TrieDBPrefetchResult::PrefetchStorageResult((
            self.hashed_address,
            self.storage_trie.clone(),
            0usize,
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
                        TrieDBPrefetchMessage::PrefetchSlotsWithValues(changed_slots) => {
                            if self.cancel_flag.load(Ordering::Relaxed) {
                                self.terminate();
                                return;
                            }
                            for (hashed_slot, _value) in changed_slots.iter() {
                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate();
                                    return;
                                }
                                if let Err(e) = self.storage_trie.touch_storage_with_hash_state(*hashed_slot) {
                                    error!(
                                        target: "engine::trie_db_prefetch",
                                        "Failed to touch storage trie for slot 0x{:x}: {:?}", hashed_slot, e
                                    );
                                }
                            }
                            if changed_slots.is_empty() {
                                continue;
                            }

                            // Commented out: update-shaped preheat (to isolate bottleneck)
                            // self.storage_update_hash_preheat_runs =
                            //     self.storage_update_hash_preheat_runs.saturating_add(1);
                            // let mut storage_trie = self.storage_trie.clone();
                            // let cancel = self.cancel_flag.clone();
                            // let addr = self.hashed_address;
                            // let changed = changed_slots.len();
                            // let run_idx = self.storage_update_hash_preheat_runs;
                            // let changed_slots_owned = changed_slots;
                            // let pool = self.executor.rayon_pool().clone();
                            // pool.spawn(move || {
                            //     let stats = update_shaped_preheat_storage_trie(
                            //         &mut storage_trie,
                            //         addr,
                            //         &changed_slots_owned,
                            //         &cancel,
                            //     );
                            //     debug!(
                            //         target: "engine::trie_db_prefetch",
                            //         trie = "storage",
                            //         mode = "update_preheat",
                            //         address = %format!("0x{:x}", addr),
                            //         run = run_idx,
                            //         changed_slots = changed,
                            //         updates_applied = stats.updates_applied,
                            //         deletes_applied = stats.deletes_applied,
                            //         update_errors = stats.update_errors,
                            //         preheat_ms = stats.elapsed.as_secs_f64() * 1000.0,
                            //         "Triedb update-shaped preheat finished"
                            //     );
                            // });
                            let _ = changed_slots;
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
