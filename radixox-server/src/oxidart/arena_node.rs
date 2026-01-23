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

#[derive(Clone)]
pub struct NodeChildIdx {
    pub idx: DefaultKey,
    pub node_type: NodeType,
}
impl NodeChildIdx {
    pub(crate) fn new(idx: DefaultKey, node_type: NodeType) -> Self {
        Self { idx, node_type }
    }
}

#[derive(Clone)]
pub struct Node {
    pub radix: u8,
    pub idx: NodeChildIdx,
    pub val: Option<Bytes>,
}

pub trait Inode {
    fn get_child(&self, radix: u8) -> Option<Node>;
    fn get_val(&self) -> Option<Bytes>;
    ///
    fn is_full(&self) -> bool {
        false
    }
}
#[derive(Default)]
pub struct SmallChilds {
    val: Option<Bytes>,
    nodes: ArrayVec<Node, { NodeType::Small.size() }>,
}

impl SmallChilds {
    /*fn insert_new_child(&mut self, radix: u8) -> Result<(), MediumChilds> {
        if self.nodes.is_full() {

        }
    }*/
    pub(crate) const fn new_node(val: Option<Bytes>) -> Self {
        Self {
            val,
            nodes: ArrayVec::new_const(),
        }
    }
    pub(crate) fn upgrade(&self, node: Node) -> MediumChilds {
        let mut nodes = ArrayVec::from_iter(self.nodes.iter().cloned());
        nodes.push(node);
        MediumChilds {
            val: self.val.clone(),
            nodes,
        }
    }
}

pub struct MediumChilds {
    val: Option<Bytes>,
    nodes: ArrayVec<Node, { NodeType::Medium.size() }>,
}

impl MediumChilds {
    pub(crate) fn upgrade(&self, node: Node) -> LargeChilds {
        let mut nodes = ArrayVec::from_iter(self.nodes.iter().cloned());
        nodes.push(node);
        LargeChilds {
            val: self.val.clone(),
            nodes,
        }
    }
}

pub struct LargeChilds {
    val: Option<Bytes>,
    nodes: ArrayVec<Node, { NodeType::Large.size() }>,
}

impl LargeChilds {
    pub(crate) fn upgrade(&self, node: Node) -> HugeChilds {
        let mut nodes: [Option<Node>; 128] = std::array::from_fn(|_| None);
        for node in &self.nodes {
            nodes[node.radix as usize] = Some(node.clone());
        }
        let radix = node.radix;
        nodes[radix as usize] = Some(node);
        HugeChilds {
            val: self.val.clone(),
            nodes,
        }
    }
}

pub struct HugeChilds {
    val: Option<Bytes>,
    nodes: [Option<Node>; 128],
}

macro_rules! impl_node_array {
    ($name:ident) => {
        impl Inode for $name {
            fn get_child(&self, radix: u8) -> Option<Node> {
                self.nodes
                    .iter()
                    .find(|&child| child.radix == radix)
                    .cloned()
            }

            fn get_val(&self) -> Option<Bytes> {
                self.val.clone()
            }
            fn is_full(&self) -> bool {
                self.nodes.is_full()
            }
        }
    };
}

impl_node_array!(SmallChilds);
impl_node_array!(MediumChilds);
impl_node_array!(LargeChilds);

impl Inode for HugeChilds {
    fn get_child(&self, radix: u8) -> Option<Node> {
        assert!(radix.is_ascii());
        self.nodes[radix as usize].clone()
    }
    fn get_val(&self) -> Option<Bytes> {
        self.val.clone()
    }
}
