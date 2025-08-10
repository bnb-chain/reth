//! Node module for BSC-style trie nodes
//!
//! This module contains the implementation of different node types used in the trie:
//! - `Node`: Main enum containing all node types
//! - `FullNode`: Node with 17 children (16 hex digits + value)
//! - `ShortNode`: Extension or leaf node with key and value
//! - `HashNode`: Reference to another node by hash
//! - `ValueNode`: Leaf node containing actual data
//! - `NodeFlag`: Flags for caching and dirty state
//! - `NodeSet`: Collection of modified nodes during commit operations

/// Node decoding utilities
pub mod decode_node;
pub mod full_node;
pub mod node;
pub mod node_set;
pub mod short_node;

// Re-export main types
pub use decode_node::{must_decode_node, decode_node, DecodeError};
pub use full_node::FullNode;
pub use node::{HashNode, Node, NodeFlag, ValueNode};
pub use node_set::{NodeSet, TrieNode};
pub use short_node::ShortNode;
