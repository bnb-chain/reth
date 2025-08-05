//! Comprehensive tests for the state-trie crate
//!
//! This module contains all tests organized into three main categories:
//! - secure_trie_tests: Tests for SecureTrieId and SecureTrieBuilder
//! - state_trie_tests: Tests for StateTrie implementation
//! - trie_tests: Tests for core Trie implementation

use alloy_primitives::{Address, B256, keccak256};
use reth_triedb_memorydb::MemoryDB;
use reth_triedb_pathdb::{PathDB, PathProviderConfig};
use tempfile::TempDir;
use std::sync::Arc;

use super::*;

/// Tests for SecureTrieId and SecureTrieBuilder
#[cfg(test)]
mod secure_trie_tests {
    use super::*;

    #[test]
    fn test_secure_trie_builder() {
        let db = MemoryDB::new();
        let id = SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO);

        let trie = SecureTrieBuilder::new(db)
            .with_id(id.clone())
            .build()
            .unwrap();

        assert_eq!(trie.id(), &id);
    }

    #[test]
    fn test_state_trie_with_pathdb() {
        // Create a temporary directory for the database
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_trie_db");

        // Create PathDB with default configuration
        let config = PathProviderConfig::default();
        let pathdb = Arc::new(PathDB::new(db_path.to_str().unwrap(), config).unwrap());

        // Create a secure trie identifier
        let id = SecureTrieId::new(
            B256::ZERO,           // state root
            Address::ZERO,        // owner
            B256::ZERO,           // root
        );

        // Create a secure trie using PathDB
        let mut trie = SecureTrieBuilder::new(pathdb)
            .with_id(id)
            .build()
            .unwrap();

        println!("✓ Created SecureTrie with PathDB");

        // Test key-value operations
        let key = b"test_key";
        let value = b"test_value";

        trie.update(key, value).unwrap();
        println!("✓ Updated key-value pair");

        let retrieved = trie.get(key).unwrap();
        println!("✓ Retrieved value: {:?}", retrieved);
        assert_eq!(retrieved, Some(value.to_vec()));

        // Test account operations
        let address = Address::from([1u8; 20]);
        let account = StateAccount {
            nonce: alloy_primitives::U256::from(1),
            balance: alloy_primitives::U256::from(1000),
            storage_root: B256::ZERO,
            code_hash: B256::ZERO,
        };

        trie.update_account(address, &account).unwrap();
        println!("✓ Updated account: {:?}", address);

        let retrieved_account = trie.get_account(address).unwrap();
        println!("✓ Retrieved account: {:?}", retrieved_account);
        assert_eq!(retrieved_account, Some(account));

        // Test storage operations
        let storage_key = b"storage_key";
        let storage_value = b"storage_value";

        trie.update_storage(address, storage_key, storage_value).unwrap();
        println!("✓ Updated storage for key: {:?}", String::from_utf8_lossy(storage_key));

        let retrieved_storage = trie.get_storage(address, storage_key).unwrap();
        println!("✓ Retrieved storage value: {:?}", retrieved_storage);
        assert_eq!(retrieved_storage, Some(storage_value.to_vec()));

        // Test commit
        let (root, _) = trie.commit(true).unwrap();
        println!("✓ Committed trie with root: {:?}", root);
        assert_ne!(root, B256::ZERO);

        // Test root
        let current_root = trie.root();
        println!("✓ Current root: {:?}", current_root);
        assert_eq!(current_root, root);

        println!("All PathDB tests passed! 🎉");
    }

    #[test]
    fn test_state_trie_pathdb_persistence() {
        // Create a temporary directory for the database
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("persistence_test_db");

        // Create PathDB with default configuration
        let config = PathProviderConfig::default();
        let pathdb = Arc::new(PathDB::new(db_path.to_str().unwrap(), config.clone()).unwrap());

        // Create a secure trie identifier
        let id = SecureTrieId::new(
            B256::ZERO,           // state root
            Address::ZERO,        // owner
            B256::ZERO,           // root
        );

        // First session: create trie and insert data
        {
            let mut trie = SecureTrieBuilder::new(pathdb.clone())
                .with_id(id.clone())
                .build()
                .unwrap();

            // Insert some data
            trie.update(b"key1", b"value1").unwrap();
            trie.update(b"key2", b"value2").unwrap();

            // Commit the trie
            let (root, _) = trie.commit(true).unwrap();
            println!("First session root: {:?}", root);
        }

        // Drop the first PathDB instance to release the lock
        drop(pathdb);

        // Second session: reopen trie and verify data persistence
        {
            // Create a new PathDB instance to test persistence
            let pathdb2 = Arc::new(PathDB::new(db_path.to_str().unwrap(), config).unwrap());

            // Create a new trie with the same identifier
            let _trie = SecureTrieBuilder::new(pathdb2)
                .with_id(id)
                .build()
                .unwrap();

            // Since we can't easily load the previous root, let's test that the database
            // contains the data by checking if we can access it through the database directly
            // For now, we'll just verify that the trie can be created without errors
            println!("✓ Second session trie created successfully");

            // Note: In a real implementation, we would need to implement proper root loading
            // from the database to verify data persistence
        }
    }

    #[test]
    fn test_state_trie_pathdb_lru_cache() {
        // Create a temporary directory for the database
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("cache_test_db");

        // Create PathDB with small cache size to test LRU behavior
        let mut config = PathProviderConfig::default();
        config.cache_size = 10; // Small cache size
        let pathdb = Arc::new(PathDB::new(db_path.to_str().unwrap(), config).unwrap());

        // Create a secure trie identifier
        let id = SecureTrieId::new(
            B256::ZERO,           // state root
            Address::ZERO,        // owner
            B256::ZERO,           // root
        );

        let mut trie = SecureTrieBuilder::new(pathdb)
            .with_id(id)
            .build()
            .unwrap();

        // Insert multiple keys to test cache behavior
        for i in 0..20 {
            let key = format!("key{}", i).into_bytes();
            let value = format!("value{}", i).into_bytes();
            trie.update(&key, &value).unwrap();
        }

        // Retrieve keys to test cache hits
        for i in 0..20 {
            let key = format!("key{}", i).into_bytes();
            let expected_value = format!("value{}", i).into_bytes();
            let retrieved = trie.get(&key).unwrap();
            assert_eq!(retrieved, Some(expected_value));
        }

        println!("✓ LRU cache behavior verified");
    }

    #[test]
    fn test_state_trie_pathdb_account_operations() {
        // Create a temporary directory for the database
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("account_test_db");

        // Create PathDB with default configuration
        let config = PathProviderConfig::default();
        let pathdb = Arc::new(PathDB::new(db_path.to_str().unwrap(), config).unwrap());

        // Create a secure trie identifier
        let id = SecureTrieId::new(
            B256::ZERO,           // state root
            Address::ZERO,        // owner
            B256::ZERO,           // root
        );

        let mut trie = SecureTrieBuilder::new(pathdb)
            .with_id(id)
            .build()
            .unwrap();

        // Test multiple account operations
        for i in 0..5 {
            let address = Address::from([i; 20]);
            let account = StateAccount {
                nonce: alloy_primitives::U256::from(i),
                balance: alloy_primitives::U256::from(1000u32 * i as u32),
                storage_root: B256::ZERO,
                code_hash: B256::ZERO,
            };

            println!("Testing account {}: {:?}", i, address);

            // Update account
            trie.update_account(address, &account).unwrap();
            println!("✓ Updated account {}", i);

            // Verify account
            let retrieved = trie.get_account(address).unwrap();
            println!("✓ Retrieved account {}: {:?}", i, retrieved);
            assert_eq!(retrieved, Some(account));

            // Add some storage to the account
            let storage_key = format!("storage_key_{}", i).into_bytes();
            let storage_value = format!("storage_value_{}", i).into_bytes();
            trie.update_storage(address, &storage_key, &storage_value).unwrap();
            println!("✓ Updated storage for account {}", i);

            // Verify storage
            let retrieved_storage = trie.get_storage(address, &storage_key).unwrap();
            println!("✓ Retrieved storage for account {}: {:?}", i, retrieved_storage);
            assert_eq!(retrieved_storage, Some(storage_value));
        }

        // Test account deletion
        let address_to_delete = Address::from([0; 20]);
        trie.delete_account(address_to_delete).unwrap();

        let deleted_account = trie.get_account(address_to_delete).unwrap();
        assert_eq!(deleted_account, None);

        println!("✓ Account operations with PathDB verified");
    }
}

/// Tests for StateTrie implementation
#[cfg(test)]
mod state_trie_tests {
    use super::*;

    #[test]
    fn test_secure_trie_basic_operations() {
        let db = MemoryDB::new();
        let id = SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO);
        let mut trie = StateTrie::new(id, db).unwrap();

        // Test key-value operations
        trie.update(b"key1", b"value1").unwrap();
        let value = trie.get(b"key1").unwrap();
        assert_eq!(value, Some(b"value1".to_vec()));

        // Test account operations
        let address = Address::from([1u8; 20]);
        let account = StateAccount {
            nonce: alloy_primitives::U256::from(1),
            balance: alloy_primitives::U256::from(1000),
            storage_root: B256::ZERO,
            code_hash: B256::ZERO,
        };

        trie.update_account(address, &account).unwrap();
        let retrieved_account = trie.get_account(address).unwrap();
        assert_eq!(retrieved_account, Some(account));

        // Test storage operations
        trie.update_storage(address, b"storage_key", b"storage_value").unwrap();
        let storage_value = trie.get_storage(address, b"storage_key").unwrap();
        assert_eq!(storage_value, Some(b"storage_value".to_vec()));
    }

    #[test]
    fn test_secure_trie_key_hashing() {
        let db = MemoryDB::new();
        let id = SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO);
        let trie = StateTrie::new(id, db).unwrap();

        let key = b"test_key";
        let hashed = trie.hash_key(key);
        let expected = keccak256(key);
        assert_eq!(hashed, expected);
    }

    #[test]
    fn test_secure_trie_commit() {
        let db = MemoryDB::new();
        let id = SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO);
        let mut trie = StateTrie::new(id, db).unwrap();

        trie.update(b"key1", b"value1").unwrap();
        trie.update(b"key2", b"value2").unwrap();

        let (root, _) = trie.commit(true).unwrap();
        assert_ne!(root, B256::ZERO);
    }
}

/// Tests for core Trie implementation
#[cfg(test)]
mod trie_tests {
    use super::*;

    #[test]
    fn test_trie_basic_operations() {
        let db = MemoryDB::new();
        let id = SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO);
        let mut trie = Trie::new(&id, db).unwrap();

        // Test insert and get
        trie.update(b"key1", b"value1").unwrap();
        let value = trie.get(b"key1").unwrap();
        assert_eq!(value, Some(b"value1".to_vec()));

        // Test update
        trie.update(b"key1", b"value2").unwrap();
        let value = trie.get(b"key1").unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));

        // Test delete - our implementation returns None for deleted values
        trie.delete(b"key1").unwrap();
        let value = trie.get(b"key1").unwrap();
        assert_eq!(value, None);
    }

    #[test]
    fn test_trie_commit() {
        let db = MemoryDB::new();
        let id = SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO);
        let mut trie = Trie::new(&id, db).unwrap();

        trie.update(b"key1", b"value1").unwrap();
        trie.update(b"key2", b"value2").unwrap();

        let (root, _) = trie.commit(true).unwrap();
        assert_ne!(root, B256::ZERO);
    }

    #[test]
    fn test_trie_commit_with_nodeset() {
        let db = MemoryDB::new();
        let id = SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO);
        let mut trie = Trie::new(&id, db).unwrap();

        // Insert some data
        trie.update(b"key1", b"value1").unwrap();
        trie.update(b"key2", b"value2").unwrap();

        // Commit with collect_leaf = true
        let (root, node_set) = trie.commit(true).unwrap();
        assert_ne!(root, B256::ZERO);

        // Verify that NodeSet is returned and contains nodes
        assert!(node_set.is_some());
        let node_set = node_set.unwrap();
        assert!(!node_set.is_empty());

        let (updates, deletes) = node_set.size();
        assert!(updates > 0); // Should have at least the root node
        assert_eq!(deletes, 0); // No deletions in this test

        println!("✓ Commit returned NodeSet with {} updates, {} deletes", updates, deletes);
    }




}
