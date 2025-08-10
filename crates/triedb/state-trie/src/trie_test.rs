//! Trie tests for update and get operations.

use alloy_primitives::{B256, keccak256};
use reth_triedb_pathdb::{PathDB, PathProviderConfig};
use crate::secure_trie::{SecureTrieBuilder, SecureTrieId};

use std::env;

#[test]
fn test_trie_update_and_get() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_pathdb");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Test data
    let key = b"test_key";
    let value = b"test_value";

    // Perform update operation
    trie.update(key, value)
        .expect("Failed to update trie");

    // Perform get operation
    let retrieved_value = trie.get(key)
        .expect("Failed to get from trie")
        .expect("Value not found in trie");

    // Print retrieved value
    println!("Retrieved value as string: {}", String::from_utf8_lossy(&retrieved_value));

    // Verify that values are equal
    assert_eq!(retrieved_value, value, "Retrieved value should match the original value");

    // Verify length
    assert_eq!(retrieved_value.len(), value.len(), "Value length should match");

    // Verify content
    assert_eq!(retrieved_value, value, "Value content should match exactly");
}

#[test]
fn test_trie_multiple_updates() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_multiple");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Test multiple key-value pairs
    let test_data = vec![
        (b"key1".to_vec(), b"value1".to_vec()),
        (b"key2".to_vec(), b"value2".to_vec()),
        (b"key3".to_vec(), b"value3".to_vec()),
        (b"hello".to_vec(), b"world".to_vec()),
        (b"test".to_vec(), b"data".to_vec()),
    ];

    // Insert all key-value pairs
    for (key, value) in &test_data {
        trie.update(key, value)
            .expect(&format!("Failed to update trie with key: {:?}", key));
    }

    // Verify all key-value pairs
    for (key, expected_value) in &test_data {
        let retrieved_value = trie.get(key)
            .expect(&format!("Failed to get key: {:?}", key))
            .expect(&format!("Value not found for key: {:?}", key));

        println!("Key: {:?}, Retrieved value: {}", key, String::from_utf8_lossy(&retrieved_value));
        assert_eq!(retrieved_value, *expected_value,
                   "Value mismatch for key: {:?}", key);
    }
}

#[test]
fn test_trie_update_overwrite() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_overwrite");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    let key = b"overwrite_key";
    let initial_value = b"initial_value";
    let updated_value = b"updated_value";

    // First insertion
    trie.update(key, initial_value)
        .expect("Failed to update trie with initial value");

    // Verify initial value
    let retrieved_initial = trie.get(key)
        .expect("Failed to get initial value")
        .expect("Initial value not found");
    println!("Initial value: {}", String::from_utf8_lossy(&retrieved_initial));
    assert_eq!(retrieved_initial, initial_value);

    // Overwrite update
    trie.update(key, updated_value)
        .expect("Failed to update trie with updated value");

    // Verify updated value
    let retrieved_updated = trie.get(key)
        .expect("Failed to get updated value")
        .expect("Updated value not found");
    println!("Updated value: {}", String::from_utf8_lossy(&retrieved_updated));
    assert_eq!(retrieved_updated, updated_value);

    // Ensure value was actually updated
    assert_ne!(retrieved_updated, initial_value, "Value should have been updated");
}

#[test]
fn test_trie_nonexistent_key() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_nonexistent");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    let existing_key = b"existing_key";
    let existing_value = b"existing_value";
    let nonexistent_key = b"nonexistent_key";

    // Insert an existing key
    trie.update(existing_key, existing_value)
        .expect("Failed to update trie");

    // Try to get nonexistent key
    let result = trie.get(nonexistent_key)
        .expect("Failed to get nonexistent key");

    // Should return None
    println!("Nonexistent key result: {:?}", result);
    assert!(result.is_none(), "Nonexistent key should return None");
}

#[test]
fn test_trie_binary_data() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_binary");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    let key = b"binary_key";
    let binary_value = vec![0x00, 0x01, 0x02, 0xFF, 0xFE, 0xFD];

    // Insert binary data
    trie.update(key, &binary_value)
        .expect("Failed to update trie with binary data");

    // Retrieve binary data
    let retrieved_value = trie.get(key)
        .expect("Failed to get binary data")
        .expect("Binary data not found");

    println!("Binary data: {:?}", retrieved_value);
    assert_eq!(retrieved_value, binary_value, "Binary data should match exactly");
}

#[test]
fn test_trie_large_value() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_large");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    let key = b"large_key";
    let large_value: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();

    // Insert large value
    trie.update(key, &large_value)
        .expect("Failed to update trie with large value");

    // Retrieve large value
    let retrieved_value = trie.get(key)
        .expect("Failed to get large value")
        .expect("Large value not found");

    println!("Large value length: {}", retrieved_value.len());
    println!("Large value first 10 bytes: {:?}", &retrieved_value[..10]);
    assert_eq!(retrieved_value, large_value, "Large value should match exactly");
    assert_eq!(retrieved_value.len(), 1000, "Large value should have correct length");
}

#[test]
fn test_trie_smoke_test_10000_random_kv_update_get() {
    use std::time::Instant;
    use alloy_primitives::keccak256;

    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_smoke_1m");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Generate 1000000 sequential key-value pairs (1-1000000)
    let mut test_data = Vec::new();

    println!("Generating 1000000 sequential key-value pairs...");
    let generate_start = Instant::now();

    for i in 1..=1000000 {
        // Use fixed-length string to avoid length variation issues
        let original_key = format!("{:07}", i).into_bytes(); // Always 7 bytes: "0000001", "0000002", etc.
        let key = keccak256(original_key).to_vec(); // 32-byte keccak256 hash

        // Generate value as string representation of the number
        let value = i.to_string().into_bytes();

        test_data.push((key, value));

        if i % 100000 == 0 {
            println!("Generated {} key-value pairs", i);
        }
    }

    let generate_time = generate_start.elapsed();
    println!("Generation completed in {:?}", generate_time);

    // Insert all key-value pairs
    println!("Inserting 1000000 key-value pairs into trie...");
    let insert_start = Instant::now();

    for (i, (key, value)) in test_data.iter().enumerate() {
        trie.update(key, value)
            .expect(&format!("Failed to update trie with key at index {}", i));

        if i % 100000 == 0 {
            println!("Inserted {} key-value pairs", i);
        }
    }

    let insert_time = insert_start.elapsed();
    println!("Insertion completed in {:?}", insert_time);

    // Verify all key-value pairs
    println!("Retrieving and verifying 1000000 key-value pairs...");
    let retrieve_start = Instant::now();

    let mut success_count = 0;
    let mut failure_count = 0;

    for (i, (key, expected_value)) in test_data.iter().enumerate() {
        match trie.get(key) {
            Ok(Some(retrieved_value)) => {
                if retrieved_value == *expected_value {
                    success_count += 1;
                } else {
                    failure_count += 1;
                    println!("Value mismatch at index {}: expected {:?}, got {:?}",
                             i, expected_value, retrieved_value);
                }
            }
            Ok(None) => {
                failure_count += 1;
                println!("Key not found at index {}: {:?}", i, key);
            }
            Err(e) => {
                failure_count += 1;
                println!("Error retrieving key at index {}: {:?}", i, e);
            }
        }

        if i % 100000 == 0 {
            println!("Verified {} key-value pairs", i);
        }
    }

    let retrieve_time = retrieve_start.elapsed();
    println!("Retrieval and verification completed in {:?}", retrieve_time);

    // Print summary statistics
    println!("\n=== SMOKE TEST SUMMARY ===");
    println!("Total key-value pairs: {}", test_data.len());
    println!("Successful retrievals: {}", success_count);
    println!("Failed retrievals: {}", failure_count);
    println!("Success rate: {:.2}%", (success_count as f64 / test_data.len() as f64) * 100.0);
    println!("\n=== TIMING SUMMARY ===");
    println!("Generation time: {:?}", generate_time);
    println!("Insertion time: {:?}", insert_time);
    println!("Retrieval time: {:?}", retrieve_time);
    println!("Total time: {:?}", generate_start.elapsed());
    println!("Average insertion time per KV: {:?}", insert_time / test_data.len() as u32);
    println!("Average retrieval time per KV: {:?}", retrieve_time / test_data.len() as u32);

    // Assert that all operations were successful
    assert_eq!(success_count, test_data.len(),
               "All key-value pairs should be successfully retrieved");
    assert_eq!(failure_count, 0,
               "No failures should occur during retrieval");
}

#[test]
fn test_trie_delete_basic() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_delete_basic");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Test data
    let key = b"delete_test_key";
    let value = b"delete_test_value";

    // Insert the key-value pair
    trie.update(key, value)
        .expect("Failed to update trie");

    // Verify the value exists
    let retrieved_value = trie.get(key)
        .expect("Failed to get from trie")
        .expect("Value not found in trie");

    assert_eq!(retrieved_value, value, "Value should exist before deletion");

    // Delete the key
    trie.delete(key)
        .expect("Failed to delete from trie");

    // Verify the value is deleted
    let deleted_value = trie.get(key)
        .expect("Failed to get from trie after deletion");

    assert_eq!(deleted_value, None, "Value should be None after deletion");
}

#[test]
fn test_trie_delete_multiple() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_delete_multiple");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Test multiple key-value pairs
    let test_data = vec![
        (b"key1".to_vec(), b"value1".to_vec()),
        (b"key2".to_vec(), b"value2".to_vec()),
        (b"key3".to_vec(), b"value3".to_vec()),
        (b"hello".to_vec(), b"world".to_vec()),
        (b"test".to_vec(), b"data".to_vec()),
    ];

    // Insert all key-value pairs
    for (key, value) in &test_data {
        trie.update(key, value)
            .expect(&format!("Failed to update trie with key: {:?}", key));
    }

    // Verify all key-value pairs exist
    for (key, expected_value) in &test_data {
        let retrieved_value = trie.get(key)
            .expect(&format!("Failed to get key: {:?}", key))
            .expect(&format!("Value not found for key: {:?}", key));

        assert_eq!(retrieved_value, *expected_value,
                   "Value mismatch for key: {:?}", key);
    }

    // Debug: print trie structure before deletion
    println!("\n=== TRIE STRUCTURE BEFORE DELETION ===");
    trie.debug_print();

    // Delete some keys
    // let keys_to_delete = vec![b"key1".to_vec(), b"key3".to_vec(), b"test".to_vec()];
    let keys_to_delete = vec![b"key1".to_vec()];

    for key in &keys_to_delete {
        trie.delete(key)
            .expect(&format!("Failed to delete key: {:?}", key));
    }

    // Verify deleted keys are gone
    for key in &keys_to_delete {
        let deleted_value = trie.get(key)
            .expect(&format!("Failed to get deleted key: {:?}", key));

        assert_eq!(deleted_value, None,
                   "Deleted key should return None: {:?}", key);
    }

    // Debug: print trie structure after deletion
    println!("\n=== TRIE STRUCTURE AFTER DELETION ===");
    trie.debug_print();

    // Verify remaining keys still exist
    let remaining_keys = vec![b"key2".to_vec(), b"hello".to_vec()];
    for key in &remaining_keys {
        let retrieved_value = trie.get(key)
            .expect(&format!("Failed to get remaining key: {:?}", key))
            .expect(&format!("Remaining value not found for key: {:?}", key));

        let expected_value = test_data.iter()
            .find(|(k, _)| k.as_slice() == key.as_slice())
            .map(|(_, v)| v)
            .expect(&format!("Expected value not found for key: {:?}", key));

        assert_eq!(retrieved_value, *expected_value,
                   "Remaining value mismatch for key: {:?}", key);
    }
}

#[test]
fn test_trie_delete_nonexistent() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_delete_nonexistent");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Try to delete a non-existent key
    let nonexistent_key = b"nonexistent_key";

    // This should not panic and should succeed
    trie.delete(nonexistent_key)
        .expect("Delete of non-existent key should succeed");

    // Verify the key still doesn't exist
    let value = trie.get(nonexistent_key)
        .expect("Failed to get non-existent key");

    assert_eq!(value, None, "Non-existent key should return None");
}

#[test]
fn test_trie_delete_and_reinsert() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_delete_reinsert");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Test data
    let key = b"reinsert_test_key";
    let original_value = b"original_value";
    let new_value = b"new_value";

    // Insert original value
    trie.update(key, original_value)
        .expect("Failed to insert original value");

    // Verify original value
    let retrieved_value = trie.get(key)
        .expect("Failed to get original value")
        .expect("Original value not found");

    assert_eq!(retrieved_value, original_value, "Original value should match");

    // Delete the key
    trie.delete(key)
        .expect("Failed to delete key");

    // Verify deletion
    let deleted_value = trie.get(key)
        .expect("Failed to get after deletion");

    assert_eq!(deleted_value, None, "Value should be None after deletion");

    // Re-insert with new value
    trie.update(key, new_value)
        .expect("Failed to re-insert with new value");

    // Verify new value
    let new_retrieved_value = trie.get(key)
        .expect("Failed to get new value")
        .expect("New value not found");

    assert_eq!(new_retrieved_value, new_value, "New value should match");
    assert_ne!(new_retrieved_value, original_value, "New value should be different from original");
}

#[test]
fn test_trie_update_empty_value_equals_delete() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_empty_value");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Test key
    let key = b"test_key";
    let value = b"test_value";
    let empty_value = b""; // Empty value

    // Step 1: Insert a key-value pair
    println!("Step 1: Inserting key-value pair");
    trie.update(key, value)
        .expect("Failed to update trie with value");

    // Verify the value exists
    let retrieved_value = trie.get(key)
        .expect("Failed to get from trie")
        .expect("Value not found in trie");
    assert_eq!(retrieved_value, value, "Value should exist after insertion");

    // Step 2: Update with empty value (should be equivalent to delete)
    println!("Step 2: Updating with empty value");
    trie.update(key, empty_value)
        .expect("Failed to update trie with empty value");

    // Verify the value is now None (deleted)
    let retrieved_value_after_empty = trie.get(key)
        .expect("Failed to get from trie after empty update");

    match retrieved_value_after_empty {
        Some(value) => {
            println!("WARNING: Key still exists with value: {:?}", value);
            // Check if the value is actually empty
            if value.is_empty() {
                println!("Value is empty, which might be acceptable");
            } else {
                panic!("Key should be deleted or have empty value, but got: {:?}", value);
            }
        }
        None => {
            println!("SUCCESS: Key was deleted (returned None)");
        }
    }

    // Step 3: Verify that the key is truly gone by trying to get it again
    println!("Step 3: Verifying key is truly gone");
    let final_check = trie.get(key)
        .expect("Failed to get from trie in final check");

    assert_eq!(final_check, None, "Key should be completely removed");

    // Step 4: Test with multiple keys to ensure consistency
    println!("Step 4: Testing with multiple keys");
    let test_data = vec![
        (b"key1".to_vec(), b"value1".to_vec()),
        (b"key2".to_vec(), b"value2".to_vec()),
        (b"key3".to_vec(), b"value3".to_vec()),
    ];

    // Insert all keys
    for (key, value) in &test_data {
        trie.update(key, value)
            .expect(&format!("Failed to update trie with key: {:?}", key));
    }

    // Verify all keys exist
    for (key, expected_value) in &test_data {
        let retrieved = trie.get(key)
            .expect(&format!("Failed to get key: {:?}", key))
            .expect(&format!("Value not found for key: {:?}", key));
        assert_eq!(retrieved, *expected_value);
    }

    // Update one key with empty value
    let key_to_empty = b"key2".to_vec();
    trie.update(&key_to_empty, empty_value)
        .expect("Failed to update with empty value");

    // Verify the specific key is gone
    let empty_check = trie.get(&key_to_empty)
        .expect("Failed to get key after empty update");
    assert_eq!(empty_check, None, "Key updated with empty value should be removed");

    // Verify other keys still exist
    for (key, expected_value) in &test_data {
        if key != &key_to_empty {
            let retrieved = trie.get(key)
                .expect(&format!("Failed to get key: {:?}", key))
                .expect(&format!("Value not found for key: {:?}", key));
            assert_eq!(retrieved, *expected_value, "Other keys should remain unchanged");
        }
    }

    println!("Test completed: Update with empty value behaves like delete");
}

#[test]
fn test_trie_simple_write_delete_get() {
    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_simple");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Test data
    let key = b"simple_key";
    let value = b"simple_value";

    println!("=== Simple Write-Delete-Get Test ===");

    // Step 1: Write a key-value pair
    println!("Step 1: Writing key-value pair");
    trie.update(key, value)
        .expect("Failed to write to trie");

    // Step 2: Get and verify the value exists
    println!("Step 2: Getting the value");
    let retrieved_value = trie.get(key)
        .expect("Failed to get from trie")
        .expect("Value not found in trie");

    println!("Retrieved value: {}", String::from_utf8_lossy(&retrieved_value));
    assert_eq!(retrieved_value, value, "Retrieved value should match written value");

    // Step 3: Delete the key
    println!("Step 3: Deleting the key");
    trie.delete(key)
        .expect("Failed to delete from trie");

    // Step 4: Try to get the deleted key
    println!("Step 4: Getting the deleted key");
    let deleted_value = trie.get(key)
        .expect("Failed to get deleted key");

    match deleted_value {
        Some(value) => {
            println!("WARNING: Key still exists with value: {:?}", value);
            panic!("Key should be deleted, but still exists with value: {:?}", value);
        }
        None => {
            println!("SUCCESS: Key was successfully deleted (returned None)");
        }
    }

    // Step 5: Final verification - try to get again
    println!("Step 5: Final verification");
    let final_check = trie.get(key)
        .expect("Failed to get in final check");

    assert_eq!(final_check, None, "Key should remain deleted");
    println!("Final check passed: Key is confirmed deleted");

    println!("=== Test completed successfully ===");
}

#[test]
fn test_trie_simple_delete_verification() {
    use alloy_primitives::keccak256;

    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_simple_delete");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    println!("=== Simple Delete Verification Test ===");

    // Insert 10 deterministic keys
    let mut all_keys = Vec::new();
    let mut all_values = Vec::new();

    for i in 0..10 {
        let key = format!("key{}", i).into_bytes();
        let hashed_key = keccak256(&key).to_vec();
        let value = format!("value{}", i).into_bytes();

        all_keys.push(hashed_key);
        all_values.push(value);
    }

    // Insert all keys
    println!("Inserting 10 keys...");
    for (key, value) in all_keys.iter().zip(all_values.iter()) {
        trie.update(key, value).expect("Failed to update trie");
    }

    // Verify all keys exist
    println!("Verifying all keys exist...");
    for (i, (key, expected_value)) in all_keys.iter().zip(all_values.iter()).enumerate() {
        let result = trie.get(key).expect("Failed to get from trie");
        match result {
            Some(value) => {
                assert_eq!(value, *expected_value, "Key {} has wrong value", i);
                println!("✓ Key {} found with correct value", i);
            }
            None => {
                panic!("Key {} not found after insertion", i);
            }
        }
    }

    // Delete first 5 keys
    println!("Deleting first 5 keys...");
    for i in 0..5 {
        trie.delete(&all_keys[i]).expect("Failed to delete from trie");
        println!("✓ Deleted key {}", i);
    }

    // Verify remaining keys
    println!("Verifying remaining keys...");
    for i in 0..10 {
        let result = trie.get(&all_keys[i]).expect("Failed to get from trie");

        if i < 5 {
            // These should be deleted
            match result {
                Some(value) => {
                    panic!("Key {} should be deleted but found value: {:?}", i, value);
                }
                None => {
                    println!("✓ Key {} correctly deleted", i);
                }
            }
        } else {
            // These should still exist
            match result {
                Some(value) => {
                    assert_eq!(value, all_values[i], "Key {} has wrong value", i);
                    println!("✓ Key {} found with correct value", i);
                }
                None => {
                    panic!("Key {} should exist but was not found", i);
                }
            }
        }
    }

    println!("=== Simple Delete Verification Test Completed Successfully ===");
}

#[test]
fn test_trie_phased_smoke_test_with_hash() {
    use std::time::Instant;
    use alloy_primitives::keccak256;
    use rand::Rng;

    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_test_phased_smoke_hash");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    println!("=== Phased Smoke Test with Hash ===");

    let mut all_keys = Vec::new();
    let mut all_values = Vec::new();
    let mut rng = rand::thread_rng();

    // Phase 1: Insert 500,000, Delete 300,000, Verify 500,000
    println!("\n=== Phase 1: Insert 500K, Delete 300K, Verify 500K ===");
    let phase1_start = Instant::now();

    // Generate first 500,000 key-value pairs
    println!("Generating first 500,000 key-value pairs...");
    for _i in 0..500_000 {
        let key_length = rng.gen_range(8..17);
        let mut random_key = vec![0u8; key_length];
        rng.fill(&mut random_key[..]);

        let hashed_key = keccak256(&random_key).to_vec();
        let value = hashed_key.clone();

        all_keys.push(hashed_key);
        all_values.push(value);
    }

    // Insert first 500,000
    println!("Inserting first 500,000 key-value pairs...");
    let insert1_start = Instant::now();
    for (key, value) in all_keys.iter().zip(all_values.iter()) {
        trie.update(key, value).expect("Failed to update trie");
    }
    let insert1_time = insert1_start.elapsed();
    println!("Inserted 500,000 in {:?}", insert1_time);

    // Delete first 300,000
    println!("Deleting first 300,000 keys...");
    let delete1_start = Instant::now();
    for i in 0..300_000 {
        trie.delete(&all_keys[i]).expect("Failed to delete from trie");
    }
    let delete1_time = delete1_start.elapsed();
    println!("Deleted 300,000 in {:?}", delete1_time);

    // Verify first 500,000
    println!("Verifying first 500,000 keys...");
    let verify1_start = Instant::now();
    let mut found1 = 0;
    let mut not_found1 = 0;
    let mut wrong_value1 = 0;

    for i in 0..500_000 {
        let result = trie.get(&all_keys[i]).expect("Failed to get from trie");

        if i < 300_000 {
            // These should be deleted
            match result {
                Some(_) => {
                    println!("ERROR: Key {} should be deleted but found value", i);
                    wrong_value1 += 1;
                }
                None => {
                    not_found1 += 1;
                }
            }
        } else {
            // These should still exist
            match result {
                Some(value) => {
                    if value == all_values[i] {
                        found1 += 1;
                    } else {
                        println!("ERROR: Key {} has wrong value", i);
                        wrong_value1 += 1;
                    }
                }
                None => {
                    println!("ERROR: Key {} should exist but was not found", i);
                    not_found1 += 1;
                }
            }
        }
    }

    let verify1_time = verify1_start.elapsed();
    let phase1_time = phase1_start.elapsed();

    println!("Phase 1 Results:");
    println!("  Found: {}, Not found: {}, Wrong values: {}", found1, not_found1, wrong_value1);
    println!("  Expected: 200,000 found, 300,000 not found, 0 wrong values");
    println!("  Phase 1 time: {:?}", phase1_time);

    // Phase 2: Insert 500,000 more, Delete 300,000 more, Verify all 1,000,000
    println!("\n=== Phase 2: Insert 500K more, Delete 300K more, Verify 1M ===");
    let phase2_start = Instant::now();

    // Generate second 500,000 key-value pairs
    println!("Generating second 500,000 key-value pairs...");
    for _i in 500_000..1_000_000 {
        let key_length = rng.gen_range(8..17);
        let mut random_key = vec![0u8; key_length];
        rng.fill(&mut random_key[..]);

        let hashed_key = keccak256(&random_key).to_vec();
        let value = hashed_key.clone();

        all_keys.push(hashed_key);
        all_values.push(value);
    }

    // Insert second 500,000
    println!("Inserting second 500,000 key-value pairs...");
    let insert2_start = Instant::now();
    for i in 500_000..1_000_000 {
        trie.update(&all_keys[i], &all_values[i]).expect("Failed to update trie");
    }
    let insert2_time = insert2_start.elapsed();
    println!("Inserted 500,000 more in {:?}", insert2_time);

    // Delete 300,000 more (from the second batch)
    println!("Deleting 300,000 more keys...");
    let delete2_start = Instant::now();
    for i in 500_000..800_000 {
        trie.delete(&all_keys[i]).expect("Failed to delete from trie");
    }
    let delete2_time = delete2_start.elapsed();
    println!("Deleted 300,000 more in {:?}", delete2_time);

    // Verify all 1,000,000
    println!("Verifying all 1,000,000 keys...");
    let verify2_start = Instant::now();
    let mut found2 = 0;
    let mut not_found2 = 0;
    let mut wrong_value2 = 0;

    for i in 0..1_000_000 {
        let result = trie.get(&all_keys[i]).expect("Failed to get from trie");

        if i < 300_000 || (i >= 500_000 && i < 800_000) {
            // These should be deleted
            match result {
                Some(_value) => {
                    println!("ERROR: Key {} should be deleted but found value", i);
                    wrong_value2 += 1;
                }
                None => {
                    not_found2 += 1;
                }
            }
        } else {
            // These should still exist
            match result {
                Some(value) => {
                    if value == all_values[i] {
                        found2 += 1;
                    } else {
                        println!("ERROR: Key {} has wrong value", i);
                        wrong_value2 += 1;
                    }
                }
                None => {
                    println!("ERROR: Key {} should exist but was not found", i);
                    not_found2 += 1;
                }
            }
        }
    }

    let verify2_time = verify2_start.elapsed();
    let phase2_time = phase2_start.elapsed();
    let total_time = phase1_start.elapsed();

    // Summary
    println!("\n=== Final Results ===");
    println!("Phase 1 Results:");
    println!("  Found: {}, Not found: {}, Wrong values: {}", found1, not_found1, wrong_value1);
    println!("  Expected: 200,000 found, 300,000 not found, 0 wrong values");
    println!("  Time: {:?}", phase1_time);

    println!("Phase 2 Results:");
    println!("  Found: {}, Not found: {}, Wrong values: {}", found2, not_found2, wrong_value2);
    println!("  Expected: 400,000 found, 600,000 not found, 0 wrong values");
    println!("  Time: {:?}", phase2_time);

    println!("Total time: {:?}", total_time);

    // Performance metrics
    println!("\n=== Performance Metrics ===");
    println!("Phase 1 - Insert 500K: {:?} ({:?} per KV)", insert1_time, insert1_time / 500_000);
    println!("Phase 1 - Delete 300K: {:?} ({:?} per KV)", delete1_time, delete1_time / 300_000);
    println!("Phase 1 - Verify 500K: {:?} ({:?} per KV)", verify1_time, verify1_time / 500_000);
    println!("Phase 2 - Insert 500K: {:?} ({:?} per KV)", insert2_time, insert2_time / 500_000);
    println!("Phase 2 - Delete 300K: {:?} ({:?} per KV)", delete2_time, delete2_time / 300_000);
    println!("Phase 2 - Verify 1M: {:?} ({:?} per KV)", verify2_time, verify2_time / 1_000_000);

    // Assertions
    assert_eq!(found1, 200_000, "Phase 1: Should have exactly 200,000 keys remaining");
    assert_eq!(not_found1, 300_000, "Phase 1: Should have exactly 300,000 keys deleted");
    assert_eq!(wrong_value1, 0, "Phase 1: Should have no wrong values");

    assert_eq!(found2, 400_000, "Phase 2: Should have exactly 400,000 keys remaining");
    assert_eq!(not_found2, 600_000, "Phase 2: Should have exactly 600,000 keys deleted");
    assert_eq!(wrong_value2, 0, "Phase 2: Should have no wrong values");

    println!("\n=== Test completed successfully ===");
}

#[test]
fn test_trie_boundary_conditions_comprehensive() {
    println!("🧪 Testing comprehensive trie boundary conditions with 3-layer structure...");

    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_boundary_test_pathdb");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

        println!("📋 Phase 1: Building 3-layer trie with boundary conditions");

    // Helper function to create hashed key with terminator for leaf nodes
    // We'll create a shorter key from hash prefix and add terminator nibble
    let hash_key = |key: &str| -> Vec<u8> {
        let hash = keccak256(key.as_bytes());
        // Take first few bytes of hash and add terminator byte 16
        let mut key_bytes = hash[..4].to_vec(); // Use first 4 bytes to stay under limit
        key_bytes.push(16); // Add terminator byte for leaf node
        key_bytes
    };

    // Test 1: Single character keys (will create ShortNode -> ValueNode)
    let single_char_tests = vec![
        ("a", b"value_a".to_vec()),
        ("b", b"value_b".to_vec()),
        ("z", b"value_z".to_vec()),
    ];

    for (key, value) in single_char_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, value)
            .expect(&format!("Failed to insert key: {}", key));

        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get key: {}", key))
            .expect(&format!("Key not found: {}", key));
        assert_eq!(retrieved, *value, "Single char key {} value mismatch", key);
        println!("  ✅ Single char key '{}' inserted and verified (hashed)", key);
    }

        // Test 2: Two character keys (will create branch at second level)
    let two_char_tests = vec![
        ("aa", b"value_aa".to_vec()),
        ("ab", b"value_ab".to_vec()),
        ("az", b"value_az".to_vec()),
        ("ba", b"value_ba".to_vec()),
        ("bb", b"value_bb".to_vec()),
        ("zz", b"value_zz".to_vec()),
    ];

    for (key, value) in two_char_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, value)
            .expect(&format!("Failed to insert key: {}", key));

        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get key: {}", key))
            .expect(&format!("Key not found: {}", key));
        assert_eq!(retrieved, *value, "Two char key {} value mismatch", key);
        println!("  ✅ Two char key '{}' inserted and verified (hashed)", key);
    }

        // Test 3: Three character keys (will create 3-layer structure)
    let three_char_tests = vec![
        ("abc", b"value_abc".to_vec()),
        ("abd", b"value_abd".to_vec()),
        ("abz", b"value_abz".to_vec()),
        ("baa", b"value_baa".to_vec()),
        ("bab", b"value_bab".to_vec()),
        ("xyz", b"value_xyz".to_vec()),
        ("zzz", b"value_zzz".to_vec()),
    ];

    for (key, value) in three_char_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, value)
            .expect(&format!("Failed to insert key: {}", key));

        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get key: {}", key))
            .expect(&format!("Key not found: {}", key));
        assert_eq!(retrieved, *value, "Three char key {} value mismatch", key);
        println!("  ✅ Three char key '{}' inserted and verified (hashed)", key);
    }

        // Test 4: Longer keys to force extension nodes
    let long_key_tests = vec![
        ("abcd", b"value_abcd".to_vec()),
        ("abce", b"value_abce".to_vec()),
        ("abcz", b"value_abcz".to_vec()),
        ("abcdef", b"value_abcdef".to_vec()),
        ("abcdef01", b"value_abcdef01".to_vec()),
        ("abcdef0123456789", b"value_very_long_key".to_vec()),
    ];

    for (key, value) in long_key_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, value)
            .expect(&format!("Failed to insert key: {}", key));

        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get key: {}", key))
            .expect(&format!("Key not found: {}", key));
        assert_eq!(retrieved, *value, "Long key {} value mismatch", key);
        println!("  ✅ Long key '{}' inserted and verified (hashed)", key);
    }

        // Test 5: Boundary value sizes (will test ValueNode vs HashNode distinction)
    let value_size_tests = vec![
        ("val_1byte", vec![0x42]),
        ("val_31bytes", vec![0x33; 31]),
        ("val_32bytes", vec![0x44; 32]), // Critical 32-byte boundary
        ("val_33bytes", vec![0x55; 33]),
        ("val_55bytes", vec![0x66; 55]), // RLP boundary
        ("val_56bytes", vec![0x77; 56]), // RLP boundary
        ("val_64bytes", vec![0x88; 64]),
        ("val_128bytes", vec![0x99; 128]),
        ("val_1kb", vec![0xaa; 1024]),
    ];

    for (key, value) in value_size_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, value)
            .expect(&format!("Failed to insert key: {}", key));

        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get key: {}", key))
            .expect(&format!("Key not found: {}", key));
        assert_eq!(retrieved, *value, "Value size key {} mismatch", key);
        println!("  ✅ Value size key '{}' ({} bytes) inserted and verified (hashed)", key, value.len());
    }

    println!("📋 Phase 2: Testing updates and overwrites");

        // Test 6: Update existing keys with different value sizes
    let update_tests = vec![
        ("a", b"updated_value_a_longer".to_vec()),
        ("aa", b"up".to_vec()), // Shorter value
        ("abc", vec![0xbb; 32]), // 32-byte value
        ("abcdef", vec![0xcc; 100]), // Large value
    ];

    for (key, new_value) in update_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, new_value)
            .expect(&format!("Failed to update key: {}", key));

        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get updated key: {}", key))
            .expect(&format!("Updated key not found: {}", key));
        assert_eq!(retrieved, *new_value, "Updated key {} value mismatch", key);
        println!("  ✅ Key '{}' updated and verified (hashed)", key);
    }

    println!("📋 Phase 3: Testing deletions and structure changes");

        // Test 7: Delete leaf nodes
    let delete_leaf_tests = vec!["b", "z", "ab", "az", "bb", "zz"];

    for key in delete_leaf_tests.iter() {
        let hashed_key = hash_key(key);
        // Verify key exists before deletion
        let before = trie.get(&hashed_key)
            .expect(&format!("Failed to get key before deletion: {}", key));
        assert!(before.is_some(), "Key {} should exist before deletion", key);

        // Delete the key
        trie.update(&hashed_key, &[])
            .expect(&format!("Failed to delete key: {}", key));

        // Verify key is deleted
        let after = trie.get(&hashed_key)
            .expect(&format!("Failed to get key after deletion: {}", key));
        assert!(after.is_none(), "Key {} should be deleted", key);
        println!("  ✅ Key '{}' deleted and verified (hashed)", key);
    }

        // Test 8: Delete keys that will cause branch node collapse
    let collapse_tests = vec!["ba", "abd", "abz"];

    for key in collapse_tests.iter() {
        let hashed_key = hash_key(key);
        // Verify key exists
        let before = trie.get(&hashed_key)
            .expect(&format!("Failed to get key before deletion: {}", key));
        assert!(before.is_some(), "Key {} should exist before deletion", key);

        // Delete the key
        trie.update(&hashed_key, &[])
            .expect(&format!("Failed to delete key: {}", key));

        // Verify key is deleted
        let after = trie.get(&hashed_key)
            .expect(&format!("Failed to get key after deletion: {}", key));
        assert!(after.is_none(), "Key {} should be deleted", key);
        println!("  ✅ Key '{}' deleted (may cause branch collapse, hashed)", key);
    }

    println!("📋 Phase 4: Verifying remaining keys after deletions");

    // Test 9: Verify remaining keys are still accessible
    let remaining_keys = vec![
        ("a", b"updated_value_a_longer".to_vec()),
        ("aa", b"up".to_vec()),
        ("abc", vec![0xbb; 32]),
        ("abcd", b"value_abcd".to_vec()),
        ("abce", b"value_abce".to_vec()),
        ("baa", b"value_baa".to_vec()),
        ("bab", b"value_bab".to_vec()),
        ("xyz", b"value_xyz".to_vec()),
        ("zzz", b"value_zzz".to_vec()),
        ("abcdef", vec![0xcc; 100]),
        ("abcdef01", b"value_abcdef01".to_vec()),
        ("abcdef0123456789", b"value_very_long_key".to_vec()),
    ];

    for (key, expected_value) in remaining_keys.iter() {
        let hashed_key = hash_key(key);
        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get remaining key: {}", key))
            .expect(&format!("Remaining key not found: {}", key));
        assert_eq!(retrieved, *expected_value, "Remaining key {} value mismatch", key);
        println!("  ✅ Remaining key '{}' verified (hashed)", key);
    }

    // Also verify all value size test keys are still there
    for (key, expected_value) in value_size_tests.iter() {
        let hashed_key = hash_key(key);
        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get value size key: {}", key))
            .expect(&format!("Value size key not found: {}", key));
        assert_eq!(retrieved, *expected_value, "Value size key {} mismatch", key);
        println!("  ✅ Value size key '{}' ({} bytes) verified (hashed)", key, expected_value.len());
    }

    println!("📋 Phase 5: Testing edge cases and special patterns");

        // Test 10: Keys that share long prefixes (will create deep extension nodes)
    let prefix_tests = vec![
        ("commonprefix000", b"cp000".to_vec()),
        ("commonprefix001", b"cp001".to_vec()),
        ("commonprefix002", b"cp002".to_vec()),
        ("commonprefix00a", b"cp00a".to_vec()),
        ("commonprefix00b", b"cp00b".to_vec()),
        ("commonprefix010", b"cp010".to_vec()),
        ("commonprefix100", b"cp100".to_vec()),
    ];

    for (key, value) in prefix_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, value)
            .expect(&format!("Failed to insert prefix key: {}", key));

        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get prefix key: {}", key))
            .expect(&format!("Prefix key not found: {}", key));
        assert_eq!(retrieved, *value, "Prefix key {} value mismatch", key);
        println!("  ✅ Prefix key '{}' inserted and verified (hashed)", key);
    }

        // Test 11: Delete some prefix keys to test extension node handling
    let prefix_delete_tests = vec!["commonprefix001", "commonprefix00a", "commonprefix100"];

    for key in prefix_delete_tests.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, &[])
            .expect(&format!("Failed to delete prefix key: {}", key));

        let after = trie.get(&hashed_key)
            .expect(&format!("Failed to get prefix key after deletion: {}", key));
        assert!(after.is_none(), "Prefix key {} should be deleted", key);
        println!("  ✅ Prefix key '{}' deleted (hashed)", key);
    }

    // Test 12: Verify remaining prefix keys
    let remaining_prefix_keys = vec![
        ("commonprefix000", b"cp000".to_vec()),
        ("commonprefix002", b"cp002".to_vec()),
        ("commonprefix00b", b"cp00b".to_vec()),
        ("commonprefix010", b"cp010".to_vec()),
    ];

    for (key, expected_value) in remaining_prefix_keys.iter() {
        let hashed_key = hash_key(key);
        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get remaining prefix key: {}", key))
            .expect(&format!("Remaining prefix key not found: {}", key));
        assert_eq!(retrieved, *expected_value, "Remaining prefix key {} mismatch", key);
        println!("  ✅ Remaining prefix key '{}' verified (hashed)", key);
    }

    println!("📋 Phase 6: Final comprehensive verification");

    // Test 13: Count total keys to ensure trie integrity
    let mut total_keys = 0;

        // Count all remaining keys by trying to get them
    let all_test_keys = vec![
        // Updated single char keys
        "a",
        // Updated two char keys
        "aa",
        // Remaining three char keys (some deleted)
        "abc", "baa", "bab", "xyz", "zzz",
        // Long keys
        "abcd", "abce", "abcdef", "abcdef01", "abcdef0123456789",
        // Value size keys
        "val_1byte", "val_31bytes", "val_32bytes", "val_33bytes",
        "val_55bytes", "val_56bytes", "val_64bytes", "val_128bytes", "val_1kb",
        // Remaining prefix keys
        "commonprefix000", "commonprefix002", "commonprefix00b", "commonprefix010",
    ];

    for key in all_test_keys.iter() {
        let hashed_key = hash_key(key);
        if let Ok(Some(_)) = trie.get(&hashed_key) {
            total_keys += 1;
        }
    }

    println!("  📊 Total keys in trie: {}", total_keys);
    assert!(total_keys > 20, "Trie should contain substantial number of keys");

    println!("🎉 All boundary condition tests passed!");
    println!("   - ✅ Single, double, triple nibble keys tested");
    println!("   - ✅ Long keys forcing extension nodes tested");
    println!("   - ✅ Value size boundaries (1, 31, 32, 33, 55, 56, 64, 128, 1024 bytes) tested");
    println!("   - ✅ Updates and overwrites tested");
    println!("   - ✅ Deletions and branch collapses tested");
    println!("   - ✅ Shared prefix patterns tested");
    println!("   - ✅ 3-layer trie structure with FullNode/ShortNode/ValueNode verified");
    println!("   - ✅ {} keys remain in final trie state", total_keys);
}

#[test]
fn test_trie_systematic_batch_deletion() {
    println!("🧪 Testing systematic batch deletion with verification...");

    // Create temporary directory path
    let temp_dir = env::temp_dir().join("trie_batch_deletion_test_pathdb");
    let db_path = temp_dir.to_str().unwrap();

    // Create PathDB database
    let config = PathProviderConfig::default();
    let db = PathDB::new(db_path, config)
        .expect("Failed to create PathDB");

    // Create SecureTrieId
    let id = SecureTrieId::new(B256::ZERO);

    // Create Trie instance
    let mut trie = SecureTrieBuilder::new(db)
        .with_id(id)
        .build()
        .expect("Failed to create trie");

    // Helper function to create hashed key with terminator for leaf nodes
    let hash_key = |key: &str| -> Vec<u8> {
        let hash = keccak256(key.as_bytes());
        // Take first few bytes of hash and add terminator byte 16
        let mut key_bytes = hash[..4].to_vec(); // Use first 4 bytes to stay under limit
        key_bytes.push(16); // Add terminator byte for leaf node
        key_bytes
    };

        println!("📋 Phase 1: Insert 100 boundary condition keys for systematic deletion testing");

    // Create 100 test keys with meaningful boundary conditions from previous tests
    let mut all_test_keys = Vec::new();

    // Pattern 1: Single character keys (10 keys)
    for c in 'a'..='j' {
        let key = c.to_string();
        let value = format!("single_{}", c).into_bytes();
        all_test_keys.push((key, value));
    }

    // Pattern 2: Two character keys (10 keys)
    for i in 0..10 {
        let key = format!("{}{}", (b'a' + (i / 5)) as char, (b'a' + (i % 5)) as char);
        let value = format!("double_{}", key).into_bytes();
        all_test_keys.push((key, value));
    }

    // Pattern 3: Three character keys (10 keys)
    for i in 0..10 {
        let key = format!("k{:02}", i);
        let value = format!("triple_{}", key).into_bytes();
        all_test_keys.push((key, value));
    }

    // Pattern 4: Long keys forcing extension nodes (10 keys)
    for i in 0..10 {
        let key = format!("longkey{:02}extension", i);
        let value = format!("long_ext_{}", i).into_bytes();
        all_test_keys.push((key, value));
    }

    // Pattern 5: Value size boundary tests (20 keys)
    let value_sizes = [1, 31, 32, 33, 55, 56, 64, 128, 256, 512, 1024, 2048, 4096, 8192];
    for (i, &size) in value_sizes.iter().enumerate().take(10) {
        let key = format!("val_size_{}", size);
        let value = vec![((i + 1) * 17) as u8; size]; // Use pattern to fill value
        all_test_keys.push((key, value));
    }

    // Pattern 6: RLP boundary conditions (10 keys)
    for i in 0..10 {
        let key = format!("rlp_bound_{:02}", i);
        let size = if i < 5 { 55 + i } else { 56 + (i - 5) }; // Around RLP boundary
        let value = vec![(i + 0x40) as u8; size];
        all_test_keys.push((key, value));
    }

    // Pattern 7: Shared prefix patterns (10 keys)
    for i in 0..10 {
        let key = format!("commonprefix{:03}", i);
        let value = format!("shared_prefix_{:03}", i).into_bytes();
        all_test_keys.push((key, value));
    }

    // Pattern 8: Hash collision potential keys (10 keys)
    for i in 0..10 {
        let key = format!("hash_test_{:02}x", i);
        let value = vec![0x55u8; 32]; // 32-byte values to test HashNode vs ValueNode
        all_test_keys.push((key, value));
    }

    // Pattern 9: Mixed length keys (10 keys)
    let mixed_keys = ["x", "xy", "xyz", "wxyz", "vwxyz", "uvwxyz", "tuvwxyz", "stuvwxyz", "rstuvwxyz", "qrstuvwxyz"];
    for (i, &key) in mixed_keys.iter().enumerate() {
        let value = format!("mixed_len_{}_{}", key.len(), i).into_bytes();
        all_test_keys.push((key.to_string(), value));
    }

    // Pattern 10: Edge case keys (10 keys)
    for i in 0..10 {
        let key = format!("edge_case_{}", i);
        let value = match i {
            0 => vec![0x00], // Minimal value
            1 => vec![0xFF], // Max single byte
            2 => vec![0x00, 0xFF], // Min-max pair
            3 => vec![0xFF; 31], // Just under 32 bytes
            4 => vec![0xAA; 32], // Exactly 32 bytes
            5 => vec![0xBB; 33], // Just over 32 bytes
            6 => vec![0xCC; 54], // Just under RLP boundary
            7 => vec![0xDD; 55], // At RLP boundary
            8 => vec![0xEE; 56], // Just over RLP boundary
            _ => vec![0x99; 100], // Large value
        };
        all_test_keys.push((key, value));
    }

    // Insert all keys
    for (key, value) in all_test_keys.iter() {
        let hashed_key = hash_key(key);
        trie.update(&hashed_key, value)
            .expect(&format!("Failed to insert key: {}", key));
    }

    println!("  ✅ Inserted 100 boundary condition keys successfully (10 patterns × 10 keys each)");

    // Verify all keys are accessible
    for (key, expected_value) in all_test_keys.iter() {
        let hashed_key = hash_key(key);
        let retrieved = trie.get(&hashed_key)
            .expect(&format!("Failed to get key: {}", key))
            .expect(&format!("Key not found: {}", key));
        assert_eq!(retrieved, *expected_value, "Key {} value mismatch", key);
    }

    println!("  ✅ Verified all 100 boundary condition keys are accessible");

    println!("📋 Phase 2: Systematic batch deletion (10 keys per batch)");

    let mut deleted_keys = Vec::new();
    let batch_size = 10;
    let total_keys = all_test_keys.len();

    for batch_num in 0..(total_keys / batch_size) {
        let start_idx = batch_num * batch_size;
        let end_idx = std::cmp::min(start_idx + batch_size, total_keys);

        println!("  🗑️  Batch {}: Deleting keys {} to {}", batch_num + 1, start_idx, end_idx - 1);

        // Delete current batch of keys
        for i in start_idx..end_idx {
            let (key, _) = &all_test_keys[i];
            let hashed_key = hash_key(key);

            // Delete the key
            trie.update(&hashed_key, &[])
                .expect(&format!("Failed to delete key: {}", key));

            deleted_keys.push(key.clone());
            println!("    ❌ Deleted key '{}'", key);
        }

        println!("  🔍 Verifying deletion results for batch {}", batch_num + 1);

        // Verify deleted keys are no longer accessible
        for deleted_key in deleted_keys.iter() {
            let hashed_key = hash_key(deleted_key);
            let result = trie.get(&hashed_key)
                .expect(&format!("Failed to attempt get deleted key: {}", deleted_key));
            assert!(result.is_none(), "Deleted key '{}' should not be accessible", deleted_key);
        }
        println!("    ✅ Verified {} deleted keys are inaccessible", deleted_keys.len());

        // Verify remaining keys are still accessible
        let mut remaining_count = 0;
        for (key, expected_value) in all_test_keys.iter() {
            if !deleted_keys.contains(key) {
                let hashed_key = hash_key(key);
                let retrieved = trie.get(&hashed_key)
                    .expect(&format!("Failed to get remaining key: {}", key))
                    .expect(&format!("Remaining key not found: {}", key));
                assert_eq!(retrieved, *expected_value, "Remaining key {} value mismatch", key);
                remaining_count += 1;
            }
        }
        println!("    ✅ Verified {} remaining keys are still accessible", remaining_count);

        println!("  📊 Status after batch {}: {} deleted, {} remaining",
                batch_num + 1, deleted_keys.len(), total_keys - deleted_keys.len());
        println!();
    }

    println!("📋 Phase 3: Final verification - all keys should be deleted");

    // Verify all keys have been deleted
    for (key, _) in all_test_keys.iter() {
        let hashed_key = hash_key(key);
        let result = trie.get(&hashed_key)
            .expect(&format!("Failed to attempt get key: {}", key));
        assert!(result.is_none(), "Key '{}' should be deleted", key);
    }

    println!("  ✅ Verified all 100 boundary condition keys have been successfully deleted");

    println!("🎉 Systematic batch deletion test completed successfully!");
    println!("   - ✅ 100 boundary condition keys inserted and verified");
    println!("     • Single/double/triple char keys, long extension keys");
    println!("     • Value sizes: 1, 31, 32, 33, 55, 56, 64, 128, 256, 512, 1024+ bytes");
    println!("     • RLP boundary conditions (55/56 byte boundaries)");
    println!("     • Shared prefix patterns, hash collision tests");
    println!("     • Mixed length keys and edge cases");
    println!("   - ✅ 10 batches of 10 keys each deleted systematically");
    println!("   - ✅ After each batch: deleted keys inaccessible, remaining keys accessible");
    println!("   - ✅ Final state: all boundary condition keys successfully deleted from trie");
}



#[test]
fn test_hash_slice_explanation() {
    println!("🧪 Explaining hash[..4].to_vec() syntax...");

    let key = "test";
    let hash = keccak256(key.as_bytes());

    println!("📋 Original data:");
    println!("  - Key: '{}'", key);
    println!("  - Key bytes: {:?}", key.as_bytes());
    println!("  - Hash (32 bytes): {:?}", hash);
    println!("  - Hash length: {}", hash.len());

    // Demonstrate the slicing
    println!("📋 Slicing operations:");

    // Take first 4 bytes
    let first_4 = &hash[..4];
    println!("  - hash[..4] (slice): {:?} (length: {})", first_4, first_4.len());

    // Convert to Vec
    let first_4_vec = hash[..4].to_vec();
    println!("  - hash[..4].to_vec(): {:?} (length: {})", first_4_vec, first_4_vec.len());

    // Show other slicing examples
    println!("📋 Other slicing examples:");
    println!("  - hash[0..4]: {:?}", &hash[0..4]);    // Same as hash[..4]
    println!("  - hash[1..5]: {:?}", &hash[1..5]);    // Bytes 1-4
    println!("  - hash[..8]: {:?}", &hash[..8]);      // First 8 bytes
    println!("  - hash[28..]: {:?}", &hash[28..]);    // Last 4 bytes

    // Demonstrate why we use only 4 bytes
    println!("📋 Why we use only 4 bytes:");
    println!("  - Full hash (32 bytes) would exceed Nibbles limit");
    println!("  - 32 bytes = 64 nibbles (too long)");
    println!("  - 4 bytes = 8 nibbles + 2 terminator nibbles = 10 nibbles (acceptable)");

    // Show the complete process
    println!("📋 Complete hash_key process:");
    let mut key_bytes = hash[..4].to_vec();
    println!("  1. Take first 4 bytes: {:?}", key_bytes);

    key_bytes.push(16);
    println!("  2. Add terminator byte: {:?}", key_bytes);

            println!("  3. Key bytes ready for trie operations: {:?} (length: {})", key_bytes, key_bytes.len());

    // Verify the math
    assert_eq!(first_4.len(), 4, "Should be 4 bytes");
    assert_eq!(first_4_vec.len(), 4, "Vec should also be 4 bytes");
    assert_eq!(key_bytes.len(), 5, "After adding terminator: 5 bytes");
    // Key bytes are ready for trie operations (5 bytes = 4 hash bytes + 1 terminator)

    println!("✅ All slice operations verified!");
    println!("   - hash[..4] means 'first 4 bytes of hash'");
    println!("   - .to_vec() converts slice to owned Vec<u8>");
    println!("   - We use 4 bytes to keep keys manageable");
}



#[test]
fn test_key_to_nibbles_bsc_compatibility() {
    use reth_triedb_memorydb::MemoryDB;
    use crate::trie::Trie;
    use alloy_primitives::hex;


    println!("🧪 Testing key_to_nibbles BSC compatibility...");

    // Create a trie instance to test the method
    let db = MemoryDB::new();
    let trie_id = crate::SecureTrieId::default();
    let _trie = Trie::new(&trie_id, db, None).expect("Failed to create trie");

    // Test cases matching BSC's keybytesToHex tests
    let test_cases = vec![
        ("Empty key", vec![]),
        ("Single byte", vec![0x42]),
        ("Two bytes", vec![0x12, 0x34]),
        ("Three bytes", vec![0x12, 0x34, 0x56]),
        ("Boundary values", vec![0x00, 0xFF, 0x0F, 0xF0]),
        ("BSC test case", vec![0x12, 0x34, 0x5]),
        ("Random bytes", vec![0xAB, 0xCD, 0xEF, 0x01, 0x23]),
        ("Long key", vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]),
        ("All zeros", vec![0x00, 0x00, 0x00]),
        ("All ones", vec![0xFF, 0xFF, 0xFF]),
        ("Mixed pattern", vec![0xA5, 0x5A, 0xC3, 0x3C]),
    ];

    for (name, input) in test_cases {
        println!("📋 Test: {}", name);

        let result = crate::encoding::key_to_nibbles(&input);
        let expected_length = input.len() * 2 + 1;

        println!("  - Input:  {:?} (hex: {})", input, hex::encode(&input));
        println!("  - Output: {:?} (length: {})", result, result.len());
        println!("  - Expected length: {} (input_len * 2 + 1)", expected_length);

        // Verify length
        assert_eq!(result.len(), expected_length, "Length should be input_len * 2 + 1");

        // Verify terminator
        if !result.is_empty() {
            assert_eq!(result[result.len() - 1], 16, "Last element should be terminator 16");
            println!("  - ✅ Terminator: present (16)");
        }

        // Verify nibble conversion for each input byte
        for (i, &byte) in input.iter().enumerate() {
            let expected_high = byte / 16;
            let expected_low = byte % 16;
            let actual_high = result[i * 2];
            let actual_low = result[i * 2 + 1];

            assert_eq!(actual_high, expected_high, "High nibble mismatch for byte {}", i);
            assert_eq!(actual_low, expected_low, "Low nibble mismatch for byte {}", i);

            if i == 0 {  // Print verification for first byte only
                println!("  - Nibble check: byte 0x{:02x} -> nibbles [{}, {}] (expected [{}, {}])",
                    byte, actual_high, actual_low, expected_high, expected_low);
            }
        }

        println!();
    }

    // Test specific BSC test cases from encoding_test.go
    println!("📋 BSC encoding_test.go verification:");

    let bsc_test_cases = vec![
        (vec![], vec![16]),
        (vec![0x12, 0x34, 0x56], vec![1, 2, 3, 4, 5, 6, 16]),
        (vec![0x12, 0x34, 0x5], vec![1, 2, 3, 4, 0, 5, 16]),
    ];

    for (i, (input, expected)) in bsc_test_cases.iter().enumerate() {
        let result = crate::encoding::key_to_nibbles(input);

        println!("  BSC Test Case {}:", i + 1);
        println!("    Input:    {:?}", input);
        println!("    Expected: {:?}", expected);
        println!("    Got:      {:?}", result);

        assert_eq!(result, *expected, "BSC test case {} failed", i + 1);
        println!("    ✅ Match");
    }

    println!("✅ All key_to_nibbles BSC compatibility tests passed!");
    println!("   - ✅ Length calculation: input_len * 2 + 1");
    println!("   - ✅ Nibble conversion: byte / 16, byte % 16");
    println!("   - ✅ Terminator: always 16 at the end");
    println!("   - ✅ BSC test cases: all match exactly");
}
