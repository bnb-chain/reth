//! Short node implementation for trie operations.
//!
//! A short node represents an extension or leaf node with a key and value,
//! used to optimize storage for paths with common prefixes.


use std::sync::Arc;
use alloy_rlp::{Decodable, Encodable, Header, Error as RlpError, };
use alloy_primitives::{keccak256, B256};
use crate::encoding::*;
use crate::node::rlp_raw::*;
use super::{Node, NodeFlag, HashNode, decode_node::decode_ref};


/// Short node (extension or leaf)
#[derive(Debug, Clone, PartialEq)]
pub struct ShortNode {
    /// Key bytes for the short node
    pub key: Vec<u8>,
    /// Value node
    pub val: Arc<Node>,
    /// Node flags for caching and dirty state
    pub flags: NodeFlag,
}

impl ShortNode {
    /// Creates a new short node with the given key and value
    ///
    /// The ownership of val is not transferred and still belongs to the caller
    ///
    /// val.clone() shallow copies the FullNode or ShortNode, not the entire Node tree
    /// and copies the HashNode and ValueNode data.
    pub fn new(key: Vec<u8>, val: &Node) -> Self {
        Self {
            key,
            val: Arc::new(val.clone()),
            flags: NodeFlag::default(),
        }
    }

    /// Get the cached hash and dirty state
    pub fn cache(&self) -> (Option<HashNode>, bool) {
        (self.flags.hash, self.flags.dirty)
    }


    /// Creates a mutable copy with write-on-copy semantics for val
    ///
    /// This method creates an independent copy where val will be cloned
    /// only when it needs to be modified (write-on-copy).
    pub fn to_mutable_copy_with_cow(&self) -> Self {
        Self {
            key: self.key.clone(),
            val: self.val.clone(),
            flags: self.flags.clone(),
        }
    }

    /// Sets a value at the specified index with write-on-copy semantics
    ///
    /// This method ensures that the child is set without affecting other references.
    pub fn set_value(&mut self, new_node: &Node) {
        self.val = Arc::new(new_node.clone());
    }

    /// Gets a reference to the value node
    pub fn get_value(&self) -> &Node {
        &self.val
    }

    /// Gets a mutable reference to the flags
    pub fn get_flags_mut(&mut self) -> &mut NodeFlag {
        &mut self.flags
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
        let mut node: Self = ShortNode::decode(&mut temp_buf)?;
        node.flags.hash = hash;
        node.flags.dirty = false;
        Ok(node)
    }
}

// RLP encoding and decoding implementations for ShortNode
// Based on BSC Go implementation: shortNode encodes as [key, val]
// where key is written as bytes and val is recursively encoded
impl Encodable for ShortNode {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        // Encode as a list with 2 elements: [key, val]

        // First, encode both elements into a temporary buffer to calculate total payload length
        let mut temp_buf = Vec::new();

        // Encode compact key
        // let compact_key = crate::encoding::hex_to_compact(&self.key);
        // compact_key.encode(&mut temp_buf);
        // self.key.encode(&mut temp_buf);
        write_bytes(&mut temp_buf, &self.key);

        // Encode value based on node type
        match self.val.as_ref() {
            Node::EmptyRoot => {
                // Empty root encoded as empty string [0x80]
                temp_buf.push(alloy_rlp::EMPTY_STRING_CODE);
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

        let payload_len = temp_buf.len();

        // Write the main list header
        Header { list: true, payload_length: payload_len }.encode(out);

        // Write the encoded content
        out.put_slice(&temp_buf);
    }
}

impl Decodable for ShortNode {
    fn decode(buf: &mut &[u8]) -> Result<Self, RlpError> {
        let (key_buf, value_buf) = split_string(buf)
            .map_err(|_| RlpError::Custom("Split list failed"))?;

        let key = compact_to_hex(&key_buf);
        if has_terminator(&key) {
            let (val, _) = split_string(value_buf)
                .map_err(|_| RlpError::Custom("Split string failed"))?;
            return Ok(ShortNode::new(key, &Node::Value(val.to_vec())));
        }

        let (val, _) = decode_ref(value_buf)?;
        return Ok(ShortNode::new(key, &val));
    }
}
