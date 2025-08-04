# PathDB - RocksDB Integration for Reth

PathDB is a thread-safe key-value database wrapper for RocksDB, designed specifically for the Reth blockchain client. It provides a clean abstraction over RocksDB with support for basic operations, batch operations, iterators, and snapshots.

## Features

- **Thread-safe**: All operations are thread-safe and can be used in concurrent environments
- **Basic Operations**: Get, put, delete, and existence checks
- **Batch Operations**: Efficient batch operations for multiple key-value pairs
- **Iterators**: Support for forward and reverse iteration with prefix and range queries
- **Snapshots**: Point-in-time snapshots for consistent reads
- **Configuration**: Flexible configuration options for RocksDB
- **Error Handling**: Comprehensive error handling with custom error types

## Quick Start

```rust
use reth_triedb_pathdb::*;

// Create a new database
let db = PathDBFactory::new("/path/to/database")?;

// Basic operations
db.put(b"key", b"value")?;
let value = db.get(b"key")?;
db.delete(b"key")?;

// Batch operations
let data = vec![
    (b"key1".to_vec(), b"value1".to_vec()),
    (b"key2".to_vec(), b"value2".to_vec()),
];
db.put_multi(&data)?;

// Iteration
let mut iter = db.iter()?;
while iter.next()? {
    if let Some((key, value)) = iter.current() {
        println!("{}: {:?}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
    }
}
```

## Architecture

### Core Components

1. **StateDB Trait**: The main trait defining the database interface
2. **PathDB**: The concrete implementation using RocksDB
3. **PathDBFactory**: Factory for creating database instances
4. **Batch Operations**: Support for atomic batch operations
5. **Iterators**: Efficient iteration over key-value pairs
6. **Snapshots**: Point-in-time consistent views

### Thread Safety

PathDB is designed to be thread-safe from the ground up. All operations can be safely called from multiple threads concurrently. The implementation uses RocksDB's built-in thread safety features and adds additional synchronization where needed.

### Error Handling

The library provides comprehensive error handling through the `StateDBError` enum:

- `Database`: RocksDB-specific errors
- `Io`: I/O errors
- `Serialization`: Data serialization errors
- `Deserialization`: Data deserialization errors
- `KeyNotFound`: Key not found errors
- `InvalidOperation`: Invalid operation errors

## Configuration

PathDB supports extensive configuration options through the `StateDBConfig` struct:

```rust
let config = StateDBConfig {
    max_open_files: 1000000,
    write_buffer_size: 64 * 1024 * 1024, // 64MB
    max_write_buffer_number: 2,
    target_file_size_base: 64 * 1024 * 1024, // 64MB
    max_background_jobs: 4,
    create_if_missing: true,
    use_fsync: true,
};

let db = PathDBFactory::with_config("/path/to/db", config)?;
```

## Examples

### Basic Usage

```rust
use reth_triedb_pathdb::*;

fn basic_example() -> StateDBResult<()> {
    let db = PathDBFactory::new("/tmp/example_db")?;
    
    // Store user data
    db.put(b"user:1:name", b"Alice")?;
    db.put(b"user:1:age", b"25")?;
    
    // Retrieve data
    let name = db.get(b"user:1:name")?;
    let age = db.get(b"user:1:age")?;
    
    println!("Name: {:?}, Age: {:?}", name, age);
    Ok(())
}
```

### Batch Operations

```rust
fn batch_example() -> StateDBResult<()> {
    let db = PathDBFactory::new("/tmp/batch_example")?;
    
    // Prepare batch data
    let user_data = vec![
        (b"user:1:name".to_vec(), b"Alice".to_vec()),
        (b"user:1:age".to_vec(), b"25".to_vec()),
        (b"user:2:name".to_vec(), b"Bob".to_vec()),
        (b"user:2:age".to_vec(), b"30".to_vec()),
    ];
    
    // Execute batch operation
    db.put_multi(&user_data)?;
    
    // Retrieve all data
    let keys = vec![
        b"user:1:name".to_vec(),
        b"user:1:age".to_vec(),
        b"user:2:name".to_vec(),
        b"user:2:age".to_vec(),
    ];
    
    let results = db.get_multi(&keys)?;
    println!("Retrieved {} items", results.len());
    
    Ok(())
}
```

### Iteration

```rust
fn iteration_example() -> StateDBResult<()> {
    let db = PathDBFactory::new("/tmp/iteration_example")?;
    
    // Insert test data
    let data = vec![
        (b"user:1:name", b"Alice"),
        (b"user:1:age", b"25"),
        (b"user:2:name", b"Bob"),
        (b"user:2:age", b"30"),
    ];
    
    for (key, value) in &data {
        db.put(key, value)?;
    }
    
    // Iterate over all data
    let mut iter = db.iter()?;
    while iter.next()? {
        if let Some((key, value)) = iter.current() {
            println!("{}: {:?}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
        }
    }
    
    // Iterate with prefix
    let mut prefix_iter = db.iter_prefix(b"user:1:")?;
    while prefix_iter.next()? {
        if let Some((key, value)) = prefix_iter.current() {
            println!("User 1 - {}: {:?}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
        }
    }
    
    Ok(())
}
```

### Snapshots

```rust
fn snapshot_example() -> StateDBResult<()> {
    let db = PathDBFactory::new("/tmp/snapshot_example")?;
    
    // Insert initial data
    db.put(b"key1", b"value1")?;
    
    // Create snapshot
    let snapshot = db.snapshot()?;
    
    // Modify data in original database
    db.put(b"key1", b"modified_value1")?;
    db.put(b"key2", b"value2")?;
    
    // Snapshot sees old data
    let snapshot_value = snapshot.get(b"key1")?;
    let db_value = db.get(b"key1")?;
    
    println!("Snapshot value: {:?}", snapshot_value);
    println!("Database value: {:?}", db_value);
    
    Ok(())
}
```

## Testing

The library includes comprehensive tests covering all functionality:

```bash
cargo test
```

Tests include:
- Basic operations (get, put, delete)
- Multi-operations (put_multi, get_multi, delete_multi)
- Batch operations
- Iterator operations
- Snapshot operations
- Thread safety
- Error handling
- Configuration

## Performance Considerations

1. **Batch Operations**: Use batch operations for multiple writes to improve performance
2. **Configuration**: Tune RocksDB configuration based on your workload
3. **Snapshots**: Use snapshots for read-heavy workloads that need consistency
4. **Iterators**: Use prefix iterators when you only need a subset of data

## Integration with Reth

PathDB is designed to be integrated into the Reth blockchain client as a submodule. It provides the foundation for state storage and can be extended with additional functionality as needed.

## License

This project is licensed under either of

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option. 
