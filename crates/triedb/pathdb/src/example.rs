//! Examples for using PathDB.

use std::thread;
use crate::pathdb::*;

/// Basic usage example demonstrating core functionality.
pub fn basic_usage_example() -> StateDBResult<()> {
    println!("=== Basic Usage Example ===");

    // Create a new database
    let db = PathDBFactory::new("/tmp/pathdb_example")?;

    // Basic operations
    println!("1. Basic put/get operations:");
    db.put(b"user:1:name", b"Alice")?;
    db.put(b"user:1:age", b"25")?;
    db.put(b"user:1:email", b"alice@example.com")?;

    let name = db.get(b"user:1:name")?;
    let age = db.get(b"user:1:age")?;
    let email = db.get(b"user:1:email")?;

    println!("   Name: {:?}", name);
    println!("   Age: {:?}", age);
    println!("   Email: {:?}", email);

    // Check existence
    println!("\n2. Existence checks:");
    println!("   user:1:name exists: {}", db.exists(b"user:1:name")?);
    println!("   user:1:address exists: {}", db.exists(b"user:1:address")?);

    // Delete operation
    println!("\n3. Delete operation:");
    db.delete(b"user:1:email")?;
    println!("   After deleting email: {:?}", db.get(b"user:1:email")?);

    println!("Basic usage example completed successfully!");
    Ok(())
}

/// Multi-operation example demonstrating batch operations.
pub fn multi_operations_example() -> StateDBResult<()> {
    println!("\n=== Multi Operations Example ===");

    let db = PathDBFactory::new("/tmp/pathdb_multi_example")?;

    // Multi-put operation
    println!("1. Multi-put operation:");
    let data = vec![
        (b"user:2:name".to_vec(), b"Bob".to_vec()),
        (b"user:2:age".to_vec(), b"30".to_vec()),
        (b"user:2:city".to_vec(), b"New York".to_vec()),
        (b"user:2:job".to_vec(), b"Engineer".to_vec()),
    ];

    db.put_multi(&data)?;
    println!("   Inserted {} key-value pairs", data.len());

    // Multi-get operation
    println!("\n2. Multi-get operation:");
    let keys = vec![
        b"user:2:name".to_vec(),
        b"user:2:age".to_vec(),
        b"user:2:city".to_vec(),
        b"user:2:job".to_vec(),
    ];

    let retrieved = db.get_multi(&keys)?;
    println!("   Retrieved {} values", retrieved.len());

    for (key, value) in &retrieved {
        println!("   {}: {:?}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
    }

    // Multi-delete operation
    println!("\n3. Multi-delete operation:");
    db.delete_multi(&keys)?;
    println!("   Deleted {} keys", keys.len());

    let remaining = db.get_multi(&keys)?;
    println!("   Remaining values: {}", remaining.len());

    println!("Multi operations example completed successfully!");
    Ok(())
}

/// Iterator operations example.
pub fn iterator_operations_example() -> StateDBResult<()> {
    println!("\n=== Iterator Operations Example ===");

    let db = PathDBFactory::new("/tmp/pathdb_iter_example")?;

    // Insert test data
    let data = vec![
        (b"user:1:name", b"Alice"),
        (b"user:1:age", b"25"),
        (b"user:2:name", b"Bob"),
        (b"user:2:age", b"30"),
        (b"user:3:name", b"Charlie"),
        (b"user:3:age", b"35"),
        (b"config:app:version", b"1.0.0"),
        (b"config:app:debug", b"true"),
    ];

    for (key, value) in &data {
        db.put(key, value)?;
    }

    // Basic iterator
    println!("1. Basic iterator (all key-value pairs):");
    let mut iter = db.iter()?;
    let mut count = 0;

    while iter.next()? {
        count += 1;
        if let Some((key, value)) = iter.current() {
            println!("   {}: {:?}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
        }
    }
    println!("   Total items: {}", count);

    // Prefix iterator
    println!("\n2. Prefix iterator (user:*):");
    let mut prefix_iter = db.iter_prefix(b"user:")?;
    let mut user_count = 0;

    while prefix_iter.next()? {
        user_count += 1;
        if let Some((key, value)) = prefix_iter.current() {
            println!("   {}: {:?}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
        }
    }
    println!("   User items: {}", user_count);

    // Range iterator
    println!("\n3. Range iterator (user:1:* to user:2:*):");
    let mut range_iter = db.iter_range(b"user:1:", b"user:3:")?;
    let mut range_count = 0;

    while range_iter.next()? {
        range_count += 1;
        if let Some((key, value)) = range_iter.current() {
            println!("   {}: {:?}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
        }
    }
    println!("   Range items: {}", range_count);

    println!("Iterator operations example completed successfully!");
    Ok(())
}

/// Batch operations example.
pub fn batch_operations_example() -> StateDBResult<()> {
    println!("\n=== Batch Operations Example ===");

    let db = PathDBFactory::new("/tmp/pathdb_batch_example")?;

    // Create a batch
    println!("1. Creating and using a batch:");
    let mut batch = db.new_batch()?;

    // Add operations to batch
    batch.put(b"batch:key1", b"value1")?;
    batch.put(b"batch:key2", b"value2")?;
    batch.put(b"batch:key3", b"value3")?;
    batch.delete(b"batch:key4")?;

    println!("   Batch size: {}", batch.len());
    println!("   Batch empty: {}", batch.is_empty());

    // Note: In this simplified implementation, write_batch is not fully implemented
    // In a real implementation, you would call db.write_batch(batch)
    println!("   Note: Batch writing is simplified in this implementation");

    // Clear batch
    batch.clear()?;
    println!("   After clear - Batch size: {}", batch.len());
    println!("   After clear - Batch empty: {}", batch.is_empty());

    println!("Batch operations example completed successfully!");
    Ok(())
}

/// Snapshot operations example.
pub fn snapshot_operations_example() -> StateDBResult<()> {
    println!("\n=== Snapshot Operations Example ===");

    let db = PathDBFactory::new("/tmp/pathdb_snapshot_example")?;

    // Insert initial data
    db.put(b"snapshot:key1", b"initial_value1")?;
    db.put(b"snapshot:key2", b"initial_value2")?;

    println!("1. Initial data:");
    println!("   key1: {:?}", db.get(b"snapshot:key1")?);
    println!("   key2: {:?}", db.get(b"snapshot:key2")?);

    // Create snapshot
    println!("\n2. Creating snapshot...");
    let snapshot = db.snapshot()?;

    // Modify data in original db
    println!("\n3. Modifying data in original database:");
    db.put(b"snapshot:key1", b"modified_value1")?;
    db.put(b"snapshot:key2", b"modified_value2")?;
    db.put(b"snapshot:key3", b"new_value3")?;

    println!("   Original DB - key1: {:?}", db.get(b"snapshot:key1")?);
    println!("   Original DB - key2: {:?}", db.get(b"snapshot:key2")?);
    println!("   Original DB - key3: {:?}", db.get(b"snapshot:key3")?);

    // Snapshot should see old data
    println!("\n4. Snapshot data (should see old values):");
    println!("   Snapshot - key1: {:?}", snapshot.get(b"snapshot:key1")?);
    println!("   Snapshot - key2: {:?}", snapshot.get(b"snapshot:key2")?);
    println!("   Snapshot - key3: {:?}", snapshot.get(b"snapshot:key3")?);

    println!("Snapshot operations example completed successfully!");
    Ok(())
}

/// Thread safety example.
pub fn thread_safety_example() -> StateDBResult<()> {
    println!("\n=== Thread Safety Example ===");

    let db = Arc::new(PathDBFactory::new("/tmp/pathdb_thread_example")?);

    println!("1. Testing concurrent operations:");

    // Spawn multiple threads
    let mut handles = vec![];

    for i in 0..5 {
        let db_clone = db.clone();
        let handle = thread::spawn(move || {
            let key = format!("thread:key{}", i);
            let value = format!("value{}", i);

            db_clone.put(key.as_bytes(), value.as_bytes()).unwrap();
            let retrieved = db_clone.get(key.as_bytes()).unwrap();

            (key, retrieved)
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for (i, handle) in handles.into_iter().enumerate() {
        let (key, result) = handle.join().unwrap();
        println!("   Thread {} - {}: {:?}", i, key, result);
    }

    println!("Thread safety example completed successfully!");
    Ok(())
}

/// Configuration example.
pub fn configuration_example() -> StateDBResult<()> {
    println!("\n=== Configuration Example ===");

    // Create custom configuration
    let config = StateDBConfig {
        max_open_files: 1000,
        write_buffer_size: 16 * 1024 * 1024, // 16MB
        max_write_buffer_number: 1,
        target_file_size_base: 16 * 1024 * 1024, // 16MB
        max_background_jobs: 2,
        create_if_missing: true,
        use_fsync: false, // Disable fsync for better performance in examples
    };

    println!("1. Creating database with custom configuration:");
    println!("   max_open_files: {}", config.max_open_files);
    println!("   write_buffer_size: {} bytes", config.write_buffer_size);
    println!("   max_write_buffer_number: {}", config.max_write_buffer_number);
    println!("   target_file_size_base: {} bytes", config.target_file_size_base);
    println!("   max_background_jobs: {}", config.max_background_jobs);
    println!("   create_if_missing: {}", config.create_if_missing);
    println!("   use_fsync: {}", config.use_fsync);

    let db = PathDBFactory::with_config("/tmp/pathdb_config_example", config)?;

    // Test the configuration
    println!("\n2. Testing database operations with custom config:");
    db.put(b"config:test", b"works")?;
    let result = db.get(b"config:test")?;
    println!("   Test result: {:?}", result);

    // Get database statistics
    println!("\n3. Database statistics:");
    let stats = db.stats()?;
    println!("   Total keys: {}", stats.total_keys);
    println!("   Total size: {} bytes", stats.total_size);
    println!("   Number of levels: {}", stats.num_levels);
    println!("   Number of files: {}", stats.num_files);

    println!("Configuration example completed successfully!");
    Ok(())
}

/// Run all examples.
pub fn run_all_examples() -> StateDBResult<()> {
    println!("Starting PathDB Examples\n");

    basic_usage_example()?;
    multi_operations_example()?;
    iterator_operations_example()?;
    batch_operations_example()?;
    snapshot_operations_example()?;
    thread_safety_example()?;
    configuration_example()?;

    println!("\nAll examples completed successfully!");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_basic_usage_example() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_basic_example");

        // Override the path for testing
        let db = PathDBFactory::new(db_path.to_str().unwrap()).unwrap();

        // Test basic operations
        db.put(b"user:1:name", b"Alice").unwrap();
        let name = db.get(b"user:1:name").unwrap();
        assert_eq!(name, Some(b"Alice".to_vec()));
    }

    #[test]
    fn test_multi_operations_example() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_multi_example");

        let db = PathDBFactory::new(db_path.to_str().unwrap()).unwrap();

        let data = vec![
            (b"user:2:name".to_vec(), b"Bob".to_vec()),
            (b"user:2:age".to_vec(), b"30".to_vec()),
        ];

        db.put_multi(&data).unwrap();

        let keys = vec![b"user:2:name".to_vec(), b"user:2:age".to_vec()];
        let retrieved = db.get_multi(&keys).unwrap();

        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved.get(&b"user:2:name".to_vec()), Some(&b"Bob".to_vec()));
    }
}
