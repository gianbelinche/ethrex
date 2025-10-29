mod branch;
mod extension;
mod leaf;

use std::sync::{Arc, OnceLock};

pub use branch::BranchNode;
use ethrex_rlp::{decode::RLPDecode, encode::RLPEncode};
pub use extension::ExtensionNode;
pub use leaf::LeafNode;

use crate::{TrieDB, error::TrieError, nibbles::Nibbles};

use super::{ValueRLP, node_hash::NodeHash};

/// A reference to a node.
#[derive(Clone, Debug)]
pub enum NodeRef {
    /// The node is embedded within the reference.
    Node(Arc<Node>, OnceLock<NodeHash>),
    /// The node is in the database, referenced by its hash.
    Hash(NodeHash),
}

impl NodeRef {
    /// Gets a shared reference to the inner node.
    pub fn get_node(&self, db: &dyn TrieDB, path: Nibbles) -> Result<Option<Arc<Node>>, TrieError> {
        match self {
            NodeRef::Node(node, _) => Ok(Some(node.clone())),
            NodeRef::Hash(hash @ NodeHash::Inline(_)) => {
                Ok(Some(Arc::new(Node::decode(hash.as_ref())?)))
            }
            NodeRef::Hash(hash @ NodeHash::Hashed(_)) => db
                .get(path)?
                .filter(|rlp| !rlp.is_empty())
                .and_then(|rlp| match Node::decode(&rlp) {
                    Ok(node) => (node.compute_hash() == *hash).then_some(Ok(Arc::new(node))),
                    Err(err) => Some(Err(TrieError::RLPDecode(err))),
                })
                .transpose(),
        }
    }

    /// Gets a mutable shared reference to the inner node.
    ///
    /// # Caution
    ///
    /// 1. If more than one strong reference exists to this node, it will be cloned (see `Arc::make_mut`).
    /// 2. Mutating the inner node without updating parents can lead to trie inconsistencies.
    pub(crate) fn get_node_mut(
        &mut self,
        db: &dyn TrieDB,
        path: Nibbles,
    ) -> Result<Option<&mut Node>, TrieError> {
        match self {
            NodeRef::Node(node, _) => Ok(Some(Arc::make_mut(node))),
            NodeRef::Hash(hash @ NodeHash::Inline(_)) => {
                let node = Node::decode(hash.as_ref())?;
                *self = NodeRef::Node(Arc::new(node), OnceLock::from(*hash));
                self.get_node_mut(db, path)
            }
            NodeRef::Hash(hash @ NodeHash::Hashed(_)) => {
                let Some(node) = db
                    .get(path.clone())?
                    .filter(|rlp| !rlp.is_empty())
                    .and_then(|rlp| match Node::decode(&rlp) {
                        Ok(node) => (node.compute_hash() == *hash).then_some(Ok(node)),
                        Err(err) => Some(Err(TrieError::RLPDecode(err))),
                    })
                    .transpose()?
                else {
                    return Ok(None);
                };
                *self = NodeRef::Node(Arc::new(node), OnceLock::from(*hash));
                self.get_node_mut(db, path)
            }
        }
    }

    pub fn is_valid(&self) -> bool {
        match self {
            NodeRef::Node(_, _) => true,
            NodeRef::Hash(hash) => hash.is_valid(),
        }
    }

    pub fn commit(&mut self, path: Nibbles, acc: &mut Vec<(Nibbles, Vec<u8>)>) -> NodeHash {
        match *self {
            NodeRef::Node(ref mut node, ref mut hash) => {
                match Arc::make_mut(node) {
                    Node::Branch(node) => {
                        for (choice, node) in &mut node.choices.iter_mut().enumerate() {
                            node.commit(path.append_new(choice as u8), acc);
                        }
                    }
                    Node::Extension(node) => {
                        node.child.commit(path.concat(&node.prefix), acc);
                    }
                    Node::Leaf(_) => {}
                }
                let mut buf = Vec::new();
                node.encode(&mut buf);
                let hash = *hash.get_or_init(|| NodeHash::from_encoded(&buf));
                if let Node::Leaf(leaf) = node.as_ref() {
                    acc.push((path.concat(&leaf.partial), leaf.value.clone()));
                }
                acc.push((path.clone(), buf));

                *self = hash.into();

                hash
            }
            NodeRef::Hash(hash) => hash,
        }
    }

    pub fn compute_hash(&self) -> NodeHash {
        match self {
            NodeRef::Node(node, hash) => *hash.get_or_init(|| node.compute_hash()),
            NodeRef::Hash(hash) => *hash,
        }
    }

    /// Resets the memoized hash of this Node
    ///
    /// This is used when mutating a node in place, in which case the memoized hash
    /// is not valid anymore.
    pub(crate) fn clear_hash(&mut self) {
        if let NodeRef::Node(_, hash) = self {
            hash.take();
        }
    }
}

impl Default for NodeRef {
    fn default() -> Self {
        Self::Hash(NodeHash::default())
    }
}

impl From<Node> for NodeRef {
    fn from(value: Node) -> Self {
        Self::Node(Arc::new(value), OnceLock::new())
    }
}

impl From<NodeHash> for NodeRef {
    fn from(value: NodeHash) -> Self {
        Self::Hash(value)
    }
}

impl PartialEq for NodeRef {
    fn eq(&self, other: &Self) -> bool {
        self.compute_hash() == other.compute_hash()
    }
}

pub enum ValueOrHash {
    Value(ValueRLP),
    Hash(NodeHash),
}

impl From<ValueRLP> for ValueOrHash {
    fn from(value: ValueRLP) -> Self {
        Self::Value(value)
    }
}

impl From<NodeHash> for ValueOrHash {
    fn from(value: NodeHash) -> Self {
        Self::Hash(value)
    }
}

/// A Node in an Ethereum Compatible Patricia Merkle Trie
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Branch(Box<BranchNode>),
    Extension(ExtensionNode),
    Leaf(LeafNode),
}

impl From<Box<BranchNode>> for Node {
    fn from(val: Box<BranchNode>) -> Self {
        Node::Branch(val)
    }
}

impl From<BranchNode> for Node {
    fn from(val: BranchNode) -> Self {
        Node::Branch(Box::new(val))
    }
}

impl From<ExtensionNode> for Node {
    fn from(val: ExtensionNode) -> Self {
        Node::Extension(val)
    }
}

impl From<LeafNode> for Node {
    fn from(val: LeafNode) -> Self {
        Node::Leaf(val)
    }
}

impl Node {
    /// Retrieves a value from the subtrie originating from this node given its path
    pub fn get(&self, db: &dyn TrieDB, path: Nibbles) -> Result<Option<ValueRLP>, TrieError> {
        match self {
            Node::Branch(n) => n.get(db, path),
            Node::Extension(n) => n.get(db, path),
            Node::Leaf(n) => n.get(path),
        }
    }

    /// Inserts a value into the subtrie originating from this node.
    pub fn insert(
        &mut self,
        db: &dyn TrieDB,
        path: Nibbles,
        value: impl Into<ValueOrHash>,
    ) -> Result<(), TrieError> {
        let new_node = match self {
            Node::Branch(n) => {
                n.insert(db, path, value.into())?;
                Ok(None)
            }
            Node::Extension(n) => n.insert(db, path, value.into()),
            Node::Leaf(n) => n.insert(path, value.into()),
        };
        if let Some(new_node) = new_node? {
            *self = new_node;
        }
        Ok(())
    }

    /// Removes a value from the subtrie originating from this node given its path
    /// Returns a bool indicating if the new subtrie is empty, and the removed value if it existed in the subtrie
    pub fn remove(
        &mut self,
        db: &dyn TrieDB,
        path: Nibbles,
    ) -> Result<(bool, Option<ValueRLP>), TrieError> {
        let (new_root, value) = match self {
            Node::Branch(n) => n.remove(db, path),
            Node::Extension(n) => n.remove(db, path),
            Node::Leaf(n) => n.remove(path),
        }?;

        let is_trie_empty = new_root.is_none();
        if let Some(NodeRemoveResult::New(new_root)) = new_root {
            *self = new_root;
        }
        Ok((is_trie_empty, value))
    }

    /// Traverses own subtrie until reaching the node containing `path`
    /// Appends all encoded nodes traversed to `node_path` (including self)
    /// Only nodes with encoded len over or equal to 32 bytes are included
    pub fn get_path(
        &self,
        db: &dyn TrieDB,
        path: Nibbles,
        node_path: &mut Vec<Vec<u8>>,
    ) -> Result<(), TrieError> {
        match self {
            Node::Branch(n) => n.get_path(db, path, node_path),
            Node::Extension(n) => n.get_path(db, path, node_path),
            Node::Leaf(n) => n.get_path(node_path),
        }
    }

    /// Computes the node's hash
    pub fn compute_hash(&self) -> NodeHash {
        match self {
            Node::Branch(n) => n.compute_hash(),
            Node::Extension(n) => n.compute_hash(),
            Node::Leaf(n) => n.compute_hash(),
        }
    }
}

/// Used as return type for `Node` remove operations that may resolve into either:
/// - a mutation of the `Node`
/// - a new `Node`
pub enum NodeRemoveResult {
    Mutated,
    New(Node),
}
