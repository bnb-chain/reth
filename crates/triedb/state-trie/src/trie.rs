//! Core trie implementation for secure trie operations.

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::{Encodable, Decodable};
use reth_triedb_common::TrieDatabase;
use std::collections::HashMap;

use super::node::{Node, FullNode, ShortNode, HashNode, ValueNode};
use super::secure_trie::{SecureTrieId, SecureTrieError};
use super::node_set::{NodeSet, TrieNode};
use super::parallel_hasher::ParallelCommitter;

/// Core trie implementation
pub struct Trie<DB> {
    root: Node,
    owner: Address,
    committed: bool,
    unhashed: usize,
    uncommitted: usize,
    database: DB,
    sec_key_cache: HashMap<String, Vec<u8>>,
    #[allow(dead_code)]
    sec_key_cache_owner: Option<*const Trie<DB>>,
}

impl<DB> std::fmt::Debug for Trie<DB>
where
    DB: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trie")
            .field("root", &self.root)
            .field("owner", &self.owner)
            .field("committed", &self.committed)
            .field("unhashed", &self.unhashed)
            .field("uncommitted", &self.uncommitted)
            .field("database", &self.database)
            .field("sec_key_cache", &self.sec_key_cache)
            .field("sec_key_cache_owner", &"<raw pointer>")
            .finish()
    }
}

impl<DB> Trie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Creates a new trie with the given identifier and database
    pub fn new(id: &SecureTrieId, database: DB) -> Result<Self, SecureTrieError> {
        Ok(Self {
            root: Node::Value(ValueNode::new(Vec::new())),
            owner: id.owner,
            committed: false,
            unhashed: 0,
            uncommitted: 0,
            database,
            sec_key_cache: HashMap::new(),
            sec_key_cache_owner: None,
        })
    }

    /// Creates a new empty trie
    pub fn new_empty(database: DB) -> Self {
        Self {
            root: Node::Value(ValueNode::new(Vec::new())),
            owner: Address::ZERO,
            committed: false,
            unhashed: 0,
            uncommitted: 0,
            database,
            sec_key_cache: HashMap::new(),
            sec_key_cache_owner: None,
        }
    }

    /// Gets a value from the trie by key
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, SecureTrieError> {
        let root_copy = self.root.copy();
        let (value, new_root, _) = self.get_internal(&root_copy, key, 0)?;
        if new_root != self.root {
            self.root = new_root;
        }
        Ok(value)
    }

    /// Updates a value in the trie by key
    pub fn update(&mut self, key: &[u8], value: &[u8]) -> Result<(), SecureTrieError> {
        let value_node = if value.is_empty() {
            None
        } else {
            Some(Node::Value(ValueNode::new(value.to_vec())))
        };

        let root_copy = self.root.copy();
        let (_, new_root, _) = self.insert(&root_copy, key, value_node)?;
        self.root = new_root;
        self.committed = false;
        Ok(())
    }

    /// Deletes a value from the trie by key
    pub fn delete(&mut self, key: &[u8]) -> Result<(), SecureTrieError> {
        let root_copy = self.root.copy();
        let (_, new_root, _) = self.delete_internal(&root_copy, key, 0)?;
        self.root = new_root;
        self.committed = false;
        Ok(())
    }

    /// Gets a node from the trie by path
    pub fn get_node(&mut self, path: &[u8]) -> Result<Option<Vec<u8>>, SecureTrieError> {
        let root_copy = self.root.copy();
        let (node_data, new_root, _) = self.get_node_internal(&root_copy, path, 0)?;
        if new_root != self.root {
            self.root = new_root;
        }
        Ok(node_data)
    }

    /// Commits the trie and returns the root hash and modified node set
    pub fn commit(&mut self, collect_leaf: bool) -> Result<(B256, Option<NodeSet>), SecureTrieError> {
        if self.committed {
            return Err(SecureTrieError::AlreadyCommitted);
        }

        // Create a new node set to track modified nodes
        let mut node_set = NodeSet::new(B256::ZERO); // Use zero for account trie owner

        // Handle empty trie case
        if matches!(self.root, Node::Value(ref val) if val.0.is_empty()) {
            // TODO: Add deleted nodes tracking when tracer is implemented
            self.committed = true;
            return Ok((B256::ZERO, None));
        }

        // Determine if we should use parallel processing (similar to BSC's logic)
        let use_parallel = self.uncommitted > 100;

        if use_parallel {
            // Use parallel committer for large tries
            let mut committer = ParallelCommitter::new(
                self.database.clone(),
                node_set,
                collect_leaf,
                true, // Use parallel processing
            );

            let committed_root = committer.commit(&self.root)?;
            node_set = committer.into_node_set();

            self.root = committed_root;
        } else {
            // Use sequential processing for small tries
            let (new_root, _) = self.hash_root_with_collection(&mut node_set, collect_leaf)?;
            self.root = new_root;
        }

        self.committed = true;
        self.unhashed = 0;
        self.uncommitted = 0;

        match &self.root {
            Node::Hash(hash) => {
                if node_set.is_empty() {
                    Ok((hash.0, None))
                } else {
                    Ok((hash.0, Some(node_set)))
                }
            }
            _ => Err(SecureTrieError::Database("Failed to hash root".to_string())),
        }
    }

    /// Returns the current hash of the trie
    pub fn hash(&self) -> B256 {
        match &self.root {
            Node::Hash(hash) => hash.0,
            _ => B256::ZERO,
        }
    }

    /// Creates a copy of the trie
    pub fn copy(&self) -> Self {
        Self {
            root: self.root.copy(),
            owner: self.owner,
            committed: self.committed,
            unhashed: self.unhashed,
            uncommitted: self.uncommitted,
            database: self.database.clone(),
            sec_key_cache: self.sec_key_cache.clone(),
            sec_key_cache_owner: None,
        }
    }

    /// Returns a mutable reference to the secure key cache
    pub fn get_sec_key_cache(&mut self) -> &mut HashMap<String, Vec<u8>> {
        &mut self.sec_key_cache
    }

    /// Gets the original key from a hashed key using the cache
    pub fn get_key(&self, sha_key: &[u8]) -> Option<Vec<u8>> {
        self.sec_key_cache.get(&String::from_utf8_lossy(sha_key).to_string()).cloned()
    }

    /// Returns a reference to the database
    pub fn database(&self) -> &DB {
        &self.database
    }

    /// Returns a mutable reference to the database
    pub fn database_mut(&mut self) -> &mut DB {
        &mut self.database
    }

    fn get_internal(
        &mut self,
        node: &Node,
        key: &[u8],
        pos: usize,
    ) -> Result<(Option<Vec<u8>>, Node, bool), SecureTrieError> {
        match node {
            Node::Value(value) => {
                if pos == key.len() {
                    // 如果值是空的，返回 None 而不是空值
                    if value.0.is_empty() {
                        Ok((None, node.copy(), false))
                    } else {
                        Ok((Some(value.0.clone()), node.copy(), false))
                    }
                } else {
                    Ok((None, node.copy(), false))
                }
            }
            Node::Short(short) => {
                let prefix_len = prefix_len(&short.key, &key[pos..]);
                if prefix_len != short.key.len() {
                    return Ok((None, node.copy(), false));
                }
                let (value, new_child, resolved) = self.get_internal(&short.val, key, pos + prefix_len)?;
                if resolved {
                    let new_short = ShortNode::new(short.key.clone(), new_child);
                    Ok((value, Node::Short(new_short), true))
                } else {
                    Ok((value, node.copy(), false))
                }
            }
            Node::Full(full) => {
                if pos >= key.len() {
                    return Ok((None, node.copy(), false));
                }
                let nibble = (key[pos] & 0x0F) as usize; // 只取低4位，确保索引在0-15范围内
                if let Some(child) = &full.children[nibble] {
                    let (value, new_child, resolved) = self.get_internal(child, key, pos + 1)?;
                    if resolved {
                        let mut new_full = full.copy();
                        new_full.set_child(nibble, Some(new_child));
                        Ok((value, Node::Full(new_full), true))
                    } else {
                        Ok((value, node.copy(), false))
                    }
                } else {
                    Ok((None, node.copy(), false))
                }
            }
            Node::Hash(hash) => {
                let resolved_node = self.resolve_and_track(&hash.0, &key[..pos])?;
                let (value, new_node, _) = self.get_internal(&resolved_node, key, pos)?;
                Ok((value, new_node, true))
            }
        }
    }

    fn insert(
        &mut self,
        node: &Node,
        key: &[u8],
        value: Option<Node>,
    ) -> Result<(bool, Node, bool), SecureTrieError> {
        match node {
            Node::Value(_) => {
                if key.is_empty() {
                    Ok((true, value.unwrap_or_else(|| Node::Value(ValueNode::new(Vec::new()))), true))
                } else {
                    // Convert value node to full node
                    let mut full = FullNode::new();
                    if let Node::Value(val) = node {
                        full.set_child(16, Some(Node::Value(val.clone())));
                    }
                    let (_, new_full, _) = self.insert(&Node::Full(full), key, value)?;
                    Ok((true, new_full, true))
                }
            }
            Node::Short(short) => {
                let prefix_len = prefix_len(&short.key, key);
                if prefix_len == short.key.len() {
                    // Key matches prefix, continue with remaining key
                    let (_, new_child, _) = self.insert(&short.val, &key[prefix_len..], value)?;
                    let new_short = ShortNode::new(short.key.clone(), new_child);
                    Ok((true, Node::Short(new_short), true))
                } else {
                    // Split the short node
                    let mut full = FullNode::new();

                    // Add the existing short node
                    if prefix_len < short.key.len() {
                        let new_short = ShortNode::new(short.key[prefix_len + 1..].to_vec(), short.val.copy());
                        full.set_child(short.key[prefix_len] as usize, Some(Node::Short(new_short)));
                    } else {
                        full.set_child(16, Some(short.val.copy()));
                    }

                    // Add the new value
                    if prefix_len < key.len() {
                        let new_short = ShortNode::new(key[prefix_len + 1..].to_vec(), value.unwrap_or_else(|| Node::Value(ValueNode::new(Vec::new()))));
                        full.set_child(key[prefix_len] as usize, Some(Node::Short(new_short)));
                    } else {
                        full.set_child(16, value);
                    }

                    // Create new short node for common prefix
                    if prefix_len > 0 {
                        let new_short = ShortNode::new(key[..prefix_len].to_vec(), Node::Full(full));
                        Ok((true, Node::Short(new_short), true))
                    } else {
                        Ok((true, Node::Full(full), true))
                    }
                }
            }
            Node::Full(full) => {
                if key.is_empty() {
                    let mut new_full = full.copy();
                    new_full.set_child(16, value);
                    return Ok((true, Node::Full(new_full), true));
                }
                let nibble = (key[0] & 0x0F) as usize; // 只取低4位，确保索引在0-15范围内
                let child = full.get_child(nibble).cloned().unwrap_or_else(|| Node::Value(ValueNode::new(Vec::new())));
                let (_, new_child, _) = self.insert(&child, &key[1..], value)?;
                let mut new_full = full.copy();
                new_full.set_child(nibble, Some(new_child));
                Ok((true, Node::Full(new_full), true))
            }
            Node::Hash(hash) => {
                let resolved_node = self.resolve_and_track(&hash.0, &key[..key.len().saturating_sub(1)])?;
                let (_, new_node, _) = self.insert(&resolved_node, key, value)?;
                Ok((true, new_node, true))
            }
        }
    }

    fn delete_internal(
        &mut self,
        node: &Node,
        key: &[u8],
        pos: usize,
    ) -> Result<(bool, Node, bool), SecureTrieError> {
        match node {
            Node::Value(_) => {
                if pos == key.len() {
                    Ok((true, Node::Value(ValueNode::new(Vec::new())), true))
                } else {
                    Ok((false, node.copy(), false))
                }
            }
            Node::Short(short) => {
                let prefix_len = prefix_len(&short.key, &key[pos..]);
                if prefix_len != short.key.len() {
                    return Ok((false, node.copy(), false));
                }
                let (deleted, new_child, resolved) = self.delete_internal(&short.val, key, pos + prefix_len)?;
                if deleted && matches!(new_child, Node::Value(ref val) if val.0.is_empty()) {
                    // Remove empty short node
                    Ok((true, Node::Value(ValueNode::new(Vec::new())), true))
                } else if resolved {
                    let new_short = ShortNode::new(short.key.clone(), new_child);
                    Ok((deleted, Node::Short(new_short), true))
                } else {
                    Ok((deleted, node.copy(), false))
                }
            }
            Node::Full(full) => {
                if pos >= key.len() {
                    let mut new_full = full.copy();
                    new_full.set_child(16, Some(Node::Value(ValueNode::new(Vec::new()))));
                    return Ok((true, Node::Full(new_full), true));
                }
                let nibble = (key[pos] & 0x0F) as usize; // 只取低4位，确保索引在0-15范围内
                if let Some(child) = &full.children[nibble] {
                    let (deleted, new_child, resolved) = self.delete_internal(child, key, pos + 1)?;
                    if resolved {
                        let mut new_full = full.copy();
                        new_full.set_child(nibble, Some(new_child));
                        Ok((deleted, Node::Full(new_full), true))
                    } else {
                        Ok((deleted, node.copy(), false))
                    }
                } else {
                    Ok((false, node.copy(), false))
                }
            }
            Node::Hash(hash) => {
                let resolved_node = self.resolve_and_track(&hash.0, &key[..pos])?;
                let (deleted, new_node, _) = self.delete_internal(&resolved_node, key, pos)?;
                Ok((deleted, new_node, true))
            }
        }
    }

    fn get_node_internal(
        &mut self,
        node: &Node,
        path: &[u8],
        pos: usize,
    ) -> Result<(Option<Vec<u8>>, Node, usize), SecureTrieError> {
        match node {
            Node::Value(_value) => {
                if pos == path.len() {
                    let mut encoded = Vec::new();
                    node.encode(&mut encoded);
                    Ok((Some(encoded), node.copy(), pos))
                } else {
                    Ok((None, node.copy(), pos))
                }
            }
            Node::Short(short) => {
                let prefix_len = prefix_len(&short.key, &path[pos..]);
                if prefix_len != short.key.len() {
                    return Ok((None, node.copy(), pos));
                }
                let (item, new_child, resolved) = self.get_node_internal(&short.val, path, pos + prefix_len)?;
                if resolved > 0 {
                    let new_short = ShortNode::new(short.key.clone(), new_child);
                    Ok((item, Node::Short(new_short), pos + prefix_len))
                } else {
                    Ok((item, node.copy(), pos))
                }
            }
            Node::Full(full) => {
                if pos >= path.len() {
                    let mut encoded = Vec::new();
                    node.encode(&mut encoded);
                    return Ok((Some(encoded), node.copy(), pos));
                }
                let nibble = (path[pos] & 0x0F) as usize; // 只取低4位，确保索引在0-15范围内
                if let Some(child) = &full.children[nibble] {
                    let (item, new_child, resolved) = self.get_node_internal(child, path, pos + 1)?;
                    if resolved > 0 {
                        let mut new_full = full.copy();
                        new_full.set_child(nibble, Some(new_child));
                        Ok((item, Node::Full(new_full), pos + 1))
                    } else {
                        Ok((item, node.copy(), pos))
                    }
                } else {
                    Ok((None, node.copy(), pos))
                }
            }
            Node::Hash(hash) => {
                let resolved_node = self.resolve_and_track(&hash.0, &path[..pos])?;
                let (item, new_node, _) = self.get_node_internal(&resolved_node, path, pos)?;
                Ok((item, new_node, pos))
            }
        }
    }

    fn resolve_and_track(&mut self, hash: &B256, _prefix: &[u8]) -> Result<Node, SecureTrieError> {
        let data = self.database.get(hash).map_err(|e| SecureTrieError::Database(format!("{:?}", e)))?;
        let data = data.ok_or(SecureTrieError::NodeNotFound)?;
        let node = Node::decode(&mut &data[..]).map_err(|_| SecureTrieError::InvalidNode)?;
        Ok(node)
    }

    fn hash_root_with_collection(&mut self, node_set: &mut NodeSet, collect_leaf: bool) -> Result<(Node, bool), SecureTrieError> {
        match &self.root {
            Node::Hash(_) => Ok((self.root.copy(), false)),
            _ => {
                let mut encoded = Vec::new();
                self.root.encode(&mut encoded);
                let hash = keccak256(&encoded);

                // Store the node in the database
                self.database.insert(hash, encoded.clone()).map_err(|e| SecureTrieError::Database(format!("{:?}", e)))?;

                // Add the root node to the node set
                node_set.add_node(&[], TrieNode::new(hash, encoded.clone()));

                // If collect_leaf is true and this is a leaf node, add it to leaves
                if collect_leaf {
                    if let Node::Value(_) = &self.root {
                        node_set.add_leaf(hash, encoded);
                    }
                }

                Ok((Node::Hash(HashNode::new(hash)), true))
            }
        }
    }
}

/// Helper function to find the common prefix length between two byte slices
fn prefix_len(a: &[u8], b: &[u8]) -> usize {
    let min_len = a.len().min(b.len());
    for i in 0..min_len {
        if a[i] != b[i] {
            return i;
        }
    }
    min_len
}
