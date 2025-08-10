//! Full node implementation for trie operations.
//!
//! A full node contains 17 children (16 hex digits + value) and is used
//! when a trie path has multiple branches.

use std::sync::Arc;
use alloy_rlp::{Decodable, Encodable, Error as RlpError, Header};
use alloy_primitives::{keccak256, B256};

use crate::node::HashNode;

use super::{Node, NodeFlag};

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
        return alloy_rlp::encode(self);
    }

    /// Decode the node from RLP bytes
    pub fn from_rlp(buf: &[u8], hash: Option<B256>) -> Result<Self, RlpError> {
        let mut node: Self = alloy_rlp::decode_exact(buf)?;
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

        // Encode children 0-15 (hash nodes or empty)
        for i in 0..16 {
            match self.children[i].as_ref() {
                Node::Hash(hash_node) => {
                    // Hash nodes: w.WriteBytes(node) - encode as RLP string
                    hash_node.as_slice().encode(&mut temp_buf);
                }
                Node::EmptyRoot => {
                    // EmptyRoot: w.Write(rlp.EmptyString) - encode as RLP empty string
                    // Directly write 0x80 (RLP empty string) instead of calling encode()
                    temp_buf.push(0x80);
                }
                Node::Value(_) => {
                    panic!("FullNode children[{}] cannot be ValueNode - only Hash or EmptyRoot allowed", i);
                }
                Node::Full(_) | Node::Short(_) => {
                    panic!("FullNode children[{}] cannot be Full or Short node - only Hash or EmptyRoot allowed", i);
                }
            }
        }

        // Encode child 16 (value position)
        match self.children[16].as_ref() {
            Node::Value(value_node) => {
                // Value nodes: w.WriteBytes(node) - encode as RLP string
                value_node.as_slice().encode(&mut temp_buf);
            }
            Node::EmptyRoot => {
                // EmptyRoot: w.Write(rlp.EmptyString) - encode as RLP empty string
                // Directly write 0x80 (RLP empty string) instead of calling encode()
                temp_buf.push(0x80);
            }
            Node::Hash(_) => {
                panic!("FullNode children[16] cannot be HashNode - only Value or EmptyRoot allowed");
            }
            Node::Full(_) | Node::Short(_) => {
                panic!("FullNode children[16] cannot be Full or Short node - only Value or EmptyRoot allowed");
            }
        }

        let payload_len = temp_buf.len();

        // Write the main list header using alloy_rlp's Header
        Header { list: true, payload_length: payload_len }.encode(out);

        // Write the encoded content
        out.put_slice(&temp_buf);
    }

    fn length(&self) -> usize {
        // Calculate the same length as in encode()
        let mut payload_len = 0;

        // Calculate children 0-15 length
        for i in 0..16 {
            match self.children[i].as_ref() {
                Node::Hash(hash_node) => {
                    // Hash nodes: encode as RLP string
                    payload_len += hash_node.as_slice().length();
                }
                Node::EmptyRoot => {
                    // EmptyRoot: RLP empty string (1 byte)
                    payload_len += 1; // alloy_rlp::EMPTY_STRING_CODE
                }
                Node::Value(_) => {
                    panic!("FullNode children[{}] cannot be ValueNode - only Hash or EmptyRoot allowed", i);
                }
                Node::Full(_) | Node::Short(_) => {
                    panic!("FullNode children[{}] cannot be Full or Short node - only Hash or EmptyRoot allowed", i);
                }
            }
        }

        // Calculate child 16 (value position) length
        match self.children[16].as_ref() {
            Node::Value(value_node) => {
                // Value nodes: encode as RLP string
                payload_len += value_node.as_slice().length();
            }
            Node::EmptyRoot => {
                // EmptyRoot: RLP empty string (1 byte)
                payload_len += 1; // alloy_rlp::EMPTY_STRING_CODE
            }
            Node::Hash(_) => {
                panic!("FullNode children[16] cannot be HashNode - only Value or EmptyRoot allowed");
            }
            Node::Full(_) | Node::Short(_) => {
                panic!("FullNode children[16] cannot be Full or Short node - only Value or EmptyRoot allowed");
            }
        }

        // Add RLP list header length using alloy_rlp's length_of_length
        alloy_rlp::length_of_length(payload_len) + payload_len
    }
}

impl Decodable for FullNode {
    fn decode(buf: &mut &[u8]) -> Result<Self, RlpError> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(RlpError::UnexpectedString);
        }

        let started_len = buf.len();
        let mut children: [Arc<Node>; 17] = std::array::from_fn(|_| Arc::new(Node::EmptyRoot));

        // Decode children 0-15 (can be hashNode or EmptyRoot)
        for i in 0..16 {
            if buf.is_empty() {
                return Err(RlpError::Custom("FullNode must have 17 elements"));
            }

            let child_header = Header::decode(buf)?;


            if child_header.list {
                return Err(RlpError::Custom("FullNode children cannot be lists"));
            }

            if child_header.payload_length == 0 {
                // Empty string -> EmptyRoot
                children[i] = Arc::new(Node::EmptyRoot);
            } else if child_header.payload_length == 32 {
                // 32 bytes -> HashNode
                let hash_bytes = buf[..32].to_vec();
                *buf = &buf[32..];
                let mut hash_array = [0u8; 32];
                hash_array.copy_from_slice(&hash_bytes);
                children[i] = Arc::new(Node::Hash(hash_array.into()));
            } else {
                return Err(RlpError::Custom("FullNode children must be either empty (EmptyRoot) or 32 bytes (HashNode)"));
            }
        }

        // Decode child 16 (can be valueNode or EmptyRoot)
        if buf.is_empty() {
            return Err(RlpError::Custom("FullNode must have 17 elements"));
        }

        let val_header = Header::decode(buf)?;
        if val_header.list {
            return Err(RlpError::Custom("FullNode children[16] cannot be a list"));
        }

        if val_header.payload_length == 0 {
            // Empty string -> EmptyRoot
            children[16] = Arc::new(Node::EmptyRoot);
        } else {
            // Non-empty string -> ValueNode
            let val_bytes = buf[..val_header.payload_length].to_vec();
            *buf = &buf[val_header.payload_length..];
            children[16] = Arc::new(Node::Value(val_bytes));
        }

        // Verify we consumed the expected amount
        let consumed = started_len - buf.len();
        if consumed != header.payload_length {
            return Err(RlpError::Custom("FullNode RLP length mismatch"));
        }

        Ok(FullNode {
            children,
            flags: NodeFlag::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use alloy_primitives::keccak256;

    // Helper function to create a hash node with specific pattern
    fn create_hash_node(index: usize, seed: usize) -> Arc<Node> {
        let mut hash = [0u8; 32];
        for j in 0..32 {
            hash[j] = ((index * 16 + j + seed) % 256) as u8;
        }
        Arc::new(Node::Hash(hash.into()))
    }

    // Helper function to create a value node with specific pattern
    fn create_value_node(length: usize, seed: usize) -> Arc<Node> {
        let mut value = vec![0u8; length];
        for i in 0..length {
            value[i] = ((i + length + seed) % 256) as u8;
        }
        Arc::new(Node::Value(value))
    }

    #[test]
    fn test_scenario1_all_children_with_value() {
        println!("=== Rust FullNode Scenario 1: All 16 Children + ValueNode ===");

        let value_lengths = [1, 16, 128, 256, 512, 1024, 10 * 1024, 100 * 1024];

        for &value_len in &value_lengths {
            println!("\n--- Value length: {} bytes ---", value_len);

            let mut full_node = FullNode::new();

            // Set all 16 children (indices 0-15) as HashNodes
            for i in 0..16 {
                full_node.children[i] = create_hash_node(i, value_len);
            }

            // Set child 16 as ValueNode
            full_node.children[16] = create_value_node(value_len, value_len);

            // Encode using RLP
            let encoded = alloy_rlp::encode(&full_node);
            let hash = keccak256(&encoded);

            println!("Encoded size: {} bytes", encoded.len());
            println!("Hash: {}", hex::encode(hash));
            if encoded.len() <= 100 {
                println!("Encoded: {}", hex::encode(&encoded));
            } else {
                println!("Encoded (first 32 bytes): {}...", hex::encode(&encoded[..32]));
            }

            // Decode and verify
            let decoded: FullNode = alloy_rlp::Decodable::decode(&mut encoded.as_slice()).expect("Failed to decode FullNode");

            // Verify all 16 children are HashNodes
            for i in 0..16 {
                match (full_node.get_child(i).as_ref(), decoded.get_child(i).as_ref()) {
                    (Node::Hash(orig), Node::Hash(dec)) => {
                        assert_eq!(orig, dec, "HashNode at index {} mismatch", i);
                    }
                    _ => panic!("Expected HashNode at index {}", i),
                }
            }

            // Verify child 16 is ValueNode
            match (full_node.get_child(16).as_ref(), decoded.get_child(16).as_ref()) {
                (Node::Value(orig), Node::Value(dec)) => {
                    assert_eq!(orig, dec, "ValueNode at index 16 mismatch");
                }
                _ => panic!("Expected ValueNode at index 16"),
            }

            println!("✅ Decode verification: All children match exactly");
            println!("✅ Value length {} bytes: Encoding/decoding successful", value_len);
        }
    }

    #[test]
    fn test_scenario2_specific_children_with_value() {
        println!("=== Rust FullNode Scenario 2: Children 1,3,5,7 + ValueNode ===");

        let value_lengths = [1, 16, 128, 256, 512, 1024, 10 * 1024, 100 * 1024];

        for &value_len in &value_lengths {
            println!("\n--- Value length: {} bytes ---", value_len);

            let mut full_node = FullNode::new();

            // Set children at indices 1, 3, 5, 7 as HashNodes
            let child_indices = [1, 3, 5, 7];
            for &i in &child_indices {
                full_node.children[i] = create_hash_node(i, value_len);
            }

            // Set child 16 as ValueNode
            full_node.children[16] = create_value_node(value_len, value_len + 100);

            // Encode using RLP
            let encoded = alloy_rlp::encode(&full_node);
            let hash = keccak256(&encoded);

            println!("Encoded size: {} bytes", encoded.len());
            println!("Hash: {}", hex::encode(hash));
            if encoded.len() <= 100 {
                println!("Encoded: {}", hex::encode(&encoded));
            } else {
                println!("Encoded (first 32 bytes): {}...", hex::encode(&encoded[..32]));
            }

            // Decode and verify
            let decoded: FullNode = alloy_rlp::Decodable::decode(&mut encoded.as_slice()).expect("Failed to decode FullNode");

            // Verify specified children are HashNodes, others are EmptyRoot
            for i in 0..16 {
                if child_indices.contains(&i) {
                    match (full_node.get_child(i).as_ref(), decoded.get_child(i).as_ref()) {
                        (Node::Hash(orig), Node::Hash(dec)) => {
                            assert_eq!(orig, dec, "HashNode at index {} mismatch", i);
                        }
                        _ => panic!("Expected HashNode at index {}", i),
                    }
                } else {
                    match (full_node.get_child(i).as_ref(), decoded.get_child(i).as_ref()) {
                        (Node::EmptyRoot, Node::EmptyRoot) => {
                            // Expected
                        }
                        _ => panic!("Expected EmptyRoot at index {}", i),
                    }
                }
            }

            // Verify child 16 is ValueNode
            match (full_node.get_child(16).as_ref(), decoded.get_child(16).as_ref()) {
                (Node::Value(orig), Node::Value(dec)) => {
                    assert_eq!(orig, dec, "ValueNode at index 16 mismatch");
                }
                _ => panic!("Expected ValueNode at index 16"),
            }

            println!("✅ Decode verification: All children match exactly");
            println!("✅ Value length {} bytes: Encoding/decoding successful", value_len);
        }
    }

    #[test]
    fn test_scenario3_specific_children_no_value() {
        println!("=== Rust FullNode Scenario 3: Children 2,4,6,8 + No ValueNode ===");

        let value_lengths = [1, 16, 128, 256, 512, 1024, 10 * 1024, 100 * 1024];

        for &value_len in &value_lengths {
            println!("\n--- Reference value length: {} bytes ---", value_len);

            let mut full_node = FullNode::new();

            // Set children at indices 2, 4, 6, 8 as HashNodes
            let child_indices = [2, 4, 6, 8];
            for &i in &child_indices {
                full_node.children[i] = create_hash_node(i, value_len);
            }

            // Child 16 remains EmptyRoot (no value)

            // Encode using RLP
            let encoded = alloy_rlp::encode(&full_node);
            let hash = keccak256(&encoded);

            println!("Encoded size: {} bytes", encoded.len());
            println!("Hash: {}", hex::encode(hash));
            if encoded.len() <= 100 {
                println!("Encoded: {}", hex::encode(&encoded));
            } else {
                println!("Encoded (first 32 bytes): {}...", hex::encode(&encoded[..32]));
            }

            // Decode and verify
            let decoded: FullNode = alloy_rlp::Decodable::decode(&mut encoded.as_slice()).expect("Failed to decode FullNode");

            // Verify specified children are HashNodes, others are EmptyRoot
            for i in 0..16 {
                if child_indices.contains(&i) {
                    match (full_node.get_child(i).as_ref(), decoded.get_child(i).as_ref()) {
                        (Node::Hash(orig), Node::Hash(dec)) => {
                            assert_eq!(orig, dec, "HashNode at index {} mismatch", i);
                        }
                        _ => panic!("Expected HashNode at index {}", i),
                    }
                } else {
                    match (full_node.get_child(i).as_ref(), decoded.get_child(i).as_ref()) {
                        (Node::EmptyRoot, Node::EmptyRoot) => {
                            // Expected
                        }
                        _ => panic!("Expected EmptyRoot at index {}", i),
                    }
                }
            }

            // Verify child 16 is EmptyRoot
            match (full_node.get_child(16).as_ref(), decoded.get_child(16).as_ref()) {
                (Node::EmptyRoot, Node::EmptyRoot) => {
                    // Expected
                }
                _ => panic!("Expected EmptyRoot at index 16"),
            }

            println!("✅ Decode verification: All children match exactly");
            println!("✅ Reference value length {} bytes: Encoding/decoding successful", value_len);
        }
    }
}
