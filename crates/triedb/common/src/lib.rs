//! Common traits and types for reth trie database implementations.
//!
//! This crate provides common interfaces and types that are shared across
//! different trie database implementations.

/// Database traits for trie operations.
mod traits;
pub use traits::TrieDatabase;
