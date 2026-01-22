use std::{
    collections::binary_heap::PeekMut,
    ops::{Index, IndexMut},
};

use bytes::Bytes;

#[derive(Clone, Default)]
pub struct Node {
    val: Option<Bytes>,
    childs: Vec<Node>,
    radix: u8,
}
impl Node {
    pub(crate) fn new(radix: u8) -> Self {
        assert!(radix.is_ascii());
        Self {
            val: None,
            childs: vec![],
            radix,
        }
    }
    pub(crate) fn add_child_mut(&mut self, radix: u8) -> &mut Self {
        self.childs.push(Node::new(radix));
        unsafe { self.childs.last_mut().unwrap_unchecked() }
    }
    pub(crate) fn last_mut(&mut self) -> Option<&mut Self> {
        self.childs.last_mut()
    }
    pub(crate) fn set_val(&mut self, val: Bytes) {
        self.val = Some(val)
    }
}

impl Node {
    pub fn get_child(&self, radix: u8) -> Option<usize> {
        self.childs.iter().position(|child| child.radix == radix)
    }

    pub fn get_val(&self) -> Option<Bytes> {
        self.val.clone()
    }
}
impl Index<usize> for Node {
    type Output = Node;
    fn index(&self, index: usize) -> &Self::Output {
        &self.childs[index]
    }
}
impl IndexMut<usize> for Node {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.childs[index]
    }
}
