//! PathProvider trait definitions for key-value database operations.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

// Default configuration constants
pub const DEFAULT_MAX_OPEN_FILES: i32 = 1000000;
pub const DEFAULT_WRITE_BUFFER_SIZE: usize = 4 * 1024 * 1024 * 1024; // 4GB
pub const DEFAULT_MAX_WRITE_BUFFER_NUMBER: i32 = 2;
pub const DEFAULT_TARGET_FILE_SIZE_BASE: u64 = 64 * 1024 * 1024; // 64MB
pub const DEFAULT_MAX_BACKGROUND_JOBS: i32 = 4;
pub const DEFAULT_CREATE_IF_MISSING: bool = true;
pub const DEFAULT_USE_FSYNC: bool = true;
pub const DEFAULT_CACHE_SIZE: u32 = 1_000_000; // 1M entries

/// Result type for PathProvider operations.
pub type PathProviderResult<T> = Result<T, PathProviderError>;

/// Error type for PathProvider operations.
#[derive(Debug, thiserror::Error)]
pub enum PathProviderError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("Deserialization error: {0}")]
    Deserialization(String),
    #[error("Key not found: {0:?}")]
    KeyNotFound(Vec<u8>),
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),
}

/// Trait for basic key-value database operations.
pub trait PathProvider: Send + Sync + Debug {
    /// Get a value by key.
    fn get(&self, key: &[u8]) -> PathProviderResult<Option<Vec<u8>>>;

    /// Put a key-value pair.
    fn put(&self, key: &[u8], value: &[u8]) -> PathProviderResult<()>;

    /// Delete a key.
    fn delete(&self, key: &[u8]) -> PathProviderResult<()>;

    /// Check if a key exists.
    fn exists(&self, key: &[u8]) -> PathProviderResult<bool>;

    /// Get multiple values by keys.
    fn get_multi(&self, keys: &[Vec<u8>]) -> PathProviderResult<HashMap<Vec<u8>, Vec<u8>>>;

    /// Put multiple key-value pairs.
    fn put_multi(&self, kvs: &[(Vec<u8>, Vec<u8>)]) -> PathProviderResult<()>;

    /// Delete multiple keys.
    fn delete_multi(&self, keys: &[Vec<u8>]) -> PathProviderResult<()>;
}

/// Trait for batch operations.
pub trait PathProviderBatch: Send + Sync + Debug {
    /// Create a new batch.
    fn new_batch(&self) -> PathProviderResult<Box<dyn PathProviderWriteBatch>>;

    /// Write a batch atomically.
    fn write_batch(&self, batch: Box<dyn PathProviderWriteBatch>) -> PathProviderResult<()>;
}

/// Trait for write batch operations.
pub trait PathProviderWriteBatch: Send + Debug {
    /// Put a key-value pair in the batch.
    fn put(&mut self, key: &[u8], value: &[u8]) -> PathProviderResult<()>;

    /// Delete a key in the batch.
    fn delete(&mut self, key: &[u8]) -> PathProviderResult<()>;

    /// Clear all operations in the batch.
    fn clear(&mut self) -> PathProviderResult<()>;

    /// Get the number of operations in the batch.
    fn len(&self) -> usize;

    /// Check if the batch is empty.
    fn is_empty(&self) -> bool;
}

/// Trait for database iteration operations.
pub trait PathProviderIterator: Send + Sync + Debug {
    /// Create an iterator over all key-value pairs.
    fn iter(&self) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;

    /// Create an iterator over key-value pairs with a prefix.
    fn iter_prefix(&self, prefix: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;

    /// Create an iterator over key-value pairs in a range.
    fn iter_range(&self, start: &[u8], end: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;

    /// Create a reverse iterator over all key-value pairs.
    fn iter_reverse(&self) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;

    /// Create a reverse iterator over key-value pairs with a prefix.
    fn iter_prefix_reverse(&self, prefix: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;

    /// Create a reverse iterator over key-value pairs in a range.
    fn iter_range_reverse(&self, start: &[u8], end: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;
}

/// Trait for database iteration.
pub trait PathProviderIter: Send + Sync + Debug {
    /// Get the current key.
    fn key(&self) -> Option<&[u8]>;

    /// Get the current value.
    fn value(&self) -> Option<&[u8]>;

    /// Get the current key-value pair.
    fn current(&self) -> Option<(&[u8], &[u8])>;

    /// Move to the next item.
    fn next(&mut self) -> PathProviderResult<bool>;

    /// Move to the previous item.
    fn prev(&mut self) -> PathProviderResult<bool>;

    /// Seek to a specific key.
    fn seek(&mut self, key: &[u8]) -> PathProviderResult<bool>;

    /// Seek to the first item.
    fn seek_to_first(&mut self) -> PathProviderResult<bool>;

    /// Seek to the last item.
    fn seek_to_last(&mut self) -> PathProviderResult<bool>;

    /// Check if the iterator is valid.
    fn valid(&self) -> bool;
}

/// Trait for database management operations.
pub trait PathProviderManager: Send + Sync + Debug {
    /// Open or create a database at the given path.
    fn open(path: &str) -> PathProviderResult<Arc<dyn PathProvider>>;

    /// Close the database.
    fn close(&self) -> PathProviderResult<()>;

    /// Flush all pending writes to disk.
    fn flush(&self) -> PathProviderResult<()>;

    /// Compact the database.
    fn compact(&self) -> PathProviderResult<()>;

    /// Get database statistics.
    fn stats(&self) -> PathProviderResult<PathProviderStats>;

    /// Create a snapshot of the database.
    fn snapshot(&self) -> PathProviderResult<Arc<dyn PathProviderSnapshot + '_>>;
}

/// Trait for database snapshots.
pub trait PathProviderSnapshot: Send + Sync + Debug {
    /// Get a value by key from the snapshot.
    fn get(&self, key: &[u8]) -> PathProviderResult<Option<Vec<u8>>>;

    /// Create an iterator over the snapshot.
    fn iter(&self) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;

    /// Create an iterator over the snapshot with a prefix.
    fn iter_prefix(&self, prefix: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>>;
}

/// Database statistics.
#[derive(Debug, Clone)]
pub struct PathProviderStats {
    /// Total number of keys in the database.
    pub total_keys: u64,
    /// Total size of the database in bytes.
    pub total_size: u64,
    /// Number of levels in the database.
    pub num_levels: u32,
    /// Number of files in the database.
    pub num_files: u32,
}

/// Configuration for PathProvider.
#[derive(Debug, Clone)]
pub struct PathProviderConfig {
    /// Maximum number of open files.
    pub max_open_files: i32,
    /// Write buffer size in bytes.
    pub write_buffer_size: usize,
    /// Maximum write buffer number.
    pub max_write_buffer_number: i32,
    /// Target file size for compaction.
    pub target_file_size_base: u64,
    /// Maximum background jobs.
    pub max_background_jobs: i32,
    /// Whether to create the database if it doesn't exist.
    pub create_if_missing: bool,
    /// Whether to use fsync for writes.
    pub use_fsync: bool,
    /// LRU cache size in number of entries (default: 1M entries).
    pub cache_size: u32,
}

impl Default for PathProviderConfig {
    fn default() -> Self {
        Self {
            max_open_files: DEFAULT_MAX_OPEN_FILES,
            write_buffer_size: DEFAULT_WRITE_BUFFER_SIZE,
            max_write_buffer_number: DEFAULT_MAX_WRITE_BUFFER_NUMBER,
            target_file_size_base: DEFAULT_TARGET_FILE_SIZE_BASE,
            max_background_jobs: DEFAULT_MAX_BACKGROUND_JOBS,
            create_if_missing: DEFAULT_CREATE_IF_MISSING,
            use_fsync: DEFAULT_USE_FSYNC,
            cache_size: DEFAULT_CACHE_SIZE,
        }
    }
}
