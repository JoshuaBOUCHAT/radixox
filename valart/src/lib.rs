use std::{
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
mod set;
pub use overflow::{HashOverflow, SetOverflow};
use set::{CollisionBuckets, SetNode};

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
    /// Zones de collision des Set (hors de l'union `ValNode`), atteintes
    /// quand les 7 octets de dispatch du hash sont épuisés.
    set_collisions: CollisionBuckets,
    /// Seed du hash des membres. Globale à tous les Set (merge-join entre
    /// Set possible) et à persister dans le dump : la position de chaque
    /// entrée en dépend.
    seed: u64,
    hasher: foldhash::quality::FixedState,
    /// Tests : masque appliqué au hash pour forcer des collisions.
    #[cfg(test)]
    hash_mask: u64,
}
impl ValTrees {
    /// Virtual capacity par défaut du slab : 2^26 nœuds × 128 B = 8 GiB
    /// réservés en virtuel, committés à la demande.
    pub const DEFAULT_VIRTUAL_CAPACITY: u32 = 1 << 26;

    pub fn new() -> Self {
        use std::hash::BuildHasher;
        let seed = std::collections::hash_map::RandomState::new().hash_one(0u64);
        Self::with_seed(seed, Self::DEFAULT_VIRTUAL_CAPACITY)
    }

    /// Seed explicite : rechargement d'un dump, tests reproductibles.
    pub fn with_seed(seed: u64, virtual_capacity: u32) -> Self {
        Self {
            nodes: HiSlab::new(0, virtual_capacity).expect("valart: mmap du slab impossible"),
            set_collisions: CollisionBuckets::default(),
            seed,
            hasher: foldhash::quality::FixedState::with_seed(seed),
            #[cfg(test)]
            hash_mask: u64::MAX,
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Octets occupés dans le slab (nœuds vivants × 128 B). Le slab est un
    /// mmap : invisible pour l'allocateur global, à ajouter à part. Les
    /// overflows et buckets de collision passent, eux, par l'allocateur.
    pub fn slab_bytes(&self) -> usize {
        self.nodes.count_occupied() * size_of::<ValNode>()
    }
}
impl Default for ValTrees {
    fn default() -> Self {
        Self::new()
    }
}
impl ValTrees {
    fn node_hash(&self, idx: u32) -> &HashNode {
        assert_eq!(self.nodes[idx].tag(), NodeType::HashSet);
        unsafe { &self.nodes[idx].hash }
    }
    fn node_hash_mut(&mut self, idx: u32) -> &mut HashNode {
        assert_eq!(self.nodes[idx].tag(), NodeType::HashSet);
        unsafe { &mut self.nodes[idx].hash }
    }
}

enum CmdHashSetResult {
    New,
    SameKey,
    SameKeyAndVal,
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
