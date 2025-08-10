//! Tests for PathDB implementation.

use std::sync::Arc;
use tempfile::TempDir;
use crate::{pathdb::*, traits::*};

#[test]
fn test_basic_operations() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_db");
    let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap();

    // Test put and get
    let key = b"test_key";
    let value = b"test_value";

    db.put(key, value).unwrap();

    let retrieved = db.get(key).unwrap();
    assert_eq!(retrieved, Some(value.to_vec()));

    // Test exists
    assert!(db.exists(key).unwrap());
    assert!(!db.exists(b"non_existent_key").unwrap());

    // Test delete
    db.delete(key).unwrap();
    assert_eq!(db.get(key).unwrap(), None);
    assert!(!db.exists(key).unwrap());
}

#[test]
fn test_multi_operations() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_multi_db");
    let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap();

    // Test put_multi
    let kvs = vec![
        (b"key1".to_vec(), b"value1".to_vec()),
        (b"key2".to_vec(), b"value2".to_vec()),
        (b"key3".to_vec(), b"value3".to_vec()),
    ];

    db.put_multi(&kvs).unwrap();

    // Test get_multi
    let keys = vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()];
    let retrieved = db.get_multi(&keys).unwrap();

    assert_eq!(retrieved.len(), 3);
    assert_eq!(retrieved.get(&b"key1".to_vec()), Some(&b"value1".to_vec()));
    assert_eq!(retrieved.get(&b"key2".to_vec()), Some(&b"value2".to_vec()));
    assert_eq!(retrieved.get(&b"key3".to_vec()), Some(&b"value3".to_vec()));

    // Test delete_multi
    db.delete_multi(&keys).unwrap();

    for key in &keys {
        assert_eq!(db.get(key).unwrap(), None);
    }
}

#[test]
fn test_batch_operations() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_batch_db");
    let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap();

    // Create a batch
    let mut batch = db.new_batch().unwrap();

    // Add operations to batch
    batch.put(b"batch_key1", b"batch_value1").unwrap();
    batch.put(b"batch_key2", b"batch_value2").unwrap();
    batch.delete(b"batch_key3").unwrap();

    assert_eq!(batch.len(), 3);
    assert!(!batch.is_empty());

    // Clear batch
    batch.clear().unwrap();
    assert_eq!(batch.len(), 0);
    assert!(batch.is_empty());
}

#[test]
fn test_iterator_operations() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_iter_db");
    let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap();

    // Insert some test data
    let test_data = vec![
        (b"a_key1", b"a_value1"),
        (b"a_key2", b"a_value2"),
        (b"b_key1", b"b_value1"),
        (b"b_key2", b"b_value2"),
    ];

    for (key, value) in &test_data {
        db.put(*key, *value).unwrap();
    }

    // Test basic iterator
    let mut iter = db.iter().unwrap();
    let mut count = 0;

    while iter.next().unwrap() {
        count += 1;
        assert!(iter.valid());
        assert!(iter.key().is_some());
        assert!(iter.value().is_some());
    }

    assert_eq!(count, 4);

    // Test prefix iterator
    let mut prefix_iter = db.iter_prefix(b"a_").unwrap();
    let mut prefix_count = 0;

    while prefix_iter.next().unwrap() {
        let key = prefix_iter.key().unwrap();
        if !key.starts_with(b"a_") {
            break;
        }
        prefix_count += 1;
        assert!(key.starts_with(b"a_"));
    }

    assert_eq!(prefix_count, 2);
}

#[test]
fn test_snapshot_operations() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_snapshot_db");
    let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap();

    // Insert initial data
    db.put(b"snapshot_key", b"snapshot_value").unwrap();

    // Create snapshot
    let snapshot = db.snapshot().unwrap();

    // Verify snapshot can read the data
    let value = snapshot.get(b"snapshot_key").unwrap();
    assert_eq!(value, Some(b"snapshot_value".to_vec()));

    // Modify data in original db
    db.put(b"snapshot_key", b"modified_value").unwrap();

    // Snapshot should still see old value
    let snapshot_value = snapshot.get(b"snapshot_key").unwrap();
    assert_eq!(snapshot_value, Some(b"snapshot_value".to_vec()));

    // Original db should see new value
    let db_value = db.get(b"snapshot_key").unwrap();
    assert_eq!(db_value, Some(b"modified_value".to_vec()));
}

#[test]
fn test_database_management() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_management_db");
    let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap();

    // Test flush
    db.put(b"flush_key", b"flush_value").unwrap();
    db.flush().unwrap();

    // Test stats (simplified implementation returns zeros)
    let stats = db.stats().unwrap();
    assert_eq!(stats.total_keys, 0);
    assert_eq!(stats.total_size, 0);

    // Test compact
    db.compact().unwrap();

    // Test close
    db.close().unwrap();
}

#[test]
fn test_configuration() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_config_db");

    let config = PathProviderConfig {
        max_open_files: 1000,
        write_buffer_size: 32 * 1024 * 1024, // 32MB
        max_write_buffer_number: 1,
        target_file_size_base: 32 * 1024 * 1024, // 32MB
        max_background_jobs: 2,
        create_if_missing: true,
        use_fsync: false,
        cache_size: 1024 * 1024, // 1MB for testing
    };

    let db = PathDB::new(db_path.to_str().unwrap(), config).unwrap();

    // Verify configuration by testing operations
    db.put(b"config_test", b"works").unwrap();
    let result = db.get(b"config_test").unwrap();
    assert_eq!(result, Some(b"works".to_vec()));
}

#[test]
fn test_error_handling() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_error_db");
    let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap();

    // Test getting non-existent key
    let result = db.get(b"non_existent");
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None);

    // Test deleting non-existent key (should not error)
    let result = db.delete(b"non_existent");
    assert!(result.is_ok());
}

#[test]
fn test_thread_safety() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_thread_db");
    let db = Arc::new(PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default()).unwrap());

    // Test concurrent reads
    let db_clone1 = db.clone();
    let db_clone2 = db.clone();

    let handle1 = std::thread::spawn(move || {
        db_clone1.put(b"thread_key1", b"thread_value1").unwrap();
        db_clone1.get(b"thread_key1").unwrap()
    });

    let handle2 = std::thread::spawn(move || {
        db_clone2.put(b"thread_key2", b"thread_value2").unwrap();
        db_clone2.get(b"thread_key2").unwrap()
    });

    let result1 = handle1.join().unwrap();
    let result2 = handle2.join().unwrap();

    assert_eq!(result1, Some(b"thread_value1".to_vec()));
    assert_eq!(result2, Some(b"thread_value2".to_vec()));
}

#[test]
fn test_factory_pattern() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_factory_db");

    // Test default factory
    let db1 = PathDBFactory::new(db_path.to_str().unwrap());
    assert!(db1.is_ok());

    // Test factory with config
    let config = PathProviderConfig::default();
    let db_path2 = temp_dir.path().join("test_factory_db2");
    let db2 = PathDBFactory::with_config(db_path2.to_str().unwrap(), config);
    assert!(db2.is_ok());
}

#[test]
fn test_invalid_path() {
    // Test with invalid path
    let result = PathDBFactory::new("/invalid/path/that/does/not/exist");
    assert!(result.is_err());

    match result.unwrap_err() {
        PathProviderError::Database(_) => {}, // Expected
        _ => panic!("Expected database error"),
    }
}

#[test]
fn test_lru_cache_functionality() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_cache_db");

    // Create config with small cache size for testing
    let mut config = PathProviderConfig::default();
    config.cache_size = 1024; // 1KB for testing

    let db = PathDB::new(db_path.to_str().unwrap(), config).unwrap();

    // Test write-then-read: should hit cache
    db.put(b"key1", b"value1").unwrap();
    let value1 = db.get(b"key1").unwrap();
    assert_eq!(value1, Some(b"value1".to_vec()));

    // Test read from DB: should populate cache
    let value1_again = db.get(b"key1").unwrap();
    assert_eq!(value1_again, Some(b"value1".to_vec()));

    // Test cache invalidation on write
    db.put(b"key1", b"new_value1").unwrap();
    let new_value1 = db.get(b"key1").unwrap();
    assert_eq!(new_value1, Some(b"new_value1".to_vec()));

    // Test cache invalidation on delete
    db.delete(b"key1").unwrap();
    let deleted_value = db.get(b"key1").unwrap();
    assert_eq!(deleted_value, None);

    // Test cache stats
    let (len, capacity) = db.cache_stats();
    assert!(len > 0);
    assert!(capacity > 0);
}

#[test]
fn test_lru_cache_multi_operations() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_cache_multi_db");

    let mut config = PathProviderConfig::default();
    config.cache_size = 2048; // 2KB for testing

    let db = PathDB::new(db_path.to_str().unwrap(), config).unwrap();

    // Insert multiple key-value pairs
    let kvs = vec![
        (b"multi_key1".to_vec(), b"multi_value1".to_vec()),
        (b"multi_key2".to_vec(), b"multi_value2".to_vec()),
        (b"multi_key3".to_vec(), b"multi_value3".to_vec()),
    ];

    db.put_multi(&kvs).unwrap();

    // Read them back - should hit cache
    let keys = vec![b"multi_key1".to_vec(), b"multi_key2".to_vec(), b"multi_key3".to_vec()];
    let retrieved = db.get_multi(&keys).unwrap();

    assert_eq!(retrieved.len(), 3);
    assert_eq!(retrieved.get(&b"multi_key1".to_vec()), Some(&b"multi_value1".to_vec()));

    // Delete multiple keys - should invalidate cache
    db.delete_multi(&keys).unwrap();

    // Try to read deleted keys
    for key in &keys {
        assert_eq!(db.get(key).unwrap(), None);
    }
}

#[test]
fn test_cache_clear() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_cache_clear_db");

    let mut config = PathProviderConfig::default();
    config.cache_size = 1024;

    let db = PathDB::new(db_path.to_str().unwrap(), config).unwrap();

    // Insert some data
    db.put(b"cache_key1", b"cache_value1").unwrap();
    db.put(b"cache_key2", b"cache_value2").unwrap();

    // Read to populate cache
    db.get(b"cache_key1").unwrap();
    db.get(b"cache_key2").unwrap();

    // Check cache has data
    let (len_before, _) = db.cache_stats();
    assert!(len_before > 0);

    // Clear cache
    db.clear_cache();

    // Check cache is empty
    let (len_after, _) = db.cache_stats();
    assert_eq!(len_after, 0);

    // Data should still be accessible from DB
    assert_eq!(db.get(b"cache_key1").unwrap(), Some(b"cache_value1".to_vec()));
    assert_eq!(db.get(b"cache_key2").unwrap(), Some(b"cache_value2".to_vec()));
}
