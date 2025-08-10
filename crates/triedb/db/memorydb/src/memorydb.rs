//! In-memory database implementation for trie nodes.

use alloy_primitives::B256;
use parking_lot::RwLock;
use reth_storage_errors::db::DatabaseError;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

use reth_triedb_common::TrieDatabase;

/// Error type for memory database operations.
#[derive(Debug, Error)]
pub enum MemoryDBError {
    /// Database operation error
    #[error("Database error: {0}")]
    Database(#[from] DatabaseError),
    /// Node not found in database
    #[error("Node not found: {0}")]
    NodeNotFound(B256),
}

/// In-memory database implementation for trie nodes.
#[derive(Debug, Clone)]
pub struct MemoryDB {
    /// Storage for trie nodes.
    nodes: Arc<RwLock<HashMap<B256, Vec<u8>>>>,
}

impl MemoryDB {
    /// Creates a new empty memory database.
    pub fn new() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Inserts a node into the database.
    pub fn insert(&self, hash: B256, data: Vec<u8>) {
        self.nodes.write().insert(hash, data);
    }

    /// Gets a node from the database.
    pub fn get(&self, hash: &B256) -> Option<Vec<u8>> {
        self.nodes.read().get(hash).cloned()
    }

    /// Removes a node from the database.
    pub fn remove(&self, hash: &B256) -> Option<Vec<u8>> {
        self.nodes.write().remove(hash)
    }

    /// Checks if a node exists in the database.
    pub fn contains(&self, hash: &B256) -> bool {
        self.nodes.read().contains_key(hash)
    }

    /// Clears all nodes from the database.
    pub fn clear(&self) {
        self.nodes.write().clear();
    }

    /// Returns the number of nodes in the database.
    pub fn len(&self) -> usize {
        self.nodes.read().len()
    }

    /// Checks if the database is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.read().is_empty()
    }
}

impl Default for MemoryDB {
    fn default() -> Self {
        Self::new()
    }
}

impl TrieDatabase for MemoryDB {
    type Error = MemoryDBError;

    fn get(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.get(hash))
    }

    fn insert(&self, hash: B256, data: Vec<u8>) -> Result<(), Self::Error> {
        self.insert(hash, data);
        Ok(())
    }

    fn contains(&self, hash: &B256) -> Result<bool, Self::Error> {
        Ok(self.contains(hash))
    }

    fn remove(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.remove(hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    #[test]
    fn test_memory_db_basic_operations() {
        let db = MemoryDB::new();
        assert!(db.is_empty());

        let data = b"test data".to_vec();
        let hash = keccak256(&data);

        // Test insert and get
        db.insert(hash, data.clone());
        assert!(!db.is_empty());
        assert_eq!(db.len(), 1);
        assert!(db.contains(&hash));
        assert_eq!(db.get(&hash), Some(data.clone()));

        // Test remove
        let removed = db.remove(&hash);
        assert_eq!(removed, Some(data));
        assert!(db.is_empty());
        assert!(!db.contains(&hash));
    }

    #[test]
    fn test_memory_db_trie_interface() {
        let db = MemoryDB::new();
        let data = b"test data".to_vec();
        let hash = keccak256(&data);

        // Test TrieDatabase trait implementation
        assert!(TrieDatabase::get(&db, &hash).unwrap().is_none());
        TrieDatabase::insert(&db, hash, data.clone()).unwrap();
        assert_eq!(TrieDatabase::get(&db, &hash).unwrap(), Some(data.clone()));
        assert!(TrieDatabase::contains(&db, &hash).unwrap());
        assert_eq!(TrieDatabase::remove(&db, &hash).unwrap(), Some(data));
    }
}
