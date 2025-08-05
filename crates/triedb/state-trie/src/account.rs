//! State account structure and implementation.

use alloy_primitives::{B256, U256};
use alloy_rlp::{Decodable, Encodable};

/// State account structure
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateAccount {
    /// Account nonce
    pub nonce: U256,
    /// Account balance
    pub balance: U256,
    /// Storage trie root hash
    pub storage_root: B256,
    /// Code hash
    pub code_hash: B256,
}

impl Encodable for StateAccount {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::Header { list: true, payload_length: 4 }.encode(out);
        self.nonce.encode(out);
        self.balance.encode(out);
        self.storage_root.encode(out);
        self.code_hash.encode(out);
    }
}

impl Decodable for StateAccount {
    fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        let header = alloy_rlp::Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::Custom("expected list"));
        }
        Ok(Self {
            nonce: Decodable::decode(buf)?,
            balance: Decodable::decode(buf)?,
            storage_root: Decodable::decode(buf)?,
            code_hash: Decodable::decode(buf)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_account_encoding_decoding() {
        let account = StateAccount {
            nonce: U256::from(1),
            balance: U256::from(1000),
            storage_root: B256::ZERO,
            code_hash: B256::ZERO,
        };

        let mut encoded = Vec::new();
        account.encode(&mut encoded);

        let decoded = StateAccount::decode(&mut &encoded[..]).unwrap();
        assert_eq!(account, decoded);
    }
}
