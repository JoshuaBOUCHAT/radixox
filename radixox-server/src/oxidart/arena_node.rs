use arrayvec::ArrayVec;
use prost::bytes::Bytes;
use slotmap::DefaultKey;

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Debug)]
pub struct ChildIdx {
    pub idx: DefaultKey,
    pub node_type: NodeType,
}
impl ChildIdx {
    #[allow(unused)]
    pub(crate) fn new(key: DefaultKey, node_type: NodeType) -> Self {
        Self {
            idx: key,
            node_type,
        }
    }
    pub(crate) fn new_small(idx: DefaultKey) -> Self {
        Self {
            idx,
            node_type: NodeType::Small,
        }
    }
}

#[derive(Clone)]
pub struct Node {
    pub radix: u8,
    pub idx: ChildIdx,
    pub val: Option<Bytes>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("radix", &(self.radix as char))
            .field("idx", &self.idx)
            .field("val", &self.val)
            .finish()
    }
}

pub trait Inode {
    fn get_child(&self, radix: u8) -> Option<Node>;
    fn get_val(&self) -> Option<Bytes>;
    fn is_full(&self) -> bool {
        false
    }
    fn insert_node(&mut self, node: Node);
    fn update_child_idx(&mut self, radix: u8, new_idx: ChildIdx);
    fn get_child_mut(&mut self, radix: u8) -> Option<&mut Node>;
    fn remove_child(&mut self, radix: u8);
    fn count(&self) -> usize;
}
#[derive(Default, Debug)]
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

#[derive(Debug)]
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
#[derive(Debug)]
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
#[derive(Debug)]
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
            fn insert_node(&mut self, node: Node) {
                self.nodes.push(node);
            }
            fn update_child_idx(&mut self, radix: u8, new_idx: ChildIdx) {
                self.nodes
                    .iter_mut()
                    .find(|node| node.radix == radix)
                    .expect("Ask to update a child idx for a child that do not exist")
                    .idx = new_idx;
            }
            fn get_child_mut(&mut self, radix: u8) -> Option<&mut Node> {
                self.nodes.iter_mut().find(|child| child.radix == radix)
            }
            fn remove_child(&mut self, radix: u8) {
                if let Some(pos) = self.nodes.iter().position(|n| n.radix == radix) {
                    self.nodes.swap_remove(pos);
                }
            }
            fn count(&self) -> usize {
                self.nodes.len()
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
    fn insert_node(&mut self, node: Node) {
        let index = node.radix as usize;
        self.nodes[index] = Some(node);
    }
    fn update_child_idx(&mut self, radix: u8, new_idx: ChildIdx) {
        self.nodes[radix as usize]
            .as_mut()
            .expect("Ask to update a child idx for a child that do not exist")
            .idx = new_idx
    }
    fn get_child_mut(&mut self, radix: u8) -> Option<&mut Node> {
        self.nodes[radix as usize].as_mut()
    }
    fn remove_child(&mut self, radix: u8) {
        self.nodes[radix as usize] = None;
    }
    fn count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_some()).count()
    }
}
impl Default for HugeChilds {
    fn default() -> Self {
        HugeChilds {
            val: None,
            nodes: [const { None }; 128],
        }
    }
}
