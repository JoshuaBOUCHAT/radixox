//! Commandes ZSet sur le ZSet de `crate::zset` (ANALYSE_ART_UNIFIE.md §13).
//!
//! `InnerZCommand` (l'ancien ZSet : `SmallVec` trié puis `BTreeSet` + `HashMap`) ne sert
//! plus que d'oracle aux tests différentiels.

use radixox_lib::shared_byte::SharedByte;

use crate::{
    OxidArt,
    error::TypeError,
    value::{RedisType, Tag, alloc_zset, zset_mut},
    zset::{Added, ZSet, decode_score, encode_score},
};

#[cfg(test)]
pub(crate) use oracle::InnerZCommand;

#[cfg(test)]
mod oracle {
    use crate::zset_inner::ZSetInner;

    use hashbrown::HashMap;
    use ordered_float::OrderedFloat;
    use radixox_lib::shared_byte::SharedByte;
    use smallvec::SmallVec;
    use std::collections::BTreeSet;


    const THRESHOLD: usize = 16;

    // ---------------------------------------------------------------------------
    // InnerZCommand — dynamic Small/Large representation
    // ---------------------------------------------------------------------------

    /// ZSet iterator — zero-overhead enum dispatch over sorted elements.
    pub struct ZIter<'a> {
        inner: ZIterInner<'a>,
    }

    enum ZIterInner<'a> {
        Small(std::slice::Iter<'a, (OrderedFloat<f64>, SharedByte)>),
        Large(std::collections::btree_set::Iter<'a, (OrderedFloat<f64>, SharedByte)>),
    }

    impl<'a> Iterator for ZIter<'a> {
        type Item = &'a (OrderedFloat<f64>, SharedByte);

        fn next(&mut self) -> Option<Self::Item> {
            match &mut self.inner {
                ZIterInner::Small(i) => i.next(),
                ZIterInner::Large(i) => i.next(),
            }
        }
    }
    const _: () = assert!(size_of::<InnerZCommand>() <= 64);

    #[derive(Clone, Debug, PartialEq)]
    pub enum InnerZCommand {
        /// Small ZSet: sorted Vec<(score, member)> for sets below the threshold.
        /// Vec = 24 bytes — cache-friendly, no heap indirection.
        Small(SmallVec<[(OrderedFloat<f64>, SharedByte); 2]>),
        /// Large ZSet: boxed double-indexed structure for O(1) score lookup + O(log n) range.
        /// Box = 8 bytes — keeps InnerZCommand ≤32 bytes total.
        Large(ZSetInner),
    }

    impl InnerZCommand {
        pub fn new() -> Self {
            InnerZCommand::Small(SmallVec::new())
        }

        /// Insert or update a member with a score.
        /// Returns true if this is a new member (not an update).
        pub(crate) fn insert(&mut self, score: f64, member: SharedByte) -> bool {
            let score = OrderedFloat(score);
            match self {
                InnerZCommand::Small(vec) => {
                    // Check if member already exists.
                    if let Some(pos) = vec.iter().position(|(_, m)| m == &member) {
                        if vec[pos].0 == score {
                            return false; // same score, nothing to do
                        }
                        // Re-insert at correct sorted position, en gardant le membre
                        // stocké (déjà externe) plutôt que l'argument.
                        let (_, member) = vec.remove(pos);
                        let new_pos =
                            vec.partition_point(|(s, m)| (*s, m.as_ref()) < (score, member.as_ref()));
                        vec.insert(new_pos, (score, member));
                        return false; // existing member, score updated
                    }
                    // Membres externes : le GC ne visite pas les ZSet (MEMORY_ARCH.md §16.9).
                    let member = member.into_external();
                    // New member — promote or push.
                    if vec.len() >= THRESHOLD {
                        let cap = vec.len() + 1;
                        let mut scores = HashMap::with_capacity(cap);
                        let mut sorted = BTreeSet::new();
                        for (s, m) in vec.drain(..) {
                            scores.insert(m.clone(), s);
                            sorted.insert((s, m));
                        }
                        scores.insert(member.clone(), score);
                        sorted.insert((score, member));
                        *self = InnerZCommand::Large(ZSetInner { sorted, scores });
                    } else {
                        let pos =
                            vec.partition_point(|(s, m)| (*s, m.as_ref()) < (score, member.as_ref()));
                        // Avoid Vec's default MIN_NON_ZERO_CAP=4 growth: allocate exactly 1 slot.
                        if vec.len() == vec.capacity() {
                            vec.reserve_exact(1);
                        }
                        vec.insert(pos, (score, member));
                    }
                    true
                }
                InnerZCommand::Large(zset) => zset.insert(score.into_inner(), member),
            }
        }

        /// Remove a member. Returns true if the member existed.
        pub(crate) fn remove(&mut self, member: SharedByte) -> bool {
            match self {
                InnerZCommand::Small(vec) => {
                    if let Some(pos) = vec.iter().position(|(_, m)| m == &member) {
                        vec.remove(pos);
                        true
                    } else {
                        false
                    }
                }
                InnerZCommand::Large(zset) => zset.remove(member),
            }
        }

        /// Get the score of a member. O(n) for Small, O(1) for Large.
        pub(crate) fn score(&self, member: SharedByte) -> Option<f64> {
            match self {
                InnerZCommand::Small(vec) => vec
                    .iter()
                    .find(|(_, m)| m == &member)
                    .map(|(s, _)| s.into_inner()),
                InnerZCommand::Large(zset) => zset.score(&member),
            }
        }

        pub(crate) fn len(&self) -> usize {
            match self {
                InnerZCommand::Small(v) => v.len(),
                InnerZCommand::Large(z) => z.len(),
            }
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// Iterate over members in ascending score order.
        /// Small is kept sorted, so both variants iterate in order.
        pub(crate) fn iter(&self) -> ZIter<'_> {
            ZIter {
                inner: match self {
                    InnerZCommand::Small(v) => ZIterInner::Small(v.iter()),
                    InnerZCommand::Large(z) => ZIterInner::Large(z.sorted.iter()),
                },
            }
        }
    }

    impl Default for InnerZCommand {
        fn default() -> Self {
            Self::new()
        }
    }

}

impl OxidArt {
    /// ZSet de `key`, créé s'il n'existe pas.
    fn get_zset_mut(&mut self, ttl: Option<u64>, key: SharedByte) -> Result<&'static mut ZSet, TypeError> {
        // Un ZSet existant ne modifie pas le nœud oxidart.
        let idx = self
            .ensure_container(&key, ttl, Tag::ZSet, || alloc_zset(ZSet::new()))
            .map_err(|_| TypeError::ValueNotSet)?;
        Ok(zset_mut(idx))
    }

    /// ZADD : nombre de membres ajoutés (les mises à jour ne comptent pas).
    pub fn cmd_zadd(&mut self, key: SharedByte, score_members: &[(f64, SharedByte)], ttl: Option<u64>) -> Result<u32, TypeError> {
        debug_assert!(!score_members.is_empty());
        if score_members.iter().any(|(s, _)| s.is_nan()) {
            return Err(TypeError::NotAFloat);
        }
        let zset = self.get_zset_mut(ttl, key)?;
        let mut added = 0;
        for (score, member) in score_members {
            match zset.add(encode_score(*score), member) {
                Some(Added::New) => added += 1,
                Some(_) => {}
                None => return Err(TypeError::OutOfMemory),
            }
        }
        Ok(added)
    }

    /// ZCARD.
    pub fn cmd_zcard(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(0);
        };
        Ok(val.as_zset()?.len() as u32)
    }

    /// ZRANGE par rang (indices négatifs depuis la fin) : sélection du premier rang en
    /// O(log n), puis les feuilles chaînées.
    pub fn cmd_zrange(&mut self, key: &[u8], start: i64, stop: i64, with_scores: bool) -> Result<Vec<SharedByte>, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(Vec::new());
        };
        let zset = val.as_zset()?;
        let len = zset.len() as i64;
        if len == 0 {
            return Ok(Vec::new());
        }
        let start = if start < 0 { (len + start).max(0) } else { start };
        let stop = if stop < 0 { len + stop } else { stop.min(len - 1) };
        if start > stop || start >= len {
            return Ok(Vec::new());
        }
        let n = (stop - start + 1) as usize;
        let mut result = Vec::with_capacity(if with_scores { 2 * n } else { n });
        for (score, member) in zset.iter_from(start as usize).take(n) {
            result.push(SharedByte::from_slice(member));
            if with_scores {
                result.push(SharedByte::from_slice(decode_score(score).to_string()));
            }
        }
        Ok(result)
    }

    /// ZSCORE.
    pub fn cmd_zscore(&mut self, key: &[u8], member: SharedByte) -> Result<Option<f64>, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(None);
        };
        Ok(val.as_zset()?.score(&member).map(decode_score))
    }

    /// ZREM : nombre de membres retirés ; un ZSet vidé disparaît.
    pub fn cmd_zrem(&mut self, key: &[u8], members: &[SharedByte]) -> Result<u32, RedisType> {
        debug_assert!(!members.is_empty());
        let (removed, empty) = {
            let Some(val) = self.get_ref(key) else {
                return Ok(0);
            };
            let zset = zset_mut(val.zset_idx()?);
            let removed = members.iter().filter(|m| zset.remove(m).is_some()).count() as u32;
            (removed, zset.is_empty())
        };
        if empty {
            self.del(key);
        }
        Ok(removed)
    }

    /// ZINCRBY : nouveau score ; un membre absent part de 0.
    pub fn cmd_zincrby(&mut self, key: SharedByte, increment: f64, member: SharedByte) -> Result<f64, TypeError> {
        if increment.is_nan() {
            return Err(TypeError::NotAFloat);
        }
        let zset = self.get_zset_mut(None, key)?;
        let new = zset.score(&member).map_or(0.0, decode_score) + increment;
        if new.is_nan() {
            return Err(TypeError::NotAFloat);
        }
        zset.add(encode_score(new), &member).ok_or(TypeError::OutOfMemory)?;
        Ok(new)
    }
}
