//! Trie hasher
//!
//! This module provides a hasher for computing trie hashes.
use std::sync::Arc;
use crate::node::{Node, ShortNode, FullNode};
use alloy_primitives::{keccak256};
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

        // // Prepare the rlp encode key
        // collapsed.key = hex_to_compact(&short.key);

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
        if rpl_enc.len() > 32 && !force {
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
        if rpl_enc.len() > 32 && !force {
            return Node::Full(full);
        }
        let hash = keccak256(rpl_enc);
        Node::Hash(hash)
    }
}
