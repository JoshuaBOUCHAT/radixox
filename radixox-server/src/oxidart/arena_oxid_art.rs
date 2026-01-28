use bytes::Bytes;
use radixox_common::protocol::{DelAction, GetAction, SetAction};

use slotmap::{DefaultKey, SlotMap};

use crate::oxidart::arena_node::{
    ChildIdx, HugeChilds, Inode, LargeChilds, MediumChilds, Node, NodeType, SmallChilds,
};
#[derive(Debug)]
pub struct OxidArtArena {
    pub small_map: SlotMap<DefaultKey, SmallChilds>,
    medium_map: SlotMap<DefaultKey, MediumChilds>,
    large_map: SlotMap<DefaultKey, LargeChilds>,
    huge_map: SlotMap<DefaultKey, HugeChilds>,
    ///Dummy radix for root
    root: Node,
}
impl OxidArtArena {
    pub fn new() -> Self {
        let mut huge_map: SlotMap<DefaultKey, HugeChilds> = SlotMap::new();

        let root_key = huge_map.insert(HugeChilds::default());
        let root_idx = ChildIdx {
            idx: root_key,
            node_type: NodeType::Huge,
        };
        //Dummy radix for root
        let root = Node {
            idx: root_idx,
            radix: 0,
            val: None,
        };
        Self {
            small_map: SlotMap::new(),
            root,
            medium_map: SlotMap::new(),
            large_map: SlotMap::new(),
            huge_map,
        }
    }
    fn insert_new_node(&mut self, val: Option<Bytes>, radix: u8) -> Node {
        let key = self.small_map.insert(SmallChilds::new_node(None));
        let idx = ChildIdx::new_small(key);
        Node { idx, val, radix }
    }

    pub fn get(&self, get_action: GetAction) -> Option<Bytes> {
        let key = get_action.into_parts();
        if key.is_empty() {
            return self.root.val.clone();
        }
        let mut actual_child_view = self.get_node_childs_view(&self.root.idx);
        // Traverse all characters except the last one
        for i in 0..(key.len() - 1) {
            let node = actual_child_view.get_child(key[i])?;
            actual_child_view = self.get_node_childs_view(&node.idx);
        }
        // Get the final node's value
        let node_child = actual_child_view.get_child(*key.last().unwrap())?;
        node_child.val
    }
    //fn get_child_val(&self, idx: &ChildIdx) -> Option<Bytes> {}
    pub fn set(&mut self, set_action: SetAction) {
        let (key, val) = set_action.into_parts();
        //STEP-1.1 Handle special case len 1 and 0
        let key_len = key.len();
        if key_len == 0 {
            self.root.val = Some(val);
            return;
        }
        if key_len == 1 {
            self.handle_first_node_set(key[0], Some(val));
            return;
        }
        //STEP-1.2 Handle normal case
        let mut fathers_childs_idx = self.root.idx.clone();
        //Ici on passe None comme val car on ne souhaite pas  definir la val du node de lvl1
        let mut actual_idx = self.handle_first_node_set(key[0], None);
        //STEP-2 Cross the tree nodes for each radix if the coresponding node do not existe create it and go forward
        //Only go to key.len() -1 because the last radix is the special case where we insert data
        for i in 1..(key.len() - 1) {
            let radix = key[i];
            //STEP-2.1 If the node allready existe simply go forward
            if let Some(node) = self.get_node_childs_view(&actual_idx).get_child(radix) {
                (fathers_childs_idx, actual_idx) = (actual_idx, node.idx);
                continue;
            }
            let new_node = self.insert_new_node(None, radix);
            let new_idx = new_node.idx.clone();

            if self.get_node_childs_view(&actual_idx).is_full() {
                //STEP-2v2 set the childs_idx to the new one as the upgrade change the index

                self.upgrade(&fathers_childs_idx, key[i - 1], &actual_idx, new_node);
            } else {
                //STEP-2v3 Simply add the new child in the root list as it is not full yet
                self.insert(&actual_idx, new_node);
            }
            (fathers_childs_idx, actual_idx) = (actual_idx, new_idx)
        }
        let view_mut = self.get_node_childs_view_mut(&actual_idx);
        let last_radix = *key.last().unwrap();
        if let Some(node) = view_mut.get_child_mut(last_radix) {
            node.val = Some(val);
            return;
        }
        let new_node = self.insert_new_node(Some(val), last_radix);
        if self.get_node_childs_view(&actual_idx).is_full() {
            self.upgrade(&fathers_childs_idx, key[key_len - 2], &actual_idx, new_node);
        } else {
            self.insert(&actual_idx, new_node);
        }
    }

    fn handle_first_node_set(&mut self, radix: u8, maybe_val: Option<Bytes>) -> ChildIdx {
        let root_child_index = self.root.idx.clone();

        //STEP-1 if the wanted child exist then just return the index
        let view_mut = self.get_node_childs_view_mut(&root_child_index);
        if let Some(node) = view_mut.get_child_mut(radix) {
            if let Some(val) = maybe_val {
                node.val = Some(val);
            }
            return node.idx.clone();
        }
        //STEP-2 Create the needed child

        let new_node = self.insert_new_node(maybe_val, radix);
        let new_idx = new_node.idx.clone();

        //STEP-3 Inserting here we don't need to check if it full because root node is by initialisation HUGE and HUGE can't be full
        self.insert(&root_child_index, new_node);

        new_idx
    }
    pub fn upgrade(
        &mut self,
        fathers_childs_idx: &ChildIdx,
        actual_radix: u8,
        idx: &ChildIdx,
        node: Node,
    ) -> ChildIdx {
        use crate::oxidart::arena_node::NodeType;
        //STEP-1 Retrieve Childs container and upgrade it
        let new_idx = match self.get_node_childs_view(&idx) {
            ChildsView::Huge(_) => panic!("Huge should no be upgrade"),
            ChildsView::Large(v) => ChildIdx {
                idx: self.huge_map.insert(v.upgrade(node)),
                node_type: NodeType::Huge,
            },
            ChildsView::Medium(v) => ChildIdx {
                idx: self.large_map.insert(v.upgrade(node)),
                node_type: NodeType::Large,
            },
            ChildsView::Small(v) => ChildIdx {
                idx: self.medium_map.insert(v.upgrade(node)),
                node_type: NodeType::Medium,
            },
        };
        //STEP-2 Remove the old child container
        self.delete_childs(&idx);
        //STEP-3 Make the Node parent point to the new child
        self.get_node_childs_view_mut(fathers_childs_idx)
            .update_child_idx(actual_radix, new_idx.clone());

        new_idx
    }

    fn get_node_childs_view(&self, index: &ChildIdx) -> ChildsView<'_> {
        use super::arena_node::NodeType::*;
        match index.node_type {
            Huge => ChildsView::Huge(&self.huge_map[index.idx]),
            Large => ChildsView::Large(&self.large_map[index.idx]),
            Medium => ChildsView::Medium(&self.medium_map[index.idx]),
            Small => ChildsView::Small(&self.small_map[index.idx]),
        }
    }

    fn get_node_childs_view_mut(&mut self, index: &ChildIdx) -> ChildsViewMut<'_> {
        use super::arena_node::NodeType::*;
        match index.node_type {
            Huge => ChildsViewMut::Huge(&mut self.huge_map[index.idx]),
            Large => ChildsViewMut::Large(&mut self.large_map[index.idx]),
            Medium => ChildsViewMut::Medium(&mut self.medium_map[index.idx]),
            Small => ChildsViewMut::Small(&mut self.small_map[index.idx]),
        }
    }

    fn delete_childs(&mut self, index: &ChildIdx) {
        use super::arena_node::NodeType::*;
        match index.node_type {
            Huge => {
                self.huge_map.remove(index.idx);
            }
            Large => {
                self.large_map.remove(index.idx);
            }
            Medium => {
                self.medium_map.remove(index.idx);
            }
            Small => {
                self.small_map.remove(index.idx);
            }
        };
    }
    fn insert(&mut self, node_child_idx: &ChildIdx, node: Node) {
        self.get_node_childs_view_mut(node_child_idx).add_node(node);
    }
    pub fn del(&mut self, del_action: DelAction) -> Option<Bytes> {
        let key = del_action.into_parts();
        if key.is_empty() {
            return self.root.val.take();
        }

        // Track branch point: (parent_childs_idx, radix_to_remove)
        let mut branch_point: (ChildIdx, u8) = (self.root.idx.clone(), key[0]);
        let mut childs_to_delete: Vec<ChildIdx> = Vec::new();

        let mut parent_idx = self.root.idx.clone();
        let mut actual_child_view = self.get_node_childs_view(&self.root.idx);

        for i in 0..(key.len() - 1) {
            let node = actual_child_view.get_child(key[i])?;

            let next_view = self.get_node_childs_view(&node.idx);

            // Is this a branch point? (has value OR multiple children)
            if node.val.is_some() || next_view.get_child_count() > 1 {
                // New branch point - reset
                branch_point = (node.idx.clone(), key[i + 1]);
                childs_to_delete.clear();
            } else {
                childs_to_delete.push(node.idx.clone());
            }

            parent_idx = node.idx.clone();
            actual_child_view = next_view;
        }

        let last_radix = *key.last().expect("key should have last delete");
        // Last node
        let node = actual_child_view.get_child(last_radix)?;
        let final_view = self.get_node_childs_view(&node.idx);
        let has_children = final_view.get_child_count() > 0;

        // Take the value from the node
        let val = self
            .get_node_childs_view_mut(&parent_idx)
            .get_child_mut(last_radix)?
            .val
            .take();

        if !has_children {
            // No children - delete the whole branch from branch point
            childs_to_delete.push(node.idx.clone());

            // Remove child from branch parent
            let (bp_idx, bp_radix) = branch_point;
            self.get_node_childs_view_mut(&bp_idx)
                .remove_child(bp_radix);

            // Delete all child containers (just pop, no search needed!)
            for idx in childs_to_delete {
                self.delete_childs(&idx);
            }
        }

        return val;
    }
}

enum ChildsView<'a> {
    Small(&'a SmallChilds),
    Medium(&'a MediumChilds),
    Large(&'a LargeChilds),
    Huge(&'a HugeChilds),
}
enum ChildsViewMut<'a> {
    Small(&'a mut SmallChilds),
    Medium(&'a mut MediumChilds),
    Large(&'a mut LargeChilds),
    Huge(&'a mut HugeChilds),
}
macro_rules! impl_view {
    ($self:ident, $fn:ident) => {
        match $self {
            Self::Small(c) => c.$fn(),
            Self::Medium(c) => c.$fn(),
            Self::Large(c) => c.$fn(),
            Self::Huge(c) => c.$fn(),
        }
    };
    ($self:ident, $fn:ident, $($arg:expr),+) => {
        match $self {
            Self::Small(c) => c.$fn($($arg),+),
            Self::Medium(c) => c.$fn($($arg),+),
            Self::Large(c) => c.$fn($($arg),+),
            Self::Huge(c) => c.$fn($($arg),+),
        }
    };
}

impl ChildsView<'_> {
    #[inline]
    fn get_child(&self, radix: u8) -> Option<Node> {
        impl_view!(self, get_child, radix)
    }
    fn is_full(&self) -> bool {
        impl_view!(self, is_full)
    }
    fn get_child_count(&self) -> usize {
        impl_view!(self, count)
    }
}

impl<'a> ChildsViewMut<'a> {
    #[inline]
    fn get_child_mut(self, radix: u8) -> Option<&'a mut Node> {
        impl_view!(self, get_child_mut, radix)
    }

    fn add_node(&mut self, node: Node) {
        impl_view!(self, insert_node, node)
    }
    fn update_child_idx(&mut self, radix: u8, new_idx: ChildIdx) {
        impl_view!(self, update_child_idx, radix, new_idx);
    }
    fn remove_child(&mut self, radix: u8) {
        impl_view!(self, remove_child, radix);
    }
}
