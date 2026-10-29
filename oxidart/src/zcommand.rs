//! Commandes ZSet sur le ZSet de `crate::zset` (ANALYSE_ART_UNIFIE.md §13).
//!
//! `InnerZCommand` (l'ancien ZSet : `SmallVec` trié puis `BTreeSet` + `HashMap`) ne sert
//! plus que d'oracle aux tests différentiels.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::BuildHasherDefault;

use radixox_lib::cmd::{ZAddFlags, ZAggregate, ZOp, ZRangeBy};
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
    use crate::zset_inner::OrdF64;
    use radixox_lib::shared_byte::SharedByte;
    use radixox_lib::small_vec::SmallVec;
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
        Small(std::slice::Iter<'a, (OrdF64, SharedByte)>),
        Large(std::collections::btree_set::Iter<'a, (OrdF64, SharedByte)>),
    }

    impl<'a> Iterator for ZIter<'a> {
        type Item = &'a (OrdF64, SharedByte);

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
        Small(SmallVec<2, (OrdF64, SharedByte)>),
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
            let score = OrdF64(score);
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
                        for (s, m) in std::mem::take(vec) {
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
    pub fn cmd_zadd<M: AsRef<[u8]>>(&mut self, key: SharedByte, score_members: &[(f64, M)], ttl: Option<u64>) -> Result<u32, TypeError> {
        debug_assert!(!score_members.is_empty());
        if score_members.iter().any(|(s, _)| s.is_nan()) {
            return Err(TypeError::NotAFloat);
        }
        let zset = self.get_zset_mut(ttl, key)?;
        let mut added = 0;
        for (score, member) in score_members {
            match zset.add(encode_score(*score), member.as_ref()) {
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
    pub fn cmd_zadd_opts<M: AsRef<[u8]>>(
        &mut self,
        key: SharedByte,
        flags: ZAddFlags,
        score_members: &[(f64, M)],
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
                .upsert(member.as_ref(), |old| match old.map(decode_score) {
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
        let Some((lo, hi)) = self.zrange_ranks(key, range, rev, limit)? else { return Ok(Vec::new()) };
        let Some(val) = self.get_ref(key) else { return Ok(Vec::new()) };
        Ok(zrange_out(val.as_zset()?, lo, hi, rev, with_scores))
    }

    /// Entrées des rangs `[lo, hi)` (à l'envers avec `rev`) de `key`.
    pub fn zrange_at(&mut self, key: &[u8], lo: usize, hi: usize, rev: bool, with_scores: bool) -> Result<Vec<SharedByte>, RedisType> {
        let Some(val) = self.get_ref(key) else { return Ok(Vec::new()) };
        Ok(zrange_out(val.as_zset()?, lo, hi, rev, with_scores))
    }

    /// Rangs `[lo, hi)` (croissants) d'une lecture ZRANGE et variantes ; `None` : clé absente
    /// ou LIMIT d'offset négatif (vide).
    pub fn zrange_ranks(
        &mut self,
        key: &[u8],
        range: ZRangeBy<'_>,
        rev: bool,
        limit: Option<(i64, i64)>,
    ) -> Result<Option<(usize, usize)>, RedisType> {
        let Some(val) = self.get_ref(key) else {
            return Ok(None);
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
                return Ok(None);
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
        Ok(Some((lo, hi)))
    }

    /// Morceau d'une lecture de ZSet par tranches (non atomique) : au plus `n` entrées à
    /// partir du rang `from` (vers le haut, ou vers le bas avec `rev`), passées à `emit`
    /// (membre, puis score). Rend le nombre d'entrées ; moins que `n` : la clé a disparu,
    /// changé de type, ou rétréci.
    pub fn zrange_part(&mut self, key: &[u8], from: usize, n: usize, rev: bool, with_scores: bool, mut emit: impl FnMut(&[u8])) -> usize {
        let Some(Ok(zset)) = self.get_ref(key).map(|v| v.as_zset()) else { return 0 };
        if from >= zset.len() {
            return 0;
        }
        let n = if rev { n.min(from + 1) } else { n };
        let mut k = 0;
        let mut push = |(s, m): (u64, &[u8])| {
            emit(m);
            if with_scores {
                crate::zset::with_score_text(decode_score(s), &mut emit);
            }
            k += 1;
        };
        if rev {
            zset.iter_back_from(from).take(n).for_each(&mut push);
        } else {
            zset.iter_from(from).take(n).for_each(&mut push);
        }
        k
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
        use radixox_lib::rng;
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
        Ok(match count {
            None => vec![at(rng::below(len as u64) as usize)],
            Some(c) if c < 0 => (0..c.unsigned_abs()).map(|_| at(rng::below(len as u64) as usize)).collect(),
            Some(c) if c as usize >= len => zset.iter().map(|(s, m)| (SharedByte::from_slice(m), decode_score(s))).collect(),
            Some(c) => {
                // rangs distincts (Floyd) : c tirages, sans rejet
                let mut picked = std::collections::BTreeSet::new();
                for j in len - c as usize..len {
                    let r = rng::below(j as u64 + 1) as usize;
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
    pub fn cmd_zincrby(&mut self, key: SharedByte, increment: f64, member: impl AsRef<[u8]>) -> Result<f64, TypeError> {
        if increment.is_nan() {
            return Err(TypeError::NotAFloat);
        }
        let zset = self.get_zset_mut(None, key)?;
        let mut new = f64::NAN;
        zset.upsert(member.as_ref(), |old| {
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

/// Entrée d'une opération d'ensemble : ZSet, Set (score 1, comme Redis) ou clé absente.
enum ZInput {
    ZSet(u32),
    SetTree(u32, u32),
    SetSmall(*const u8),
    Empty,
}

impl ZInput {
    fn len(&self) -> usize {
        match *self {
            ZInput::ZSet(i) => zset_ref(i).len(),
            ZInput::SetTree(_, card) => card as usize,
            ZInput::SetSmall(p) => unsafe { crate::small::entries::<1>(p).len() },
            ZInput::Empty => 0,
        }
    }

    fn for_each(&self, mut f: impl FnMut(&[u8], f64)) {
        match *self {
            ZInput::ZSet(i) => zset_ref(i).iter().for_each(|(s, m)| f(m, decode_score(s))),
            ZInput::SetTree(root, _) => crate::value::valart().set_for_each(root, |m| f(m, 1.0)),
            ZInput::SetSmall(p) => unsafe { crate::small::entries::<1>(p) }.iter().for_each(|[m]| f(m, 1.0)),
            ZInput::Empty => {}
        }
    }

    /// Membres tant que `f` rend vrai.
    fn for_each_while(&self, mut f: impl FnMut(&[u8]) -> bool) {
        match *self {
            ZInput::ZSet(i) => {
                for (_, m) in zset_ref(i).iter() {
                    if !f(m) {
                        return;
                    }
                }
            }
            ZInput::SetTree(root, _) => {
                // pas d'arrêt dans le parcours valart : le reste est sauté
                let mut go = true;
                crate::value::valart().set_for_each(root, |m| go = go && f(m));
            }
            ZInput::SetSmall(p) => {
                for [m] in unsafe { crate::small::entries::<1>(p) } {
                    if !f(m) {
                        return;
                    }
                }
            }
            ZInput::Empty => {}
        }
    }

    fn score(&self, m: &[u8]) -> Option<f64> {
        match *self {
            ZInput::ZSet(i) => zset_ref(i).score(m).map(decode_score),
            ZInput::SetTree(root, _) => crate::value::valart().set_contains(root, m).then_some(1.0),
            ZInput::SetSmall(p) => unsafe { crate::small::find::<1>(p, m) }.map(|_| 1.0),
            ZInput::Empty => None,
        }
    }
}

type MemberTable = HashMap<&'static [u8], f64, BuildHasherDefault<MemberHasher>>;

/// Entrée sondée (ZINTER, ZDIFF, ZINTERCARD) : une petite forme se cherche en linéaire ;
/// au-delà de 16 membres, une table construite une fois revient moins cher que les sondes.
struct Probe<'a> {
    input: &'a ZInput,
    table: Option<MemberTable>,
}

impl<'a> Probe<'a> {
    fn new(input: &'a ZInput) -> Self {
        let linear = match *input {
            ZInput::ZSet(i) => matches!(zset_ref(i), ZSet::Small(_)),
            ZInput::SetSmall(_) => true,
            _ => false,
        };
        let table = (linear && input.len() > 16).then(|| {
            let mut t = MemberTable::with_capacity_and_hasher(input.len(), Default::default());
            // SAFETY: comme dans `zsetop_with` : membres lus avant toute modification
            input.for_each(|m, s| {
                t.insert(unsafe { std::mem::transmute::<&[u8], &'static [u8]>(m) }, s);
            });
            t
        });
        Self { input, table }
    }

    #[inline]
    fn score(&self, m: &[u8]) -> Option<f64> {
        match &self.table {
            Some(t) => t.get(m).copied(),
            None => self.input.score(m),
        }
    }
}

/// Hasher des membres pour les tables d'une commande : le hash de valart (un seul appel
/// par membre ; la longueur que `Hash for [u8]` écrit d'abord est ignorée).
#[derive(Default)]
struct MemberHasher(u64);

impl std::hash::Hasher for MemberHasher {
    #[inline]
    fn write(&mut self, b: &[u8]) {
        self.0 = crate::zset::large::member_hash(b);
    }
    #[inline]
    fn write_usize(&mut self, _: usize) {}
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Score pondéré : NaN (0 × ±inf) ramené à 0, comme Redis.
#[inline]
fn weighted(s: f64, w: f64) -> f64 {
    let x = s * w;
    if x.is_nan() { 0.0 } else { x }
}

#[inline]
fn aggregate(agg: ZAggregate, a: f64, b: f64) -> f64 {
    match agg {
        ZAggregate::Sum => {
            let x = a + b;
            if x.is_nan() { 0.0 } else { x }
        }
        // comme Redis : un NaN (poids × score des entrées sondées de ZINTER) laisse `a`
        ZAggregate::Min => if b < a { b } else { a },
        ZAggregate::Max => if b > a { b } else { a },
    }
}

impl OxidArt {
    fn zop_inputs(&mut self, keys: &[&[u8]]) -> Result<Vec<ZInput>, RedisType> {
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            out.push(match self.get_ref(k) {
                None => ZInput::Empty,
                Some(v) => unsafe {
                    match v.tag {
                        Tag::ZSet => ZInput::ZSet(v.val.idx),
                        Tag::Set => ZInput::SetTree(v.val.idx, v.val.cont.card),
                        Tag::SetSmall => ZInput::SetSmall(v.val.ptr),
                        t => return Err(t.redis_type()),
                    }
                },
            });
        }
        Ok(out)
    }

    /// ZUNION / ZINTER / ZDIFF : `f` reçoit les `(score, membre)` triés, membres empruntés
    /// aux entrées (aucune copie). `weights` vide : 1 partout ; ZDIFF : scores de la
    /// première entrée, sans poids.
    pub fn zsetop_with<R>(
        &mut self,
        op: ZOp,
        keys: &[&[u8]],
        weights: &[f64],
        agg: ZAggregate,
        f: impl FnOnce(&[(f64, &[u8])]) -> R,
    ) -> Result<R, RedisType> {
        let ins = self.zop_inputs(keys)?;
        let w = |i: usize| weights.get(i).copied().unwrap_or(1.0);
        // entrées par taille croissante, comme Redis (avec des infinis, l'ordre de
        // l'agrégation change le résultat : inf + -inf → 0)
        let mut order: Vec<usize> = (0..ins.len()).collect();
        order.sort_by_key(|&i| ins[i].len());
        // SAFETY: les membres restent dans les slabs (ZSet, valart, petites formes) tant
        // que la base ne change pas ; rien n'est modifié avant le retour de `f`, et les
        // tranches ne sortent pas de cette fonction.
        let keep = |m: &[u8]| -> &'static [u8] { unsafe { std::mem::transmute::<&[u8], &'static [u8]>(m) } };
        let mut out: Vec<(f64, &'static [u8])> = Vec::new();
        match op {
            ZOp::Union => {
                let cap: usize = ins.iter().map(ZInput::len).sum();
                out.reserve(cap);
                let mut pos: HashMap<&'static [u8], u32, BuildHasherDefault<MemberHasher>> =
                    HashMap::with_capacity_and_hasher(cap, Default::default());
                for i in order {
                    let wi = w(i);
                    ins[i].for_each(|m, s| {
                        let v = weighted(s, wi);
                        match pos.entry(keep(m)) {
                            Entry::Occupied(e) => {
                                let a = &mut out[*e.get() as usize].0;
                                *a = aggregate(agg, *a, v);
                            }
                            Entry::Vacant(e) => {
                                e.insert(out.len() as u32);
                                out.push((v, keep(m)));
                            }
                        }
                    });
                }
            }
            ZOp::Inter => {
                // la plus petite entrée, les autres sondées
                let first = order[0];
                out.reserve(ins[first].len());
                let probes: Vec<(usize, Probe)> = order[1..].iter().map(|&j| (j, Probe::new(&ins[j]))).collect();
                ins[first].for_each(|m, s| {
                    let mut v = weighted(s, w(first));
                    for (j, p) in &probes {
                        let j = *j;
                        match p.score(m) {
                            // pas ramené à 0 ici (Redis) : SUM → 0, MIN/MAX → inchangé
                            Some(sj) => v = aggregate(agg, v, sj * w(j)),
                            None => return,
                        }
                    }
                    out.push((v, keep(m)));
                });
            }
            ZOp::Diff => {
                let probes: Vec<Probe> = ins[1..].iter().map(Probe::new).collect();
                ins[0].for_each(|m, s| {
                    if probes.iter().all(|p| p.score(m).is_none()) {
                        out.push((s, keep(m)));
                    }
                });
            }
        }
        // -0 = 0 comme Redis (jamais de NaN ici : `weighted` et `aggregate` les ramènent à
        // 0) ; membres uniques : tri instable
        out.sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal).then_with(|| a.1.cmp(b.1)));
        Ok(f(&out))
    }

    /// [`Self::zsetop_with`] en `(score, membre)` possédés.
    pub fn cmd_zsetop(&mut self, op: ZOp, keys: &[&[u8]], weights: &[f64], agg: ZAggregate) -> Result<Vec<(f64, SharedByte)>, RedisType> {
        self.zsetop_with(op, keys, weights, agg, |r| r.iter().map(|&(s, m)| (s, SharedByte::from_slice(m))).collect())
    }

    /// ZUNIONSTORE / ZINTERSTORE / ZDIFFSTORE : `dest` remplacée (quel que soit son type)
    /// par le résultat, construit d'un coup (trié, sans doublon : comme au rechargement)
    /// avant la suppression de `dest`, qui peut être une entrée. Rend le cardinal.
    pub fn cmd_zsetop_store(&mut self, dest: &[u8], op: ZOp, keys: &[&[u8]], weights: &[f64], agg: ZAggregate) -> Result<u32, TypeError> {
        // −0 rangé comme 0 (même ordre que le tri du résultat)
        let z = self
            .zsetop_with(op, keys, weights, agg, |r| ZSet::from_sorted(r.iter().map(|&(s, m)| (encode_score(s + 0.0), m))))
            .map_err(|_| TypeError::ValueNotSet)?
            .ok_or(TypeError::OutOfMemory)?;
        self.del(dest);
        let n = z.len() as u32;
        if n == 0 {
            z.free();
            return Ok(0);
        }
        self.ensure_container(dest, None, Tag::ZSet, || alloc_zset(z)).map_err(|_| TypeError::ValueNotSet)?;
        Ok(n)
    }

    /// ZINTERCARD : cardinal de l'intersection, arrêté à `limit` (0 : sans limite).
    pub fn cmd_zintercard(&mut self, keys: &[&[u8]], limit: u64) -> Result<u64, RedisType> {
        let ins = self.zop_inputs(keys)?;
        let mut order: Vec<usize> = (0..ins.len()).collect();
        order.sort_by_key(|&i| ins[i].len());
        let probes: Vec<Probe> = order[1..].iter().map(|&j| Probe::new(&ins[j])).collect();
        let mut n = 0u64;
        ins[order[0]].for_each_while(|m| {
            if probes.iter().all(|p| p.score(m).is_some()) {
                n += 1;
            }
            limit == 0 || n < limit
        });
        Ok(n)
    }

    /// Page de ZSCAN : jusqu'à `count` entrées examinées strictement après `from`
    /// (`(score encodé, membre)`), dans l'ordre du ZSet ; et la position où reprendre
    /// (`None` : fini). Un petit ZSet (ou qui tient dans `count`) est rendu en entier.
    pub fn zscan_page(&mut self, key: &[u8], from: Option<(u64, &[u8])>, count: usize) -> Result<(Vec<(SharedByte, f64)>, Option<(u64, Vec<u8>)>), RedisType> {
        let Some(v) = self.get_ref(key) else { return Ok((Vec::new(), None)) };
        let zset = v.as_zset()?;
        let all = |z: &ZSet| z.iter().map(|(s, m)| (SharedByte::from_slice(m), decode_score(s))).collect::<Vec<_>>();
        if from.is_none() && (matches!(zset, ZSet::Small(_)) || zset.len() <= count) {
            return Ok((all(zset), None));
        }
        let start = match from {
            None => 0,
            Some((fs, fm)) => zset.lower_bound(|s, m| if (s, m) <= (fs, fm) { Ordering::Greater } else { Ordering::Less }),
        };
        let mut out = Vec::with_capacity(count.min(zset.len().saturating_sub(start)));
        let mut last = None;
        for (s, m) in zset.iter_from(start).take(count) {
            out.push((SharedByte::from_slice(m), decode_score(s)));
            last = Some((s, m));
        }
        let next = (start + out.len() < zset.len()).then(|| last.map(|(s, m)| (s, m.to_vec()))).flatten();
        Ok((out, next))
    }
}
