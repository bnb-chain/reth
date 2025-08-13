//! Main node types and traits for trie operations.
//!
//! This module contains the core Node enum and NodeFlag
//! structure that are shared across all node implementations.

#[allow(unused_imports)]
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use std::sync::Arc;
use alloy_primitives::B256;

use super::{FullNode, ShortNode};

/// Hash node (reference to another node)
/// A hash node is a reference to another node by its hash, used for
/// efficient storage and retrieval in the trie.
pub type HashNode = B256;

/// Value node (leaf value)
/// A value node is a leaf node that contains the actual data stored
/// in the trie, representing the end of a trie path.
pub type ValueNode = Vec<u8>;

/// Node types in the BSC-style trie
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Empty root node
    EmptyRoot,
    /// Full node with 17 children
    Full(Arc<FullNode>),
    /// Short node (extension or leaf)
    Short(Arc<ShortNode>),
    /// Hash node (reference to another node)
    Hash(HashNode),
    /// Value node (leaf value)
    Value(ValueNode),
}

impl Default for Node {
    fn default() -> Self {
        Node::EmptyRoot
    }
}

impl Node {
    /// Get the cached hash and dirty state
    pub fn cache(&self) -> (Option<HashNode>, bool) {
        match self {
            Node::Full(full) => return full.cache(),
            Node::Short(short) => return short.cache(),
            Node::Hash(_) => return (None, false),
            Node::Value(_) => return (None, false),
            Node::EmptyRoot => return (None, false),
        }
    }
}

/// Node flags for caching and dirty state
#[derive(Debug, Clone, PartialEq)]
pub struct NodeFlag {
    /// Cached hash of the node
    pub hash: Option<HashNode>,
    /// Whether the node has been modified
    pub dirty: bool,
}

impl Default for NodeFlag {
    fn default() -> Self {
        Self {
            hash: None,
            dirty: true,
        }
    }
}

impl NodeFlag {
    /// Sets the dirty flag and returns self for chaining
    pub fn with_dirty(mut self, dirty: bool) -> Self {
        self.dirty = dirty;
        self
    }
}
