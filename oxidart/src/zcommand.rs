use crate::zset_inner::ZSetInner;

use ordered_float::OrderedFloat;
use radixox_lib::shared_byte::SharedByte;
use std::collections::{BTreeSet, HashMap};

use crate::{
    OxidArt, Value,
    error::TypeError,
    value::{RedisType, Tag, value_into_raw},
};

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

#[derive(Clone, Debug, PartialEq)]
pub enum InnerZCommand {
    /// Small ZSet: sorted Vec<(score, member)> for sets below the threshold.
    /// Vec = 24 bytes — cache-friendly, no heap indirection.
    Small(Vec<(OrderedFloat<f64>, SharedByte)>),
    /// Large ZSet: boxed double-indexed structure for O(1) score lookup + O(log n) range.
    /// Box = 8 bytes — keeps InnerZCommand ≤32 bytes total.
    Large(Box<ZSetInner>),
}

impl InnerZCommand {
    pub fn new() -> Self {
        InnerZCommand::Small(Vec::new())
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
                    vec.remove(pos);
                    // Re-insert at correct sorted position.
                    let new_pos =
                        vec.partition_point(|(s, m)| (*s, m.as_ref()) < (score, member.as_ref()));
                    vec.insert(new_pos, (score, member));
                    return false; // existing member, score updated
                }
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
                    *self = InnerZCommand::Large(Box::new(ZSetInner { sorted, scores }));
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

// ---------------------------------------------------------------------------
// OxidArt — ZSet commands
// ---------------------------------------------------------------------------

impl OxidArt {
    /// Get or create a zset at the given key, ensuring type correctness.
    fn get_zset_mut<'a>(
        &'a mut self,
        ttl: Option<u64>,
        key: SharedByte,
    ) -> Result<&'a mut InnerZCommand, TypeError> {
        let now = self.now;
        let node_key = self.ensure_key(&key);
        let node: &mut crate::Node = self.get_node_mut(node_key);

        let need_tag = match node.get_value_mut(now) {
            Some(ref v) if *v.tag == Tag::ZSet => false,
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                let (tag, val) = value_into_raw(Value::ZSet(InnerZCommand::default()));
                node.tag = tag;
                node.val = val;
                if let Some(ttl) = ttl {
                    node.exp_and_radix.set_exp(ttl);
                    true
                } else {
                    false
                }
            }
        };
        if need_tag {
            self.map.tag(node_key);
        }

        self.get_node_mut(node_key)
            .get_value_mut(now)
            .unwrap()
            .as_zset_mut()
            .map_err(|_| TypeError::ValueNotSet)
    }

    /// ZADD - add one or more members with scores to a sorted set.
    /// Returns the number of new elements added (not including updates).
    pub fn cmd_zadd(
        &mut self,
        key: SharedByte,
        score_members: &[(f64, SharedByte)],
        ttl: Option<u64>,
    ) -> Result<u32, TypeError> {
        debug_assert!(!score_members.is_empty());

        let zset = self.get_zset_mut(ttl, key)?;
        let mut added = 0;

        for (score, member) in score_members {
            if zset.insert(*score, member.clone()) {
                added += 1;
            }
        }

        Ok(added)
    }

    /// ZCARD - get the number of members in a sorted set.
    pub fn cmd_zcard(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        let Some(val) = self.get_mut(key) else {
            return Ok(0);
        };
        Ok(val.as_zset()?.len() as u32)
    }

    /// ZRANGE - return a range of members in a sorted set, by index.
    /// Indices are 0-based. Negative indices count from the end.
    pub fn cmd_zrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        with_scores: bool,
    ) -> Result<Vec<SharedByte>, RedisType> {
        let Some(val) = self.get_mut(key) else {
            return Ok(Vec::new());
        };
        let zset = val.as_zset()?;

        let len = zset.len() as i64;
        if len == 0 {
            return Ok(Vec::new());
        }

        // Normalize negative indices
        let start = if start < 0 {
            (len + start).max(0) as usize
        } else {
            start.min(len) as usize
        };
        let stop = if stop < 0 {
            (len + stop).max(0) as usize
        } else {
            stop.min(len - 1) as usize
        };

        if start > stop {
            return Ok(Vec::new());
        }

        let mut result = Vec::new();
        for (score, member) in zset.iter().skip(start).take(stop - start + 1) {
            result.push(member.clone());
            if with_scores {
                result.push(SharedByte::from_slice(score.into_inner().to_string()));
            }
        }
        Ok(result)
    }

    /// ZSCORE - get the score of a member in a sorted set.
    pub fn cmd_zscore(&mut self, key: &[u8], member: SharedByte) -> Result<Option<f64>, RedisType> {
        let Some(val) = self.get_mut(key) else {
            return Ok(None);
        };
        Ok(val.as_zset()?.score(member))
    }

    /// ZREM - remove one or more members from a sorted set.
    /// Returns the number of members removed.
    pub fn cmd_zrem(&mut self, key: &[u8], members: &[SharedByte]) -> Result<u32, RedisType> {
        debug_assert!(!members.is_empty());

        let (removed, need_cleanup) = {
            let Some(mut val) = self.get_mut(key) else {
                return Ok(0);
            };
            let zset = val.as_zset_mut()?;
            let mut removed = 0;

            for member in members {
                if zset.remove(member.clone()) {
                    removed += 1;
                }
            }
            (removed, zset.is_empty())
        };

        if need_cleanup {
            let _ = self.del(key);
        }

        Ok(removed)
    }

    /// ZINCRBY - increment the score of a member in a sorted set.
    /// If the member doesn't exist, it's created with score = increment.
    /// Returns the new score.
    pub fn cmd_zincrby(
        &mut self,
        key: SharedByte,
        increment: f64,
        member: SharedByte,
    ) -> Result<f64, TypeError> {
        let zset = self.get_zset_mut(None, key)?;

        let new_score = match zset.score(member.clone()) {
            Some(current) => current + increment,
            None => increment,
        };

        zset.insert(new_score, member);
        Ok(new_score)
    }
}
