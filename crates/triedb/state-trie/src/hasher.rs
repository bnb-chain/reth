//! Hasher structure for computing trie hashes
use std::sync::Arc;
use crate::node::{Node, ShortNode};
use super::encoding::hex_to_compact;

/// Hasher structure for computing trie hashes
#[derive(Clone, Debug)]
pub struct Hasher {
    /// Root node of the trie
    pub root: Arc<Node>,
    /// Whether to use parallel processing
    pub parallel: bool,
}

impl Hasher {
    /// Create a new Hasher instance
    ///
    /// # Arguments
    /// * `root` - The root node of the trie
    /// * `parallel` - Whether to enable parallel processing
    pub fn new(root: Arc<Node>, parallel: bool) -> Self {
        Self {
            root,
            parallel,
        }
    }

    pub fn hash(&self, node: Arc<Node>, force: bool) -> Result<Arc<Node>, Arc<Node>> {
        let (hash, _) = self.root.cache();
        if !hash.is_none() {
            return Ok(node);
        }

        match node.as_ref() {
            Node::ShortNode(short) => {
                let (collapsed, cached) = self.hash_short_node_children(Arc::clone(short))?;
                Ok(collapsed)
            }
            Node::FullNode(_full) => {
                // TODO: Implement full node hashing
                Ok(node)
            }
            _ => {
                Ok(node)
            }
        }
    }

    pub fn hash_short_node_children(&self, short: Arc<ShortNode>) -> Result<(Arc<ShortNode>, Arc<ShortNode>), Arc<ShortNode>> {
        let mut collapsed = (*short).clone();
        let mut cached = (*short).clone();

        collapsed.key = hex_to_compact(&short.key);

        match &short.val {
            Node::ShortNode(child_short) => {
                let (new_collapsed, new_cached) = self.hash(Arc::clone(child_short), false)?;
                collapsed.val = new_collapsed;
                cached.val = new_cached;
            }
            Node::FullNode(child_full) => {
                let (new_collapsed, new_cached) = self.hash(Arc::clone(child_full), false)?;
                collapsed.val = new_collapsed;
                cached.val = new_cached;
            }
            _ => {
                // No changes needed for other node types
            }
        }

        Ok((Arc::new(collapsed), Arc::new(cached)))
    }
}
