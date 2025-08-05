//! Basic usage example for SecureTrie.
//!
//! This example demonstrates how to create and use a SecureTrie
//! with an in-memory database.

use alloy_primitives::{Address, B256};
use reth_triedb_memorydb::MemoryDB;
use reth_triedb_state_trie::{SecureTrieBuilder, SecureTrieId, StateAccount, SecureTrieTrait};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create an in-memory database
    let db = MemoryDB::new();

    // Create a secure trie identifier
    let id = SecureTrieId::new(
        B256::ZERO,           // state root
        Address::ZERO,        // owner
        B256::ZERO,           // root
    );

    // Create a secure trie using the builder pattern
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()?;

    println!("Created SecureTrie with ID: {:?}", trie.id());

    // Create a test account
    let address = Address::from([1u8; 20]);
    let account = StateAccount {
        nonce: alloy_primitives::U256::from(1),
        balance: alloy_primitives::U256::from(1000),
        storage_root: B256::ZERO,
        code_hash: B256::ZERO,
    };

    // Update the account in the trie
    trie.update_account(address, &account)?;
    println!("Updated account: {:?}", address);

    // Get the account from the trie
    let retrieved_account = trie.get_account(address)?;
    println!("Retrieved account: {:?}", retrieved_account);

    // Update a storage value
    let storage_key = b"storage_key";
    let storage_value = b"storage_value";
    trie.update_storage(address, storage_key, storage_value)?;
    println!("Updated storage for key: {:?}", String::from_utf8_lossy(storage_key));

    // Get the storage value
    let retrieved_value = trie.get_storage(address, storage_key)?;
    println!("Retrieved storage value: {:?}", retrieved_value);

    // Commit the trie
    let (root, _) = trie.commit(true)?;
    println!("Committed trie with root: {:?}", root);

    // Get the current root
    let current_root = trie.root();
    println!("Current root: {:?}", current_root);

    Ok(())
}
