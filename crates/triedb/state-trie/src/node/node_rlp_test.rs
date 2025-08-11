//! RLP encoding and decoding tests for trie nodes
//!
//! This module contains comprehensive tests for the RLP encoding and decoding
//! functionality of all node types in the BSC-style trie implementation.

use super::*;
use alloy_primitives::B256;
use std::sync::Arc;
use crate::encoding::{key_to_nibbles};
use crate::node::decode_node::decode_node;

#[cfg(test)]
mod tests {
    use super::*;

    /// Test basic RLP encoding and decoding for ShortNode
    ///
    /// This test verifies that ShortNode with simple key-value pair can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_rlp() {
        let original_key = b"test_key".to_vec();
        let key = key_to_nibbles(&original_key);
        let value = Node::Value(b"test_value".to_vec());
        let short_node = ShortNode::new(key, &value);

        // Encode the node
        let encoded = short_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            // The decoded key should match the original key (after conversion)
            assert_eq!(decoded_short.key, short_node.key);
            assert_eq!(decoded_short.val, short_node.val);
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test basic RLP encoding and decoding for FullNode
    ///
    /// This test verifies that FullNode with all children as EmptyRoot can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_full_node_rlp() {
        let full_node = FullNode::new();

        // Encode the node
        let encoded = full_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Full(decoded_full) = decoded {
            assert_eq!(decoded_full.children.len(), full_node.children.len());
            for i in 0..17 {
                assert_eq!(decoded_full.children[i], full_node.children[i]);
            }
        } else {
            panic!("Expected FullNode");
        }
    }

    /// Test RLP encoding and decoding for ShortNode with nested HashNode
    ///
    /// This test verifies that ShortNode containing a HashNode can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_with_hash_nested() {
        let original_key = b"hash_key".to_vec();
        let mut key = key_to_nibbles(&original_key);
        key = key[..key.len() - 1].to_vec();
        let hash = B256::from_slice(&[2u8; 32]);
        let hash_node = Node::Hash(hash);
        let short_node = ShortNode::new(key, &hash_node);

        // Encode the node
        let encoded = short_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            assert_eq!(decoded_short.key, short_node.key);
            assert_eq!(decoded_short.val, short_node.val);
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test RLP encoding and decoding for ShortNode with nested ShortNode
    ///
    /// This test verifies that ShortNode containing another ShortNode can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_with_short_nested() {
        let inner_original_key = b"inner_key".to_vec();
        let inner_key = key_to_nibbles(&inner_original_key);
        let inner_value = Node::Value(b"inner_value".to_vec());
        let inner_short = ShortNode::new(inner_key, &inner_value);
        let inner_short_clone = inner_short.clone();

        let outer_original_key = b"outer_key".to_vec();
        let outer_key = key_to_nibbles(&outer_original_key);
        let outer_short = ShortNode::new(outer_key, &Node::Short(Arc::new(inner_short)));

        // Encode the node
        let encoded = outer_short.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            assert_eq!(decoded_short.key, outer_short.key);
            // Note: We can't directly compare val due to Arc cloning, but we can verify structure
            assert!(matches!(*decoded_short.val, Node::Short(_)));

            if let Node::Short(decoded_short_val) = decoded_short.val.as_ref() {
                assert_eq!(decoded_short_val.key, inner_short_clone.key);
                assert_eq!(decoded_short_val.val, Arc::new(inner_value));
            }
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test RLP encoding and decoding for FullNode with nested ShortNode children
    ///
    /// This test verifies that FullNode containing ShortNode children can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_full_node_with_short_node_children() {
        let mut full_node = FullNode::new();

        // Create nested ShortNode
        let short_original_key = b"nested_short_key".to_vec();
        let short_key = key_to_nibbles(&short_original_key);
        let short_node_clone = short_key.clone();
        let short_value = Node::Value(b"nested_short_value".to_vec());
        let short_node = ShortNode::new(short_key, &short_value);

        // Set ShortNode as child
        full_node.set_child(7, &Node::Short(Arc::new(short_node)));

        // Encode the node
        let encoded = full_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Full(decoded_full) = decoded {
            assert!(matches!(*decoded_full.children[7], Node::Short(_)));
            if let Node::Short(short_child) = decoded_full.children[7].as_ref() {
                assert_eq!(short_child.key, short_node_clone);
                assert_eq!(*short_child.val, Node::Value(b"nested_short_value".to_vec()));
            } else {
                panic!("Expected ShortNode at children[7]");
            }
        } else {
            panic!("Expected FullNode");
        }
    }

    /// Test RLP encoding and decoding for FullNode with nested FullNode children
    ///
    /// This test verifies that FullNode containing FullNode children can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_full_node_with_full_node_children() {
        let mut inner_full = FullNode::new();
        inner_full.set_child(3, &Node::Hash(B256::from_slice(&[3u8; 32])));

        let mut outer_full = FullNode::new();
        outer_full.set_child(12, &Node::Full(Arc::new(inner_full)));

        // Encode the node
        let encoded = outer_full.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Full(decoded_full) = decoded {
            // 验证 children[12] 是 FullNode
            assert!(matches!(*decoded_full.children[12], Node::Full(_)));

            // 解析 children[12] 的 FullNode
            if let Node::Full(inner_full_decoded) = decoded_full.children[12].as_ref() {
                // 验证 inner_full 的 children[3] 是 HashNode
                assert!(matches!(*inner_full_decoded.children[3], Node::Hash(_)));

                // 验证 HashNode 的值
                if let Node::Hash(hash_val) = inner_full_decoded.children[3].as_ref() {
                    assert_eq!(hash_val, &B256::from_slice(&[3u8; 32]));
                } else {
                    panic!("Expected HashNode at inner_full.children[3]");
                }
            } else {
                panic!("Expected FullNode at children[12]");
            }
        } else {
            panic!("Expected FullNode");
        }
    }

    /// Test RLP encoding and decoding for complex nested structure
    ///
    /// This test verifies that a complex nested structure with multiple
    /// node types can be properly encoded and decoded without data loss.
    #[test]
    fn test_complex_nested_structure() {
        // Create a complex nested structure:
        // FullNode -> ShortNode -> FullNode -> HashNode
        let hash = B256::from_slice(&[4u8; 32]);
        let hash_node = Node::Hash(hash);

        let mut inner_full = FullNode::new();
        inner_full.set_child(8, &hash_node);

        let short_original_key = b"complex_key".to_vec();
        let short_key = key_to_nibbles(&short_original_key);
        let short_key_clone = short_key.clone();
        let short_node = ShortNode::new(short_key, &Node::Full(Arc::new(inner_full)));

        let mut outer_full = FullNode::new();
        outer_full.set_child(1, &Node::Short(Arc::new(short_node)));
        outer_full.set_child(15, &Node::Hash(B256::from_slice(&[5u8; 32])));

        // Encode the node
        let encoded = outer_full.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Full(decoded_full) = decoded {
            assert!(matches!(*decoded_full.children[1], Node::Short(_)));
            assert!(matches!(*decoded_full.children[15], Node::Hash(_)));
            if let Node::Short(short_child) = decoded_full.children[1].as_ref() {
                assert_eq!(short_child.key, short_key_clone);
                assert!(matches!(*short_child.val, Node::Full(_)));
                if let Node::Full(inner_full_decoded) = short_child.val.as_ref() {
                    assert!(matches!(*inner_full_decoded.children[8], Node::Hash(_)));
                    if let Node::Hash(hash_val) = inner_full_decoded.children[8].as_ref() {
                        assert_eq!(hash_val, &B256::from_slice(&[4u8; 32]));
                    } else {
                        panic!("Expected HashNode at inner_full.children[8]");
                    }
                } else {
                    panic!("Expected FullNode in ShortNode.val");
                }
            } else {
                panic!("Expected ShortNode at children[1]");
            }

            if let Node::Hash(hash_val) = decoded_full.children[15].as_ref() {
                assert_eq!(hash_val, &B256::from_slice(&[5u8; 32]));
            } else {
                panic!("Expected HashNode at children[15]");
            }
        } else {
            panic!("Expected FullNode");
        }
    }

    /// Test RLP encoding and decoding for ShortNode with empty key
    ///
    /// This test verifies that ShortNode with an empty key can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_with_empty_key() {
        let original_key = Vec::new();
        let key = key_to_nibbles(&original_key);
        let value = Node::Value(b"empty_key_value".to_vec());
        let short_node = ShortNode::new(key, &value);

        // Encode the node
        let encoded = short_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            assert_eq!(decoded_short.key, short_node.key);
            assert_eq!(decoded_short.val, short_node.val);
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test RLP encoding and decoding for FullNode with all EmptyRoot children
    ///
    /// This test verifies that FullNode with all children as EmptyRoot can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_full_node_all_empty_children() {
        let full_node = FullNode::new();

        // Encode the node
        let encoded = full_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Full(decoded_full) = decoded {
            assert_eq!(decoded_full.children.len(), full_node.children.len());
            for i in 0..17 {
                assert_eq!(decoded_full.children[i], full_node.children[i]);
            }
        } else {
            panic!("Expected FullNode");
        }
    }

    /// Test RLP encoding and decoding for ShortNode with very long key
    ///
    /// This test verifies that ShortNode with a very long key can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_with_long_key() {
        let original_key = vec![b'a'; 1000]; // 1000 bytes long key
        let key = key_to_nibbles(&original_key);
        let value = Node::Value(b"long_key_value".to_vec());
        let short_node = ShortNode::new(key, &value);

        // Encode the node
        let encoded = short_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            assert_eq!(decoded_short.key, short_node.key);
            assert_eq!(decoded_short.val, short_node.val);
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test RLP encoding and decoding for ShortNode with very long value
    ///
    /// This test verifies that ShortNode with a very long value can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_with_long_value() {
        let original_key = b"long_value_key".to_vec();
        let key = key_to_nibbles(&original_key);
        let value = Node::Value(vec![b'b'; 1000]); // 1000 bytes long value
        let short_node = ShortNode::new(key, &value);

        // Encode the node
        let encoded = short_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            assert_eq!(decoded_short.key, short_node.key);
            assert_eq!(decoded_short.val, short_node.val);
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test RLP encoding and decoding for ShortNode with HashNode value
    ///
    /// This test verifies that ShortNode with a HashNode value can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_with_hash_value() {
        let original_key = b"hash_value_key".to_vec();
        let key = key_to_nibbles(&original_key);
        let key = key[..key.len() - 1].to_vec();
        let hash = B256::from_slice(&[5u8; 32]);
        let hash_node = Node::Hash(hash);
        let short_node = ShortNode::new(key, &hash_node);

        // Encode the node
        let encoded = short_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            assert_eq!(decoded_short.key, short_node.key);
            if let Node::Hash(hash_val) = decoded_short.val.as_ref() {
                assert_eq!(hash_val, &B256::from_slice(&[5u8; 32]));
            } else {
                panic!("Expected HashNode in ShortNode.val");
            }
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test RLP encoding and decoding for deep nested structure
    ///
    /// This test verifies that a deeply nested structure can be
    /// properly encoded and decoded without data loss.
    #[test]
    fn test_deep_nested_structure() {
        // Create a deeply nested structure: 5 levels deep
        let mut current = Node::Value(b"deepest_value".to_vec());

        for i in 0..5 {
            let original_key = format!("level_{}", i).into_bytes();
            let key = key_to_nibbles(&original_key);
            let short_node = ShortNode::new(key, &current);
            current = Node::Short(Arc::new(short_node));
        }

        // For deep nested structures, we need to test each level individually
        // since we can't directly encode/decode the entire Node enum
        if let Node::Short(outer_short) = &current {
            // Encode the outer ShortNode
            let encoded = outer_short.to_rlp();

            // Decode the outer ShortNode using decode_node function
            let decoded_arc = decode_node(None, &encoded).unwrap();
            let decoded = decoded_arc.as_ref();

            // Assert that decoded data matches original
            if let Node::Short(decoded_short) = decoded {
                // Verify that the decoded key matches the original key (level_4)
                let expected_key = key_to_nibbles(&format!("level_{}", 4).into_bytes());
                assert_eq!(decoded_short.key, expected_key);

                // Verify that the value is a ShortNode
                assert!(matches!(*decoded_short.val, Node::Short(_)));

                // Parse the nested ShortNode and verify its structure
                if let Node::Short(nested_short) = decoded_short.val.as_ref() {
                    // Verify the nested ShortNode's key (level_3)
                    let expected_nested_key = key_to_nibbles(&format!("level_{}", 3).into_bytes());
                    assert_eq!(nested_short.key, expected_nested_key);

                    // Verify that the nested value is also a ShortNode
                    assert!(matches!(*nested_short.val, Node::Short(_)));

                    // Continue parsing deeper levels if needed
                    if let Node::Short(deep_nested_short) = nested_short.val.as_ref() {
                        // Verify the deep nested ShortNode's key (level_2)
                        let expected_deep_key = key_to_nibbles(&format!("level_{}", 2).into_bytes());
                        assert_eq!(deep_nested_short.key, expected_deep_key);

                        // Verify that the deep nested value is also a ShortNode
                        assert!(matches!(*deep_nested_short.val, Node::Short(_)));

                        // Continue for level_1 and level_0
                        if let Node::Short(level1_short) = deep_nested_short.val.as_ref() {
                            let expected_level1_key = key_to_nibbles(&format!("level_{}", 1).into_bytes());
                            assert_eq!(level1_short.key, expected_level1_key);

                            if let Node::Short(level0_short) = level1_short.val.as_ref() {
                                let expected_level0_key = key_to_nibbles(&format!("level_{}", 0).into_bytes());
                                assert_eq!(level0_short.key, expected_level0_key);

                                // Verify the deepest value is the expected ValueNode
                                assert_eq!(*level0_short.val, Node::Value(b"deepest_value".to_vec()));
                            } else {
                                panic!("Expected ShortNode at level 0");
                            }
                        } else {
                            panic!("Expected ShortNode at level 1");
                        }
                    } else {
                        panic!("Expected ShortNode at level 2");
                    }
                } else {
                    panic!("Expected ShortNode at level 3");
                }
            } else {
                panic!("Expected ShortNode");
            }
        } else {
            panic!("Expected ShortNode at outer level");
        }
    }

    /// Test RLP encoding and decoding for mixed node types in complex structure
    ///
    /// This test verifies that a complex structure with mixed node types
    /// can be properly encoded and decoded without data loss.
    #[test]
    fn test_mixed_node_types_complex_structure() {
        // Create a complex structure with mixed node types
        let hash1 = B256::from_slice(&[6u8; 32]);
        let hash2 = B256::from_slice(&[7u8; 32]);

        let mut full_node = FullNode::new();
        full_node.set_child(0, &Node::Hash(hash1));
        full_node.set_child(8, &Node::Hash(hash2));
        full_node.set_child(16, &Node::Value(b"mixed_value".to_vec()));

        let short_original_key = b"mixed_key".to_vec();
        let short_key = key_to_nibbles(&short_original_key);
        let short_node = ShortNode::new(short_key, &Node::Full(Arc::new(full_node)));

        // Encode the ShortNode
        let encoded = short_node.to_rlp();

        // Decode the ShortNode using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            // Verify that the decoded key matches the original key
            assert_eq!(decoded_short.key, short_node.key);

            // Verify that the value is a FullNode
            assert!(matches!(*decoded_short.val, Node::Full(_)));

            // Parse the nested FullNode and verify its structure
            if let Node::Full(nested_full) = decoded_short.val.as_ref() {
                assert!(matches!(*nested_full.children[0], Node::Hash(_)));
                assert!(matches!(*nested_full.children[16], Node::Value(_)));

                // Verify that children[0] is a HashNode with the expected value
                if let Node::Hash(hash_val) = nested_full.children[0].as_ref() {
                    assert_eq!(hash_val, &B256::from_slice(&[6u8; 32]));
                } else {
                    panic!("Expected HashNode at children[8]");
                }

                // Verify that children[8] is a HashNode with the expected value
                if let Node::Hash(hash_val) = nested_full.children[8].as_ref() {
                    assert_eq!(hash_val, &B256::from_slice(&[7u8; 32]));
                } else {
                    panic!("Expected HashNode at children[8]");
                }

                // Verify that all other children are EmptyRoot
                for i in 0..17 {
                    if i != 0 && i != 8 && i != 16{
                        assert_eq!(*nested_full.children[i], Node::EmptyRoot);
                    }
                }
            } else {
                panic!("Expected FullNode in ShortNode.val");
            }
        } else {
            panic!("Expected ShortNode");
        }
    }

    /// Test RLP encoding and decoding for ShortNode with special characters in key
    ///
    /// This test verifies that ShortNode with special characters in the key
    /// can be properly encoded and decoded without data loss.
    #[test]
    fn test_short_node_with_special_key() {
        let original_key = vec![0x00, 0xFF, 0x7F, 0x80]; // Special byte values
        let key = key_to_nibbles(&original_key);
        let value = Node::Value(b"special_key_value".to_vec());
        let short_node = ShortNode::new(key, &value);

        // Encode the node
        let encoded = short_node.to_rlp();

        // Decode the node using decode_node function
        let decoded_arc = decode_node(None, &encoded).unwrap();
        let decoded = decoded_arc.as_ref();

        // Assert that decoded data matches original
        if let Node::Short(decoded_short) = decoded {
            assert_eq!(decoded_short.key, short_node.key);
            assert_eq!(decoded_short.val, short_node.val);
        } else {
            panic!("Expected ShortNode");
        }
    }
}
