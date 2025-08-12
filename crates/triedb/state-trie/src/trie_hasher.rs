//! Trie hasher
//!
//! This module provides a hasher for computing trie hashes.
use std::sync::Arc;
use crate::secure_trie::{SecureTrieBuilder, SecureTrieId};
use alloy_primitives::{keccak256, B256};
use reth_triedb_memorydb::MemoryDB;
use crate::node::{Node, ShortNode, FullNode};
use crate::encoding::hex_to_compact;
use rayon::prelude::*;

/// Hasher structure for computing trie hashes
#[derive(Clone, Debug)]
pub struct Hasher {
    /// Whether to use parallel processing
    pub parallel: bool,
}

impl Hasher {
    /// Create a new Hasher instance
    ///
    /// # Arguments
    /// * `parallel` - Whether to enable parallel processing
    pub fn new(parallel: bool) -> Self {
        Self {
            parallel,
        }
    }

    /// Hash a node and return both the hashed and cached versions
    pub fn hash(&self, node: Arc<Node>, force: bool) -> (Arc<Node>, Arc<Node>) {
        let (hash, _) = node.cache();
        if !hash.is_none() {
            return (Arc::new(Node::Hash(hash.unwrap())), node)
        }

        match &*node {
            Node::Short(short) => {
                let (collapsed, cached) = self.hash_short_node_children(short.clone());
                let mut cached = cached.to_mutable_copy_with_cow();

                let hashed = self.short_node_to_hash(collapsed, force);
                match &hashed {
                    Node::Hash(hash) => {
                        // Note: This would need proper access to flags
                        cached.flags.hash = Some(*hash);
                    }
                    _ => {
                        cached.flags.hash = None;
                    }
                }
                (Arc::new(hashed), Arc::new(Node::Short(Arc::new(cached))))
            }
            Node::Full(full) => {
                let (collapsed, cached) = self.hash_full_node_children(full.clone());
                let mut cached = cached.to_mutable_copy_with_cow();

                let hashed = self.full_node_to_hash(collapsed, force);
                match &hashed {
                    Node::Hash(hash) => {
                        // Note: This would need proper access to flags
                        cached.flags.hash = Some(*hash);
                    }
                    _ => {
                        cached.flags.hash = None;
                    }
                }

                (Arc::new(hashed), Arc::new(Node::Full(Arc::new(cached))))
            }
            _ => {
                (node.clone(), node)
            }
        }
    }

    /// Hash the children of a short node
    pub fn hash_short_node_children(&self, short: Arc<ShortNode>) -> (Arc<ShortNode>, Arc<ShortNode>) {
        let mut collapsed = short.to_mutable_copy_with_cow();
        let mut cached = short.to_mutable_copy_with_cow();

        // Prepare the rlp encode key
        collapsed.key = hex_to_compact(&short.key);

        match &*short.val {
            Node::Short(_) | Node::Full(_) => {
                // Note: This would need proper implementation
                (collapsed.val, cached.val) = self.hash(short.val.clone(), false);
            }
            _ => { }
        }

        (Arc::new(collapsed), Arc::new(cached))
    }

    /// Convert a short node to its hash representation
    pub fn short_node_to_hash(&self, short: Arc<ShortNode>, force: bool) -> Node {
        // Note: This is a placeholder implementation
        let rpl_enc = short.to_rlp();
        if rpl_enc.len() < 32 && !force {
            return Node::Short(short);
        }
        let hash = keccak256(rpl_enc);
        // Placeholder hash
        Node::Hash(hash)
    }

    /// Hash the children of a full node
    pub fn hash_full_node_children(&self, full: Arc<FullNode>) -> (Arc<FullNode>, Arc<FullNode>) {
        let mut collapsed = full.to_mutable_copy_with_cow();
        let mut cached = full.to_mutable_copy_with_cow();

        if self.parallel {
            let child_results: Vec<(Arc<Node>, Arc<Node>)> = (0..16)
                .into_par_iter()
                .map(|i| {
                    match &*full.children[i] {
                        Node::EmptyRoot => {
                            (Arc::new(Node::EmptyRoot), Arc::new(Node::EmptyRoot))
                        }
                        _ => {
                            // Initialize a new hasher for each parallel task
                            let hasher = Hasher::new(false);
                            hasher.hash(full.children[i].clone(), false)
                        }
                    }
                })
                .collect();

            // Write results to collapsed and cached children
            for i in 0..16 {
                let (child_collapsed, child_cached) = child_results[i].clone();
                collapsed.set_child(i, &*child_collapsed);
                cached.set_child(i, &*child_cached);
            }
        } else {
            for i in 0..16 {
                match &*full.children[i] {
                    Node::EmptyRoot => {
                        continue;
                    }
                    _ => {
                        // Note: This would need proper implementation
                        let (child_collapsed, child_cached) = self.hash(full.children[i].clone(), false);
                        collapsed.set_child(i, &*child_collapsed);
                        cached.set_child(i, &*child_cached);
                    }
                }
            }
        }
        (Arc::new(collapsed), Arc::new(cached))
    }

    /// Convert a full node to its hash representation
    pub fn full_node_to_hash(&self, full: Arc<FullNode>, force: bool) -> Node {
        // Note: This is a placeholder implementation
        let rpl_enc = full.to_rlp();
        if rpl_enc.len() < 32 && !force {
            return Node::Full(full);
        }
        let hash = keccak256(rpl_enc);
        Node::Hash(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_trie::{SecureTrieBuilder, SecureTrieId};
    use crate::trie::Trie;
    use reth_triedb_pathdb::{PathDB, PathProviderConfig};
    use std::env;
    use alloy_primitives::{B256, keccak256};

    /// Create a test trie with specified operations
    fn create_test_trie(operations: &[(Vec<u8>, Option<Vec<u8>>)]) -> Trie<PathDB> {
        let temp_dir = env::temp_dir().join(format!("trie_hasher_test_{}", rand::random::<u64>()));
        let db_path = temp_dir.to_str().unwrap();

        let config = PathProviderConfig::default();
        let db = PathDB::new(db_path, config).expect("Failed to create PathDB");
        let id = SecureTrieId::new(B256::ZERO);

        let mut trie = SecureTrieBuilder::new(db.clone())
            .with_id(id.clone())
            .build()
            .expect("Failed to create trie")
            .trie_mut();

        // Apply all operations
        for (key, value_opt) in operations {
            match value_opt {
                Some(value) => {
                    // Insert or update
                    trie.update(key, value).expect("Failed to update trie");
                }
                None => {
                    // Delete
                    trie.delete(key).expect("Failed to delete from trie");
                }
            }
        }

        trie
    }

    /// Generate test data with khash computation
    fn generate_test_data() -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut test_data = Vec::new();

        for i in 0..10000 {
            let key = format!("test_key_{:05}", i).into_bytes();
            let value = format!("test_value_{:05}", i).into_bytes();

            // Apply khash computation to key (keccak256)
            let hashed_key = keccak256(&key);

            test_data.push((hashed_key.to_vec(), value));
        }

        test_data
    }

    /// Generate delete operations for 2000 keys
    fn generate_delete_operations(test_data: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let mut operations = Vec::new();

        // First insert all keys
        for (key, value) in test_data {
            operations.push((key.clone(), Some(value.clone())));
        }

        // Then delete first 2000 keys
        for i in 0..2000 {
            let key = &test_data[i].0;
            operations.push((key.clone(), None));
        }

        operations
    }

    #[test]
    fn test_parallel_vs_serial_hasher_consistency() {
        println!("🧪 Testing parallel vs serial hasher consistency...");

        // Generate test data
        let test_data = generate_test_data();
        let operations = generate_delete_operations(&test_data);

        println!("📊 Test setup:");
        println!("   - Total keys: {}", test_data.len());
        println!("   - Insert operations: {}", test_data.len());
        println!("   - Delete operations: 2000");
        println!("   - Final trie size: {}", test_data.len() - 2000);

        // Create two identical tries
        println!("🔨 Creating first trie...");
        let trie1 = create_test_trie(&operations);

        println!("🔨 Creating second trie...");
        let trie2 = create_test_trie(&operations);

        // Create hashers
        let parallel_hasher = Hasher::new(true);
        let serial_hasher = Hasher::new(false);

        println!("🔍 Testing hasher consistency with mock data...");

        // Test both hashers on the same input
        let (parallel_result, _) = parallel_hasher.hash(trie1.root().clone(), true);
        let (serial_result, _) = serial_hasher.hash(trie2.root().clone(), true);

        println!("Parallel result: {:?}, Serial result: {:?}", parallel_result, serial_result);
        // Results should be identical for the same input
        assert_eq!(parallel_result, serial_result,
                   "Parallel and serial hasher should produce identical results for same input");

        println!("✅ Parallel and serial hasher consistency test passed!");
        println!("   - Both hashers produced identical results");
        println!("   - Test data: {} keys, {} deletions", test_data.len(), 2000);
    }

    #[test]
    fn test_hasher_performance_comparison() {
        println!("🚀 Testing hasher performance comparison...");

        // Generate test data
        let test_data = generate_test_data();
        let operations = generate_delete_operations(&test_data);

        // Create test tries
        let trie1 = create_test_trie(&operations);
        let trie2 = create_test_trie(&operations);

        let parallel_hasher = Hasher::new(true);
        let serial_hasher = Hasher::new(false);

        // Performance test with timing
        let start_time = std::time::Instant::now();

        // Test parallel hasher
        let (parallel_result, _) = parallel_hasher.hash(trie1.root().clone(), true);

        let parallel_time = start_time.elapsed();

        // Test serial hasher
        let start_time = std::time::Instant::now();
        let (serial_result, _) = serial_hasher.hash(trie2.root().clone(), true);
        let serial_time = start_time.elapsed();

        println!("📊 Performance Results:");
        println!("   - Parallel hasher: {:?}", parallel_time);
        println!("   - Serial hasher: {:?}", serial_time);

        if parallel_time < serial_time {
            println!("   - 🚀 Parallel hasher is faster!");
        } else {
            println!("   - 🐌 Serial hasher is faster (unexpected for large datasets)");
        }

        println!("Parallel result: {:?}, Serial result: {:?}", parallel_result, serial_result);
        // Verify results are identical
        assert_eq!(parallel_result, serial_result,
                   "Performance test: results should be identical");

        println!("✅ Performance comparison test completed!");
    }

    #[test]
    fn test_hasher_edge_cases() {
        println!("🔍 Testing hasher edge cases...");

        let parallel_hasher = Hasher::new(true);
        let serial_hasher = Hasher::new(false);

        // Test with empty node
        let empty_node = Arc::new(Node::EmptyRoot);
        let (parallel_empty, _) = parallel_hasher.hash(empty_node.clone(), false);
        let (serial_empty, _) = serial_hasher.hash(empty_node.clone(), false);

        println!("Parallel result: {:?}, Serial result: {:?}", parallel_empty, serial_empty);
        assert_eq!(parallel_empty, serial_empty, "Empty node results should match");

        // Test with force flag
        let (parallel_force, _) = parallel_hasher.hash(empty_node.clone(), true);
        let (serial_force, _) = serial_hasher.hash(empty_node.clone(), true);

        println!("Parallel result: {:?}, Serial result: {:?}", parallel_force, serial_force);
        assert_eq!(parallel_force, serial_force, "Force flag results should match");

        println!("✅ Edge cases test passed!");
        println!("   - Empty node handling: ✅");
        println!("   - Force flag handling: ✅");
    }

    #[test]
    fn test_compare_hash_large_scale() {
        // Expected hash values from BSC implementation
        // These values are computed by BSC Go implementation for comparison
        let expected_hashes = [
            ("b91b7b8c08d1a294d4bf19825f920e58d2d93cd9095aa19ba0df4338576e9c80", "Batch 1 after write"),
            ("8ef58c64127c3dccc5a738d73fa4eed01659bf78750b7e1373e570e08e004bc7", "Batch 1 after delete"),

            ("dc92832d26fb9fbc03dc62624ab53e03900a7cfd8bda0f32e32684f584d08769", "Batch 2 after write"),
            ("6a73fccca58ef663a737604b715aed117f6b72c327ee64d9fc391684172752fd", "Batch 2 after delete"),

            ("9804b766e275c483234802e60323453bced49d10f102b8348003f6ff868a0e6d", "Batch 3 after write"),
            ("65f42acf271c3544c8ce04fbb0de3d21eac4626e0fdd49291c281dced917815c", "Batch 3 after delete"),

            ("1aa80c21142ccd358e19bdec041b039eaca14acf588778e22f6945e11a3cf1fa", "Batch 4 after write"),
            ("e0e83061cfb7191602b1ba0e6f56f0af43f083cf88325df3689f40fc182546f0", "Batch 4 after delete"),

            ("6febc56be1c122079f832f1d02a3316cc0c49d640d38c0e4ca7afcdff174978e", "Batch 5 after write"),
            ("b5a7131cb98ac0b0f713c082fc5f614a2d728b1dc3facfa2ca4440f0154d9b34", "Batch 5 after delete"),

            ("bdf2cc17dedd6f573226c06a943d9b4b2202450d1637dd7eaaa1e85d8cfce21f", "Batch 6 after write"),
            ("520be8eb447402ed7c0398d0dd5e19edff61bf74577ae413912619dbafc387fe", "Batch 6 after delete"),

            ("2fb02efeb7366a62cb6d197d3231ca71ad7f8870e748196311d3aa169199da20", "Batch 7 after write"),
            ("efc04b7a29bef9af4456e87b1b55e94bb00e70178fd1c3269c7c556333651856", "Batch 7 after delete"),

            ("30594be5f29ef8bd7738748bf2fa5170355778eb6ba58a1088dfadcee5c96c9f", "Batch 8 after write"),
            ("96038ea51961477250d766cb3e35cf843da654492e81dfb34147f401061d7866", "Batch 8 after delete"),

            ("4bc1debaeda69a1a314eaa6d33b5424fb327e101d704bf5faabd640497b2e422", "Batch 9 after write"),
            ("de4da730afc973d3321fdf45bb1a7a7b9afde379f4acc64e0cb31e99457f2650", "Batch 9 after delete"),

            ("dd251f7aadeb02ae7f853032e554b530e1ea2d4f1aea21f255994b1e6e65c629", "Batch 10 after write"),
            ("2638998f7d31f78bac496a7c0f99f81d9732b3e052595e7f6f27f575c7364b17", "Batch 10 after delete (final)"),
        ];

        println!("🚀 Testing large scale trie hash comparison...");
        println!("   - Total keys: 1,000,000");
        println!("   - Batches: 10");
        println!("   - Keys per batch: 100,000");
        println!("   - Delete per batch: 20,000");

        let db = reth_triedb_memorydb::MemoryDB::default();
        let id = SecureTrieId::new(B256::ZERO);

        let mut state_trie = SecureTrieBuilder::new(db.clone())
            .with_id(id.clone())
            .build()
            .expect("Failed to create trie");

        let trie = state_trie.trie_mut();

        const TOTAL_KEYS: usize = 1_000_000;
        const BATCH_SIZE: usize = 100_000;
        const DELETE_SIZE: usize = 20_000;
        const TOTAL_BATCHES: usize = 10;

        // Store all keys for deletion
        let mut all_keys: Vec<Vec<u8>> = Vec::with_capacity(TOTAL_KEYS);

        // Create hasher for computing hashes
        let hasher = Hasher::new(true); // Use serial hasher for consistency

        let mut hash_index = 0;
        for batch in 0..TOTAL_BATCHES {
            println!("\n=== 第 {} 批开始 ===", batch + 1);

            let start_idx = batch * BATCH_SIZE;
            let end_idx = start_idx + BATCH_SIZE;

            // Write current batch of key-value pairs
            for i in start_idx..end_idx {
                let key_data = format!("key_{}", i).into_bytes();
                let key = keccak256(&key_data);
                let value = key.to_vec(); // key and value are the same

                trie.update(&key.to_vec(), &value).expect("Failed to update trie");
                all_keys.push(key.to_vec());
            }

            println!("开始写入第 {} - {} 批 {} 个 key", start_idx + 1, end_idx, BATCH_SIZE);

            // Calculate and print hash after writing
            let (hash_after_write, _) = hasher.hash(trie.root().clone(), true);
            println!("第 {} 批写入后 Hash: {:?}", batch + 1, hash_after_write);

            // Extract hash from Node and compare
            if let Node::Hash(hash) = &*hash_after_write {
                assert_eq!(hex::encode(hash), expected_hashes[hash_index].0, "Batch {} write hash mismatch", batch + 1);
            } else {
                panic!("Expected Hash node, got: {:?}", hash_after_write);
            }
            hash_index += 1;

            // Delete earliest 20,000 keys from current batch
            let delete_start_idx = start_idx;
            let delete_end_idx = start_idx + DELETE_SIZE;

            println!("删除当前批次第 {} - {} 个 key (共 {} 个)",
                    delete_start_idx + 1, delete_end_idx, DELETE_SIZE);

            for i in delete_start_idx..delete_end_idx {
                if i < all_keys.len() {
                    trie.delete(&all_keys[i]).expect("Failed to delete from trie");
                }
            }

            // Calculate and print hash after deletion
            let (hash_after_delete, _) = hasher.hash(trie.root().clone(), true);
            println!("第 {} 批删除后 Hash: {:?}", batch + 1, hash_after_delete);

            println!("=== 第 {} 批完成 ===", batch + 1);

            // Extract hash from Node and compare
            if let Node::Hash(hash) = &*hash_after_delete {
                assert_eq!(hex::encode(hash), expected_hashes[hash_index].0, "Batch {} delete hash mismatch", batch + 1);
            } else {
                panic!("Expected Hash node, got: {:?}", hash_after_delete);
            }
            hash_index +=1;
        }

        // Final state
        println!("\n=== 测试完成 ===");
        let (final_hash, _) = hasher.hash(trie.root().clone(), true);
        println!("最终 Hash: {:?}", final_hash);

                let expected_size = TOTAL_KEYS - TOTAL_BATCHES * DELETE_SIZE;
        println!("期望最终 key 数量: {}", expected_size);

        println!("✅ Large scale trie hash test completed!");
    }



// Add rand dependency for testing
#[cfg(test)]
extern crate rand;

}
