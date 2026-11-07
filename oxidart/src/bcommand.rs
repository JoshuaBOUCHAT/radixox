//! Commandes bitset (BITSET_DESIGN.md §10.4) : noms et numéros de bits de Redis sur un
//! type à part. La « longueur » (indices négatifs, bornes) est déduite du contenu :
//! `⌈(dernier bit à 1 + 1) / 8⌉` octets. Un bitset vide n'existe pas : la clé part avec son
//! dernier bit à 1.

#[allow(unused_imports)]
use crate::prelude::*;

pub use crate::bitset::op::BitOp;
use crate::{
    OxidArt,
    bitset::{self, Bitset},
    value::{RawVal, RedisType, Tag, ValUnion},
};

/// Unité des bornes de BITCOUNT / BITPOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitUnit {
    Byte,
    Bit,
}

/// Bornes `start..=end` (négatives depuis la fin) d'une longueur `totlen`, à la Redis.
/// `None` : plage vide.
fn span(start: i64, end: i64, totlen: i64) -> Option<(i64, i64)> {
    let mut start = if start < 0 { totlen.saturating_add(start) } else { start };
    let mut end = if end < 0 { totlen.saturating_add(end) } else { end };
    start = start.max(0);
    end = end.max(0);
    if end >= totlen {
        end = totlen - 1;
    }
    (start <= end && totlen > 0).then_some((start, end))
}

/// Plage de bits `[a, b]` d'une plage dans l'unité `unit`.
fn bit_span(b: &Bitset, start: i64, end: i64, unit: BitUnit) -> Option<(u64, u64)> {
    let bytes = b.last().map_or(0, |l| l as i64 / 8 + 1);
    match unit {
        BitUnit::Byte => span(start, end, bytes).map(|(s, e)| (s as u64 * 8, e as u64 * 8 + 7)),
        BitUnit::Bit => span(start, end, bytes * 8).map(|(s, e)| (s as u64, e as u64)),
    }
}

impl OxidArt {
    /// En-tête du bitset à `key` : `None` si absente.
    fn bitset_idx(&mut self, key: &[u8]) -> Result<Option<u32>, RedisType> {
        match self.get_ref(key) {
            Some(v) => v.bitset_idx().map(Some),
            None => Ok(None),
        }
    }

    /// SETBIT : rend l'ancien bit. Mettre à 0 le dernier bit à 1 supprime la clé ; mettre
    /// à 0 sur une clé absente ne la crée pas.
    pub fn cmd_setbit(&mut self, key: &[u8], off: u32, on: bool) -> Result<bool, RedisType> {
        if on {
            let idx = match self.bitset_idx(key)? {
                Some(idx) => idx,
                None => self
                    .ensure_container(key, None, Tag::Bitset, || bitset::alloc(Bitset::from_sorted(&[])))
                    .map_err(Tag::redis_type)?,
            };
            let old = bitset::get_mut(idx).set(off, true);
            bitset::note_migration(idx);
            return Ok(old);
        }
        let Some(idx) = self.bitset_idx(key)? else { return Ok(false) };
        let b = bitset::get_mut(idx);
        let old = b.clear(off, true);
        if b.count() == 0 {
            self.del(key);
        } else {
            bitset::note_migration(idx);
        }
        Ok(old)
    }

    pub fn cmd_getbit(&mut self, key: &[u8], off: u32) -> Result<bool, RedisType> {
        Ok(self.bitset_idx(key)?.is_some_and(|i| bitset::get(i).get(off)))
    }

    /// BITCOUNT : sans plage en O(1).
    pub fn cmd_bitcount(&mut self, key: &[u8], range: Option<(i64, i64, BitUnit)>) -> Result<u64, RedisType> {
        let Some(idx) = self.bitset_idx(key)? else { return Ok(0) };
        let b = bitset::get(idx);
        Ok(match range {
            None => b.count(),
            Some((s, e, unit)) => bit_span(b, s, e, unit).map_or(0, |(a, z)| b.count_in(a, z + 1)),
        })
    }

    /// BITOP : résultat dans `dest` (écrasée quel que soit son type, sans TTL ; supprimée
    /// si le résultat est vide). Rend la longueur en octets de la plus longue source.
    /// Toutes les sources lues avant d'écrire : `dest` peut en être une.
    pub fn cmd_bitop(&mut self, op: BitOp, dest: &[u8], keys: &[&[u8]]) -> Result<u64, RedisType> {
        let mut idxs = Vec::with_capacity(keys.len());
        for k in keys {
            idxs.push(self.bitset_idx(k)?);
        }
        let srcs: Vec<Option<&Bitset>> = idxs.iter().map(|i| i.map(bitset::get)).collect();
        let len = srcs.iter().flatten().map(|b| b.byte_len()).max().unwrap_or(0);
        match bitset::op::bitop(op, &srcs, len * 8) {
            None => {
                self.del(dest);
            }
            Some(b) => {
                let idx = bitset::alloc(b);
                self.set_raw(dest, RawVal::new(Tag::Bitset, ValUnion { idx }), None, None);
            }
        }
        Ok(len)
    }

    /// BITPOS : premier bit à `bit` dans la plage, -1 sinon. Cas limites de Redis : clé
    /// absente → `bit ? -1 : 0` ; un 0 cherché sans `end` et pas trouvé dans la plage →
    /// le bit juste après (au-delà, tout vaut 0).
    pub fn cmd_bitpos(
        &mut self,
        key: &[u8],
        bit: bool,
        start: Option<i64>,
        end: Option<i64>,
        unit: BitUnit,
    ) -> Result<i64, RedisType> {
        let Some(idx) = self.bitset_idx(key)? else { return Ok(if bit { -1 } else { 0 }) };
        let b = bitset::get(idx);
        let Some((a, z)) = bit_span(b, start.unwrap_or(0), end.unwrap_or(-1), unit) else { return Ok(-1) };
        if bit {
            return Ok(b.next_set(a, z + 1).map_or(-1, |k| k as i64));
        }
        Ok(match b.next_clear(a, z + 1) {
            Some(k) => k as i64,
            None if end.is_none() => z as i64 + 1,
            None => -1,
        })
    }
}
