//! Core trie implementation for secure trie operations.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{B256};
use alloy_trie::EMPTY_ROOT_HASH;
use reth_triedb_common::TrieDatabase;

use super::encoding::{common_prefix_length, key_to_nibbles};
use super::node::{Node, NodeFlag, FullNode, ShortNode, must_decode_node};
use super::secure_trie::{SecureTrieId, SecureTrieError};
use super::trie_hasher::Hasher;

/// Core trie implementation
#[derive(Clone, Debug)]
pub struct Trie<DB> {
    root: Arc<Node>,
    #[allow(dead_code)]
    owner: B256,
    committed: bool,
    unhashed: usize,
    uncommitted: usize,
    database: DB,
    #[allow(dead_code)]
    sec_key_cache: HashMap<String, Vec<u8>>,
    difflayer: Option<HashMap<String, Arc<Node>>>,
    #[allow(dead_code)]
    sec_key_cache_owner: Option<*const Trie<DB>>,
}

/// Basic Trie operations
impl<DB> Trie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Creates a new trie with the given identifier and database
    pub fn new(id: &SecureTrieId, database: DB, difflayer: Option<HashMap<String, Arc<Node>>>) -> Result<Self, SecureTrieError> {
        let mut tr = Self {
            root: Arc::new(Node::EmptyRoot),
            owner: id.owner,
            committed: false,
            unhashed: 0,
            uncommitted: 0,
            database,
            sec_key_cache: HashMap::new(),
            difflayer: difflayer,
            sec_key_cache_owner: None,
        };

        // Check if this is an empty trie (root is EmptyRootHash)
        let root = if id.state_root == alloy_trie::EMPTY_ROOT_HASH {
            Arc::new(Node::EmptyRoot)
        } else if id.state_root == B256::ZERO {
            Arc::new(Node::EmptyRoot)
        } else {
            let root = tr.resolve_and_track(&id.state_root, &[])?;
            root
        };
        tr.root = root;

        Ok(tr)
    }

    /// Sets the difflayer for the trie
    pub fn with_difflayer(&mut self, difflayer: &HashMap<String, Arc<Node>>) -> &mut Self {
        self.difflayer = Some(difflayer.clone());
        self
    }

    /// Creates a new flag for the trie
    pub fn new_flag(&self) -> NodeFlag {
        NodeFlag::default()
    }

    /// Gets the root node of the trie
    pub fn root(&self) -> Arc<Node> {
        self.root.clone()
    }

    /// Gets the root hash of the trie
    pub fn hash(&mut self) -> B256 {
        if self.root == Arc::new(Node::EmptyRoot) {
            return EMPTY_ROOT_HASH;
        }
        let hasher = Hasher::new(self.unhashed > 100);
        let(hashed, cached) = hasher.hash(self.root.clone(), true);
        self.root = cached;
        if let Node::Hash(h) = &*hashed {
            *h
        } else {
            panic!("Expected Hash node, got: {:?}", hashed);
        }
    }
}

/// Trie interface
impl<DB> Trie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Gets a value from the trie by key
    /// Gets a value from the trie by key
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, SecureTrieError> {
        // Check if trie is already committed
        if self.committed {
            return Err(SecureTrieError::AlreadyCommitted);
        }

        // Convert key to nibbles + terminator format
        let nibbles_key = key_to_nibbles(key);

        // Get value from internal trie structure
        let (value, new_root, did_resolve) = self.get_internal(
            Arc::clone(&self.root),
            nibbles_key,
            0
        )?;

        // Update root if it was resolved (CoW optimization)
        if did_resolve {
            self.root = new_root;
        }

        // Return the found value (or None if not found)
        Ok(value)
    }

    /// Updates a value in the trie by key
    pub fn update(&mut self, key: &[u8], value: &[u8]) -> Result<(), SecureTrieError> {
        // Check if trie is already committed
        if self.committed {
            return Err(SecureTrieError::AlreadyCommitted);
        }

        // Update trie statistics
        self.unhashed += 1;
        self.uncommitted += 1;

        // Create value node from input value
        let value_node = if value.is_empty() {
            None
        } else {
            Some(Node::Value(value.to_vec()))
        };

        // Convert key to nibbles + terminator format
        let nibbles_key = key_to_nibbles(key);

        // Handle empty value (delete operation)
        if value_node.is_none() {
            // Delete the value from the trie
            let (_, new_root) = self.delete_internal(
                self.root.clone(),
                vec![],
                nibbles_key)?;

            // Update the root with the new trie structure
            self.root = new_root;
        } else {
            // Insert the new value into the trie
            let (_, new_root) = self.insert_internal(
                self.root.clone(),
                vec![],
                nibbles_key,
                Arc::new(value_node.unwrap())
            )?;

            // Update the root with the new trie structure
            self.root = new_root;
        }

        Ok(())
    }

    /// Deletes a value from the trie by key
    pub fn delete(&mut self, key: &[u8]) -> Result<(), SecureTrieError> {
        // Check if trie is already committed
        if self.committed {
            return Err(SecureTrieError::AlreadyCommitted);
        }

        // Update trie statistics
        self.unhashed += 1;
        self.uncommitted += 1;

        // Convert key to nibbles + terminator format
        let nibbles_key = key_to_nibbles(key);

        // Delete the value from the trie
        let (_, new_root) = self.delete_internal(
            self.root.clone(),
            vec![],
            nibbles_key
        )?;

        // Update the root with the new trie structure
        self.root = new_root;

        Ok(())
    }
}

/// Trie internal implementation
impl<DB> Trie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Gets a value from the trie by key
    /// Internal function to get a value from the trie
    /// Returns: (value, new_node, resolved)
    /// - value: The found value or None
    /// - new_node: The potentially updated node (for CoW)
    /// - resolved: Whether the node was resolved from hash
    fn get_internal(
        &self, node: Arc<Node>,
        nibbles_key: Vec<u8>,
        pos: usize
    ) -> Result<(Option<Vec<u8>>, Arc<Node>, bool), SecureTrieError> {
        match &*node {
            // Empty root - no value found
            Node::EmptyRoot => {
                Ok((None, node, false))
            }

            // Value node - return the stored value
            Node::Value(value) => {
                Ok((Some(value.clone()), node, false))
            }

            // Short node - check if key matches and continue traversal
            Node::Short(short) => {
                // Check if the remaining key starts with the short node's key
                if !nibbles_key[pos..].starts_with(&short.key) {
                    return Ok((None, node, false));
                }

                // Recursively get from the child node
                let (value, new_child, resolved) = self.get_internal(
                    Arc::clone(&short.val),
                    nibbles_key,
                    pos + short.key.len()
                )?;

                // If child was resolved, create a new short node with CoW
                if resolved {
                    let mut new_short = short.to_mutable_copy_with_cow();
                    new_short.set_value(&new_child);
                    let new_node = Arc::new(Node::Short(Arc::new(new_short)));
                    Ok((value, new_node, true))
                } else {
                    Ok((value, node, false))
                }
            }

            // Full node - traverse to the appropriate child
            Node::Full(full) => {
                let nibble = nibbles_key[pos] as usize;
                // Recursively get from the child node
                let (value, new_child, resolved) = self.get_internal(
                    full.get_child(nibble),
                    nibbles_key,
                    pos + 1
                )?;

                // If child was resolved, create a new full node with CoW
                if resolved {
                    let mut new_full = full.to_mutable_copy_with_cow();
                    new_full.set_child(nibble, &new_child);
                    let new_node = Arc::new(Node::Full(Arc::new(new_full)));
                    Ok((value, new_node, true))
                } else {
                    Ok((value, node, false))
                }
            }

            // Hash node - resolve and continue traversal
            Node::Hash(hash) => {
                let resolved_node = self.resolve_and_track(
                    &hash,
                    &nibbles_key[..pos]
                )?;
                let (value, new_node, _) = self.get_internal(resolved_node, nibbles_key, pos)?;
                Ok((value, new_node, true))
            }
        }
    }

        /// Internal function to insert a value into the trie
    /// Returns: (dirty, new_node)
    /// - dirty: Whether the node was modified
    /// - new_node: The potentially updated node (for CoW)
    fn insert_internal(
        &mut self, node: Arc<Node>,
        prefix: Vec<u8>,
        nibbles_key: Vec<u8>,
        value: Arc<Node>
    ) -> Result<(bool, Arc<Node>), SecureTrieError> {
        // Base case: reached the end of the key
        if nibbles_key.len() == 0 {
            match &*node {
                Node::Value(existing_value) => {
                    if let Node::Value(new_value) = &*value {
                        if existing_value == new_value {
                            // No change needed
                            return Ok((false, node));
                        } else {
                            // Value changed, need update
                            return Ok((true, value));
                        }
                    } else {
                        // Replace with new value
                        return Ok((true, value));
                    }
                }
                _ => {
                    // Replace with new value
                    return Ok((true, value));
                }
            }
        }

        match &*node {
            // Short node - handle key matching and splitting
            Node::Short(short) => {
                let matchlen = common_prefix_length(&nibbles_key, &short.key);

                // If the short node's key is a prefix of the insertion key
                if matchlen == short.key.len() {
                    let mut new_prefix = prefix.clone();
                    new_prefix.extend(&nibbles_key[..matchlen]);

                    let (dirty, new_child) = self.insert_internal(
                        short.val.clone(),
                        new_prefix,
                        nibbles_key[matchlen..].to_vec(),
                        value
                    )?;

                    if !dirty {
                        return Ok((false, node));
                    } else {
                        let new_short = ShortNode {
                            key: short.key.clone(),
                            val: new_child,
                            flags: self.new_flag(),
                        };
                        return Ok((true, Arc::new(Node::Short(Arc::new(new_short)))));
                    }
                }

                // Create a branch node to split the short node
                let mut branch = FullNode::new();

                // Insert the short node's remaining key into the branch
                let mut short_prefix = prefix.clone();
                short_prefix.extend(&short.key[..matchlen + 1]);

                let (_, new_child1) = self.insert_internal(
                    Arc::new(Node::EmptyRoot),
                    short_prefix,
                    short.key[matchlen + 1..].to_vec(),
                    Arc::clone(&short.val)
                )?;
                branch.set_child(short.key[matchlen] as usize, new_child1.as_ref());

                // Insert the new key into the branch
                let mut new_prefix = prefix.clone();
                new_prefix.extend(&nibbles_key[..matchlen + 1]);
                let (_, new_child2) = self.insert_internal(
                    Arc::new(Node::EmptyRoot),
                    new_prefix,
                    nibbles_key[matchlen + 1..].to_vec(),
                    value
                )?;
                branch.set_child(nibbles_key[matchlen] as usize, new_child2.as_ref());

                // If no common prefix, return the branch directly
                if matchlen == 0 {
                    return Ok((true, Arc::new(Node::Full(Arc::new(branch)))));
                }

                // Create a new short node with the common prefix
                let new_short = ShortNode {
                    key: nibbles_key[..matchlen].to_vec(),
                    val: Arc::new(Node::Full(Arc::new(branch))),
                    flags: self.new_flag(),
                };
                return Ok((true, Arc::new(Node::Short(Arc::new(new_short)))));
            }

            // Full node - traverse to appropriate child
            Node::Full(full) => {
                let mut new_prefix = prefix.clone();
                new_prefix.extend(&nibbles_key[0..1]);

                let child = full.get_child(nibbles_key[0] as usize);
                let (dirty, new_child) = self.insert_internal(
                    child,
                    new_prefix,
                    nibbles_key[1..].to_vec(),
                    value
                )?;

                if !dirty {
                    return Ok((false, node));
                } else {
                    let mut new_full = full.to_mutable_copy_with_cow();
                    new_full.flags = self.new_flag();
                    new_full.set_child(nibbles_key[0] as usize, &new_child);
                    return Ok((true, Arc::new(Node::Full(Arc::new(new_full)))));
                }
            }

            // Empty root - create new short node
            Node::EmptyRoot => {
                let new_short = ShortNode::new(nibbles_key, value.as_ref());
                return Ok((true, Arc::new(Node::Short(Arc::new(new_short)))));
            }

            // Hash node - resolve and continue insertion
            Node::Hash(hash) => {
                let resolved_node = self.resolve_and_track(hash, &prefix.to_vec())?;
                let (dirty, new_node) = self.insert_internal(
                    Arc::clone(&resolved_node),
                    prefix,
                    nibbles_key,
                    value
                )?;

                if !dirty {
                    return Ok((false, resolved_node));
                } else {
                    return Ok((true, new_node));
                }
            }

            // Value node should not be in the trie structure
            Node::Value(_) => {
                panic!("Value node should not be in the trie");
            }
        }
    }

    /// Internal function to delete a value from the trie
    /// Returns: (dirty, new_node)
    /// - dirty: Whether the node was modified
    /// - new_node: The potentially updated node (for CoW)
    pub fn delete_internal(
        &mut self,
        node: Arc<Node>,
        prefix: Vec<u8>,
        nibbles_key: Vec<u8>
    ) -> Result<(bool, Arc<Node>), SecureTrieError> {

        match &*node {
            // Handle ShortNode deletion
            Node::Short(short) => {
                let matchlen = common_prefix_length(&nibbles_key, &short.key);

                // Key doesn't match this short node - no deletion needed
                if matchlen < short.key.len() {
                    return Ok((false, Arc::clone(&node)));
                }

                // Complete key match - delete this node by returning EmptyRoot
                if matchlen == nibbles_key.len() {
                    return Ok((true, Arc::new(Node::EmptyRoot)));
                }

                // Partial match - continue deletion in child node
                let mut new_prefix = prefix.clone();
                new_prefix.extend(&nibbles_key[..short.key.len()]);

                let (dirty, new_child) = self.delete_internal(
                    short.val.clone(),
                    new_prefix,
                    nibbles_key[short.key.len()..].to_vec()
                )?;

                // Child wasn't modified - return unchanged node
                if !dirty {
                    return Ok((false, Arc::clone(&node)));
                }

                // Child was modified - handle the result
                match &*new_child {
                    Node::Short(new_child_short) => {
                        // Merge keys when child is also a ShortNode
                        let mut merged_key = short.key.clone();
                        merged_key.extend(&new_child_short.key);

                        let new_short = ShortNode {
                            key: merged_key,
                            val: new_child_short.val.clone(),
                            flags: self.new_flag(),
                        };
                        Ok((true, Arc::new(Node::Short(Arc::new(new_short)))))
                    }
                    _ => {
                        // Keep current key, update child
                        let new_short = ShortNode {
                            key: short.key.clone(),
                            val: new_child,
                            flags: self.new_flag(),
                        };
                        Ok((true, Arc::new(Node::Short(Arc::new(new_short)))))
                    }
                }
            }

            // Handle FullNode deletion
            Node::Full(full) => {
                // Prepare prefix for recursive call
                let mut new_prefix = prefix.clone();
                new_prefix.extend(&nibbles_key[0..1]);

                // Get child index from first nibble
                let child_index = nibbles_key[0] as usize;

                // Recursively delete from child
                let (dirty, new_child) = self.delete_internal(
                    full.get_child(child_index),
                    new_prefix,
                    nibbles_key[1..].to_vec(),
                )?;

                // Child wasn't modified - return unchanged node
                if !dirty {
                    return Ok((false, Arc::clone(&node)));
                }

                // Create modified copy with new child
                let mut new_full = full.to_mutable_copy_with_cow();
                new_full.flags = self.new_flag();
                new_full.set_child(child_index, &new_child);
                let full_copy = new_full.clone();

                match &*new_child {
                    Node::EmptyRoot => {
                        // Child became empty - check if we can collapse the FullNode
                        let mut non_empty_pos = -1i32;
                        let mut non_empty_count = 0;

                        // Count non-empty children and find their position
                        for (i, child) in full_copy.children.iter().enumerate() {
                            if !matches!(&**child, Node::EmptyRoot) {
                                non_empty_count += 1;
                                if non_empty_pos == -1 {
                                    non_empty_pos = i as i32;
                                } else {
                                    non_empty_pos = -2; // Multiple children
                                    break;
                                }
                            }
                        }

                        if non_empty_pos >= 0 && non_empty_count == 1 {
                            // Only one non-empty child - collapse to ShortNode
                            let pos_nibbles = vec![non_empty_pos as u8];

                            if non_empty_pos != 16 {
                                // Non-value child - try to merge with ShortNode
                                let mut child_prefix = prefix.clone();
                                child_prefix.extend(&pos_nibbles);

                                let resolved_child = self.resolve(
                                    full_copy.get_child(non_empty_pos as usize),
                                    &child_prefix.to_vec()
                                )?;

                                if let Node::Short(child_short) = &*resolved_child {
                                    // Merge with child ShortNode
                                    let mut merged_key = vec![non_empty_pos as u8];
                                    merged_key.extend(&child_short.key);

                                    let new_short = ShortNode {
                                        key: merged_key,
                                        val: child_short.val.clone(),
                                        flags: self.new_flag(),
                                    };
                                    return Ok((true, Arc::new(Node::Short(Arc::new(new_short)))));
                                }
                            }

                            // Create ShortNode with single child
                            let new_short = ShortNode {
                                key: pos_nibbles,
                                val: full_copy.get_child(non_empty_pos as usize),
                                flags: self.new_flag(),
                            };
                            Ok((true, Arc::new(Node::Short(Arc::new(new_short)))))
                        } else {
                            // Multiple children remain - keep as FullNode
                            Ok((true, Arc::new(Node::Full(Arc::new(full_copy)))))
                        }
                    }
                    _ => {
                        // Child is not empty - keep as FullNode
                        Ok((true, Arc::new(Node::Full(Arc::new(full_copy)))))
                    }
                }
            }

            // Handle ValueNode deletion - replace with EmptyRoot
            Node::Value(_) => {
                Ok((true, Arc::new(Node::EmptyRoot)))
            }

            // Handle EmptyRoot - nothing to delete
            Node::EmptyRoot => {
                Ok((false, Arc::new(Node::EmptyRoot)))
            }

            // Handle HashNode - resolve and recurse
            Node::Hash(hash) => {
                let resolved_node = self.resolve_and_track(hash, &prefix.to_vec())?;
                let resolved_node_backup = Arc::clone(&resolved_node);

                let (dirty, new_node) = self.delete_internal(
                    resolved_node,
                    prefix,
                    nibbles_key
                )?;

                if !dirty {
                    Ok((false, resolved_node_backup))
                } else {
                    Ok((true, new_node))
                }
            }
        }
    }
}

// Trie Helper operations
impl<DB> Trie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{

    /// Resolves a node from a hash
    fn resolve(&self, node: Arc<Node> , _prefix: &[u8]) -> Result<Arc<Node>, SecureTrieError> {
        match &*node {
            Node::Hash(hash) => {
                return self.resolve_and_track(hash, _prefix);
            }
            _ => {
                return Ok(node);
            }
        }
    }

    /// Resolves a hash and tracks it in the difflayer
    fn resolve_and_track(&self, hash: &B256, _prefix: &[u8]) -> Result<Arc<Node>, SecureTrieError> {
        // 1. Check if the hash is in the difflayer
        if let Some(difflayer) = &self.difflayer {
            if let Some(node) = difflayer.get(&hash.to_string()) {
                return Ok(node.clone());
            }
        }

        // 2. Check if the hash is in the database
        if let Some(node_data) = self.database.get(hash).map_err(|e| SecureTrieError::Database(format!("{:?}", e)))? {
            let node = must_decode_node(Some(*hash), &node_data);
            return Ok(node);
        }

        return Ok(Arc::new(Node::EmptyRoot))
    }

}
// Debug implementation for Trie
impl<DB> Trie<DB>
where
    DB: TrieDatabase + Clone + Send + Sync,
    DB::Error: std::fmt::Debug,
{
    /// Debug method to print the trie structure in a tree format
    pub fn debug_print(&self) {
        println!("=== TRIE STRUCTURE ===");
        self.debug_print_node(&self.root, "", true);
        println!("=====================");
    }

    /// Internal method to recursively print node structure
    fn debug_print_node(&self, node: &Arc<Node>, prefix: &str, is_last: bool) {
        let connector = if is_last { "└── " } else { "├── " };

        match &**node {
            Node::EmptyRoot => {
                println!("{}{}EmptyRoot", prefix, connector);
            }
            Node::Value(value) => {
                println!("{}{}Value: {}",
                    prefix, connector,
                    hex::encode(value));
            }
            Node::Short(short) => {
                println!("{}{}Short: key={}",
                    prefix, connector,
                    hex::encode(&short.key));
                let new_prefix = format!("{}    ", prefix);
                self.debug_print_node(&short.val, &new_prefix, true);
            }
            Node::Full(full) => {
                println!("{}{}Full:", prefix, connector);
                let new_prefix = format!("{}    ", prefix);

                // Print non-empty children
                let mut non_empty_count = 0;
                for (_i, child) in full.children.iter().enumerate() {
                    if !matches!(&**child, Node::EmptyRoot) {
                        non_empty_count += 1;
                    }
                }

                let mut current_count = 0;
                for (i, child) in full.children.iter().enumerate() {
                    if !matches!(&**child, Node::EmptyRoot) {
                        current_count += 1;
                        let is_last_child = current_count == non_empty_count;

                        let child_prefix = if i == 16 {
                            format!("{}[VALUE]", new_prefix)
                        } else {
                            format!("{}[{:x}]", new_prefix, i)
                        };

                        self.debug_print_node(child, &child_prefix, is_last_child);
                    }
                }

                if non_empty_count == 0 {
                    println!("{}    (no children)", new_prefix);
                }
            }
            Node::Hash(hash) => {
                println!("{}{}Hash: {:02x?}", prefix, connector, hash);
            }
        }
    }
}
