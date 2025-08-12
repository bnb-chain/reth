//! Short node implementation for trie operations.
//!
//! A short node represents an extension or leaf node with a key and value,
//! used to optimize storage for paths with common prefixes.


use std::sync::Arc;
use alloy_rlp::{Decodable, Encodable, Header, PayloadView, Error as RlpError, };
use alloy_primitives::{keccak256, B256};
use crate::node::decode_node::write_bytes;
use super::{Node, NodeFlag, HashNode, decode_node::decode_node};


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
                let mut val_buf = Vec::new();
                full_node.encode(&mut val_buf);
                write_bytes(&mut temp_buf, &val_buf.as_slice());
            }
            Node::Short(short_node) => {
                // Short nodes encoded as a list of 2 elements
                let mut val_buf = Vec::new();
                short_node.encode(&mut val_buf);
                write_bytes(&mut temp_buf, &val_buf.as_slice());
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
        let payload_view = Header::decode_raw(buf)?;

        let PayloadView::List(mut items) = payload_view else {
            return Err(RlpError::Custom("ShortNode must be a list"));
        };

        if items.len() != 2 {
            return Err(RlpError::Custom("ShortNode must have 2 elements"));
        }

        let mut short_node = ShortNode::new(Vec::new(), &Node::EmptyRoot);

        // Decode key (first element)
        let compact_key = Vec::<u8>::decode(&mut items[0])?;
        let key = crate::encoding::compact_to_hex(&compact_key);
        let has_terminator = crate::encoding::has_term(&key);
        short_node.key = key;

        // Decode value (second element)
        let mut temp_item = items[1];
        let child_view = Header::decode_raw(&mut temp_item)?;

        match child_view {
            PayloadView::String(val) => {
                if has_terminator {
                    // If key has terminator, value is a value node
                    short_node.val = Arc::new(Node::Value(val.to_vec()));
                } else if val == &[alloy_rlp::EMPTY_STRING_CODE] {
                    // Empty string indicates empty root
                    println!("ShortNode val is empty string - this is unexpected and should be investigated");
                    short_node.val = Arc::new(Node::EmptyRoot);
                } else if val.len() == 32 {
                    // 32-byte value indicates hash node
                    short_node.val = Arc::new(Node::Hash(B256::from_slice(val)));
                } else {
                    // Unexpected hash node length
                    println!("ShortNode val contains less than 32 bytes hash node - this is unexpected and should be investigated");
                    short_node.val = Arc::new(Node::Hash(B256::from_slice(val)));
                }
            }
            PayloadView::List(_) => {
                // List value indicates complex node structure
                let node = decode_node(None, &mut items[1])?;
                short_node.val = node.clone();
            }
        }

        Ok(short_node)
    }
}
