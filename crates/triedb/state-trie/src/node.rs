use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use std::fmt;

/// Node types in the BSC-style trie
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Full node with 17 children
    Full(FullNode),
    /// Short node (extension or leaf)
    Short(ShortNode),
    /// Hash node (reference to another node)
    Hash(HashNode),
    /// Value node (leaf value)
    Value(ValueNode),
}

/// Full node with 17 children (16 hex digits + value)
#[derive(Debug, Clone, PartialEq)]
pub struct FullNode {
    /// Array of 17 children (16 hex digits + value)
    pub children: [Option<Box<Node>>; 17],
    /// Node flags for caching and dirty state
    pub flags: NodeFlag,
}

/// Short node (extension or leaf)
#[derive(Debug, Clone, PartialEq)]
pub struct ShortNode {
    /// Key bytes for the short node
    pub key: Vec<u8>,
    /// Value node
    pub val: Box<Node>,
    /// Node flags for caching and dirty state
    pub flags: NodeFlag,
}

/// Hash node (reference to another node)
#[derive(Debug, Clone, PartialEq)]
pub struct HashNode(pub B256);

/// Value node (leaf value)
#[derive(Debug, Clone, PartialEq)]
pub struct ValueNode(pub Vec<u8>);

/// Node flags for caching and dirty state
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeFlag {
    /// Cached hash of the node
    pub hash: Option<B256>,
    /// Whether the node has been modified
    pub dirty: bool,
}

impl Node {
    /// Returns the cached hash and dirty state of the node
    pub fn cache(&self) -> (Option<B256>, bool) {
        match self {
            Node::Full(n) => (n.flags.hash, n.flags.dirty),
            Node::Short(n) => (n.flags.hash, n.flags.dirty),
            Node::Hash(_) => (None, true),
            Node::Value(_) => (None, true),
        }
    }

    /// Creates a deep copy of the node
    pub fn copy(&self) -> Self {
        match self {
            Node::Full(n) => Node::Full(n.copy()),
            Node::Short(n) => Node::Short(n.copy()),
            Node::Hash(h) => Node::Hash(h.clone()),
            Node::Value(v) => Node::Value(v.clone()),
        }
    }

    /// Checks if the node is empty (no children)
    pub fn is_empty(&self) -> bool {
        match self {
            Node::Full(n) => n.children.iter().all(|c| c.is_none()),
            Node::Short(_) => false,
            Node::Hash(_) => false,
            Node::Value(_) => false,
        }
    }
}

impl FullNode {
    /// Creates a new empty full node
    pub fn new() -> Self {
        Self {
            children: std::array::from_fn(|_| None),
            flags: NodeFlag::default(),
        }
    }

    /// Creates a deep copy of the full node
    pub fn copy(&self) -> Self {
        Self {
            children: self.children.clone(),
            flags: self.flags.clone(),
        }
    }

    /// Sets a child at the specified index
    pub fn set_child(&mut self, index: usize, child: Option<Node>) {
        self.children[index] = child.map(Box::new);
        self.flags.dirty = true;
    }

    /// Gets a reference to the child at the specified index
    pub fn get_child(&self, index: usize) -> Option<&Node> {
        self.children[index].as_ref().map(|n| n.as_ref())
    }

    /// Gets a mutable reference to the child at the specified index
    pub fn get_child_mut(&mut self, index: usize) -> Option<&mut Node> {
        self.children[index].as_mut().map(|n| n.as_mut())
    }

    /// Checks if the full node is empty (no children)
    pub fn is_empty(&self) -> bool {
        self.children.iter().all(|c| c.is_none())
    }
}

impl ShortNode {
    /// Creates a new short node with the given key and value
    pub fn new(key: Vec<u8>, val: Node) -> Self {
        Self {
            key,
            val: Box::new(val),
            flags: NodeFlag::default(),
        }
    }

    /// Creates a deep copy of the short node
    pub fn copy(&self) -> Self {
        Self {
            key: self.key.clone(),
            val: Box::new(self.val.copy()),
            flags: self.flags.clone(),
        }
    }
}

impl HashNode {
    /// Creates a new hash node with the given hash
    pub fn new(hash: B256) -> Self {
        Self(hash)
    }
}

impl ValueNode {
    /// Creates a new value node with the given value
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }
}

// RLP encoding for nodes
impl Encodable for Node {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Node::Full(n) => n.encode(out),
            Node::Short(n) => n.encode(out),
            Node::Hash(h) => h.encode(out),
            Node::Value(v) => v.encode(out),
        }
    }
}

impl Decodable for Node {
    fn decode(buf: &mut &[u8]) -> Result<Self, RlpError> {
        let header = alloy_rlp::Header::decode(buf)?;

        if header.payload_length == 0 {
            return Err(RlpError::Custom("empty node"));
        }

        // Check if it's a list (full node or short node)
        if header.list {
            let mut items = Vec::new();
            let mut remaining = header.payload_length;
            let _start = buf.len();

            while remaining > 0 {
                let item_start = buf.len();
                let item_header = alloy_rlp::Header::decode(buf)?;
                remaining -= item_start - buf.len() + item_header.payload_length;

                if item_header.list {
                    // This is a nested node
                    let mut item_buf = &buf[..item_header.payload_length];
                    let node = Node::decode(&mut item_buf)?;
                    items.push(node);
                } else {
                    // This is a value
                    let value = Vec::<u8>::decode(buf)?;
                    items.push(Node::Value(ValueNode::new(value)));
                }
            }

            match items.len() {
                2 => {
                    // Short node: [key, value]
                    let key = match &items[0] {
                        Node::Value(v) => v.0.clone(),
                        _ => return Err(RlpError::Custom("invalid short node key")),
                    };
                    let val = items[1].clone();
                    Ok(Node::Short(ShortNode::new(key, val)))
                }
                17 => {
                    // Full node: [child0, child1, ..., child15, value]
                    let mut children = std::array::from_fn(|_| None);
                    for (i, item) in items.into_iter().enumerate() {
                        children[i] = Some(Box::new(item));
                    }
                    Ok(Node::Full(FullNode { children, flags: NodeFlag::default() }))
                }
                _ => Err(RlpError::Custom("invalid node format")),
            }
        } else {
            // Hash node or value node
            let data: Vec<u8> = Decodable::decode(buf)?;
            if data.len() == 32 {
                Ok(Node::Hash(HashNode(B256::from_slice(&data))))
            } else {
                Ok(Node::Value(ValueNode(data)))
            }
        }
    }
}

impl Encodable for FullNode {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::Header { list: true, payload_length: 17 }.encode(out);
        for child in &self.children {
            if let Some(node) = child {
                node.encode(out);
            } else {
                // Encode empty bytes for nil children
                Vec::<u8>::new().encode(out);
            }
        }
    }
}

impl Encodable for ShortNode {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::Header { list: true, payload_length: 2 }.encode(out);
        self.key.encode(out);
        self.val.encode(out);
    }
}

impl Encodable for HashNode {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.0.as_slice().encode(out);
    }
}

impl Encodable for ValueNode {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.0.encode(out);
    }
}

// Pretty printing
impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Full(n) => write!(f, "FullNode({})", n),
            Node::Short(n) => write!(f, "ShortNode({})", n),
            Node::Hash(h) => write!(f, "HashNode({:?})", h.0),
            Node::Value(v) => write!(f, "ValueNode({:?})", v.0),
        }
    }
}

impl fmt::Display for FullNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, child) in self.children.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            match child {
                Some(node) => write!(f, "{}: {}", i, node)?,
                None => write!(f, "{}: nil", i)?,
            }
        }
        write!(f, "]")
    }
}

impl fmt::Display for ShortNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{key: {:?}, val: {}}}", self.key, self.val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_node_creation() {
        let mut node = FullNode::new();
        assert!(node.is_empty());

        node.set_child(0, Some(Node::Value(ValueNode::new(b"test".to_vec()))));
        assert!(!node.is_empty());
    }

    #[test]
    fn test_short_node_creation() {
        let key = b"test_key".to_vec();
        let value = Node::Value(ValueNode::new(b"test_value".to_vec()));
        let node = ShortNode::new(key.clone(), value);

        assert_eq!(node.key, key);
    }

    #[test]
    fn test_node_rlp_encoding() {
        let value_node = Node::Value(ValueNode::new(b"test".to_vec()));
        let mut encoded = Vec::new();
        value_node.encode(&mut encoded);

        // For ValueNode, we can decode it directly
        let decoded_value = Vec::<u8>::decode(&mut &encoded[..]).unwrap();
        assert_eq!(decoded_value, b"test");
    }
}
