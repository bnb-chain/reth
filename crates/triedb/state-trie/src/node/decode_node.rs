use std::sync::Arc;
use alloy_primitives::B256;
use alloy_rlp::{Error as RlpError, Header};

use crate::node::{FullNode, Node, ShortNode};

/// Error types for node decoding
#[derive(Debug)]
pub enum DecodeError {
    /// RLP decoding error
    Rlp(RlpError),
    /// Invalid RLP format with description
    InvalidRlp(String),
    /// Invalid number of elements in RLP list
    InvalidElementCount(usize),
    /// Unexpected end of input
    UnexpectedEof,
    /// Error decoding ShortNode
    Short(Box<DecodeError>),
    /// Error decoding FullNode
    Full(Box<DecodeError>),
    /// Embedded node exceeds size limit
    OversizedEmbeddedNode(usize),
    /// Invalid RLP string size
    InvalidRlpStringSize(usize),
}

impl From<RlpError> for DecodeError {
    fn from(err: RlpError) -> Self {
        DecodeError::Rlp(err)
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Rlp(e) => write!(f, "RLP error: {}", e),
            DecodeError::InvalidRlp(s) => write!(f, "invalid RLP: {}", s),
            DecodeError::InvalidElementCount(c) => write!(f, "invalid number of list elements: {}", c),
            DecodeError::UnexpectedEof => write!(f, "unexpected EOF"),
            DecodeError::Short(e) => write!(f, "short node error: {}", e),
            DecodeError::Full(e) => write!(f, "full node error: {}", e),
            DecodeError::OversizedEmbeddedNode(size) => write!(f, "oversized embedded node: {} bytes", size),
            DecodeError::InvalidRlpStringSize(size) => write!(f, "invalid RLP string size: {}", size),
        }
    }
}

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
pub fn decode_node(hash: Option<B256>, buf: &[u8]) -> Result<Arc<Node>, DecodeError> {
    if buf.is_empty() {
        return Err(DecodeError::UnexpectedEof);
    }

    // First, peek at the RLP header to determine the type
    let mut decoder_peek = buf;
    let header = Header::decode(&mut decoder_peek).map_err(DecodeError::Rlp)?;

    if !header.list {
        return Err(DecodeError::InvalidRlp("Node must be RLP list".to_string()));
    }

    // Determine node type based on list length
    // We need to count the number of elements in the list
    let element_count = count_rlp_list_elements(&buf[header.length()..], header.payload_length)?;

    match element_count {
        2 => {
            // ShortNode has 2 elements: [key, val]
            let short_node = ShortNode::from_rlp(buf, hash)
                .map_err(|e| DecodeError::InvalidRlp(format!("Failed to decode ShortNode: {:?}", e)))?;
            Ok(Arc::new(Node::Short(Arc::new(short_node))))
        },
        17 => {
            // FullNode has 17 elements: [child0, child1, ..., child15, value]
            let full_node = FullNode::from_rlp(buf, hash)
                .map_err(|e| DecodeError::InvalidRlp(format!("Failed to decode FullNode: {:?}", e)))?;
            Ok(Arc::new(Node::Full(Arc::new(full_node))))
        },
        _ => {
            Err(DecodeError::InvalidRlp(format!(
                "Invalid node type: list with {} elements (expected 2 for ShortNode or 17 for FullNode)",
                element_count
            )))
        }
    }
}

/// Count the number of elements in an RLP list payload
fn count_rlp_list_elements(payload: &[u8], payload_length: usize) -> Result<usize, DecodeError> {
    if payload.len() < payload_length {
        return Err(DecodeError::UnexpectedEof);
    }

    let mut count = 0;
    let mut remaining = &payload[..payload_length];

    while !remaining.is_empty() {
        let header = Header::decode(&mut remaining).map_err(DecodeError::Rlp)?;

        // Skip the payload for this element
        if remaining.len() < header.payload_length {
            return Err(DecodeError::UnexpectedEof);
        }
        remaining = &remaining[header.payload_length..];
        count += 1;
    }

    Ok(count)
}



/// Must decode node - panics on error
pub fn must_decode_node(hash: Option<B256>, buf: &[u8]) -> Arc<Node> {
    decode_node(hash, buf).unwrap_or_else(|e| {
        panic!("Failed to decode node: {:?}", e);
    })
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp;

    #[test]
    fn test_short_node_roundtrip() {
        // Test ShortNode's own Encodable/Decodable directly
        use crate::node::{ShortNode, Node};


        // Create a leaf node (key with terminator)
        let key = vec![1, 2, 16]; // Direct byte array
        let value = Arc::new(Node::Value(b"test_value".to_vec()));
        let original_short_node = ShortNode::new(key, value.as_ref());

        // Encode it to RLP
        let rlp_data = alloy_rlp::encode(&original_short_node);

        // Try to decode it back using ShortNode::from_rlp
        let decoded_short_node = ShortNode::from_rlp(&rlp_data, None).expect("Should decode successfully");

        // Verify it's the same
        assert_eq!(decoded_short_node.key.to_vec(), original_short_node.key.to_vec());
        match decoded_short_node.val.as_ref() {
            Node::Value(v) => assert_eq!(v, b"test_value"),
            _ => panic!("Expected value node"),
        }
    }

    #[test]
    fn test_decode_short_node_via_decode_node() {
        // Test using our decode_node function
        use crate::node::{ShortNode, Node};


        // Create a leaf node (key with terminator)
        let key = vec![1, 2, 16]; // Direct byte array
        let value = Arc::new(Node::Value(b"test_value".to_vec()));
        let short_node = ShortNode::new(key, value.as_ref());

        // Encode it to RLP
        let rlp_data = alloy_rlp::encode(&short_node);

        // Decode it back using our decode_node function
        let decoded = must_decode_node(None, &rlp_data);

        // Verify it's the same
        match decoded.as_ref() {
            Node::Short(decoded_short) => {
                assert_eq!(decoded_short.key.to_vec(), short_node.key.to_vec());
                match decoded_short.val.as_ref() {
                    Node::Value(v) => assert_eq!(v, b"test_value"),
                    _ => panic!("Expected value node"),
                }
            }
            _ => panic!("Expected short node"),
        }
    }

    #[test]
    fn test_decode_full_node_basic() {
        // Create a simple full node using our existing FullNode
        use crate::node::{FullNode, Node};

        let mut full_node = FullNode::new();

        // Add a hash node at position 0
        let hash_value = [1u8; 32];
        full_node.children[0] = Arc::new(Node::Hash(hash_value.into()));

        // Add a value at position 16
        full_node.children[16] = Arc::new(Node::Value(b"test_value".to_vec()));

        // Encode it to RLP
        let rlp_data = alloy_rlp::encode(&full_node);

        // Decode it back
        let decoded = must_decode_node(None, &rlp_data);

        // Verify it's the same
        match decoded.as_ref() {
            Node::Full(decoded_full) => {
                match decoded_full.children[0].as_ref() {
                    Node::Hash(h) => assert_eq!(h.as_slice(), &hash_value),
                    _ => panic!("Expected hash node at position 0"),
                }
                match decoded_full.children[16].as_ref() {
                    Node::Value(v) => assert_eq!(v, b"test_value"),
                    _ => panic!("Expected value node at position 16"),
                }
            }
            _ => panic!("Expected full node"),
        }
    }

    #[test]
    fn test_decode_invalid_element_count() {
        // Create a node with 3 elements (invalid - only 2 and 17 are valid)
        let invalid_node = vec![vec![1u8, 2], vec![3u8, 4], vec![5u8, 6]];
        let rlp_data = alloy_rlp::encode(&invalid_node);

        let result = std::panic::catch_unwind(|| {
            must_decode_node(None, &rlp_data)
        });
        assert!(result.is_err()); // Should panic for invalid element count
    }
}
