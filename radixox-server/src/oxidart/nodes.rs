use arrayvec::ArrayVec;
use prost::bytes::Bytes;
use slotmap::DefaultKey;

#[derive(Clone, Copy)]
pub enum NodeType {
    Small,
    Medium,
    Large,
    Huge,
}
impl NodeType {
    const fn size(&self) -> usize {
        match self {
            Self::Small => 4,
            Self::Medium => 8,
            Self::Large => 32,
            Self::Huge => 128,
        }
    }
}

pub struct Node {
    is_word: bool,
    children: NodeChild,
}

#[derive(Clone)]
pub struct NodeChild {
    node_type: NodeType,
    idx: DefaultKey,
    radix: u8,
}

pub trait Inode {
    fn get_child(&self, radix: u8) -> Option<NodeChild>;
    fn get_val(&self) -> Option<Bytes>;
}

pub struct SmallNode {
    val: Option<Bytes>,
    nodes: ArrayVec<NodeChild, { NodeType::Small.size() }>,
}

pub struct MediumNode {
    val: Option<Bytes>,
    nodes: ArrayVec<NodeChild, { NodeType::Medium.size() }>,
}
pub struct LargeNode {
    val: Option<Bytes>,
    nodes: ArrayVec<NodeChild, { NodeType::Large.size() }>,
}
pub struct HugeNode {
    val: Option<Bytes>,
    nodes: [Option<NodeChild>; 128],
}

macro_rules! impl_node_array {
    ($name:ident) => {
        impl Inode for $name {
            fn get_child(&self, radix: u8) -> Option<NodeChild> {
                self.nodes
                    .iter()
                    .find(|&child| child.radix == radix)
                    .cloned()
            }

            fn get_val(&self) -> Option<Bytes> {
                self.val.clone()
            }
        }
    };
}

impl_node_array!(SmallNode);
impl_node_array!(MediumNode);
impl_node_array!(LargeNode);

impl Inode for HugeNode {
    fn get_child(&self, radix: u8) -> Option<NodeChild> {
        assert!(radix.is_ascii());
        self.nodes[radix as usize].clone()
    }
    fn get_val(&self) -> Option<Bytes> {
        self.val.clone()
    }
}
