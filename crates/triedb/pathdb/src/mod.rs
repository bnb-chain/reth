//! PathDB module for RocksDB integration.
//!
//! This module provides a thread-safe abstraction over RocksDB with support for:
//! - Basic key-value operations (get, put, delete)
//! - Batch operations
//! - Iterators
//! - Snapshots
//! - Thread safety

pub mod traits;
pub mod pathdb;
pub mod tests;
pub mod example;

pub use traits::*;
pub use pathdb::*;
