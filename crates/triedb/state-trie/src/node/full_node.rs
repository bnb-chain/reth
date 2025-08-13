//! Full node implementation for trie operations.
//!
//! A full node contains 17 children (16 hex digits + value) and is used
//! when a trie path has multiple branches.

use std::sync::Arc;

use alloy_rlp::{Decodable, Encodable, Header, Error as RlpError};
use alloy_primitives::{keccak256, B256};
use crate::node::rlp_raw::*;
use crate::node::{HashNode, Node, NodeFlag, decode_node::decode_ref};

/// Full node with 17 children (16 hex digits + value)
#[derive(Clone, Debug, PartialEq)]
pub struct FullNode {
    /// Array of 17 children (16 hex digits + value)
    pub children: [Arc<Node>; 17],
    /// Node flags for caching and dirty state
    pub flags: NodeFlag,
}

impl FullNode {
    /// Creates a new empty full node
    pub fn new() -> Self {
        Self {
            children: std::array::from_fn(|_| Arc::new(Node::EmptyRoot)),
            flags: NodeFlag::default(),
        }
    }

    /// Get the cached hash and dirty state
    pub fn cache(&self) -> (Option<HashNode>, bool) {
        (self.flags.hash, self.flags.dirty)
    }

    /// Creates a mutable copy with write-on-copy semantics
    ///
    /// This method creates an independent copy where children will be cloned
    /// only when they need to be modified (write-on-copy).
    pub fn to_mutable_copy_with_cow(&self) -> Self {
        Self {
            children: self.children.clone(), // 初始共享，写时复制
            flags: self.flags.clone(),
        }
    }

    /// Sets a child at the specified index with write-on-copy semantics
    ///
    /// This method ensures that the child is set without affecting other references.
    pub fn set_child(&mut self, index: usize, new_node: &Node) {
        self.children[index] = Arc::new(new_node.clone());
    }

    /// Gets a mutable reference to the flags
    pub fn get_flags_mut(&mut self) -> &mut NodeFlag {
        &mut self.flags
    }

    /// Gets a reference to the child at the specified index
    pub fn get_child(&self, index: usize) -> Arc<Node> {
        Arc::clone(&self.children[index])
    }

    /// Compute hash as committed to in the MPT trie without memorizing.
    /// This method RLP encodes the node and computes its Keccak256 hash.
    pub fn trie_hash(&self) -> B256 {
        let encoded = alloy_rlp::encode(self);
        keccak256(&encoded)
    }

    /// RLP encode the node
    pub fn to_rlp(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.encode(&mut buf);
        return buf;
    }

    /// Decode the node from RLP bytes
    pub fn from_rlp(buf: &[u8], hash: Option<B256>) -> Result<Self, RlpError> {
        let mut temp_buf = buf;
        let mut node: Self = FullNode::decode(&mut temp_buf)?;
        node.flags.hash = hash;
        node.flags.dirty = false;
        Ok(node)
    }
}

// RLP encoding and decoding implementations for FullNode
// Based on BSC Go implementation: fullNode encodes as [child0, child1, ..., child15, value]
// where children 0-15 can be hashNode or EmptyRoot, and child 16 can be valueNode or EmptyRoot
impl Encodable for FullNode {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        // Encode as a list with 17 elements: [children[0], children[1], ..., children[15], children[16]]
        // This matches BSC Go's implementation: w.List(), w.WriteBytes() for each child, w.ListEnd()

        // First, encode all children into a temporary buffer to calculate total payload length
        let mut temp_buf = Vec::new();

        // Encode all children nodes (0-16)
        for (_, child) in self.children.iter().enumerate() {
            match child.as_ref() {
                Node::EmptyRoot => {
                    // Empty root encoded as empty string [0x80]
                    write_bytes(&mut temp_buf, &Vec::new());
                }
                Node::Full(full_node) => {
                    // Full nodes encoded as a list of 17 elements
                    full_node.encode(&mut temp_buf);
                }
                Node::Short(short_node) => {
                    // Short nodes encoded as a list of 2 elements
                    short_node.encode(&mut temp_buf);
                }
                Node::Hash(hash_node) => {
                    // Hash nodes encoded as byte strings
                    write_bytes(&mut temp_buf, &hash_node.as_slice());
                }
                Node::Value(value_node) => {
                    // Value nodes encoded as byte strings
                    write_bytes(&mut temp_buf, &value_node.as_slice());
                }
            }
        }

        let payload_len = temp_buf.len();

        // Write the main list header using alloy_rlp's Header
        Header { list: true, payload_length: payload_len }.encode(out);

        // Write the encoded content
        out.put_slice(&temp_buf);
    }
}

impl Decodable for FullNode {
    fn decode(buf: &mut &[u8]) -> Result<Self, RlpError> {
        let mut full_node = FullNode::new();
        for i in 0..16 {
            let (child, reset) = decode_ref(buf)?;
            full_node.children[i] = child;
            *buf = reset;
        }
        let(val, _) = split_string(buf)
            .map_err(|_| RlpError::Custom("Split string failed"))?;
        if !val.is_empty() {
            full_node.children[16] = Arc::new(Node::Value(val.to_vec()));
        }
        Ok(full_node)
    }
}
