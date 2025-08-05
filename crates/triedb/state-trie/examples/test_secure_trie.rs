//! Simple test for SecureTrie implementation.

use alloy_primitives::{Address, B256};
use reth_triedb_memorydb::MemoryDB;
use reth_triedb_state_trie::{SecureTrieBuilder, SecureTrieId, StateAccount, SecureTrieTrait};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing SecureTrie implementation...");

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

    println!("✓ Created SecureTrie with ID: {:?}", trie.id());

    // Test key-value operations
    let key = b"test_key";
    let value = b"test_value";

    trie.update(key, value)?;
    println!("✓ Updated key-value pair");

    let retrieved = trie.get(key)?;
    println!("✓ Retrieved value: {:?}", retrieved);

    // Test account operations
    let address = Address::from([1u8; 20]);
    let account = StateAccount {
        nonce: alloy_primitives::U256::from(1),
        balance: alloy_primitives::U256::from(1000),
        storage_root: B256::ZERO,
        code_hash: B256::ZERO,
    };

    trie.update_account(address, &account)?;
    println!("✓ Updated account: {:?}", address);

    let retrieved_account = trie.get_account(address)?;
    println!("✓ Retrieved account: {:?}", retrieved_account);

    // Test storage operations
    let storage_key = b"storage_key";
    let storage_value = b"storage_value";

    trie.update_storage(address, storage_key, storage_value)?;
    println!("✓ Updated storage for key: {:?}", String::from_utf8_lossy(storage_key));

    let retrieved_storage = trie.get_storage(address, storage_key)?;
    println!("✓ Retrieved storage value: {:?}", retrieved_storage);

    // Commit the trie
    let (root, _) = trie.commit(true)?;
    println!("✓ Committed trie with root: {:?}", root);

    // Test root
    let current_root = trie.root();
    println!("✓ Current root: {:?}", current_root);

    println!("All tests passed! 🎉");
    Ok(())
}
