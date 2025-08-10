//! Short node implementation for trie operations.
//!
//! A short node represents an extension or leaf node with a key and value,
//! used to optimize storage for paths with common prefixes.


use std::sync::Arc;
use alloy_rlp::{Decodable, Encodable, Error as RlpError, Header};
#[allow(unused_imports)]
use alloy_primitives::{keccak256, hex, B256};

use super::{Node, NodeFlag, HashNode};

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

// RLP encoding and decoding implementations for ShortNode
// Based on BSC Go implementation: shortNode encodes as [key, val]
// where key is written as bytes and val is recursively encoded
impl Encodable for ShortNode {
        fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        // Encode as a list with 2 elements: [key, val]
        // This matches BSC Go's implementation: w.List(), w.WriteBytes(key), val.encode(), w.ListEnd()

        // First, encode both elements into a temporary buffer to calculate total payload length
        let mut temp_buf = Vec::new();

        // Encode key using compact encoding (matching BSC's shortNode RLP encoding)
        // BSC uses hexToCompact to compress the key before RLP encoding
        let compact_key = crate::encoding::hex_to_compact(&self.key);
        compact_key.as_slice().encode(&mut temp_buf);

        // Encode val based on type (matching BSC's n.Val.encode(w))
        // Both hashNode and valueNode use w.WriteBytes() in BSC
        match self.val.as_ref() {
            Node::Hash(hash_node) => {
                // Hash nodes are encoded as byte strings (matching BSC's w.WriteBytes(n))
                hash_node.as_slice().encode(&mut temp_buf);
            }
            Node::Value(value_node) => {
                // Value nodes are encoded as byte strings (matching BSC's w.WriteBytes(n))
                value_node.as_slice().encode(&mut temp_buf);
            }
            Node::EmptyRoot => {
                panic!("ShortNode val cannot be EmptyRoot - not supported in this implementation");
            }
            Node::Full(_) | Node::Short(_) => {
                panic!("ShortNode val cannot be Full or Short node - not supported in this implementation");
            }
        }

        let payload_len = temp_buf.len();

        // Write the main list header
        Header { list: true, payload_length: payload_len }.encode(out);

        // Write the encoded content
        out.put_slice(&temp_buf);
    }

    fn length(&self) -> usize {
        let key_bytes = &self.key;
        let key_encoded_len = key_bytes.length();

        let val_encoded_len = match self.val.as_ref() {
            Node::Hash(hash_node) => hash_node.as_slice().length(),
            Node::Value(value_node) => value_node.as_slice().length(),
            Node::EmptyRoot => {
                panic!("ShortNode val cannot be EmptyRoot - not supported in this implementation");
            }
            Node::Full(_) | Node::Short(_) => {
                panic!("ShortNode val cannot be Full or Short node - not supported in this implementation");
            }
        };

        let payload_len = key_encoded_len + val_encoded_len;
        alloy_rlp::length_of_length(payload_len) + payload_len
    }
}

impl Decodable for ShortNode {
    fn decode(buf: &mut &[u8]) -> Result<Self, RlpError> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(RlpError::UnexpectedString);
        }

        let started_len = buf.len();

        // Decode compact key manually (alloy_rlp has issues with Vec<u8> decoding)
        let compact_key: Vec<u8> = {
            if buf.is_empty() {
                return Err(RlpError::InputTooShort);
            }

            let first_byte = buf[0];
            if first_byte >= 0x80 && first_byte <= 0xb7 {
                // Short string (0-55 bytes)
                let len = (first_byte - 0x80) as usize;
                if buf.len() < len + 1 {
                    return Err(RlpError::InputTooShort);
                }
                let data = buf[1..len+1].to_vec();
                *buf = &buf[len+1..];
                data
            } else if first_byte >= 0xb8 && first_byte <= 0xbf {
                // Long string (56+ bytes)
                let len_of_len = (first_byte - 0xb7) as usize;
                if buf.len() < len_of_len + 1 {
                    return Err(RlpError::InputTooShort);
                }

                // Read the length
                let mut len = 0usize;
                for i in 0..len_of_len {
                    len = (len << 8) | (buf[1 + i] as usize);
                }

                if buf.len() < len_of_len + 1 + len {
                    return Err(RlpError::InputTooShort);
                }

                let data = buf[len_of_len + 1..len_of_len + 1 + len].to_vec();
                *buf = &buf[len_of_len + 1 + len..];
                data
            } else if first_byte < 0x80 {
                // Single byte
                let data = vec![first_byte];
                *buf = &buf[1..];
                data
            } else {
                return Err(RlpError::Custom("Unsupported string format for compact key"));
            }
        };
        let key = crate::encoding::compact_to_hex(&compact_key);

        // Check if key has terminator (like BSC's hasTerm function)
        let has_terminator = crate::encoding::has_term(&key);

                // Decode val - could be either a string or a direct byte
        let val_bytes: Vec<u8> = {
            // Peek at the next byte to determine how to decode
            if buf.is_empty() {
                return Err(RlpError::InputTooShort);
            }

            let first_byte = buf[0];
            if first_byte < 0x80 {
                // Direct byte (0x00-0x7f) - consume it directly
                let byte = buf[0];
                *buf = &buf[1..];
                vec![byte]
            } else if first_byte >= 0x80 && first_byte <= 0xb7 {
                // Short string (0-55 bytes) - manual decode like compact key
                let len = (first_byte - 0x80) as usize;
                if buf.len() < len + 1 {
                    return Err(RlpError::InputTooShort);
                }
                let data = buf[1..len+1].to_vec();
                *buf = &buf[len+1..];
                data
            } else if first_byte >= 0xb8 && first_byte <= 0xbf {
                // Long string (56+ bytes)
                let len_of_len = (first_byte - 0xb7) as usize;
                if buf.len() < len_of_len + 1 {
                    return Err(RlpError::InputTooShort);
                }

                let mut len = 0usize;
                for i in 0..len_of_len {
                    len = (len << 8) | (buf[1 + i] as usize);
                }

                if buf.len() < 1 + len_of_len + len {
                    return Err(RlpError::InputTooShort);
                }

                let data = buf[1 + len_of_len..1 + len_of_len + len].to_vec();
                *buf = &buf[1 + len_of_len + len..];
                data
            } else {
                return Err(RlpError::Custom("Unsupported RLP format for value"));
            }
        };

        // Determine node type based on key terminator and value length
        let val = if val_bytes.is_empty() {
            return Err(RlpError::Custom("ShortNode val cannot be empty - EmptyRoot not supported"));
        } else if has_terminator {
            // Key has terminator -> this is a valueNode (regardless of length)
            Node::Value(val_bytes)
        } else if val_bytes.len() == 32 {
            // Key has no terminator and value is 32 bytes -> this is a hashNode
            let mut hash_array = [0u8; 32];
            hash_array.copy_from_slice(&val_bytes);
            Node::Hash(hash_array.into())
        } else {
            // Key has no terminator but value is not 32 bytes -> invalid in our implementation
            return Err(RlpError::Custom("ShortNode with non-terminator key must have 32-byte value (hashNode)"));
        };

        let consumed = started_len - buf.len();
        if consumed != header.payload_length {
            return Err(RlpError::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }

        Ok(ShortNode {
            key,
            val: Arc::new(val),
            flags: NodeFlag::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{key_to_nibbles, hex_to_compact};
    use alloy_primitives::{B256, keccak256, hex};

    /// Test ShortNode with HashNode values for different key lengths
    #[test]
    fn test_short_node_with_hash_values() {
        println!("=== Rust ShortNode with HashNode Values ===");

        // Test key lengths from 1 to 65 bytes (comprehensive coverage, same as BSC test)
        for hex_key_len in 1..=65 {
            println!("\n--- Testing hex key length: {} bytes ---", hex_key_len);

            // Create a fixed base value to hash (same as BSC)
            let base_value = b"test_key_for_bsc_short_node_comparison_12345";

            // Get hash of the base value
            let base_hash = keccak256(base_value);

            // Process hash using Rust's key_to_nibbles to get full hex key
            let full_hex_key = key_to_nibbles(base_hash.as_slice());

            // Truncate from the end to get desired length (same as BSC)
            // Take the last hex_key_len bytes from full_hex_key
            let mut hex_key = if hex_key_len >= full_hex_key.len() {
                full_hex_key.clone()
            } else {
                full_hex_key[full_hex_key.len() - hex_key_len..].to_vec()
            };

            // For HashNode tests, ensure key does NOT have terminator (extension node)
            if let Some(last) = hex_key.last_mut() {
                if *last == 16 {
                    *last = 15; // Change terminator to a valid nibble
                }
            }

            println!("Base hash: {}", hex::encode(&base_hash));
            println!("Full hex key ({} bytes): {:?}", full_hex_key.len(), full_hex_key);
            println!("Truncated hex key ({} bytes): {:?}", hex_key.len(), hex_key);

            // Verify hex key length matches expected
            assert_eq!(hex_key.len(), hex_key_len, "Hex key length should match expected");

            // Create hash value (32 bytes, same pattern as BSC)
            let mut hash_bytes = [0u8; 32];
            for i in 0..32 {
                hash_bytes[i] = ((0x80 + hex_key_len + i) % 256) as u8;
            }
            let hash_value = B256::from(hash_bytes);

            // Create ShortNode using Rust's implementation
            let short_node = ShortNode::new(hex_key, &Node::Hash(hash_value));

            // Encode using RLP
            let encoded = alloy_rlp::encode(&short_node);

            // Immediately decode and verify roundtrip
            let decoded: ShortNode = alloy_rlp::Decodable::decode(&mut encoded.as_slice())
                .expect("Failed to decode ShortNode");

            // Assert key matches exactly
            assert_eq!(short_node.key.len(), decoded.key.len(),
                "Key length mismatch: original={}, decoded={}", short_node.key.len(), decoded.key.len());
            assert_eq!(short_node.key, decoded.key,
                "Key content mismatch: original={:?}, decoded={:?}", short_node.key, decoded.key);

            // Assert value matches exactly
            match (&*short_node.val, &*decoded.val) {
                (Node::Hash(h1), Node::Hash(h2)) => {
                    assert_eq!(h1.len(), h2.len(), "Hash length should match");
                    assert_eq!(h1, h2, "Hash values should match exactly: original={}, decoded={}",
                        hex::encode(h1), hex::encode(h2));
                }
                _ => panic!("Expected hash nodes, got original={:?}, decoded={:?}",
                    &*short_node.val, &*decoded.val),
            }

            // Calculate hash after successful decode verification
            let hash = keccak256(&encoded);

            println!("ShortNode encoded size: {} bytes", encoded.len());
            println!("ShortNode encoded: {}", hex::encode(&encoded));
            println!("ShortNode hash: {}", hex::encode(hash));
            println!("✅ Decode verification: Key and Hash value match exactly");

            println!("✅ Hex key length {} bytes: Encoding/decoding successful", hex_key_len);
        }
    }

    /// Test ShortNode with ValueNode values for different key and value lengths
    #[test]
    fn test_short_node_with_value_nodes() {
        println!("=== Rust ShortNode with ValueNode Values ===");

        // Test key lengths from 1 to 65 bytes (comprehensive coverage, same as BSC test)
        // Value lengths to test: 1, 16, 32, 64, 128, 256, 512, 1024, 10K, 100K (same as BSC test)
        let value_lengths = vec![1, 16, 32, 64, 128, 256, 512, 1024, 10*1024, 100*1024];

        for hex_key_len in 1..=65 {
            println!("\n--- Testing hex key length: {} bytes with various value lengths ---", hex_key_len);

            // Create a fixed base value to hash (different pattern for value tests, same as BSC)
            let base_value = b"value_test_key_for_bsc_short_node_comparison_67890";

            // Get hash of the base value
            let base_hash = keccak256(base_value);

            // Process hash using Rust's key_to_nibbles to get full hex key
            let full_hex_key = key_to_nibbles(base_hash.as_slice());

            // Truncate from the end to get desired length (same as BSC)
            // Take the last hex_key_len bytes from full_hex_key
            let mut hex_key = if hex_key_len >= full_hex_key.len() {
                full_hex_key.clone()
            } else {
                full_hex_key[full_hex_key.len() - hex_key_len..].to_vec()
            };

            // For ValueNode tests, ensure key HAS terminator (leaf node)
            if let Some(last) = hex_key.last_mut() {
                if *last != 16 {
                    *last = 16; // Set terminator
                }
            }

            // Verify hex key length matches expected
            assert_eq!(hex_key.len(), hex_key_len, "Hex key length should match expected");

            for value_len in &value_lengths {
                println!("\n  Testing value length: {} bytes", value_len);

                // Create value data (same pattern as BSC)
                let value_data: Vec<u8> = (0..*value_len).map(|i| ((i + value_len + hex_key_len) % 256) as u8).collect();

                // Create ShortNode with value
                let short_node = ShortNode::new(hex_key.clone(), &Node::Value(value_data.clone()));

                // Encode using RLP
                let encoded = alloy_rlp::encode(&short_node);

                // Immediately decode and verify roundtrip
                let decoded: ShortNode = alloy_rlp::Decodable::decode(&mut encoded.as_slice())
                    .expect("Failed to decode ShortNode");

                // Assert key matches exactly
                assert_eq!(short_node.key.len(), decoded.key.len(),
                    "Key length mismatch: original={}, decoded={}", short_node.key.len(), decoded.key.len());
                assert_eq!(short_node.key, decoded.key,
                    "Key content mismatch: original={:?}, decoded={:?}", short_node.key, decoded.key);

                // Assert value matches exactly
                match (&*short_node.val, &*decoded.val) {
                    (Node::Value(v1), Node::Value(v2)) => {
                        assert_eq!(v1.len(), v2.len(),
                            "Value length mismatch: original={}, decoded={}", v1.len(), v2.len());
                        assert_eq!(v1, v2, "Value content should match exactly");

                        // Additional verification: compare with original value_data
                        assert_eq!(v1.len(), value_data.len(),
                            "Decoded value length should match original: decoded={}, original={}",
                            v1.len(), value_data.len());
                        assert_eq!(*v1, value_data, "Decoded value should match original exactly");

                        // Detailed byte-by-byte verification for large values
                        if !v1.is_empty() {
                            assert_eq!(v1[0], value_data[0],
                                "First byte mismatch: decoded={:02x}, original={:02x}", v1[0], value_data[0]);
                            if v1.len() > 1 {
                                assert_eq!(v1[v1.len()-1], value_data[value_data.len()-1],
                                    "Last byte mismatch: decoded={:02x}, original={:02x}",
                                    v1[v1.len()-1], value_data[value_data.len()-1]);
                            }
                            // For medium-sized values, check middle bytes too
                            if v1.len() > 10 {
                                let mid = v1.len() / 2;
                                assert_eq!(v1[mid], value_data[mid],
                                    "Middle byte mismatch at position {}: decoded={:02x}, original={:02x}",
                                    mid, v1[mid], value_data[mid]);
                            }
                        }
                    }
                    _ => panic!("Expected value nodes, got original={:?}, decoded={:?}",
                        &*short_node.val, &*decoded.val),
                }

                // Calculate hash after successful decode verification
                let hash = keccak256(&encoded);

                println!("    Hex key: {} bytes, Value: {} bytes", hex_key_len, value_len);
                println!("    Encoded size: {} bytes", encoded.len());
                if encoded.len() <= 200 { // Only show full encoding for smaller data
                    println!("    Encoded: {}", hex::encode(&encoded));
                } else {
                    println!("    Encoded (first 32 bytes): {}...", hex::encode(&encoded[..32]));
                }
                println!("    Hash: {}", hex::encode(hash));
                println!("    ✅ Decode verification: Key and Value match exactly");

                println!("    ✅ Hex key length {}, Value length {}: Success", hex_key_len, value_len);
            }
        }
    }

    /// Test that demonstrates the key processing pipeline
    #[test]
    fn test_key_processing_pipeline() {
        println!("=== Key Processing Pipeline Test ===");

        let test_cases = vec![
            ("Short key", vec![0x12, 0x34, 0x56]),
            ("Medium key", vec![0xab; 31]),
            ("Hash-size key", vec![0xcd; 32]),
            ("Long key", vec![0xef; 65]),
        ];

        for (name, raw_key) in test_cases {
            println!("\n--- {} ({} bytes) ---", name, raw_key.len());

            // Step 1: Raw key
            println!("1. Raw key: {}", hex::encode(&raw_key));

            // Step 2: Convert to nibbles (equivalent to BSC's keybytesToHex)
            let hex_key = key_to_nibbles(&raw_key);
            println!("2. Hex key: {:?} (length: {})", hex_key, hex_key.len());

            // Step 3: Convert to compact (what gets stored in RLP)
            let compact_key = hex_to_compact(&hex_key);
            println!("3. Compact key: {} (length: {})", hex::encode(&compact_key), compact_key.len());

            // Step 4: Create ShortNode and encode
            let short_node = ShortNode::new(hex_key.clone(), &Node::Value(b"test".to_vec()));
            let encoded = alloy_rlp::encode(&short_node);
            println!("4. RLP encoded: {} (length: {})", hex::encode(&encoded), encoded.len());

            // Step 4.5: Immediately decode and verify
            let decoded: ShortNode = alloy_rlp::Decodable::decode(&mut encoded.as_slice())
                .expect("Failed to decode ShortNode in pipeline test");

            // Verify key matches
            assert_eq!(short_node.key, decoded.key, "Pipeline test: Key should match after roundtrip");

            // Verify value matches
            match (&*short_node.val, &*decoded.val) {
                (Node::Value(v1), Node::Value(v2)) => {
                    assert_eq!(v1, v2, "Pipeline test: Value should match after roundtrip");
                    assert_eq!(*v1, b"test".to_vec(), "Pipeline test: Value should be 'test'");
                }
                _ => panic!("Pipeline test: Expected value nodes"),
            }
            println!("4.5. ✅ Decode verification successful");

            // Step 5: Verify the compact key is in the RLP
            // The RLP should start with a list header, then the compact key
            let mut pos = 0;

            // Skip list header
            if encoded[0] >= 0xc0 {
                if encoded[0] <= 0xf7 {
                    pos = 1; // Short list
                } else {
                    let len_of_len = (encoded[0] - 0xf7) as usize;
                    pos = 1 + len_of_len; // Long list
                }
            }

            // The next part should be the compact key
            if pos < encoded.len() {
                let key_header = encoded[pos];
                let key_len = if key_header <= 0x7f {
                    // Single byte
                    1
                } else if key_header <= 0xb7 {
                    // Short string
                    (key_header - 0x80) as usize
                } else {
                    // Long string - need to read length
                    let len_of_len = (key_header - 0xb7) as usize;
                    let mut key_len = 0;
                    for i in 1..=len_of_len {
                        key_len = (key_len << 8) | (encoded[pos + i] as usize);
                    }
                    key_len
                };

                if key_header <= 0x7f {
                    println!("5. Key in RLP: {} (single byte)", hex::encode(&encoded[pos..pos+1]));
                } else if key_header <= 0xb7 {
                    println!("5. Key in RLP: {} (short string, {} bytes)",
                           hex::encode(&encoded[pos+1..pos+1+key_len]), key_len);
                } else {
                    let len_of_len = (key_header - 0xb7) as usize;
                    println!("5. Key in RLP: {} (long string, {} bytes)",
                           hex::encode(&encoded[pos+1+len_of_len..pos+1+len_of_len+key_len]), key_len);
                }
            }

            println!("✅ {}: Pipeline successful", name);
        }
    }

    /// Performance test for large values
    #[test]
    #[ignore] // Ignored by default due to memory usage
    fn test_short_node_performance() {
        println!("=== ShortNode Performance Test ===");

        let large_sizes = vec![1024*1024, 10*1024*1024]; // 1MB, 10MB

        for size in large_sizes {
            println!("\n--- Testing {} bytes ({:.1} MB) ---", size, size as f64 / 1024.0 / 1024.0);

            let raw_key = vec![0x42; 32]; // 32-byte key
            let hex_key = key_to_nibbles(&raw_key);
            let large_value = vec![0x55; size];

            let start = std::time::Instant::now();
            let short_node = ShortNode::new(hex_key, &Node::Value(large_value));
            let creation_time = start.elapsed();

            let start = std::time::Instant::now();
            let encoded = alloy_rlp::encode(&short_node);
            let encoding_time = start.elapsed();

            let start = std::time::Instant::now();
            let decoded: ShortNode = alloy_rlp::Decodable::decode(&mut encoded.as_slice())
                .expect("Failed to decode ShortNode in performance test");
            let decoding_time = start.elapsed();

            // Verify the decode was successful and data matches
            assert_eq!(short_node.key, decoded.key, "Performance test: Key should match");
            match (&*short_node.val, &*decoded.val) {
                (Node::Value(v1), Node::Value(v2)) => {
                    assert_eq!(v1.len(), v2.len(), "Performance test: Value length should match");
                    assert_eq!(v1, v2, "Performance test: Value content should match");
                }
                _ => panic!("Performance test: Expected value nodes"),
            }

            println!("    Creation time: {:?}", creation_time);
            println!("    Encoding time: {:?}", encoding_time);
            println!("    Decoding time: {:?}", decoding_time);
            println!("    Encoded size: {} bytes", encoded.len());
            println!("    ✅ Performance test passed");
        }
    }
}

