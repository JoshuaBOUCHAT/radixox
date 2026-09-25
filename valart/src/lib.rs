use std::{
    collections::VecDeque,
    mem::{ManuallyDrop, MaybeUninit, offset_of},
    ptr::drop_in_place,
};

use hislab::HiSlab;
use oxidart::{ExpAndRadix, compact_str::CompactStr};
use radixox_lib::{
    prefetch::{self, PreFetch},
    shared_byte::SharedByte,
    thin_vec::ThinVec,
};
use std::collections::BTreeSet;
mod overflow;
pub use overflow::{HashOverflow, SetOverflow};

use crate::FindResult::None;

type NodeMap = [ValNode];

// Toutes les variantes doivent porter leur `node_type` au même offset : c'est
// ce qui permet de lire le tag depuis l'union sans savoir quelle variante est
// active (common initial sequence, comme un tagged union en C).
const NODE_TYPE_OFFSET: usize = offset_of!(HashNode, node_type);
const _: () = assert!(NODE_TYPE_OFFSET == offset_of!(SetNode, node_type));

union ValNode {
    hash: ManuallyDrop<HashNode>,
    set: ManuallyDrop<SetNode>,
}

impl ValNode {
    /// Lit le tag sans passer par le champ `NodeType` typé : un `u8` n'a pas
    /// de bit pattern invalide, donc c'est toujours sound même si la
    /// variante "active" de l'union n'est pas celle qu'on relit ici.
    #[inline]
    fn tag_u8(&self) -> u8 {
        unsafe { *(self as *const ValNode as *const u8).add(NODE_TYPE_OFFSET) }
    }
    fn tag(&self) -> NodeType {
        unsafe { *(self as *const ValNode as *const NodeType).add(NODE_TYPE_OFFSET) }
    }
}

impl Drop for ValNode {
    fn drop(&mut self) {
        match self.tag_u8() {
            t if t == NodeType::HashSet as u8 => unsafe { ManuallyDrop::drop(&mut self.hash) },
            t if t == NodeType::Set as u8 => unsafe { ManuallyDrop::drop(&mut self.set) },
            t if t == NodeType::ZSet as u8 => unreachable!("ZSet node not implemented yet"),
            _ => unreachable!("invalid NodeType tag"),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum NodeType {
    HashSet,
    Set,
    ZSet,
}

const INLINE_LEN: usize = 5;

struct KeyVal {
    key: SharedByte,
    val: SharedByte,
}

#[repr(C, align(128))]
pub struct HashNode {
    inline_data: [KeyVal; INLINE_LEN],
    ttls: Option<ThinVec<ExpAndRadix>>,
    overflow: Option<HashOverflow>,

    childs_idx: [u32; INLINE_LEN],
    prefix: [u8; INLINE_LEN],
    count: u8,
    ttl_map: u8,
    node_type: NodeType,
}
impl PreFetch for HashNode {}

impl HashNode {
    /*fn find_inline(&self, prefix: u8, now: u64) -> Option<SharedByte> {
        for i in 0..(self.count as usize) {
            if self.prefix[i] != prefix {
                continue;
            }
            if !self.have_ttl(i) {
                return Some(self.inline_data[i].val.clone());
            }
            let expiracy = self
                .ttls
                .expect("There is a value with a ttl but there is no ttls store associated")
                .find(|er| er.parent_radix() == prefix)
                .expect(
                    "prefix is associated to a tll but no coresponding tll entry found in ttls map",
                )
                .exp()
                .expect("value with a ttl, a ttl entry but the field ttl is empty");
            if now > expiracy {
                return None;
            } else {
                return Some(self.inline_data[i].clone());
            }
        }
        let () = self.overflow?.get_values(prefix).expect(
            "prefix is associated to a tll but no coresponding tll entry found in ttls map",
        );
    }
    fn have_ttl(&self, index: usize) -> bool {
        self.ttl_map | 1 << index != 0
    }*/
}

const INLINE_SET_LEN: usize = 8;

#[repr(C, align(128))]
pub struct SetNode {
    prefix: [u8; INLINE_SET_LEN],
    childs_idx: [u32; INLINE_SET_LEN],
    inline_keys: [MaybeUninit<SharedByte>; INLINE_SET_LEN],
    ttls: Option<ThinVec<ExpAndRadix>>,

    overflow: Option<SetOverflow>,
    count: u8,
    ttl_map: u8,
    // Pousse `node_type` au même offset que dans `HashNode` (voir
    // NODE_TYPE_OFFSET) : nécessaire pour lire le tag depuis l'union sans
    // connaître la variante active.
    _pad: u8,
    node_type: NodeType,
}
impl PreFetch for SetNode {}

enum FindResult {
    None,
    Inline { idx: usize },
    Overflow { idx: usize },
    Expired { child_idx: u32 },
}

impl SetNode {
    fn find_from_prefix(&mut self, prefix: u8, now: u64) -> FindResult {
        if let Some(idx) = self.prefix[0..(self.count as usize)]
            .iter()
            .position(|&c| c == prefix)
        {
            if !self.have_ttl(idx) || !self.expired_and_remove(prefix, now) {
                return FindResult::Inline { idx };
            }

            let child_idx = self.remove_inline(idx);

            if child_idx == u32::MAX {
                return None;
            }

            return FindResult::Expired { child_idx };
        }
        let ttl_idx = {
            let Some(overflow) = self.overflow.as_mut() else {
                return FindResult::None;
            };
            let Some((idx, has_ttl)) = overflow.position_and_ttl(prefix) else {
                return FindResult::None;
            };
            if !has_ttl {
                return FindResult::Overflow { idx };
            }

            idx
        };
        if !self.expired_and_remove(prefix, now) {
            return FindResult::Overflow { idx: ttl_idx };
        }
        //SAFETY: la valeur de overflow a déja était check plus haut
        let (_, child_idx) =
            unsafe { self.overflow.as_mut().unwrap_unchecked() }.swap_remove(ttl_idx);

        if child_idx == u32::MAX {
            return None;
        }

        FindResult::Expired { child_idx }
    }
    fn have_ttl(&self, index: usize) -> bool {
        (self.ttl_map & (1 << index)) != 0
    }
    fn expired_and_remove(&mut self, prefix: u8, now: u64) -> bool {
        let ttls = self
            .ttls
            .as_mut()
            .expect("There is a value with a ttl but there is no ttls store associated");

        let idx = ttls
            .iter()
            .position(|&exp| exp.parent_radix() == prefix)
            .expect(
                "prefix is associated to a tll but no coresponding tll entry found in ttls map",
            );
        if ttls[idx]
            .exp()
            .expect("value with a ttl, a ttl entry but the field ttl is empty")
            > now
        {
            return false;
        }
        ttls.swap_remove(idx);
        true
    }
    fn remove_inline(&mut self, idx: usize) -> u32 {
        self.count -= 1;
        let count = self.count as usize;
        let bit = (self.ttl_map >> count) & 1;
        self.ttl_map = (self.ttl_map & !(1 << idx)) | (bit << idx);
        self.prefix[idx] = self.prefix[count];
        let ret = self.childs_idx[idx];
        self.childs_idx[idx] = self.childs_idx[count];
        // Branchless : si idx == count (on retire justement le dernier
        // élément), le write ci-dessous ne fait que réécrire les mêmes
        // octets sur place. Seul `removed` est droppé — `idx`/`count` sont
        // hors de la plage valide [0, count) après le décrément, donc ce
        // slot n'est plus jamais relu ni redroppé.
        let ptr = self.inline_keys.as_mut_ptr();
        unsafe {
            let removed = ptr.add(idx).read();
            ptr.add(idx).write(ptr.add(count).read());
            drop(removed);
        }
        ret
    }
    fn inline_get_child_idx(&self, idx: usize) -> u32 {
        assert!(idx < self.count as usize);
        unsafe {
            std::hint::assert_unchecked(idx < INLINE_SET_LEN);
        }
        self.childs_idx[idx]
    }
    fn inline_get_key(&self, idx: usize) -> &SharedByte {
        assert!(idx < self.count as usize);
        unsafe {
            std::hint::assert_unchecked(idx < INLINE_SET_LEN);
            self.inline_keys[idx].assume_init_ref()
        }
    }
}

trait Lookup {
    type LookUpResult;
    fn lookup(&self, store: &[ValNode], key: &[u8]) -> Self::LookUpResult;
}

pub struct TtlNode {
    next_node_idx: u32,
    count: u32,
    ttl: [ExpAndRadix; 7],
}
impl TtlNode {
    fn new(exp: u64, radix: u8) -> Self {
        TtlNode {
            count: 1,
            next_node_idx: u32::MAX,
            ttl: [
                ExpAndRadix::new(exp, radix),
                ExpAndRadix::default(),
                ExpAndRadix::default(),
                ExpAndRadix::default(),
                ExpAndRadix::default(),
                ExpAndRadix::default(),
                ExpAndRadix::default(),
            ],
        }
    }
    fn push(&mut self, exp: u64, radix: u8) {
        assert!(self.count < 7);
        self.ttl[self.count as usize] = ExpAndRadix::new(exp, radix);
        self.count += 1;
    }
}

pub struct ValTrees {
    nodes: HiSlab<ValNode>,
}
impl ValTrees {
    fn node_hash(&self, idx: u32) -> &HashNode {
        assert_eq!(self.nodes[idx].tag(), NodeType::HashSet);
        unsafe { &self.nodes[idx].hash }
    }
    fn node_set(&self, idx: u32) -> &SetNode {
        assert_eq!(self.nodes[idx].tag(), NodeType::Set);
        unsafe { &self.nodes[idx].set }
    }
    fn node_hash_mut(&mut self, idx: u32) -> &mut HashNode {
        assert_eq!(self.nodes[idx].tag(), NodeType::HashSet);
        unsafe { &mut self.nodes[idx].hash }
    }
    fn node_set_mut(&mut self, idx: u32) -> &mut SetNode {
        assert_eq!(self.nodes[idx].tag(), NodeType::Set);
        unsafe { &mut self.nodes[idx].set }
    }
}

enum CmdHashSetResult {
    New,
    SameKey,
    SameKeyAndVal,
}

impl ValTrees {
    fn set_is_set(&mut self, start_index: u32, key: &[u8], now: u64) -> bool {
        let mut idx = start_index;
        let mut prefix = key[0];
        let mut i = 0;
        let mut to_fill = VecDeque::new();
        loop {
            let node = self.node_set_mut(idx);
            match node.find_from_prefix(prefix, now) {
                FindResult::None => return false,
                FindResult::Inline { idx: val_idx } => {
                    let child_idx = node.childs_idx[val_idx];
                    if child_idx != u32::MAX {
                        self.node_set(child_idx).prefetch();
                        if self.node_set(idx).inline_get_key(val_idx).as_ref() == key {
                            return true;
                        }
                        idx = child_idx;
                    } else {
                        return self.node_set(idx).inline_get_key(val_idx).as_ref() == key;
                    }
                }
                FindResult::Overflow { idx: val_idx } => {
                    let child_idx = node.overflow.as_ref().unwrap().idx(val_idx);
                    if child_idx != u32::MAX {
                        self.node_set(child_idx).prefetch();
                        if self.validate_value(idx, val_idx, key) {
                            return true;
                        }
                    } else {
                        return self.validate_value(idx, val_idx, key);
                    }
                }
                FindResult::Expired { child_idx } => {
                    //Expired occurs only and only if the value expired and have childs
                    //if it expired and have no childs it simply return None
                    self.node_set(child_idx).prefetch();
                    to_fill.push_back(idx);
                    idx = child_idx;
                }
            }
            i += 1;
            if i >= key.len() {
                return false;
            }
            prefix = key[i];
        }
    }
    fn hash_get(&self, start_index: u32, key: &[u8]) -> Option<SharedByte> {}
    ///return true if the key was already presente
    fn set_set(&mut self, start_index: u32, key: &[u8]) -> bool {}
    fn hash_set(&mut self, start_index: u32, key: SharedByte, val: SharedByte) -> CmdHashSetResult {
    }

    fn validate_value(&self, idx: u32, val_idx: usize, key: &[u8]) -> bool {
        self.node_set(idx)
            .overflow
            .as_ref()
            .unwrap()
            .key(val_idx)
            .as_ref()
            == key
    }
}

// Sondes de taille : rust-analyzer se trompe souvent sur le niche-filling des
// types custom (ex: affiche 16 pour `Option<ThinVec<_>>` alors que rustc dit
// 8). Ces asserts sont vérifiés par le vrai compilateur à chaque build — la
// source de vérité, pas le hover de l'éditeur.
const _: () = assert!(size_of::<SharedByte>() == 8);
const _: () = assert!(size_of::<Option<ThinVec<ExpAndRadix>>>() == 8);
const _: () = assert!(size_of::<HashNode>() == 128);
const _: () = assert!(size_of::<SetNode>() == 128);
const _: () = assert!(size_of::<ValNode>() == 128);
