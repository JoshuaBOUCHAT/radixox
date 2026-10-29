#[allow(unused_imports)]
use crate::prelude::*;
use core::{mem::ManuallyDrop, ptr::NonNull};

use crate::{
    ExpAndRadix,
    compact_path::CompactPath,
    value::{Tag, ValUnion},
};

const INLINE_CAPACITY: usize = 8;
pub struct Node {
    prefix: CompactPath,
    exp_and_radix: ExpAndRadix,
    val: ValUnion,
    tag: Tag,
    ttls_map: u16,
    tag_or_val_map: u16,
    test: NodeType,
    childs_count: u8,
    val_count: u8,
    /// Parent node index (for TTL eviction)
    parent_idx: u32,
    ///Les unions sont la pour canibaliser les store de radix/ tags en bitmap
    ///  quand on passe dasn les catégorie d'au dessus pour ne pas a avoir a paye les bitmap pour pour les niveau plsu gros
    radixs: RadixsOrBitMap,
    tags: TagsOrBitMap,

    inline: [NodeUnion; 8],
    ptr: NonNull<*mut u8>,
}
union NodeUnion {
    idx: [u32; 2],
    ttl: u64,
    val: ManuallyDrop<ValUnion>,
}
enum NodeType {
    Inline,
    Overflow8,
    Overflow24,
}
union RadixsOrBitMap {
    radixs: [u8; INLINE_CAPACITY],
    map: u64,
}
union TagsOrBitMap {
    tags: [Tag; INLINE_CAPACITY],
    map: u64,
}
