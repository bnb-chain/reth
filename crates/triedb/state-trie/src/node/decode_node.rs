//!
//! Decoding utilities for trie nodes.
//!
//! This module provides `decode_node` and `must_decode_node`, translating the
//! logic of go-ethereum's trie node decoding into Rust.

use std::sync::Arc;
use alloy_primitives::B256;
use alloy_rlp::{Error as RlpError, Header, PayloadView};

use crate::node::{FullNode, Node, ShortNode};

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

    let mut view_buf = buf;
    let header_view = Header::decode_raw(&mut view_buf)?;
    let element_count = match header_view {
        PayloadView::List(list) => list.len(),
        _ => {
            return Err(RlpError::Custom("Node must be RLP list"));
        }
    };

    match element_count {
        2 => {
            // ShortNode has 2 elements: [key, val]
            let short_node = ShortNode::from_rlp(buf, hash)?;
            Ok(Arc::new(Node::Short(Arc::new(short_node))))
        },
        17 => {
            // FullNode has 17 elements: [child0, child1, ..., child15, value]
            let full_node = FullNode::from_rlp(buf, hash)?;
            Ok(Arc::new(Node::Full(Arc::new(full_node))))
        },
        _ => {
            Err(RlpError::UnexpectedList)
        }
    }
}

/// Must decode node - panics on error
pub fn must_decode_node(hash: Option<B256>, buf: &[u8]) -> Arc<Node> {
    decode_node(hash, buf).unwrap_or_else(|e| {
        panic!("Failed to decode node: {:?}", e);
    })
}
