//! Node set implementation for tracking modified trie nodes during commit operations.

use alloy_primitives::B256;
use std::collections::HashMap;

/// Represents a trie node with its hash and encoded data
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrieNode {
    /// Node hash, empty for deleted node
    pub hash: B256,
    /// Encoded node data, empty for deleted node
    pub blob: Vec<u8>,
}

impl TrieNode {
    /// Creates a new trie node
    pub fn new(hash: B256, blob: Vec<u8>) -> Self {
        Self { hash, blob }
    }

    /// Creates a deleted node marker
    pub fn deleted() -> Self {
        Self {
            hash: B256::ZERO,
            blob: Vec::new(),
        }
    }

    /// Returns true if this node is marked as deleted
    pub fn is_deleted(&self) -> bool {
        self.blob.is_empty()
    }

    /// Returns the total memory size used by this node
    pub fn size(&self) -> usize {
        self.blob.len() + 32 // 32 bytes for hash
    }
}

/// Leaf node representation
#[derive(Debug, Clone)]
struct Leaf {
    /// Raw blob of leaf
    #[allow(dead_code)]
    blob: Vec<u8>,
    /// Hash of parent node
    #[allow(dead_code)]
    parent: B256,
}

/// NodeSet contains a set of nodes collected during the commit operation.
/// Each node is keyed by path. It's not thread-safe to use.
#[derive(Debug, Clone)]
pub struct NodeSet {
    /// Owner hash (zero for account trie, account address hash for storage tries)
    pub owner: B256,
    /// Leaf nodes
    leaves: Vec<Leaf>,
    /// Node map keyed by path
    nodes: HashMap<String, TrieNode>,
    /// Count of updated and inserted nodes
    updates: usize,
    /// Count of deleted nodes
    deletes: usize,
}

impl NodeSet {
    /// Creates a new node set
    pub fn new(owner: B256) -> Self {
        Self {
            owner,
            leaves: Vec::new(),
            nodes: HashMap::new(),
            updates: 0,
            deletes: 0,
        }
    }

    /// Adds a node to the set
    pub fn add_node(&mut self, path: &[u8], node: TrieNode) {
        if node.is_deleted() {
            self.deletes += 1;
        } else {
            self.updates += 1;
        }
        self.nodes.insert(String::from_utf8_lossy(path).to_string(), node);
    }

    /// Adds a leaf node to the set
    pub fn add_leaf(&mut self, parent: B256, blob: Vec<u8>) {
        self.leaves.push(Leaf { blob, parent });
    }

    /// Returns the number of dirty nodes in the set
    pub fn size(&self) -> (usize, usize) {
        (self.updates, self.deletes)
    }

    /// Returns a reference to the nodes map
    pub fn nodes(&self) -> &HashMap<String, TrieNode> {
        &self.nodes
    }

    /// Returns a mutable reference to the nodes map
    pub fn nodes_mut(&mut self) -> &mut HashMap<String, TrieNode> {
        &mut self.nodes
    }

    /// Returns a set of trie nodes keyed by node hash
    pub fn hash_set(&self) -> HashMap<B256, Vec<u8>> {
        let mut ret = HashMap::new();
        for node in self.nodes.values() {
            if !node.is_deleted() {
                ret.insert(node.hash, node.blob.clone());
            }
        }
        ret
    }

    /// Merges another node set into this one
    pub fn merge(&mut self, other: &NodeSet) -> Result<(), String> {
        if self.owner != other.owner {
            return Err(format!(
                "nodesets belong to different owner are not mergeable {:?}-{:?}",
                self.owner, other.owner
            ));
        }

        for (path, node) in &other.nodes {
            let prev = self.nodes.get(path);
            if let Some(prev_node) = prev {
                // Overwrite happens, revoke the counter
                if prev_node.is_deleted() {
                    self.deletes -= 1;
                } else {
                    self.updates -= 1;
                }
            }
            if node.is_deleted() {
                self.deletes += 1;
            } else {
                self.updates += 1;
            }
            self.nodes.insert(path.clone(), node.clone());
        }

        // Append leaves
        self.leaves.extend(other.leaves.clone());

        Ok(())
    }

    /// Returns true if the node set is empty
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.leaves.is_empty()
    }

    /// Clears all nodes and leaves
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.leaves.clear();
        self.updates = 0;
        self.deletes = 0;
    }
}

impl Default for NodeSet {
    fn default() -> Self {
        Self::new(B256::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trie_node_creation() {
        let hash = B256::from([1u8; 32]);
        let blob = vec![1, 2, 3, 4];
        let node = TrieNode::new(hash, blob.clone());

        assert_eq!(node.hash, hash);
        assert_eq!(node.blob, blob);
        assert!(!node.is_deleted());
        assert_eq!(node.size(), 36); // 4 bytes blob + 32 bytes hash
    }

    #[test]
    fn test_trie_node_deleted() {
        let node = TrieNode::deleted();

        assert_eq!(node.hash, B256::ZERO);
        assert!(node.blob.is_empty());
        assert!(node.is_deleted());
        assert_eq!(node.size(), 32); // 0 bytes blob + 32 bytes hash
    }

    #[test]
    fn test_node_set_creation() {
        let owner = B256::from([1u8; 32]);
        let node_set = NodeSet::new(owner);

        assert_eq!(node_set.owner, owner);
        assert!(node_set.nodes().is_empty());
        assert_eq!(node_set.size(), (0, 0));
        assert!(node_set.is_empty());
    }

    #[test]
    fn test_node_set_add_node() {
        let mut node_set = NodeSet::new(B256::ZERO);
        let hash = B256::from([1u8; 32]);
        let blob = vec![1, 2, 3];
        let node = TrieNode::new(hash, blob);

        node_set.add_node(b"test_path", node);

        assert_eq!(node_set.size(), (1, 0));
        assert!(!node_set.is_empty());
        assert_eq!(node_set.nodes().len(), 1);
    }

    #[test]
    fn test_node_set_add_deleted_node() {
        let mut node_set = NodeSet::new(B256::ZERO);
        let node = TrieNode::deleted();

        node_set.add_node(b"test_path", node);

        assert_eq!(node_set.size(), (0, 1));
        assert!(!node_set.is_empty());
        assert_eq!(node_set.nodes().len(), 1);
    }

    #[test]
    fn test_node_set_merge() {
        let mut node_set1 = NodeSet::new(B256::ZERO);
        let mut node_set2 = NodeSet::new(B256::ZERO);

        let hash1 = B256::from([1u8; 32]);
        let hash2 = B256::from([2u8; 32]);

        node_set1.add_node(b"path1", TrieNode::new(hash1, vec![1, 2]));
        node_set2.add_node(b"path2", TrieNode::new(hash2, vec![3, 4]));

        node_set1.merge(&node_set2).unwrap();

        assert_eq!(node_set1.size(), (2, 0));
        assert_eq!(node_set1.nodes().len(), 2);
    }

    #[test]
    fn test_node_set_merge_different_owners() {
        let mut node_set1 = NodeSet::new(B256::ZERO);
        let node_set2 = NodeSet::new(B256::from([1u8; 32]));

        let result = node_set1.merge(&node_set2);
        assert!(result.is_err());
    }
}
