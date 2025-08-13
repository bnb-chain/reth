//! State account structure and implementation.

use alloy_primitives::{B256, U256, keccak256};
#[allow(unused_imports)]
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};


/// State account structure
#[derive(Copy, Clone, Debug, PartialEq, Eq, RlpDecodable, RlpEncodable)]
pub struct StateAccount {
    /// Account nonce
    pub nonce: u64,
    /// Account balance
    pub balance: U256,
    /// Storage trie root hash
    pub storage_root: B256,
    /// Code hash
    pub code_hash: B256,
}

impl Default for StateAccount {
    fn default() -> Self {
        Self {
            nonce: 0,
            balance: U256::ZERO,
            storage_root: alloy_trie::EMPTY_ROOT_HASH,
            code_hash: alloy_trie::KECCAK_EMPTY,
        }
    }
}

impl StateAccount {
    /// Set custom nonce
    pub fn with_nonce(mut self, nonce: u64) -> Self {
        self.nonce = nonce;
        self
    }
    /// Set custom balance
    pub fn with_balance(mut self, balance: U256) -> Self {
        self.balance = balance;
        self
    }
    /// Set custom storage_root
    pub fn with_storage_root(mut self, storage_root: B256) -> Self {
        self.storage_root = storage_root;
        self
    }

    /// Set custom code_hash
    pub fn with_code_hash(mut self, code_hash: B256) -> Self {
        self.code_hash = code_hash;
        self
    }

    /// Compute  hash as committed to in the MPT trie without memorizing.
    pub fn trie_hash(&self) -> B256 {
        keccak256(self.to_rlp())
    }

    /// Encode the account as RLP.
    pub fn to_rlp(&self) -> Vec<u8> {
        alloy_rlp::encode(self)
    }

    /// Decode a StateAccount from RLP encoded bytes
    pub fn from_rlp(data: &[u8]) -> Result<Self, alloy_rlp::Error> {
        StateAccount::decode(&mut &*data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_account_empty() {
        let account = StateAccount::default();
        let mut encoded = Vec::new();
        account.encode(&mut encoded);
        let encoded_hash = account.trie_hash();

        // the expected hash is from the BSC method
        let expected_hex = "0943e8ddb43403e237cc56ac8ec3e256006e0f75d8e79ca1457b123e5d51a45c";
        let actual_hex = format!("{:x}", encoded_hash);

        assert_eq!(actual_hex, expected_hex);

        let decoded_account = StateAccount::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded_account, account);
    }

    #[test]
    fn test_state_account_rlp_encode_and_decode() {
        let account = StateAccount::default()
        .with_nonce(99)
        .with_balance(U256::from(100))
        .with_storage_root(keccak256(b"test_account_storage_root_1"))
        .with_code_hash(keccak256(b"test_account_code_hash_1"));

        let mut encoded = Vec::new();
        account.encode(&mut encoded);
        let encoded_hash = account.trie_hash();

        // the expected hash is from the BSC method
        let expected_hex = "50ff7a13cd631ecb8098f811526d74d03c319f90ef01012930c6de21534cf4f6";
        let actual_hex = format!("{:x}", encoded_hash);

        assert_eq!(actual_hex, expected_hex);

        let decoded_account = StateAccount::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded_account, account);
    }
}
