use std::sync::Arc;
use crate::node::Node;
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

    // pub fn hash(&self, node: Arc<Node>, force: bool) -> Result<Arc<Node>, Arc<Node>> {
    //     let(hash, _) = self.root.cache();
    //     if !hash.is_none() {
    //         Ok((hash, node))
    //     }

    //     match node {
    //         Node::ShortNode(short) => {
    //             let(collapsed, cached) = self.hash_short_node_children(short);
    //         }
    //         Node::FullNode(full) => {}
    //         _ => {
    //             return OK(node, node)
    //         }
    //     }
    // }

    // pub fn hash_short_node_children(&self, short: Arc<ShortNode>) -> Result<Arc<ShortNode>, Arc<ShortNode>> {
    //     let mut (collapsed, cached) = short.clone(), short.clone();

    //     collapsed.key = hex_to_compact(short.key);

    //     match short.val {
    //         Node::ShortNode(short), Node::FullNode(full) => {
    //             (collapsed.val, cached.val) = self.hash(short.val, false)?;
    //         }
    //         _ => {
    //             return OK(collapsed, cached)
    //         }
    //     }
    // }
}
