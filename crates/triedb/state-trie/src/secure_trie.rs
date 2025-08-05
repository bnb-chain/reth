//! Secure trie identifier and builder implementation.

use alloy_primitives::{Address, B256};
use reth_triedb_common::TrieDatabase;
use thiserror::Error;

use super::state_trie::StateTrie;

/// Secure trie identifier
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureTrieId {
    /// State root hash
    pub state_root: B256,
    /// Owner address
    pub owner: Address,
    /// Trie root hash
    pub root: B256,
}

impl SecureTrieId {
    /// Creates a new secure trie identifier
    pub fn new(state_root: B256, owner: Address, root: B256) -> Self {
        Self {
            state_root,
            owner,
            root,
        }
    }
}

/// Secure trie error types
#[derive(Debug, Error)]
pub enum SecureTrieError {
    /// Database operation error
    #[error("Database error: {0}")]
    Database(String),
    /// RLP encoding/decoding error
    #[error("RLP encoding error: {0}")]
    Rlp(#[from] alloy_rlp::Error),
    /// Node not found in trie
    #[error("Node not found")]
    NodeNotFound,
    /// Invalid node data
    #[error("Invalid node")]
    InvalidNode,
    /// Trie already committed
    #[error("Trie already committed")]
    AlreadyCommitted,
    /// Invalid account data
    #[error("Invalid account data")]
    InvalidAccount,
    /// Invalid storage data
    #[error("Invalid storage data")]
    InvalidStorage,
}

/// Secure trie builder for constructing secure tries
#[derive(Debug)]
pub struct SecureTrieBuilder<DB> {
    database: DB,
    id: Option<SecureTrieId>,
}

impl<DB> SecureTrieBuilder<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Creates a new secure trie builder
    pub fn new(database: DB) -> Self {
        Self {
            database,
            id: None,
        }
    }

    /// Sets the trie identifier
    pub fn with_id(mut self, id: SecureTrieId) -> Self {
        self.id = Some(id);
        self
    }

    /// Builds the secure trie
    pub fn build(self) -> Result<StateTrie<DB>, SecureTrieError> {
        let id = self.id.unwrap_or_else(|| SecureTrieId::new(B256::ZERO, Address::ZERO, B256::ZERO));
        StateTrie::new(id, self.database)
    }
}
