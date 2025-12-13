//! TrieDBPrefetchTask is a task that is responsible for prefetching the triedb from the database.

use std::collections::{HashMap};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvError, Sender, TryRecvError},
    Arc,
};

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
                        MultiProofMessage::StateUpdate(_, _) => {
                            // let state = evm_state_to_trie_db_prefetch_state(&update);
                            // if let Err(e) = self.state_message_tx.send(TrieDBPrefetchMessage::PrefetchState(state)) {
                            //     error!(
                            //         target: "engine::trie_db_prefetch",
                            //         "Triedb prefetch handle failed to send prefetch state message(state update) to account task: {:?}", e.to_string()
                            //     );
                            // }
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
                            for slot in slots.iter() {
                                if self.cancel_flag.load(Ordering::Relaxed) {
                                    self.terminate();
                                    return;
                                }
                                if self.touched_slots.contains(slot) {
                                    continue;
                                }
                                if let Err(e) = self.storage_trie.touch_storage_with_hash_state(*slot) {
                                    error!(
                                        target: "engine::trie_db_prefetch",
                                        "Failed to touch storage trie for slot 0x{:x}: {:?}", slot, e
                                    );
                                } else {
                                    self.touched_slots.insert(*slot);
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
