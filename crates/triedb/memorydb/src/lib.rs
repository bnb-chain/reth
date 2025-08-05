//! In-memory database implementation for reth trie.
//!
//! This module provides an in-memory implementation of the trie database
//! that can be used for testing and debugging purposes.

pub mod memorydb;

// Re-export main types
pub use memorydb::{MemoryDB, MemoryDBError};
