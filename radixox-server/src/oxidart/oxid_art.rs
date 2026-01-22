use bytes::Bytes;
use radixox_common::protocol::{GetAction, SetAction};

use crate::oxidart::nodes::Node;

pub(crate) struct OxidART {
    val: Option<Bytes>,
    childs: [Node; 128],
}
impl Default for OxidART {
    fn default() -> Self {
        Self {
            val: None,
            childs: core::array::from_fn(|radix| Node::new(radix as u8)),
        }
    }
}
impl OxidART {
    pub(crate) fn get(&self, get_action: GetAction) -> Option<Bytes> {
        let key = get_action.into_byte();
        if key.is_empty() {
            return self.val.clone();
        }
        let mut actual_node = &self.childs[key[0] as usize];
        for byte in key.into_iter().skip(1) {
            let Some(child_idx) = actual_node.get_child(byte) else {
                return None;
            };
            actual_node = &actual_node[child_idx]
        }
        actual_node.get_val()
    }
    pub(crate) fn set(&mut self, set_action: SetAction) {
        let (key, val) = set_action.into_parts();
        if key.is_empty() {
            self.val = Some(val);
            return;
        }
        let mut actual_node = &mut self.childs[key[0] as usize];

        for byte in key.into_iter().skip(1) {
            actual_node = match actual_node.get_child(byte) {
                Some(idx) => &mut actual_node[idx],
                None => actual_node.add_child_mut(byte),
            };
        }
        actual_node.set_val(val);
    }
}
