//! Commandes ZSet sur le ZSet de `crate::zset` (ANALYSE_ART_UNIFIE.md §13).
//!
//! `InnerZCommand` (l'ancien ZSet : `SmallVec` trié puis `BTreeSet` + `HashMap`) ne sert
//! plus que d'oracle aux tests différentiels.

use radixox_lib::cmd::{ZAddFlags, ZRangeBy};
use radixox_lib::shared_byte::SharedByte;

use crate::{
    OxidArt,
    error::TypeError,
    value::{RedisType, Tag, alloc_zset, zset_mut, zset_ref},
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

/// Entrées des rangs `[lo, hi)`, dans le sens demandé (scores formatés sans allocation).
fn zrange_out(zset: &ZSet, lo: usize, hi: usize, rev: bool, with_scores: bool) -> Vec<SharedByte> {
    let n = hi.saturating_sub(lo);
    let mut out = Vec::with_capacity(if with_scores { 2 * n } else { n });
    let mut push = |(s, m): (u64, &[u8])| {
        out.push(SharedByte::from_slice(m));
        if with_scores {
            out.push(crate::zset::format_score(decode_score(s)));
        }
    };
    if n > 0 {
        if rev {
            zset.iter_back_from(hi - 1).take(n).for_each(&mut push);
        } else {
            zset.iter_from(lo).take(n).for_each(&mut push);
        }
    }
    out
}

/// Réponse de ZADD : un compte, ou le score (INCR ; `None` si rien n'a été fait).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZAddReply {
    Count(u32),
    Score(Option<f64>),
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

    /// ZADD avec options (NX, XX, GT, LT, CH, INCR ; combinaisons validées par le
    /// parseur), comme `zsetAdd` de Redis : NX n'ajoute que les nouveaux, XX ne met à jour
    /// que les présents, GT / LT ne mettent à jour que si le score monte / descend (les
    /// nouveaux sont ajoutés), CH compte aussi les mises à jour. XX sur une clé absente ne
    /// la crée pas.
    pub fn cmd_zadd_opts(
        &mut self,
        key: SharedByte,
        flags: ZAddFlags,
        score_members: &[(f64, SharedByte)],
        ttl: Option<u64>,
    ) -> Result<ZAddReply, TypeError> {
        if score_members.iter().any(|(s, _)| s.is_nan()) {
            return Err(TypeError::NotAFloat);
        }
        let zset = if flags.xx {
            let Some(val) = self.get_ref(&key) else {
                return Ok(if flags.incr { ZAddReply::Score(None) } else { ZAddReply::Count(0) });
            };
            zset_mut(val.zset_idx().map_err(|_| TypeError::ValueNotSet)?)
        } else {
            self.get_zset_mut(ttl, key)?
        };
        if !flags.xx && !flags.incr {
            zset.reserve(score_members);
        }
        let (mut added, mut updated) = (0u32, 0u32);
        let mut last = None;
        for &(score, ref member) in score_members {
            last = None;
            let mut nan = false;
            // une seule recherche du membre : la décision se prend sur son ancien score
            let r = zset
                .upsert(member, |old| match old.map(decode_score) {
                    Some(_) if flags.nx => None,
                    None if flags.xx => None,
                    Some(old) => {
                        let new = if flags.incr { old + score } else { score };
                        if new.is_nan() {
                            nan = true;
                            return None;
                        }
                        if (flags.gt && new <= old) || (flags.lt && new >= old) {
                            return None;
                        }
                        last = Some(new);
                        (new != old).then(|| encode_score(new))
                    }
                    None => {
                        last = Some(score);
                        Some(encode_score(score))
                    }
                })
                .ok_or(TypeError::OutOfMemory)?;
            if nan {
                return Err(TypeError::NotAFloat);
            }
            match r {
                Added::New => added += 1,
                Added::Updated(_) => updated += 1,
                Added::Unchanged => {}
            }
        }
        Ok(if flags.incr {
            ZAddReply::Score(last)
        } else {
            ZAddReply::Count(if flags.ch { added + updated } else { added })
        })
    }

    /// ZRANGE et ses variantes : la plage en rangs `[lo, hi)`, puis `LIMIT offset count`
    /// dans le sens du parcours (REV : du plus grand au plus petit). Rang + REV : les
    /// indices comptent depuis la fin.
    pub fn cmd_zrange_by(
        &mut self,
        key: &[u8],
        range: ZRangeBy<'_>,
        rev: bool,
        limit: Option<(i64, i64)>,
        with_scores: bool,
    ) -> Result<Vec<SharedByte>, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(Vec::new());
        };
        let zset = val.as_zset()?;
        let len = zset.len();
        let (mut lo, mut hi) = match range {
            // rangs comptés dans l'ordre inverse : ramenés à l'ordre croissant
            ZRangeBy::Rank(a, b) if rev => {
                let (l, h) = zset.rank_range(a, b);
                (len - h, len - l)
            }
            r => zset.resolve(r),
        };
        if let Some((offset, count)) = limit {
            // Offset négatif : non documenté ; Valkey y rend vide ou le dernier élément
            // selon l'encodage du ZSet (listpack ou skiplist). Ici : toujours vide.
            if offset < 0 {
                return Ok(Vec::new());
            }
            let n = hi - lo;
            let off = (offset as usize).min(n);
            let take = if count < 0 { n - off } else { (count as usize).min(n - off) };
            if rev {
                hi -= off;
                lo = hi - take;
            } else {
                lo += off;
                hi = lo + take;
            }
        }
        Ok(zrange_out(zset, lo, hi, rev, with_scores))
    }

    /// ZRANK / ZREVRANK : `(rang, score)`.
    pub fn cmd_zrank(&mut self, key: &[u8], member: &[u8], rev: bool) -> Result<Option<(u64, f64)>, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(None);
        };
        let zset = val.as_zset()?;
        let Some(r) = zset.rank(member) else { return Ok(None) };
        let score = decode_score(zset.score(member).expect("membre classé"));
        Ok(Some((if rev { zset.len() - 1 - r } else { r } as u64, score)))
    }

    /// ZCOUNT / ZLEXCOUNT : deux rangs, O(log n).
    pub fn cmd_zcount(&mut self, key: &[u8], range: ZRangeBy<'_>) -> Result<u64, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(0);
        };
        let (lo, hi) = val.as_zset()?.resolve(range);
        Ok((hi - lo) as u64)
    }

    /// ZPOPMIN / ZPOPMAX : `(membre, score)` retirés, du plus petit (ou plus grand) ; un
    /// ZSet vidé disparaît.
    pub fn cmd_zpop(&mut self, key: &[u8], max: bool, count: u64) -> Result<Vec<(SharedByte, f64)>, RedisType> {
        let (popped, empty) = {
            let Some(val) = self.get_ref(key) else {
                return Ok(Vec::new());
            };
            let zset = zset_mut(val.zset_idx()?);
            let n = (count as usize).min(zset.len());
            let picked: Vec<(SharedByte, f64)> = if max {
                zset.iter_back_from(zset.len().wrapping_sub(1)).take(n).map(|(s, m)| (SharedByte::from_slice(m), decode_score(s))).collect()
            } else {
                zset.iter().take(n).map(|(s, m)| (SharedByte::from_slice(m), decode_score(s))).collect()
            };
            for (m, _) in &picked {
                zset.remove(m);
            }
            (picked, zset.is_empty())
        };
        if empty {
            self.del(key);
        }
        Ok(popped)
    }

    /// ZREMRANGEBYRANK / BYSCORE / BYLEX : nombre de membres retirés.
    pub fn cmd_zremrange(&mut self, key: &[u8], range: ZRangeBy<'_>) -> Result<u64, RedisType> {
        let (removed, empty) = {
            let Some(val) = self.get_ref(key) else {
                return Ok(0);
            };
            let idx = val.zset_idx()?;
            let (lo, hi) = zset_ref(idx).resolve(range);
            if lo >= hi {
                return Ok(0);
            }
            let zset = zset_mut(idx);
            let removed = match zset {
                // grande plage : feuilles du milieu décrochées d'un coup, rendues par tranches
                ZSet::Large(z) => z.remove_range(idx, lo as u32, hi as u32) as u64,
                ZSet::Small(_) => {
                    let members = zset_ref(idx).members_in(lo, hi);
                    for m in &members {
                        zset.remove(m);
                    }
                    members.len() as u64
                }
            };
            (removed, zset.is_empty())
        };
        if empty {
            self.del(key);
        }
        Ok(removed)
    }

    /// ZMSCORE.
    pub fn cmd_zmscore(&mut self, key: &[u8], members: &[&[u8]]) -> Result<Vec<Option<f64>>, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(vec![None; members.len()]);
        };
        let zset = val.as_zset()?;
        Ok(members.iter().map(|m| zset.score(m).map(decode_score)).collect())
    }

    /// ZRANDMEMBER : `count > 0` = membres distincts (au plus tous), `count < 0` =
    /// `|count|` tirages avec répétition ; sans count, un seul. Tirage uniforme par rang
    /// (sélection O(log n)).
    pub fn cmd_zrandmember(&mut self, key: &[u8], count: Option<i64>) -> Result<Vec<(SharedByte, f64)>, RedisType> {
        use rand::Rng;
        let Some(val) = self.get_ref(key) else {
            return Ok(Vec::new());
        };
        let zset = val.as_zset()?;
        let len = zset.len();
        if len == 0 {
            return Ok(Vec::new());
        }
        let at = |r: usize| {
            let (s, m) = zset.iter_from(r).next().expect("rang dans le ZSet");
            (SharedByte::from_slice(m), decode_score(s))
        };
        let mut rng = rand::thread_rng();
        Ok(match count {
            None => vec![at(rng.gen_range(0..len))],
            Some(c) if c < 0 => (0..c.unsigned_abs()).map(|_| at(rng.gen_range(0..len))).collect(),
            Some(c) if c as usize >= len => zset.iter().map(|(s, m)| (SharedByte::from_slice(m), decode_score(s))).collect(),
            Some(c) => {
                // rangs distincts (Floyd) : c tirages, sans rejet
                let mut picked = std::collections::BTreeSet::new();
                for j in len - c as usize..len {
                    let r = rng.gen_range(0..=j);
                    if !picked.insert(r) {
                        picked.insert(j);
                    }
                }
                picked.into_iter().map(at).collect()
            }
        })
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
        let mut new = f64::NAN;
        zset.upsert(&member, |old| {
            new = old.map_or(0.0, decode_score) + increment;
            (!new.is_nan()).then(|| encode_score(new))
        })
        .ok_or(TypeError::OutOfMemory)?;
        if new.is_nan() {
            return Err(TypeError::NotAFloat);
        }
        Ok(new)
    }
}
