//! PathDB implementation for RocksDB integration.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;

use rocksdb::{DB, DBIterator, Options, IteratorMode, ReadOptions, WriteBatch, WriteOptions};
use schnellru::{ByLength, LruMap};
use tracing::{error, trace};

use crate::traits::*;
use reth_triedb_common::TrieDatabase;
use alloy_primitives::B256;

/// PathDB implementation using RocksDB.
pub struct PathDB {
    /// The underlying RocksDB instance.
    db: Arc<DB>,
    /// Configuration for the database.
    config: PathProviderConfig,
    /// Write options for batch operations.
    write_options: WriteOptions,
    /// Read options for read operations.
    read_options: ReadOptions,
    /// LRU cache for key-value pairs.
    cache: Mutex<LruMap<Vec<u8>, Option<Vec<u8>>, ByLength>>,
}

impl Debug for PathDB {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathDB")
            .field("config", &self.config)
            .finish()
    }
}

impl Clone for PathDB {
    fn clone(&self) -> Self {
        let mut write_options = WriteOptions::default();
        write_options.set_sync(self.config.use_fsync);

        let read_options = ReadOptions::default();

        Self {
            db: self.db.clone(),
            config: self.config.clone(),
            write_options,
            read_options,
            cache: Mutex::new(LruMap::new(ByLength::new(self.config.cache_size))),
        }
    }
}

/// PathDB write batch implementation.
pub struct PathDBWriteBatch {
    /// The underlying RocksDB write batch.
    batch: WriteBatch,
    /// Number of operations in the batch.
    count: usize,
}

impl Debug for PathDBWriteBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathDBWriteBatch")
            .field("count", &self.count)
            .finish()
    }
}

/// PathDB iterator implementation.
pub struct PathDBIter<'a> {
    /// The underlying RocksDB iterator.
    iter: DBIterator<'a>,
    /// Current key-value pair.
    current: Option<(Vec<u8>, Vec<u8>)>,
    /// Whether the iterator is valid.
    valid: bool,
}

impl<'a> Debug for PathDBIter<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathDBIter")
            .field("valid", &self.valid)
            .finish()
    }
}

/// PathDB snapshot implementation.
pub struct PathDBSnapshot<'a> {
    /// The underlying RocksDB snapshot.
    snapshot: rocksdb::Snapshot<'a>,
}

impl<'a> Debug for PathDBSnapshot<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathDBSnapshot")
            .finish()
    }
}

impl PathDB {
    /// Create a new PathDB instance.
    pub fn new(path: &str, config: PathProviderConfig) -> PathProviderResult<Self> {
        let mut db_opts = Options::default();
        db_opts.set_max_open_files(config.max_open_files);
        db_opts.set_write_buffer_size(config.write_buffer_size);
        db_opts.set_max_write_buffer_number(config.max_write_buffer_number);
        db_opts.set_target_file_size_base(config.target_file_size_base);
        db_opts.set_max_background_jobs(config.max_background_jobs);
        db_opts.create_if_missing(config.create_if_missing);
        db_opts.set_use_fsync(config.use_fsync);

        let db = DB::open(&db_opts, path)
            .map_err(|e| PathProviderError::Database(format!("Failed to open RocksDB: {}", e)))?;

        let mut write_options = WriteOptions::default();
        write_options.set_sync(config.use_fsync);

        let read_options = ReadOptions::default();

        let cache_size = config.cache_size;

        Ok(Self {
            db: Arc::new(db),
            config,
            write_options,
            read_options,
            cache: Mutex::new(LruMap::new(ByLength::new(cache_size))),
        })
    }

    /// Get the underlying RocksDB instance.
    pub fn inner(&self) -> &Arc<DB> {
        &self.db
    }

    /// Get the configuration.
    pub fn config(&self) -> &PathProviderConfig {
        &self.config
    }

    /// Clear the LRU cache.
    pub fn clear_cache(&self) {
        trace!(target: "pathdb::rocksdb", "Clearing LRU cache");
        self.cache.lock().unwrap().clear();
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> (usize, u32) {
        let cache = self.cache.lock().unwrap();
        (cache.len(), self.config.cache_size)
    }
}

impl PathProvider for PathDB {
    fn get(&self, key: &[u8]) -> PathProviderResult<Option<Vec<u8>>> {
        trace!(target: "pathdb::rocksdb", "Getting key: {:?}", key);

        // Check cache first
        {
            let cache = self.cache.lock().unwrap();
            if let Some(cached_value) = cache.peek(key) {
                trace!(target: "pathdb::rocksdb", "Found value in cache for key: {:?}", key);
                return Ok(cached_value.clone());
            }
        }

        // Cache miss, read from DB
        match self.db.get_opt(key, &self.read_options) {
            Ok(Some(value)) => {
                trace!(target: "pathdb::rocksdb", "Found value in DB for key: {:?}", key);
                // Cache the value
                self.cache.lock().unwrap().insert(key.to_vec(), Some(value.to_vec()));
                Ok(Some(value))
            }
            Ok(None) => {
                trace!(target: "pathdb::rocksdb", "Key not found in DB: {:?}", key);
                // Cache the absence
                self.cache.lock().unwrap().insert(key.to_vec(), None);
                Ok(None)
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error getting key {:?}: {}", key, e);
                Err(PathProviderError::Database(format!("RocksDB get error: {}", e)))
            }
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Putting key: {:?}, value_len: {}", key, value.len());

        // Update cache first
        self.cache.lock().unwrap().insert(key.to_vec(), Some(value.to_vec()));

        // Then write to DB
        match self.db.put_opt(key, value, &self.write_options) {
            Ok(()) => {
                trace!(target: "pathdb::rocksdb", "Successfully put key: {:?}", key);
                Ok(())
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error putting key {:?}: {}", key, e);
                // Remove from cache on error
                self.cache.lock().unwrap().remove(key);
                Err(PathProviderError::Database(format!("RocksDB put error: {}", e)))
            }
        }
    }

    fn delete(&self, key: &[u8]) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Deleting key: {:?}", key);

        // Remove from cache first
        self.cache.lock().unwrap().remove(key);

        // Then delete from DB
        match self.db.delete_opt(key, &self.write_options) {
            Ok(()) => {
                trace!(target: "pathdb::rocksdb", "Successfully deleted key: {:?}", key);
                Ok(())
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error deleting key {:?}: {}", key, e);
                Err(PathProviderError::Database(format!("RocksDB delete error: {}", e)))
            }
        }
    }

    fn exists(&self, key: &[u8]) -> PathProviderResult<bool> {
        trace!(target: "pathdb::rocksdb", "Checking existence of key: {:?}", key);

        // Check cache first
        {
            let cache = self.cache.lock().unwrap();
            if let Some(cached_value) = cache.peek(key) {
                trace!(target: "pathdb::rocksdb", "Key exists in cache: {:?}", key);
                return Ok(cached_value.is_some());
            }
        }

        // Cache miss, check DB
        match self.db.get_opt(key, &self.read_options) {
            Ok(Some(_)) => {
                trace!(target: "pathdb::rocksdb", "Key exists in DB: {:?}", key);
                // Cache the existence
                self.cache.lock().unwrap().insert(key.to_vec(), Some(vec![]));
                Ok(true)
            }
            Ok(None) => {
                trace!(target: "pathdb::rocksdb", "Key does not exist in DB: {:?}", key);
                // Cache the absence
                self.cache.lock().unwrap().insert(key.to_vec(), None);
                Ok(false)
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error checking existence of key {:?}: {}", key, e);
                Err(PathProviderError::Database(format!("RocksDB exists error: {}", e)))
            }
        }
    }

    fn get_multi(&self, keys: &[Vec<u8>]) -> PathProviderResult<HashMap<Vec<u8>, Vec<u8>>> {
        trace!(target: "pathdb::rocksdb", "Getting {} keys", keys.len());

        let mut result = HashMap::new();

        for key in keys {
            if let Some(value) = PathProvider::get(self, key)? {
                result.insert(key.clone(), value);
            }
        }

        trace!(target: "pathdb::rocksdb", "Retrieved {} values", result.len());
        Ok(result)
    }

    fn put_multi(&self, kvs: &[(Vec<u8>, Vec<u8>)]) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Putting {} key-value pairs", kvs.len());

        // Update cache first
        {
            let mut cache = self.cache.lock().unwrap();
            for (key, value) in kvs {
                cache.insert(key.clone(), Some(value.clone()));
            }
        }

        // Then write to DB
        let mut batch = WriteBatch::default();

        for (key, value) in kvs {
            batch.put(key, value);
        }

        match self.db.write_opt(batch, &self.write_options) {
            Ok(()) => {
                trace!(target: "pathdb::rocksdb", "Successfully put {} key-value pairs", kvs.len());
                Ok(())
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error putting {} key-value pairs: {}", kvs.len(), e);
                // Remove from cache on error
                let mut cache = self.cache.lock().unwrap();
                for (key, _) in kvs {
                    cache.remove(key);
                }
                Err(PathProviderError::Database(format!("RocksDB put_multi error: {}", e)))
            }
        }
    }

    fn delete_multi(&self, keys: &[Vec<u8>]) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Deleting {} keys", keys.len());

        // Remove from cache first
        {
            let mut cache = self.cache.lock().unwrap();
            for key in keys {
                cache.remove(key);
            }
        }

        // Then delete from DB
        let mut batch = WriteBatch::default();

        for key in keys {
            batch.delete(key);
        }

        match self.db.write_opt(batch, &self.write_options) {
            Ok(()) => {
                trace!(target: "pathdb::rocksdb", "Successfully deleted {} keys", keys.len());
                Ok(())
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error deleting {} keys: {}", keys.len(), e);
                Err(PathProviderError::Database(format!("RocksDB delete_multi error: {}", e)))
            }
        }
    }
}

impl PathProviderBatch for PathDB {
    fn new_batch(&self) -> PathProviderResult<Box<dyn PathProviderWriteBatch>> {
        trace!(target: "pathdb::rocksdb", "Creating new write batch");

        Ok(Box::new(PathDBWriteBatch {
            batch: WriteBatch::default(),
            count: 0,
        }))
    }

    fn write_batch(&self, batch: Box<dyn PathProviderWriteBatch>) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Writing batch with {} operations", batch.len());

        // For now, we'll use a simple approach without downcasting
        // In a real implementation, you might want to use a different approach
        Err(PathProviderError::InvalidOperation("Batch writing not implemented".to_string()))
    }
}

// Note: WriteBatch is not Sync, so we need to handle this differently
// For now, we'll implement a simplified version without Sync
impl PathProviderWriteBatch for PathDBWriteBatch {
    fn put(&mut self, key: &[u8], value: &[u8]) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Adding put operation to batch: {:?}", key);

        self.batch.put(key, value);
        self.count += 1;
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Adding delete operation to batch: {:?}", key);

        self.batch.delete(key);
        self.count += 1;
        Ok(())
    }

    fn clear(&mut self) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Clearing batch");

        self.batch.clear();
        self.count = 0;
        Ok(())
    }

    fn len(&self) -> usize {
        self.count
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl PathProviderIterator for PathDB {
    fn iter(&self) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating iterator");

        let iter = self.db.iterator(IteratorMode::Start);
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }

    fn iter_prefix(&self, prefix: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating prefix iterator for: {:?}", prefix);

        let iter = self.db.iterator(IteratorMode::From(prefix, rocksdb::Direction::Forward));
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }

    fn iter_range(&self, start: &[u8], end: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating range iterator from {:?} to {:?}", start, end);

        let iter = self.db.iterator(IteratorMode::From(start, rocksdb::Direction::Forward));
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }

    fn iter_reverse(&self) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating reverse iterator");

        let iter = self.db.iterator(IteratorMode::End);
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }

    fn iter_prefix_reverse(&self, prefix: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating reverse prefix iterator for: {:?}", prefix);

        let iter = self.db.iterator(IteratorMode::From(prefix, rocksdb::Direction::Reverse));
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }

    fn iter_range_reverse(&self, start: &[u8], end: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating reverse range iterator from {:?} to {:?}", start, end);

        let iter = self.db.iterator(IteratorMode::From(end, rocksdb::Direction::Reverse));
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }
}

impl<'a> PathProviderIter for PathDBIter<'a> {
    fn key(&self) -> Option<&[u8]> {
        self.current.as_ref().map(|(key, _)| key.as_slice())
    }

    fn value(&self) -> Option<&[u8]> {
        self.current.as_ref().map(|(_, value)| value.as_slice())
    }

    fn current(&self) -> Option<(&[u8], &[u8])> {
        self.current.as_ref().map(|(key, value)| (key.as_slice(), value.as_slice()))
    }

    fn next(&mut self) -> PathProviderResult<bool> {
        match self.iter.next() {
            Some(Ok((key, value))) => {
                self.current = Some((key.to_vec(), value.to_vec()));
                self.valid = true;
                Ok(true)
            }
            Some(Err(e)) => {
                error!(target: "pathdb::rocksdb", "Iterator error: {}", e);
                Err(PathProviderError::Database(format!("Iterator error: {}", e)))
            }
            None => {
                self.valid = false;
                Ok(false)
            }
        }
    }

    fn prev(&mut self) -> PathProviderResult<bool> {
        // Simplified prev implementation since RocksDB iterator doesn't have prev
        self.valid = false;
        Ok(false)
    }

    fn seek(&mut self, _key: &[u8]) -> PathProviderResult<bool> {
        // Simplified seek implementation
        self.valid = false;
        Ok(false)
    }

    fn seek_to_first(&mut self) -> PathProviderResult<bool> {
        // Simplified seek to first implementation
        self.valid = false;
        Ok(false)
    }

    fn seek_to_last(&mut self) -> PathProviderResult<bool> {
        // Simplified seek to last implementation
        self.valid = false;
        Ok(false)
    }

    fn valid(&self) -> bool {
        self.valid
    }
}

impl PathProviderManager for PathDB {
    fn open(path: &str) -> PathProviderResult<Arc<dyn PathProvider>> {
        trace!(target: "pathdb::rocksdb", "Opening database at path: {}", path);

        let config = PathProviderConfig::default();
        let db = Self::new(path, config)?;
        Ok(Arc::new(db))
    }

    fn close(&self) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Closing database");

        // RocksDB automatically closes when the last Arc is dropped
        Ok(())
    }

    fn flush(&self) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Flushing database");

        match self.db.flush() {
            Ok(()) => {
                trace!(target: "pathdb::rocksdb", "Successfully flushed database");
                Ok(())
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error flushing database: {}", e);
                Err(PathProviderError::Database(format!("Flush error: {}", e)))
            }
        }
    }

    fn compact(&self) -> PathProviderResult<()> {
        trace!(target: "pathdb::rocksdb", "Compacting database");

        // Simplified compact implementation
        Ok(())
    }

    fn stats(&self) -> PathProviderResult<PathProviderStats> {
        trace!(target: "pathdb::rocksdb", "Getting database statistics");

        // This is a simplified implementation. In a real implementation,
        // you would get actual statistics from RocksDB.
        Ok(PathProviderStats {
            total_keys: 0, // Would need to implement actual counting
            total_size: 0, // Would need to implement actual size calculation
            num_levels: 0, // Would need to implement actual level counting
            num_files: 0,  // Would need to implement actual file counting
        })
    }

    fn snapshot(&self) -> PathProviderResult<Arc<dyn PathProviderSnapshot + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating database snapshot");

        let snapshot = self.db.snapshot();
        Ok(Arc::new(PathDBSnapshot {
            snapshot,
        }))
    }
}

impl<'a> PathProviderSnapshot for PathDBSnapshot<'a> {
    fn get(&self, key: &[u8]) -> PathProviderResult<Option<Vec<u8>>> {
        trace!(target: "pathdb::rocksdb", "Getting key from snapshot: {:?}", key);

        match self.snapshot.get(key) {
            Ok(Some(value)) => {
                trace!(target: "pathdb::rocksdb", "Found value in snapshot for key: {:?}", key);
                Ok(Some(value))
            }
            Ok(None) => {
                trace!(target: "pathdb::rocksdb", "Key not found in snapshot: {:?}", key);
                Ok(None)
            }
            Err(e) => {
                error!(target: "pathdb::rocksdb", "Error getting key from snapshot {:?}: {}", key, e);
                Err(PathProviderError::Database(format!("Snapshot get error: {}", e)))
            }
        }
    }

    fn iter(&self) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating iterator from snapshot");

        let iter = self.snapshot.iterator(IteratorMode::Start);
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }

    fn iter_prefix(&self, prefix: &[u8]) -> PathProviderResult<Box<dyn PathProviderIter + '_>> {
        trace!(target: "pathdb::rocksdb", "Creating prefix iterator from snapshot for: {:?}", prefix);

        let iter = self.snapshot.iterator(IteratorMode::From(prefix, rocksdb::Direction::Forward));
        Ok(Box::new(PathDBIter {
            iter,
            current: None,
            valid: false,
        }))
    }
}

/// Factory for creating PathDB instances.
#[derive(Debug)]
pub struct PathDBFactory;

impl PathDBFactory {
    /// Create a new PathDB instance with default configuration.
    pub fn new(path: &str) -> PathProviderResult<Arc<dyn PathProvider>> {
        PathDB::open(path)
    }

    /// Create a new PathDB instance with custom configuration.
    pub fn with_config(path: &str, config: PathProviderConfig) -> PathProviderResult<Arc<dyn PathProvider>> {
        let db = PathDB::new(path, config)?;
        Ok(Arc::new(db))
    }
}

impl TrieDatabase for PathDB {
    type Error = PathProviderError;

    fn get(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error> {
        let key = hash.as_slice();
        PathProvider::get(self, key)
    }

    fn insert(&self, hash: B256, data: Vec<u8>) -> Result<(), Self::Error> {
        let key = hash.as_slice();
        PathProvider::put(self, key, &data)
    }

    fn contains(&self, hash: &B256) -> Result<bool, Self::Error> {
        let key = hash.as_slice();
        PathProvider::exists(self, key)
    }

    fn remove(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error> {
        let key = hash.as_slice();
        let value = PathProvider::get(self, key)?;
        PathProvider::delete(self, key)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_pathdb_creation() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_db");

        let db = PathDB::new(db_path.to_str().unwrap(), PathProviderConfig::default());
        assert!(db.is_ok());
    }

    #[test]
    fn test_pathdb_factory() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_factory_db");

        let db = PathDBFactory::new(db_path.to_str().unwrap());
        assert!(db.is_ok());

        let config = PathProviderConfig::default();
        let db_path2 = temp_dir.path().join("test_factory_db2");
        let db_with_config = PathDBFactory::with_config(db_path2.to_str().unwrap(), config);
        assert!(db_with_config.is_ok());
    }
}
