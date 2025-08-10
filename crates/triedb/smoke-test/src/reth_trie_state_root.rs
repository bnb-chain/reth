use alloy_primitives::{Address, B256, U256};
use reth_trie_common::HashedPostState;
use reth_trie::StateRoot;
use reth_trie_db::{DatabaseTrieCursorFactory, DatabaseHashedCursorFactory};
use reth_db::test_utils::create_test_rw_db;
use reth_db_api::database::Database;
use std::collections::BTreeMap;

/// Structure for preparing data for Reth Trie StateRoot calculation
pub struct RethTrieStateRootPreparer {
    /// HashedPostState for storing account data
    hashed_state: HashedPostState,
    /// Account trie nodes
    account_trie_nodes: BTreeMap<reth_trie_common::Nibbles, reth_trie_common::BranchNodeCompact>,
    /// Storage trie nodes, grouped by address
    storage_trie_nodes: BTreeMap<B256, BTreeMap<reth_trie_common::Nibbles, reth_trie_common::BranchNodeCompact>>,
}

impl RethTrieStateRootPreparer {
    /// Create a new preparer
    pub fn new() -> Self {
        Self {
            hashed_state: HashedPostState::default(),
            account_trie_nodes: BTreeMap::new(),
            storage_trie_nodes: BTreeMap::new(),
        }
    }

    /// Add account data
    pub fn add_account(&mut self, address: Address, nonce: U256, balance: U256, _storage_root: B256, code_hash: B256) {
        let hashed_address = B256::from(keccak256(address));
        let account = reth_primitives_traits::Account {
            nonce: nonce.try_into().unwrap_or(0),
            balance,
            bytecode_hash: Some(code_hash),
        };

        self.hashed_state.accounts.insert(hashed_address, Some(account));
    }

    /// Add storage data
    pub fn add_storage(&mut self, address: Address, key: B256, value: U256) {
        let hashed_address = B256::from(keccak256(address));
        let hashed_key = B256::from(keccak256(key));

        self.hashed_state.storages.entry(hashed_address).or_default().storage.insert(hashed_key, value);
    }

    /// Delete account
    pub fn delete_account(&mut self, address: Address) {
        let hashed_address = B256::from(keccak256(address));
        self.hashed_state.accounts.insert(hashed_address, None);
    }

    /// Delete storage data
    pub fn delete_storage(&mut self, address: Address, key: B256) {
        let hashed_address = B256::from(keccak256(address));
        let hashed_key = B256::from(keccak256(key));

        if let Some(storage) = self.hashed_state.storages.get_mut(&hashed_address) {
            storage.storage.remove(&hashed_key);
        }
    }

    /// Calculate state root using real StateRoot implementation
    pub fn calculate_root(&self) -> Result<B256, Box<dyn std::error::Error>> {
        // Create a test database
        let db = create_test_rw_db();

        // Get a transaction from the database
        let tx = db.tx()?;

        // Create prefix sets from the hashed state
        let prefix_sets = self.hashed_state.construct_prefix_sets().freeze();

        // Convert hashed state to sorted format
        let state_sorted = self.hashed_state.clone().into_sorted();

        // Create StateRoot instance with proper cursor factories
        let state_root = StateRoot::new(
            DatabaseTrieCursorFactory::new(&tx),
            reth_trie::hashed_cursor::HashedPostStateCursorFactory::new(
                DatabaseHashedCursorFactory::new(&tx),
                &state_sorted,
            ),
        )
        .with_prefix_sets(prefix_sets);

        // Calculate the root
        let root = state_root.root()?;

        Ok(root)
    }

    /// Get current HashedPostState
    pub fn get_hashed_state(&self) -> &HashedPostState {
        &self.hashed_state
    }

    /// Clear all data
    pub fn clear(&mut self) {
        self.hashed_state = HashedPostState::default();
        self.account_trie_nodes.clear();
        self.storage_trie_nodes.clear();
    }
}

impl Default for RethTrieStateRootPreparer {
    fn default() -> Self {
        Self::new()
    }
}



/// Helper function: calculate keccak256 hash
fn keccak256(data: impl AsRef<[u8]>) -> [u8; 32] {
    *alloy_primitives::keccak256(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_reth_trie_state_root_preparer() {
        let mut preparer = RethTrieStateRootPreparer::new();

        // Add an account
        let address = Address::from([1u8; 20]);
        preparer.add_account(
            address,
            U256::from(1),
            U256::from(1000),
            B256::from_str("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").unwrap(),
            B256::from_str("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470").unwrap(),
        );

        // Add storage data
        let key = B256::from([2u8; 32]);
        preparer.add_storage(address, key, U256::from(42));

        // Calculate root
        let root = preparer.calculate_root().unwrap();
        println!("Calculated root: {:?}", root);

        // Verify root is not zero
        assert_ne!(root, B256::ZERO);
    }

    #[test]
    fn test_empty_state_root() {
        let preparer = RethTrieStateRootPreparer::new();
        let root = preparer.calculate_root().unwrap();

        // Empty state root should be the correct empty trie hash
        // This is the keccak256 hash of the RLP encoding of an empty string
        let expected_empty_root = B256::from_str("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").unwrap();
        assert_eq!(root, expected_empty_root);
    }
}
