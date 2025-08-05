//! Secure state trie implementation for reth
//!
//! This crate provides a BSC-style secure trie implementation that wraps a trie with key hashing.
//! In a secure trie, all access operations hash the key using keccak256 to prevent calling code
//! from creating long chains of nodes that increase access time.

/// Encoding utilities for trie operations
pub mod encoding;
/// Node structures for trie implementation
pub mod node;
/// Node set for tracking modified nodes during commit
pub mod node_set;
/// Parallel hasher and committer implementation
pub mod parallel_hasher;
/// Core trie implementation
pub mod trie;
/// Traits for secure trie operations
pub mod traits;
/// Account structure and implementation
pub mod account;
/// Secure trie identifier and builder
pub mod secure_trie;
/// State trie implementation
pub mod state_trie;

#[cfg(test)]
mod trie_test;
#[cfg(test)]
mod parallel_hasher_test;

// Re-export main types and traits
pub use crate::trie::Trie;
pub use secure_trie::{SecureTrieId, SecureTrieBuilder, SecureTrieError};
pub use state_trie::{StateTrie, SecureTrie};
pub use account::StateAccount;
pub use traits::SecureTrieTrait;
pub use node_set::{NodeSet, TrieNode};
pub use parallel_hasher::{ParallelHasher, ParallelCommitter};
