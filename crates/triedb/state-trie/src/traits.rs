//! Traits for secure trie operations.

use alloy_primitives::{Address, B256};
use super::account::StateAccount;
use super::node::NodeSet;

/// Error type for secure trie operations
pub type SecureTrieError = super::secure_trie::SecureTrieError;

/// Trait for secure trie operations
pub trait SecureTrieTrait {
    /// Associated error type
    type Error;

    /// Returns the trie identifier
    fn id(&self) -> &super::secure_trie::SecureTrieId;

    /// Gets an account from the trie by address
    fn get_account(&mut self, address: Address) -> Result<Option<StateAccount>, Self::Error>;

    /// Updates an account in the trie by address
    fn update_account(&mut self, address: Address, account: &StateAccount) -> Result<(), Self::Error>;

    /// Deletes an account from the trie by address
    fn delete_account(&mut self, address: Address) -> Result<(), Self::Error>;

    /// Gets storage value for an account by key
    fn get_storage(&mut self, address: Address, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Updates storage value for an account by key
    fn update_storage(&mut self, address: Address, key: &[u8], value: &[u8]) -> Result<(), Self::Error>;

    /// Deletes storage value for an account by key
    fn delete_storage(&mut self, address: Address, key: &[u8]) -> Result<(), Self::Error>;

    /// Commits the trie and returns the root hash and modified node set
    ///
    /// The `collect_leaf` parameter determines whether to include leaf nodes in the returned node set.
    /// The returned `NodeSet` contains all modified nodes that need to be persisted to disk.
    fn commit(&mut self, collect_leaf: bool) -> Result<(B256, Option<NodeSet>), Self::Error>;

    /// Returns the current root hash of the trie
    fn hash(&mut self) -> B256;
}
