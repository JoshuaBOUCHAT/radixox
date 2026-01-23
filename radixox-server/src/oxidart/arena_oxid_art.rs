use std::ops::Index;

use bytes::Bytes;
use radixox_common::protocol::{GetAction, SetAction};

use slotmap::{DefaultKey, SlotMap};

use crate::oxidart::arena_node::{
    HugeChilds, Inode, LargeChilds, MediumChilds, Node, NodeChildIdx, NodeType, SmallChilds,
};

pub(crate) struct OxidArtArena {
    small_map: SlotMap<DefaultKey, SmallChilds>,
    medium_map: SlotMap<DefaultKey, MediumChilds>,
    large_map: SlotMap<DefaultKey, LargeChilds>,
    huge_map: SlotMap<DefaultKey, HugeChilds>,
    root: [NodeChildIdx; 128],
    root_data: Option<Bytes>,
}
impl OxidArtArena {
    fn new() -> Self {
        let mut small_map: SlotMap<DefaultKey, SmallChilds> = SlotMap::new();
        let root = core::array::from_fn(|_| {
            let idx = small_map.insert(SmallChilds::default());
            NodeChildIdx::new(idx, super::arena_node::NodeType::Small)
        });
        Self {
            small_map,
            root,
            medium_map: SlotMap::new(),
            large_map: SlotMap::new(),
            huge_map: SlotMap::new(),
            root_data: None,
        }
    }
    pub fn insert_new_node(&mut self, val: Option<Bytes>) -> NodeChildIdx {
        let idx = self.small_map.insert(SmallChilds::new_node(val));
        NodeChildIdx {
            idx,
            node_type: super::arena_node::NodeType::Small,
        }
    }

    pub fn get(&self, get_action: GetAction) -> Option<Bytes> {
        let key = get_action.into_byte();
        if key.is_empty() {
            return self.root_data.clone();
        }
        let mut actual_child_view = self.get_node_childs_view(&self.root[key[0] as usize]);
        //This code iter to key.len -1 as we need the the child view that point to the desired node
        for i in 1..(key.len() - 1) {
            actual_child_view =
                self.get_node_childs_view(&actual_child_view.get_child(key[i])?.idx);
        }
        let node_child = actual_child_view.get_child(*key.last().unwrap())?;
        node_child.val
    }
    pub fn set(&mut self, set_action: SetAction) {
        let (key, val) = set_action.into_parts();
        if key.is_empty() {
            self.root_data = Some(val);
            return;
        }
        let mut actual_child_idx = self.root[key[0] as usize].clone();
        if self.get_node_childs_view(&actual_child_idx).is_full() {
            if let Some(node) = self
                .get_node_childs_view(&actual_child_idx)
                .get_child(key[1])
            {
                actual_child_idx = node.idx;
            }
        }

        for i in 1..(key.len() - 1) {
            if let Some(node) = self
                .get_node_childs_view(&actual_child_idx)
                .get_child(key[i])
            {
                actual_child_idx = node.idx;
                continue;
            }
            let new_node_idx = self.insert_new_node(None);
            let view_mut = self.get_node_childs_view_mut(&actual_child_idx);
            view_mut.is_full();
        }
    }

    fn get_node_childs_view(&self, index: &NodeChildIdx) -> NodeChildsView<'_> {
        use super::arena_node::NodeType::*;
        match index.node_type {
            Huge => NodeChildsView::Huge(&self.huge_map[index.idx]),
            Large => NodeChildsView::Large(&self.large_map[index.idx]),
            Medium => NodeChildsView::Medium(&self.medium_map[index.idx]),
            Small => NodeChildsView::Small(&self.small_map[index.idx]),
        }
    }

    fn get_node_childs_view_mut(&mut self, index: &NodeChildIdx) -> NodeChildsViewMut<'_> {
        use super::arena_node::NodeType::*;
        match index.node_type {
            Huge => NodeChildsViewMut::Huge(&mut self.huge_map[index.idx]),
            Large => NodeChildsViewMut::Large(&mut self.large_map[index.idx]),
            Medium => NodeChildsViewMut::Medium(&mut self.medium_map[index.idx]),
            Small => NodeChildsViewMut::Small(&mut self.small_map[index.idx]),
        }
    }
    pub fn upgrade(&mut self, idx: NodeChildIdx, node: Node) -> NodeChildIdx {
        match self.get_node_childs_view(&idx) {
            NodeChildsView::Huge(_) => panic!("Huge should no be upgrade"),
            NodeChildsView::Large(v) => NodeChildIdx {
                idx: self.huge_map.insert(v.upgrade(node)),
                node_type: NodeType::Huge,
            },
            NodeChildsView::Medium(v) => NodeChildIdx {
                idx: self.large_map.insert(v.upgrade(node)),
                node_type: NodeType::Large,
            },
            NodeChildsView::Small(v) => NodeChildIdx {
                idx: self.medium_map.insert(v.upgrade(node)),
                node_type: NodeType::Medium,
            },
        }
    }
}

enum NodeChildsView<'a> {
    Small(&'a SmallChilds),
    Medium(&'a MediumChilds),
    Large(&'a LargeChilds),
    Huge(&'a HugeChilds),
}
enum NodeChildsViewMut<'a> {
    Small(&'a mut SmallChilds),
    Medium(&'a mut MediumChilds),
    Large(&'a mut LargeChilds),
    Huge(&'a mut HugeChilds),
}
impl NodeChildsView<'_> {
    #[inline]
    fn get_child(&self, radix: u8) -> Option<Node> {
        match self {
            Self::Medium(a) => a.get_child(radix),
            Self::Huge(a) => a.get_child(radix),
            Self::Small(a) => a.get_child(radix),
            Self::Large(a) => a.get_child(radix),
        }
    }
    fn is_full(&self) -> bool {
        match self {
            Self::Medium(a) => a.is_full(),
            Self::Huge(a) => a.is_full(),
            Self::Small(a) => a.is_full(),
            Self::Large(a) => a.is_full(),
        }
    }
}
impl NodeChildsViewMut<'_> {
    #[inline]
    fn get_child(&mut self, radix: u8) -> Option<Node> {
        match self {
            Self::Medium(a) => a.get_child(radix),
            Self::Huge(a) => a.get_child(radix),
            Self::Small(a) => a.get_child(radix),
            Self::Large(a) => a.get_child(radix),
        }
    }
    fn is_full(&self) -> bool {
        match self {
            Self::Medium(a) => a.is_full(),
            Self::Huge(a) => a.is_full(),
            Self::Small(a) => a.is_full(),
            Self::Large(a) => a.is_full(),
        }
    }
}
