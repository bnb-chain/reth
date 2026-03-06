//! TrieDBPrefetchTask is a task that is responsible for prefetching the triedb from the database.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvError, Sender, TryRecvError},
    Arc, Mutex,
};
use std::time::Duration;

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

use alloy_evm::block::StateChangeSource;
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

/// Unified handle for TrieDB prefetch used by both fullnode and miner.
///
/// Single entry point: all input is via [`MultiProofMessage`] on the channel returned from [`new`](Self::new).
/// - **Prewarm path**: PrefetchProofs from prewarm task (or miner's prewarm sender).
/// - **Execution path**: [`send_state_update`](Self::send_state_update) (miner state hook) or
///   `StateUpdate` from fullnode's state hook over the same channel.
///
/// When the sender is dropped (or `FinishedStateUpdates` is sent), the forwarder sends
/// `PrefetchFinished` and the account task produces the result. Use [`finish`](Self::finish)
/// (miner) or [`recv_result`](Self::recv_result) (fullnode) to obtain it.
#[derive(Debug, Clone)]
pub struct TrieDBPrefetchHandle {
    inner: Arc<TrieDBPrefetchHandleInner>,
}

#[derive(Debug)]
struct TrieDBPrefetchHandleInner {
    result_rx: Mutex<Option<Receiver<TrieDBPrefetchResult>>>,
    cancel_flag: Arc<AtomicBool>,
    /// Clone of the multi_proof sender; dropped in finish()/before recv_result so forwarder gets RecvError and sends PrefetchFinished.
    multi_proof_tx: Mutex<Option<Sender<MultiProofMessage>>>,
}

impl TrieDBPrefetchHandle {
    /// Creates and starts the prefetcher: account task + forwarder loop.
    /// Returns `(handle, multi_proof_sender)`. Wire prewarm to the sender; use
    /// [`send_state_update`](Self::send_state_update) for execution (miner) or the same sender
    /// in a state hook (fullnode). Drop the sender when done, then call [`finish`](Self::finish)
    /// or [`recv_result`](Self::recv_result).
    pub fn new(
        root_hash: B256,
        path_db: PathDB,
        difflayers: Option<DiffLayers>,
        executor: WorkloadExecutor,
    ) -> Result<(Self, Sender<MultiProofMessage>), TrieDBPrefetchError> {
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

        spawn_exec.spawn_blocking(move || {
            account_task.run();
        });

        let (multi_proof_tx, message_rx) = mpsc::channel();
        let forward_state_tx = state_tx;
        let forward_cancel = cancel_flag.clone();
        spawn_exec.spawn_blocking(move || {
            loop {
                match message_rx.recv() {
                    Ok(message) => {
                        match message {
                            MultiProofMessage::PrefetchProofs(targets) => {
                                if forward_state_tx
                                    .send(TrieDBPrefetchMessage::PrefetchState(targets))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            MultiProofMessage::StateUpdate(_, update) => {
                                let targets = evm_state_to_trie_db_prefetch_state(&update);
                                if forward_state_tx
                                    .send(TrieDBPrefetchMessage::PrefetchState(targets))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            MultiProofMessage::FinishedStateUpdates => {
                                forward_cancel.store(true, Ordering::Relaxed);
                                let _ = forward_state_tx.send(TrieDBPrefetchMessage::PrefetchFinished());
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => {
                        forward_cancel.store(true, Ordering::Relaxed);
                        let _ = forward_state_tx.send(TrieDBPrefetchMessage::PrefetchFinished());
                        break;
                    }
                }
            }
        });

        let handle = Self {
            inner: Arc::new(TrieDBPrefetchHandleInner {
                result_rx: Mutex::new(Some(prefetch_result_rx)),
                cancel_flag,
                multi_proof_tx: Mutex::new(Some(multi_proof_tx.clone())),
            }),
        };
        Ok((handle, multi_proof_tx))
    }

    /// Feeds a state update into the prefetcher (miner state hook). Equivalent to sending
    /// `MultiProofMessage::StateUpdate(Execution, state)` on the multi_proof channel.
    pub fn send_state_update(&self, update: &EvmState) {
        if self.inner.cancel_flag.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(guard) = self.inner.multi_proof_tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(MultiProofMessage::StateUpdate(
                    StateChangeSource::Transaction(0),
                    update.clone(),
                ));
            }
        }
    }

    /// Finishes and returns the prefetch state (miner path). Drops the multi_proof sender so the
    /// forwarder exits and sends PrefetchFinished, then waits up to 2s for the result.
    pub fn finish(self) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        self.inner.cancel_flag.store(true, Ordering::Relaxed);
        drop(self.inner.multi_proof_tx.lock().ok()?.take());
        let rx = self.inner.result_rx.lock().ok()?.take()?;
        match rx.recv_timeout(Duration::from_secs(2)).ok()? {
            TrieDBPrefetchResult::PrefetchAccountResult(state) => Some(state),
            TrieDBPrefetchResult::PrefetchStorageResult((_, _, _)) => None,
        }
    }

    /// Receives the prefetch result (fullnode path). Caller must drop the PayloadHandle's
    /// to_multi_proof before calling this so the forwarder exits and sends PrefetchFinished.
    pub fn recv_result(
        &mut self,
        timeout: Duration,
    ) -> Option<Arc<TrieDBPrefetchState<PathDB>>> {
        drop(self.inner.multi_proof_tx.lock().ok()?.take());
        let rx = self.inner.result_rx.lock().ok()?.take()?;
        match rx.recv_timeout(timeout).ok()? {
            TrieDBPrefetchResult::PrefetchAccountResult(state) => Some(state),
            TrieDBPrefetchResult::PrefetchStorageResult((_, _, _)) => None,
        }
    }
}

/// Task for prefetching the account trie.
#[derive(Debug)]
pub(super) struct TrieDBPrefetchAccountTask {
    #[allow(dead_code)]
    root_hash: B256,
    path_db: PathDB,
    difflayers: Option<Arc<DiffLayers>>,
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

    /// Timestamp when the account task was created.
    creation_time: std::time::Instant,
    /// Number of storage tries spawned.
    storage_tries_created: usize,
    /// Number of unique accounts touched in the account trie.
    accounts_touched: usize,
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

        let difflayers = difflayers.map(Arc::new);
        let id = SecureTrieId::new(root_hash);
        let account_trie = SecureTrieBuilder::new(path_db.clone())
            .with_id(id)
            .build_with_difflayer(difflayers.as_deref())
            .map_err(|e| TrieDBPrefetchError::BuildAccountTrie(format!("Failed to build account trie for root hash: 0x{}, error: {}", hex::encode(root_hash), e)))?;

        let prefetch_state = Box::new(TrieDBPrefetchState {
            account_trie,
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
            creation_time: std::time::Instant::now(),
            storage_tries_created: 0,
            accounts_touched: 0,
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
    /// Receives results in parallel via rayon, then inserts serially.
    pub(super) fn receive_prefetch_storage_results(&mut self) -> usize {
        if self.storage_results.is_empty() {
            return 0;
        }

        let start = std::time::Instant::now();
        let receivers: Vec<(B256, Receiver<TrieDBPrefetchResult>)> = self.storage_results.drain().collect();

        // Receive from all storage tasks in parallel — each recv() blocks
        // until its task finishes, so parallelism avoids head-of-line blocking.
        let results: Vec<Option<(B256, StateTrie<PathDB>, usize)>> = receivers
            .into_par_iter()
            .map(|(hashed_address, receiver)| {
                match receiver.recv() {
                    Ok(TrieDBPrefetchResult::PrefetchStorageResult((addr, storage_trie, touched_slot_count))) => {
                        if addr == hashed_address {
                            Some((hashed_address, storage_trie, touched_slot_count))
                        } else {
                            warn!(
                                target: "engine::trie_db_prefetch",
                                "Address mismatch in storage result: expected 0x{:x}, got 0x{:x}", hashed_address, addr
                            );
                            None
                        }
                    }
                    Ok(_) => {
                        error!(
                            target: "engine::trie_db_prefetch",
                            "Unexpected result type for address 0x{:x}, prefetch account result", hashed_address
                        );
                        None
                    }
                    Err(e) => {
                        error!(
                            target: "engine::trie_db_prefetch",
                            "Failed to receive prefetch storage result for address 0x{:x}: {:?}", hashed_address, e
                        );
                        None
                    }
                }
            })
            .collect();

        // Insert collected results serially
        let mut slot_count = 0;
        for (hashed_address, storage_trie, touched_slot_count) in results.into_iter().flatten() {
            slot_count += touched_slot_count;
            self.prefetch_state.storage_tries.insert(hashed_address, storage_trie);
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

    pub(super) fn terminate_all_tasks(mut self) {
        self.send_prefetch_finished_to_all_storage_tasks();
        self.receive_prefetch_storage_results();
        // Clear account trie tracer — not needed for prefetch reuse, only for commit
        self.prefetch_state.account_trie.trie_mut().tracer = Default::default();
        let total_prefetch_ms = self.creation_time.elapsed().as_secs_f64() * 1000.0;
        info!(
            target: "engine::trie_db_prefetch",
            evm_state_processed = self.evm_state_processed,
            accounts_touched = self.accounts_touched,
            storage_tries_created = self.storage_tries_created,
            storage_tries_collected = self.prefetch_state.storage_tries.len(),
            storage_roots_collected = self.prefetch_state.storage_roots.len(),
            total_prefetch_ms,
            "Account prefetch task finished"
        );
        let prefetch_state = Arc::new(*self.prefetch_state);
        if let Err(e) = self.prefetch_result_tx.send(TrieDBPrefetchResult::PrefetchAccountResult(
            prefetch_state,
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
                                        self.accounts_touched += 1;
                                        if let Err(e) = self
                                            .prefetch_state
                                            .account_trie
                                            .touch_account_with_hash_state(*address)
                                        {
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
                .build_with_difflayer(self.difflayers.as_deref())
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
            self.storage_tries_created += 1;

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

    /// Number of slots touched in this storage trie.
    touch_count: usize,
    /// Total time spent in touch operations.
    touch_duration: std::time::Duration,
    /// Number of PrefetchSlots messages received.
    messages_received: usize,
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
            touch_count: 0,
            touch_duration: std::time::Duration::ZERO,
            messages_received: 0,
        };
        (task, prefetch_result_rx)
    }

    pub(super) fn terminate(mut self) {
        // Clear tracer — not needed for prefetch reuse, only for commit
        self.storage_trie.trie_mut().tracer = Default::default();
        trace!(
            target: "engine::trie_db_prefetch",
            address = %format!("0x{:x}", self.hashed_address),
            touch_count = self.touch_count,
            touch_ms = self.touch_duration.as_secs_f64() * 1000.0,
            messages_received = self.messages_received,
            "Storage prefetch task finished"
        );
        let hashed_address = self.hashed_address;
        let storage_trie = self.storage_trie;
        if let Err(e) = self.prefetch_result_tx.send(TrieDBPrefetchResult::PrefetchStorageResult((
            hashed_address,
            storage_trie,
            0usize,
        ))) {
            error!(
                target: "engine::trie_db_prefetch",
                "Failed to send PrefetchStorageResult for address 0x{:x}: {:?}",
                hashed_address, e
            );
        }
    }

    pub(super) fn run(mut self) {
        loop {
            match self.state_message_rx.recv() {
                Ok(message) => {
                    match message {
                        TrieDBPrefetchMessage::PrefetchSlots(slots) => {
                            if self.cancel_flag.load(Ordering::Relaxed) {
                                self.terminate();
                                return;
                            }
                            // Sort by hashed slot for better trie traversal locality:
                            // adjacent hashed keys share trie path prefixes, so earlier
                            // CoW resolutions are reused by subsequent touches.
                            let mut sorted_slots: Vec<B256> = slots.iter().copied().collect();
                            if sorted_slots.len() > 1 {
                                sorted_slots.sort_unstable();
                            }
                            for slot in sorted_slots.iter() {
                            self.messages_received += 1;
                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate();
                                    return;
                                }
                                if self.touched_slots.contains(slot) {
                                    continue;
                                }
                                let touch_start = std::time::Instant::now();
                                if let Err(e) = self.storage_trie.touch_storage_with_hash_state(*slot) {
                                    error!(
                                        target: "engine::trie_db_prefetch",
                                        "Failed to touch storage trie for slot 0x{:x}: {:?}", slot, e
                                    );
                                } else {
                                    self.touched_slots.insert(*slot);
                                }
                                self.touch_duration += touch_start.elapsed();
                                self.touch_count += 1;
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
