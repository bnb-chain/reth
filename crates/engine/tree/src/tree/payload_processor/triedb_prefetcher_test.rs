//! Unit tests for TrieDBPrefetchHandle

use std::collections::HashMap;
use std::sync::mpsc;

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::{keccak256, Address, B256, U256};
use rust_eth_triedb::{
    init_global_triedb_manager, get_global_triedb, TrieDBHashedPostState,
};
use rust_eth_triedb_state_trie::account::StateAccount;
use reth_revm::state::EvmState;
use reth_trie::MultiProofTargets;
use alloy_primitives::map::B256Set;
use revm_state::{Account, AccountInfo};

use crate::tree::payload_processor::triedb_prefetcher::TrieDBPrefetchHandle;
use crate::tree::payload_processor::triedb_prefetcher::TrieDBPrefetchResult;
use crate::tree::payload_processor::multiproof::MultiProofMessage;
use crate::tree::payload_processor::executor::WorkloadExecutor;
use alloy_evm::block::StateChangeSource;
use revm_state::EvmStorageSlot;
use alloy_primitives::map::HashMap as AlloyHashMap;

#[test]
fn test_triedb_prefetch_handle() {    
    // Initialize tracing for test output
    reth_tracing::init_test_tracing();
    
    // Step 1: Create a temporary directory for the triedb
    let temp_dir = std::env::temp_dir().join(format!("triedb_test_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).expect("Failed to create temp directory");
    let path_str = temp_dir.to_str().expect("Failed to convert path to string");

    // Step 2: Initialize the global triedb manager
    init_global_triedb_manager(path_str);

    // Step 3: Get the global triedb instance
    let mut triedb = get_global_triedb();

    // Step 4: Create hashed_post_state with 100 accounts, all with storage
    const NUM_ACCOUNTS: usize = 100;
    const NUM_STORAGE_SLOTS_PER_ACCOUNT: usize = 10; // Each account has 10 storage slots
    const NUM_PREFETCH_SLOTS: usize = 5; // Only prefetch 5 slots per account

    let mut hashed_post_state = TrieDBHashedPostState::default();
    let mut expected_accounts = Vec::new();
    let mut address_list: Vec<Address> = Vec::new();
    
    for i in 0..NUM_ACCOUNTS {
        // Generate a unique address_bytes and create Address first
        // Then hash the Address to get hashed_address (consistent with evm_state_to_trie_db_prefetch_state)
        let address_bytes = format!("account_{}", i).into_bytes();
        // Create Address from address_bytes using a deterministic method
        // We'll use keccak256 of address_bytes and take first 20 bytes as Address
        // Then keccak256(Address) will give us the hashed_address
        let address_hash = keccak256(&address_bytes);
        let address = Address::from_slice(&address_hash[..20]);
        let hashed_address = keccak256(address.as_slice());
        
        // Store address for later use in EvmState
        address_list.push(address);

        // Create StateAccount
        let mut state_account = StateAccount::default()
            .with_nonce(i as u64)
            .with_balance(U256::from(i as u64) * U256::from(1_000_000_000_000_000_000u64));

        // Add storage for all accounts (10 slots each)
        let mut account_storage: HashMap<B256, Option<U256>> = HashMap::new();
        for j in 0..NUM_STORAGE_SLOTS_PER_ACCOUNT {
            let slot_key = format!("slot_{}_{}", i, j).into_bytes();
            let hashed_key = keccak256(slot_key);
            let value = U256::from(i * 100 + j);
            account_storage.insert(hashed_key, Some(value));
        }
        hashed_post_state.storage_states.insert(hashed_address, account_storage);

        // Set storage root (will be computed during commit)
        state_account = state_account.with_storage_root(EMPTY_ROOT_HASH);
        hashed_post_state.states.insert(hashed_address, Some(state_account));
        expected_accounts.push(hashed_address);
    }

    // Step 5: Commit the hashed post state
    let initial_root = EMPTY_ROOT_HASH;
    let (new_root, difflayer) = triedb
        .commit_hashed_post_state(initial_root, None, &hashed_post_state)
        .expect("Failed to commit hashed post state");

    // Step 6: Flush to disk
    triedb
        .flush(1, new_root, &difflayer)
        .expect("Failed to flush triedb");

    // Step 7: Get PathDB and create WorkloadExecutor
    let path_db = triedb.get_mut_path_db_ref().clone();
    let executor = WorkloadExecutor::default();

    // Step 8: Create channel for MultiProofMessage
    let (message_tx, message_rx) = mpsc::channel::<MultiProofMessage>();

    // Step 9: Create TrieDBPrefetchHandle
    let (handle, prefetch_result_rx) = TrieDBPrefetchHandle::new(
        new_root,
        path_db,
        None,
        executor.clone(),
        message_rx,
    )
    .expect("Failed to create TrieDBPrefetchHandle");
    
    // Step 10: Spawn the handle's run loop to process messages
    executor.spawn_blocking(move || {
        handle.run();
    });

    // Step 11: Send PrefetchProofs message for first 50 accounts
    const NUM_PREFETCH_PROOFS_ACCOUNTS: usize = 50;
    let mut prefetch_targets = MultiProofTargets::default();
    
    // Add first 50 accounts with 5 storage slots each
    for i in 0..NUM_PREFETCH_PROOFS_ACCOUNTS {
        let hashed_address = expected_accounts[i];
        let mut slots = B256Set::default();
        
        // Add 5 storage slots for each account
        let account_storage = hashed_post_state.storage_states.get(&hashed_address);
        if let Some(storage) = account_storage {
            let storage_vec: Vec<_> = storage.keys().take(NUM_PREFETCH_SLOTS).collect();
            for slot in storage_vec {
                slots.insert(*slot);
            }
        }
        
        prefetch_targets.insert(hashed_address, slots);
    }
    eprintln!("[TEST] PrefetchProofs targets prepared: {} accounts, each with {} slots", 
        prefetch_targets.len(), NUM_PREFETCH_SLOTS);
    message_tx
        .send(MultiProofMessage::PrefetchProofs(prefetch_targets))
        .expect("Failed to send PrefetchProofs message");

    // Step 12: Create EvmState for StateUpdate message for last 50 accounts
    let mut evm_state = EvmState::default();
    for i in NUM_PREFETCH_PROOFS_ACCOUNTS..NUM_ACCOUNTS {
        // Use the same Address as when creating accounts to ensure consistency
        let address = address_list[i];
        // This will match the hashed_address we stored in expected_accounts
        let hashed_address = keccak256(address.as_slice());
        
        // Create account info
        let account_info = AccountInfo {
            balance: U256::from(i as u64) * U256::from(1_000_000_000_000_000_000u64),
            nonce: i as u64,
            code_hash: keccak256(b"code_hash"),
            code: Some(Default::default()),
        };
        
        // Create storage map with 5 slots per account
        let mut storage = AlloyHashMap::default();
        
        // Add 5 storage slots for each account
        if let Some(account_storage) = hashed_post_state.storage_states.get(&hashed_address) {
            for (hashed_key, value_opt) in account_storage.iter().take(NUM_PREFETCH_SLOTS) {
                if let Some(value) = value_opt {
                    // Convert hashed_key (B256) to U256 slot
                    let slot_bytes: [u8; 32] = (*hashed_key).into();
                    let slot = U256::from_be_bytes(slot_bytes);
                    // Create EvmStorageSlot
                    let storage_slot = EvmStorageSlot::new_changed(U256::ZERO, *value, 0);
                    storage.insert(slot, storage_slot);
                }
            }
        }
        
        // Create Account with info and storage
        let account = Account {
            info: account_info,
            storage,
            status: revm_state::AccountStatus::Touched,
            transaction_id: 0,
        };
        
        evm_state.insert(address, account);
    }
    eprintln!("[TEST] EvmState created with {} accounts, each with {} storage slots", 
        evm_state.len(), NUM_PREFETCH_SLOTS);
    message_tx
        .send(MultiProofMessage::StateUpdate(
            StateChangeSource::Transaction(0),
            evm_state,
        ))
        .expect("Failed to send StateUpdate message");

    // Step 13: Send FinishedStateUpdates to complete prefetching
    message_tx
        .send(MultiProofMessage::FinishedStateUpdates)
        .expect("Failed to send FinishedStateUpdates message");

    // Step 14: Get prefetch result
    let result = match prefetch_result_rx.recv() {
        Ok(TrieDBPrefetchResult::PrefetchAccountResult(state)) => {
            Some(state)
        }
        Ok(other) => {
            eprintln!("[TEST] Unexpected result type: {:?}", std::any::type_name_of_val(&other));
            None
        }
        Err(e) => {
            eprintln!("[TEST] Failed to receive prefetch result: {:?}", e);
            None
        }
    };
    assert!(result.is_some(), "Prefetch result should not be None");

    let prefetch_state = result.unwrap();

    // Step 16: Verify results

    // Verify storage_roots count - should have storage roots for all 100 accounts
    eprintln!("[TEST] Verifying storage_roots count: expected {}, got {}", 
        NUM_ACCOUNTS, prefetch_state.storage_roots.len());
    assert_eq!(
        prefetch_state.storage_roots.len(),
        NUM_ACCOUNTS,
        "Expected {} storage roots, got {}",
        NUM_ACCOUNTS,
        prefetch_state.storage_roots.len()
    );

    // Verify storage_tries count - should have tries for all 100 accounts
    eprintln!("[TEST] Verifying storage_tries count: expected {}, got {}", 
        NUM_ACCOUNTS, prefetch_state.storage_tries.len());
    assert_eq!(
        prefetch_state.storage_tries.len(),
        NUM_ACCOUNTS,
        "Expected {} storage tries, got {}",
        NUM_ACCOUNTS,
        prefetch_state.storage_tries.len()
    );

    // Verify storage_roots contain all expected accounts
    eprintln!("[TEST] Verifying storage_roots contain all {} accounts", NUM_ACCOUNTS);
    for i in 0..NUM_ACCOUNTS {
        let hashed_address = expected_accounts[i];
        assert!(
            prefetch_state.storage_roots.contains_key(&hashed_address),
            "Storage root should exist for account {}",
            i
        );
    }

    // Verify storage_tries contain all expected accounts
    eprintln!("[TEST] Verifying storage_tries contain all {} accounts", NUM_ACCOUNTS);
    for i in 0..NUM_ACCOUNTS {
        let hashed_address = expected_accounts[i];
        assert!(
            prefetch_state.storage_tries.contains_key(&hashed_address),
            "Storage trie should exist for account {}",
            i
        );
    }
    
    eprintln!("[TEST] All verifications passed!");

    // All verifications passed!
}

