//!
//! Decoding utilities for trie nodes.
//!
//! This module provides `decode_node` and `must_decode_node`, translating the
//! logic of go-ethereum's trie node decoding into Rust.

use std::sync::Arc;
use alloy_primitives::B256;
use alloy_rlp::{Error as RlpError};

use crate::node::{FullNode, Node, ShortNode};
use crate::node::rlp_raw::*;

/// Decodes an RLP-encoded trie node.
///
/// This function handles the decoding of different node types:
/// - Short nodes (extension or leaf nodes)
/// - Full nodes (branch nodes with 17 children)
///
/// # Arguments
/// * `hash` - Optional hash of the node
/// * `buf` - RLP-encoded node data
///
/// # Returns
/// `Ok(Node)` on success, `Err(DecodeError)` on failure
pub fn decode_node(hash: Option<B256>, buf: &[u8]) -> Result<Arc<Node>, RlpError> {
    if buf.is_empty() {
        return Err(RlpError::InputTooShort);
    }

    let (elements, _) = split_list(buf)
        .map_err(|_| RlpError::Custom("Split list failed"))?;
    let element_count = count_values(elements)
        .map_err(|_| RlpError::Custom("Invalid elements count"))?;
    match element_count {
        2 => {
            let short_node = ShortNode::from_rlp(elements, hash)?;
            Ok(Arc::new(Node::Short(Arc::new(short_node))))
        }
        17 => {
            let full_node = FullNode::from_rlp(elements, hash)?;
            Ok(Arc::new(Node::Full(Arc::new(full_node))))
        }
        _ => {
            Err(RlpError::Custom("Invalid number of list elements"))
        }
    }
}

/// Decodes a reference to a node and returns the decoded node and the remaining bytes.
///
/// Mirrors go-ethereum's `decodeRef` logic:
/// - If the first item is a list, it's an embedded node. The total encoded size must be < 32 bytes
///   (the hash length). The node is decoded from the original buffer and the rest is returned.
/// - If it's an empty string, this is an empty node reference.
/// - If it's a 32-byte string, this is a hash node reference.
/// - Otherwise, returns an error.
pub fn decode_ref(buf: &[u8]) -> Result<(Arc<Node>, &[u8]), RlpError> {
    let (kind, val, rest) = split(buf).map_err(|_| RlpError::Custom("split failed"))?;

    match kind {
        // Embedded node; ensure it's smaller than hash length
        Kind::List => {
            let consumed = buf.len().saturating_sub(rest.len());
            const HASH_LEN: usize = 32; // B256 length
            if consumed > HASH_LEN {
                return Err(RlpError::Custom("oversized embedded node, wants 32 bytes, got more"));
            }
            let n = decode_node(None, buf)?;
            Ok((n, rest))
        }
        // Empty reference
        Kind::String if val.is_empty() => {
            Ok((Arc::new(Node::EmptyRoot), rest))
        }
        // Hash reference
        Kind::String if val.len() == 32 => {
            let hash = B256::from_slice(val);
            Ok((Arc::new(Node::Hash(hash)), rest))
        }
        // Invalid string length
        _ => Err(RlpError::Custom("invalid RLP string size, want 0 or 32 bytes")),
    }
}

/// Must decode node - panics on error
pub fn must_decode_node(hash: Option<B256>, buf: &[u8]) -> Arc<Node> {
    decode_node(hash, buf).unwrap_or_else(|e| {
        panic!("Failed to decode node: {:?}", e);
    })
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::ShortNode;
    use crate::encoding::*;

    #[test]
    fn fullnode_roundtrip_basic() {
        // Build a simple full node with only value (17th) set
        let mut original = FullNode::new();
        let value_bytes = vec![0xAA, 0xBB, 0xCC];
        original.set_child(16, &Node::Value(value_bytes.clone()));

        // Encode to RLP
        let encoded = original.to_rlp();

        // Decode via high-level decode_node
        let decoded = decode_node(None, &encoded).expect("decode_node should succeed");

        // Validate structure and important fields
        match decoded.as_ref() {
            Node::Full(full) => {
                // Children 0..=15 should be EmptyRoot by default
                for i in 0..16 {
                    assert!(matches!(full.get_child(i).as_ref(), Node::EmptyRoot));
                }
                // 17th child is value node and should match
                match full.get_child(16).as_ref() {
                    Node::Value(v) => assert_eq!(v, &value_bytes),
                    other => panic!("expected value node at index 16, got {:?}", other),
                }
            }
            other => panic!("expected full node, got {:?}", other),
        }
    }

    #[test]
    fn shortnode_roundtrip_basic() {
        // Build a short node: key is a nibble-terminated path, value is a byte string
        let key_bytes = vec![0x12, 0x34];
        let hex_key = key_to_nibbles(&key_bytes); // includes terminator
        let compact_key = hex_to_compact(&hex_key);

        let original = ShortNode::new(compact_key.clone(), &Node::Value(vec![0xDE, 0xAD, 0xBE, 0xEF]));

        // Encode to RLP
        let encoded = original.to_rlp();

        // Decode via high-level decode_node
        let decoded = decode_node(None, &encoded).expect("decode_node should succeed");

        // Validate it is a Short node with same key and value
        match decoded.as_ref() {
            Node::Short(short) => {
                assert_eq!(short.key, hex_key);
                match short.get_value() {
                    Node::Value(v) => assert_eq!(v, &vec![0xDE, 0xAD, 0xBE, 0xEF]),
                    other => panic!("expected value node, got {:?}", other),
                }
            }
            other => panic!("expected short node, got {:?}", other),
        }
    }

    #[test]
    fn nested_roundtrip_complex() {

    }

    #[test]
    fn fullnode_child1_short_with_1byte_value() {
        // Build leaf short with 1-byte value
        let hex_key = key_to_nibbles(&[0x0A]);
        let compact_key = hex_to_compact(&hex_key);
        let short = ShortNode::new(compact_key.clone(), &Node::Value(vec![0x01]));

        // Root full node with child1 = short
        let mut root = FullNode::new();
        root.set_child(1, &Node::Short(Arc::new(short)));

        let encoded = root.to_rlp();
        let decoded = decode_node(None, &encoded).expect("decode_node should succeed");

        match decoded.as_ref() {
            Node::Full(full) => {
                match full.get_child(1).as_ref() {
                    Node::Short(s) => {
                        assert_eq!(s.key, hex_key);
                        match s.get_value() {
                            Node::Value(v) => assert_eq!(v, &vec![0x01]),
                            other => panic!("expected 1-byte value at child1, got {:?}", other),
                        }
                    }
                    other => panic!("expected short at child1, got {:?}", other),
                }
            }
            other => panic!("expected full node root, got {:?}", other),
        }
    }

    #[test]
    fn shortnode_with_fullnode_value_with_1byte_in_17th_child() {
        // Inner full node with 17th (index 16) child = 1-byte value
        let mut inner_full = FullNode::new();
        inner_full.set_child(16, &Node::Value(vec![0x02]));

        // Extension short: key without terminator
        let mut dst = vec![0u8; 2];
        let hex_no_term = write_hex_key(&mut dst, &[0x0B]).to_vec();
        let compact_ext_key = hex_to_compact(&hex_no_term);

        let short = ShortNode::new(compact_ext_key.clone(), &Node::Full(Arc::new(inner_full)));

        let encoded = short.to_rlp();
        let decoded = decode_node(None, &encoded).expect("decode_node should succeed");

        match decoded.as_ref() {
            Node::Short(s) => {
                assert_eq!(s.key, hex_no_term);
                match s.get_value() {
                    Node::Full(f) => match f.get_child(16).as_ref() {
                        Node::Value(v) => assert_eq!(v, &vec![0x02]),
                        other => panic!("expected 1-byte value in 17th child, got {:?}", other),
                    },
                    other => panic!("expected full node as short value, got {:?}", other),
                }
            }
            other => panic!("expected short node root, got {:?}", other),
        }
    }
}
