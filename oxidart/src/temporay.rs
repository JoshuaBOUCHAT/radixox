use crate::{compact_str::CompactStr, value::Value};

//On sépare Val et exp meme si exp n'as pas de sens si val == None
//car en utilisant u64::max comme niche + les 8 bit de poids fort on save de la mémoire pour
//qu inline child passe a 10 et donc sois capable de tenir de 0->9 pour être efficient sur user:[DECIMALS] par exemple
struct NodeTest {
    compression: CompactStr,
    inline_childs: ChildTest,
    val: Option<Value>,
    parent_idx: u32,
    overflow_idx: u32, //u32::MAX marque None
    exp: u64,          //utilise les 8 bit de poid fort pour parent_radix:u8 et u64::max comme None
}
#[repr(C)]
pub(crate) struct ChildTest {
    idxs: [u32; 10],
    radixs: [u8; 10],
    len: u8,
}
