//! Parallel hasher implementation for trie operations.

use alloy_primitives::keccak256;
use alloy_rlp::Encodable;
use rayon::prelude::*;

use super::node::{Node, FullNode, ShortNode, HashNode};
use super::node_set::{NodeSet, TrieNode};
use super::secure_trie::SecureTrieError;
use reth_triedb_common::TrieDatabase;

/// Parallel hasher for trie operations
#[derive(Debug)]
pub struct ParallelHasher {
    /// Whether to use parallel processing
    pub parallel: bool,
    /// Thread pool for parallel processing
    pub thread_pool: Option<rayon::ThreadPool>,
}

impl ParallelHasher {
    /// Creates a new hasher
    pub fn new(parallel: bool) -> Self {
        let thread_pool = if parallel {
            rayon::ThreadPoolBuilder::new()
                .num_threads(16) // 16-way parallel like BSC
                .build()
                .ok()
        } else {
            None
        };

        Self { parallel, thread_pool }
    }

    /// Creates a hasher based on unhashed count (similar to BSC's logic)
    pub fn new_with_threshold(unhashed: usize) -> Self {
        Self::new(unhashed >= 100)
    }

    /// Hashes a node and returns the hashed node and cached node
    pub fn hash(&self, node: &Node) -> (Node, Node) {
        match node {
            Node::Value(_) => {
                // Value nodes don't need hashing
                (node.copy(), node.copy())
            }
            Node::Hash(_) => {
                // Hash nodes are already hashed
                (node.copy(), node.copy())
            }
            Node::Short(short) => {
                self.hash_short_node(short)
            }
            Node::Full(full) => {
                self.hash_full_node(full)
            }
        }
    }

    /// Hashes a short node
    fn hash_short_node(&self, short: &ShortNode) -> (Node, Node) {
        // Hash the child first (recursive, but not parallel for short nodes)
        let (hashed_child, cached_child) = self.hash(&short.val);

        // Create collapsed and cached versions
        let mut collapsed = short.copy();
        let mut cached = short.copy();

        collapsed.val = Box::new(hashed_child);
        cached.val = Box::new(cached_child);

        // Hash the short node itself
        let hashed = self.hash_node_to_hash(&Node::Short(collapsed));
        let cached_with_hash = if let Node::Hash(hash) = &hashed {
            let mut cached = cached;
            cached.flags.hash = Some(hash.0);
            Node::Short(cached)
        } else {
            Node::Short(cached)
        };

        (hashed, cached_with_hash)
    }

    /// Hashes a full node with optional parallel processing
    fn hash_full_node(&self, full: &FullNode) -> (Node, Node) {
        if self.parallel {
            self.hash_full_node_parallel(full)
        } else {
            self.hash_full_node_sequential(full)
        }
    }

    /// Sequential hashing of full node
    fn hash_full_node_sequential(&self, full: &FullNode) -> (Node, Node) {
        let mut collapsed = full.copy();
        let mut cached = full.copy();

        // Hash all 16 children sequentially
        for i in 0..16 {
            if let Some(child) = &full.children[i] {
                let (hashed_child, cached_child) = self.hash(child);
                collapsed.children[i] = Some(Box::new(hashed_child));
                cached.children[i] = Some(Box::new(cached_child));
            }
        }

        // Hash the full node itself
        let hashed = self.hash_node_to_hash(&Node::Full(collapsed));
        let cached_with_hash = if let Node::Hash(hash) = &hashed {
            let mut cached = cached;
            cached.flags.hash = Some(hash.0);
            Node::Full(cached)
        } else {
            Node::Full(cached)
        };

        (hashed, cached_with_hash)
    }

    /// Parallel hashing of full node (16-way parallel, first level only)
    /// This implements BSC-style parallelism: 16-way parallel at first level,
    /// sequential processing for deeper levels
    fn hash_full_node_parallel(&self, full: &FullNode) -> (Node, Node) {
        let mut collapsed = full.copy();
        let mut cached = full.copy();

        // Collect children that need processing
        let mut children_to_process = Vec::new();
        for i in 0..16 {
            if let Some(child) = &full.children[i] {
                children_to_process.push((i, child));
            }
        }

        if let Some(pool) = &self.thread_pool {
            // Use thread pool for parallel processing (first level only)
            let results: Vec<_> = pool.install(|| {
                children_to_process.par_iter().map(|(i, child)| {
                    // Create a sequential hasher for child nodes (no recursion parallelism)
                    let child_hasher = ParallelHasher::new(false);
                    let (hashed_child, cached_child) = child_hasher.hash(child);
                    (*i, hashed_child, cached_child)
                }).collect()
            });

            // Apply results
            for (i, hashed_child, cached_child) in results {
                collapsed.children[i] = Some(Box::new(hashed_child));
                cached.children[i] = Some(Box::new(cached_child));
            }
        } else {
            // Fallback to sequential if thread pool is not available
            for (i, child) in children_to_process {
                let (hashed_child, cached_child) = self.hash(child);
                collapsed.children[i] = Some(Box::new(hashed_child));
                cached.children[i] = Some(Box::new(cached_child));
            }
        }

        // Hash the full node itself
        let hashed = self.hash_node_to_hash(&Node::Full(collapsed));
        let cached_with_hash = if let Node::Hash(hash) = &hashed {
            let mut cached = cached;
            cached.flags.hash = Some(hash.0);
            Node::Full(cached)
        } else {
            Node::Full(cached)
        };

        (hashed, cached_with_hash)
    }

    /// Converts a node to its hash representation
    fn hash_node_to_hash(&self, node: &Node) -> Node {
        let mut encoded = Vec::new();
        node.encode(&mut encoded);
        let hash = keccak256(&encoded);
        Node::Hash(HashNode::new(hash))
    }
}

/// Parallel committer for trie operations
#[derive(Debug)]
pub struct ParallelCommitter<DB> {
    /// Database reference
    database: DB,
    /// Node set for collecting modified nodes
    node_set: NodeSet,
    /// Whether to collect leaf nodes
    collect_leaf: bool,
    /// Whether to use parallel processing
    parallel: bool,
    /// Thread pool for parallel processing
    pub thread_pool: Option<rayon::ThreadPool>,
}

impl<DB> ParallelCommitter<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Creates a new parallel committer
    pub fn new(database: DB, node_set: NodeSet, collect_leaf: bool, parallel: bool) -> Self {
        let thread_pool = if parallel {
            rayon::ThreadPoolBuilder::new()
                .num_threads(16) // 16-way parallel like BSC
                .build()
                .ok()
        } else {
            None
        };

        Self {
            database,
            node_set,
            collect_leaf,
            parallel,
            thread_pool,
        }
    }

    /// Commits a node and returns the committed node
    pub fn commit(&mut self, node: &Node) -> Result<Node, SecureTrieError> {
        match node {
            Node::Value(_) => {
                // Value nodes are already committed
                Ok(node.copy())
            }
            Node::Hash(_) => {
                // Hash nodes are already committed
                Ok(node.copy())
            }
            Node::Short(short) => {
                self.commit_short_node(short)
            }
            Node::Full(full) => {
                self.commit_full_node(full)
            }
        }
    }

    /// Commits a short node
    fn commit_short_node(&mut self, short: &ShortNode) -> Result<Node, SecureTrieError> {
        // Commit the child first (recursive, but not parallel for short nodes)
        let committed_child = self.commit(&short.val)?;

        // Create new short node with committed child
        let new_short = ShortNode::new(short.key.clone(), committed_child);

        // Hash and store the short node
        self.store_node(&Node::Short(new_short))
    }

    /// Commits a full node with optional parallel processing
    fn commit_full_node(&mut self, full: &FullNode) -> Result<Node, SecureTrieError> {
        if self.parallel {
            self.commit_full_node_parallel(full)
        } else {
            self.commit_full_node_sequential(full)
        }
    }

    /// Sequential commit of full node
    fn commit_full_node_sequential(&mut self, full: &FullNode) -> Result<Node, SecureTrieError> {
        let mut new_full = full.copy();

        // Commit all 16 children sequentially
        for i in 0..16 {
            if let Some(child) = &full.children[i] {
                let committed_child = self.commit(child)?;
                new_full.children[i] = Some(Box::new(committed_child));
            }
        }

        // Hash and store the full node
        self.store_node(&Node::Full(new_full))
    }

    /// Parallel commit of full node (16-way parallel, first level only)
    /// This implements BSC-style parallelism: 16-way parallel at first level,
    /// sequential processing for deeper levels
    fn commit_full_node_parallel(&mut self, full: &FullNode) -> Result<Node, SecureTrieError> {
        let mut new_full = full.copy();

        // Collect children that need processing
        let mut children_to_commit = Vec::new();
        for i in 0..16 {
            if let Some(child) = &full.children[i] {
                children_to_commit.push((i, child));
            }
        }

        if let Some(pool) = &self.thread_pool {
            // Use thread pool for parallel processing (first level only)
            let results: Result<Vec<_>, SecureTrieError> = pool.install(|| {
                children_to_commit.par_iter().map(|(i, child)| {
                    // Create a sequential committer for child nodes (no recursion parallelism)
                    let child_set = NodeSet::new(self.node_set.owner);
                    let mut child_committer = ParallelCommitter::new(
                        self.database.clone(),
                        child_set,
                        self.collect_leaf,
                        false, // No recursion parallelism
                    );
                    let committed_child = child_committer.commit(child)?;

                    // Merge child node set back to parent
                    let child_node_set = child_committer.into_node_set();
                    Ok((*i, committed_child, child_node_set))
                }).collect()
            });

            match results {
                Ok(results) => {
                    // Apply results and merge node sets
                    for (i, committed_child, child_node_set) in results {
                        new_full.children[i] = Some(Box::new(committed_child));
                        self.node_set.merge(&child_node_set).expect("Failed to merge node sets");
                    }
                }
                Err(e) => return Err(e),
            }
        } else {
            // Fallback to sequential if thread pool is not available
            for (i, child) in children_to_commit {
                let committed_child = self.commit(child)?;
                new_full.children[i] = Some(Box::new(committed_child));
            }
        }

        // Hash and store the full node
        self.store_node(&Node::Full(new_full))
    }

    /// Stores a node in the database and adds it to the node set
    fn store_node(&mut self, node: &Node) -> Result<Node, SecureTrieError> {
        let mut encoded = Vec::new();
        node.encode(&mut encoded);
        let hash = keccak256(&encoded);

        // Store in database
        self.database.insert(hash, encoded.clone())
            .map_err(|e| SecureTrieError::Database(format!("{:?}", e)))?;

        // Add to node set
        self.node_set.add_node(&[], TrieNode::new(hash, encoded.clone()));

        // Add to leaves if needed
        if self.collect_leaf {
            if let Node::Short(short) = node {
                if let Node::Value(_) = &*short.val {
                    self.node_set.add_leaf(hash, encoded);
                }
            }
        }

        Ok(Node::Hash(HashNode::new(hash)))
    }

    /// Returns the node set
    pub fn into_node_set(self) -> NodeSet {
        self.node_set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_triedb_memorydb::MemoryDB;
    use alloy_primitives::B256;

    #[test]
    fn test_parallel_hasher_creation() {
        let hasher = ParallelHasher::new(true);
        assert!(hasher.parallel);

        let hasher = ParallelHasher::new(false);
        assert!(!hasher.parallel);
    }

    #[test]
    fn test_parallel_hasher_threshold() {
        let hasher = ParallelHasher::new_with_threshold(50);
        assert!(!hasher.parallel);

        let hasher = ParallelHasher::new_with_threshold(100);
        assert!(hasher.parallel);

        let hasher = ParallelHasher::new_with_threshold(200);
        assert!(hasher.parallel);
    }

    #[test]
    fn test_parallel_hasher_value_node() {
        let hasher = ParallelHasher::new(false);
        let value_node = Node::Value(crate::node::ValueNode::new(vec![1, 2, 3]));

        let (hashed, cached) = hasher.hash(&value_node);
        assert_eq!(hashed, value_node);
        assert_eq!(cached, value_node);
    }

    #[test]
    fn test_parallel_committer_creation() {
        let db = MemoryDB::new();
        let node_set = NodeSet::new(B256::ZERO);
        let committer = ParallelCommitter::new(db, node_set, true, false);

        assert!(committer.collect_leaf);
        assert!(!committer.parallel);
    }
}
