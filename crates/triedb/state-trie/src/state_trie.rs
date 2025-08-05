//! State trie implementation for secure trie operations.

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::{Encodable, Decodable};
use reth_triedb_common::TrieDatabase;

use super::account::StateAccount;
use super::secure_trie::{SecureTrieId, SecureTrieError};
use super::traits::SecureTrieTrait;
use super::trie::Trie;
use super::node_set::NodeSet;

/// State trie implementation that wraps a trie with secure key hashing
pub struct StateTrie<DB> {
    trie: Trie<DB>,
    id: SecureTrieId,
}

impl<DB> std::fmt::Debug for StateTrie<DB>
where
    DB: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateTrie")
            .field("trie", &self.trie)
            .field("id", &self.id)
            .finish()
    }
}

impl<DB> StateTrie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Creates a new state trie with the given identifier and database
    pub fn new(id: SecureTrieId, database: DB) -> Result<Self, SecureTrieError> {
        let trie = Trie::new(&id, database)?;

        // If a non-zero root is provided, we need to load the existing trie
        // For now, we'll create a new trie and let the user load data as needed
        // TODO: Implement proper root loading from database

        Ok(Self { trie, id })
    }

    /// Returns the identifier of this state trie
    pub fn id(&self) -> &SecureTrieId {
        &self.id
    }

    /// Returns a reference to the underlying trie
    pub fn trie(&self) -> &Trie<DB> {
        &self.trie
    }

    /// Returns a mutable reference to the underlying trie
    pub fn trie_mut(&mut self) -> &mut Trie<DB> {
        &mut self.trie
    }

    /// Returns a reference to the database
    pub fn database(&self) -> &DB {
        self.trie.database()
    }

    /// Returns a mutable reference to the database
    pub fn database_mut(&mut self) -> &mut DB {
        self.trie.database_mut()
    }

    /// Returns a reference to the secure key cache
    pub fn get_sec_key_cache(&mut self) -> &mut std::collections::HashMap<String, Vec<u8>> {
        self.trie.get_sec_key_cache()
    }

    /// Gets the original key from a hashed key using the cache
    pub fn get_key(&self, sha_key: &[u8]) -> Option<Vec<u8>> {
        self.trie.get_key(sha_key)
    }

    /// Creates a copy of this state trie
    pub fn copy(&self) -> Self {
        Self {
            trie: self.trie.copy(),
            id: self.id.clone(),
        }
    }

    /// Hashes a key using keccak256
    pub fn hash_key(&self, key: &[u8]) -> B256 {
        keccak256(key)
    }
}

impl<DB> SecureTrieTrait for StateTrie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    type Error = SecureTrieError;

    fn id(&self) -> &SecureTrieId {
        &self.id
    }

    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        let hashed_key = self.hash_key(key);
        self.trie.get(hashed_key.as_slice())
    }

    fn update(&mut self, key: &[u8], value: &[u8]) -> Result<(), Self::Error> {
        let hashed_key = self.hash_key(key);
        self.trie.update(hashed_key.as_slice(), value)?;

        // Cache the original key
        let key_str = String::from_utf8_lossy(hashed_key.as_slice()).to_string();
        self.trie.get_sec_key_cache().insert(key_str, key.to_vec());

        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<(), Self::Error> {
        let hashed_key = self.hash_key(key);
        self.trie.delete(hashed_key.as_slice())?;

        // Remove from cache
        let key_str = String::from_utf8_lossy(hashed_key.as_slice()).to_string();
        self.trie.get_sec_key_cache().remove(&key_str);

        Ok(())
    }

    fn get_account(&mut self, address: Address) -> Result<Option<StateAccount>, Self::Error> {
        let hashed_address = self.hash_key(address.as_slice());
        if let Some(data) = self.trie.get(hashed_address.as_slice())? {
            let account = StateAccount::decode(&mut &data[..])
                .map_err(|_| SecureTrieError::InvalidAccount)?;
            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    fn update_account(&mut self, address: Address, account: &StateAccount) -> Result<(), Self::Error> {
        let hashed_address = self.hash_key(address.as_slice());
        let mut encoded_account = Vec::new();
        account.encode(&mut encoded_account);
        self.trie.update(hashed_address.as_slice(), &encoded_account)?;

        // Cache the original address
        let addr_str = String::from_utf8_lossy(hashed_address.as_slice()).to_string();
        self.trie.get_sec_key_cache().insert(addr_str, address.to_vec());

        Ok(())
    }

    fn delete_account(&mut self, address: Address) -> Result<(), Self::Error> {
        let hashed_address = self.hash_key(address.as_slice());
        self.trie.delete(hashed_address.as_slice())?;

        // Remove from cache
        let addr_str = String::from_utf8_lossy(hashed_address.as_slice()).to_string();
        self.trie.get_sec_key_cache().remove(&addr_str);

        Ok(())
    }

    fn get_storage(&mut self, _address: Address, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        let hashed_key = self.hash_key(key);
        self.trie.get(hashed_key.as_slice())
    }

    fn update_storage(&mut self, _address: Address, key: &[u8], value: &[u8]) -> Result<(), Self::Error> {
        let hashed_key = self.hash_key(key);
        self.trie.update(hashed_key.as_slice(), value)?;

        // Cache the original key
        let key_str = String::from_utf8_lossy(hashed_key.as_slice()).to_string();
        self.trie.get_sec_key_cache().insert(key_str, key.to_vec());

        Ok(())
    }

    fn delete_storage(&mut self, _address: Address, key: &[u8]) -> Result<(), Self::Error> {
        let hashed_key = self.hash_key(key);
        self.trie.delete(hashed_key.as_slice())?;

        // Remove from cache
        let key_str = String::from_utf8_lossy(hashed_key.as_slice()).to_string();
        self.trie.get_sec_key_cache().remove(&key_str);

        Ok(())
    }

    fn commit(&mut self, collect_leaf: bool) -> Result<(B256, Option<NodeSet>), Self::Error> {
        self.trie.commit(collect_leaf)
    }

    fn root(&self) -> B256 {
        self.trie.hash()
    }
}

/// Type alias for secure trie
pub type SecureTrie<DB> = StateTrie<DB>;
