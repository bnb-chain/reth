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


/// Write bytes to the output buffer with RLP string header encoding
/// Similar to Go's encBuffer.writeBytes method
pub fn write_bytes(out: &mut dyn alloy_rlp::BufMut, b: &[u8]) {
    if b.len() == 1 && b[0] <= 0x7F {
        // fits single byte, no string header needed
        out.put_u8(b[0]);
    } else {
        // encode string header and then the bytes
        let mut temp_buf = Vec::new();
        encode_string_header(&mut temp_buf, b.len());
        temp_buf.extend_from_slice(b);
        out.put_slice(&temp_buf);
    }
}

/// Encode RLP string header for the given length
/// This follows the RLP encoding rules:
/// - If length < 56, use single byte: 0x80 + length
/// - If length >= 56, use: 0xB7 + length_of_length, followed by length bytes
fn encode_string_header(out: &mut Vec<u8>, length: usize) {
    if length < 56 {
        // Single byte header: 0x80 + length
        out.push(0x80 + (length as u8));
    } else {
        // Multi-byte header: 0xB7 + length_of_length, followed by length
        // Similar to Go's implementation: sizesize := putint(buf.sizebuf[1:], uint64(size))
        let mut size_buf = [0u8; 8]; // Buffer for size bytes
        let size_size = putint(&mut size_buf[1..], length as u64);

        // Set header byte: 0xB7 + size_size
        size_buf[0] = 0xB7 + size_size as u8;

        // Write header byte and size bytes
        out.push(size_buf[0]);
        out.extend_from_slice(&size_buf[1..size_size + 1]);
    }
}

/// Put integer into byte buffer (big-endian encoding)
/// Similar to Go's putint function
/// Returns the number of bytes written
pub fn putint(b: &mut [u8], i: u64) -> usize {
    match i {
        i if i < (1 << 8) => {
            b[0] = i as u8;
            1
        }
        i if i < (1 << 16) => {
            b[0] = (i >> 8) as u8;
            b[1] = i as u8;
            2
        }
        i if i < (1 << 24) => {
            b[0] = (i >> 16) as u8;
            b[1] = (i >> 8) as u8;
            b[2] = i as u8;
            3
        }
        i if i < (1 << 32) => {
            b[0] = (i >> 24) as u8;
            b[1] = (i >> 16) as u8;
            b[2] = (i >> 8) as u8;
            b[3] = i as u8;
            4
        }
        i if i < (1 << 40) => {
            b[0] = (i >> 32) as u8;
            b[1] = (i >> 24) as u8;
            b[2] = (i >> 16) as u8;
            b[3] = (i >> 8) as u8;
            b[4] = i as u8;
            5
        }
        i if i < (1 << 48) => {
            b[0] = (i >> 40) as u8;
            b[1] = (i >> 32) as u8;
            b[2] = (i >> 24) as u8;
            b[3] = (i >> 16) as u8;
            b[4] = (i >> 8) as u8;
            b[5] = i as u8;
            6
        }
        i if i < (1 << 56) => {
            b[0] = (i >> 48) as u8;
            b[1] = (i >> 40) as u8;
            b[2] = (i >> 32) as u8;
            b[3] = (i >> 24) as u8;
            b[4] = (i >> 16) as u8;
            b[5] = (i >> 8) as u8;
            b[6] = i as u8;
            7
        }
        _ => {
            b[0] = (i >> 56) as u8;
            b[1] = (i >> 48) as u8;
            b[2] = (i >> 40) as u8;
            b[3] = (i >> 32) as u8;
            b[4] = (i >> 24) as u8;
            b[5] = (i >> 16) as u8;
            b[6] = (i >> 8) as u8;
            b[7] = i as u8;
            8
        }
    }
}
