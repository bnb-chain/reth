//! Database traits for trie operations.

use alloy_primitives::B256;
use auto_impl::auto_impl;

/// Simple database trait for trie operations
#[auto_impl(Box, Arc)]
pub trait TrieDatabase {
    /// Associated error type for database operations
    type Error;

    /// Get a node from the database by its hash
    fn get(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Insert a node into the database with the given hash
    fn insert(&self, hash: B256, data: Vec<u8>) -> Result<(), Self::Error>;

    /// Check if a node exists in the database
    fn contains(&self, hash: &B256) -> Result<bool, Self::Error>;

    /// Remove a node from the database and return its data if found
    fn remove(&self, hash: &B256) -> Result<Option<Vec<u8>>, Self::Error>;
}
