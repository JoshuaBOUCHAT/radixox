use crate::value::Value::ZSet;
use crate::zset_inner::ZSetInner;

use bytes::Bytes;
use ordered_float::OrderedFloat;
use std::collections::{BTreeSet, HashMap};

use crate::{error::TypeError, value::RedisType, OxidArt, NO_EXPIRY};

const THRESHOLD: usize = 16;

// ---------------------------------------------------------------------------
// InnerZCommand — dynamic Small/Large representation
// ---------------------------------------------------------------------------

/// ZSet iterator — zero-overhead enum dispatch over sorted elements.
pub struct ZIter<'a> {
    inner: ZIterInner<'a>,
}

enum ZIterInner<'a> {
    Small(std::slice::Iter<'a, (OrderedFloat<f64>, Bytes)>),
    Large(std::collections::btree_set::Iter<'a, (OrderedFloat<f64>, Bytes)>),
}

impl<'a> Iterator for ZIter<'a> {
    type Item = &'a (OrderedFloat<f64>, Bytes);

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
    Small(Vec<(OrderedFloat<f64>, Bytes)>),
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
    pub(crate) fn insert(&mut self, score: f64, member: Bytes) -> bool {
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
                    let new_pos = vec
                        .partition_point(|(s, m)| (*s, m.as_ref()) < (score, member.as_ref()));
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
                    let pos = vec
                        .partition_point(|(s, m)| (*s, m.as_ref()) < (score, member.as_ref()));
                    vec.insert(pos, (score, member));
                }
                true
            }
            InnerZCommand::Large(zset) => zset.insert(score.into_inner(), member),
        }
    }

    /// Remove a member. Returns true if the member existed.
    pub(crate) fn remove(&mut self, member: &Bytes) -> bool {
        match self {
            InnerZCommand::Small(vec) => {
                if let Some(pos) = vec.iter().position(|(_, m)| m == member) {
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
    pub(crate) fn score(&self, member: &[u8]) -> Option<f64> {
        match self {
            InnerZCommand::Small(vec) => vec
                .iter()
                .find(|(_, m)| m.as_ref() == member)
                .map(|(s, _)| s.into_inner()),
            InnerZCommand::Large(zset) => zset.score(member),
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
        key: &[u8],
    ) -> Result<&'a mut InnerZCommand, TypeError> {
        let now = self.now;
        let node_key = self.ensure_key(key);
        let node = self.get_node_mut(node_key);

        match node.get_value_mut(now) {
            Some(ZSet(_)) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                node.val = Some((ZSet(InnerZCommand::default()), ttl.unwrap_or(NO_EXPIRY)));
            }
        };

        let val = node.get_value_mut(now).unwrap();
        let ZSet(zset) = val else { unreachable!() };
        Ok(zset)
    }

    /// ZADD - add one or more members with scores to a sorted set.
    /// Returns the number of new elements added (not including updates).
    pub fn cmd_zadd(
        &mut self,
        key: &[u8],
        score_members: &[(f64, Bytes)],
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
        let Some(val) = self.get(key) else {
            return Ok(0);
        };
        let zset = val.as_zset()?;
        Ok(zset.len() as u32)
    }

    /// ZRANGE - return a range of members in a sorted set, by index.
    /// Indices are 0-based. Negative indices count from the end.
    pub fn cmd_zrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        with_scores: bool,
    ) -> Result<Vec<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
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
                result.push(Bytes::from(score.into_inner().to_string()));
            }
        }
        Ok(result)
    }

    /// ZSCORE - get the score of a member in a sorted set.
    pub fn cmd_zscore(&mut self, key: &[u8], member: &[u8]) -> Result<Option<f64>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(None);
        };
        let zset = val.as_zset()?;
        Ok(zset.score(member))
    }

    /// ZREM - remove one or more members from a sorted set.
    /// Returns the number of members removed.
    pub fn cmd_zrem(&mut self, key: &[u8], members: &[Bytes]) -> Result<u32, RedisType> {
        debug_assert!(!members.is_empty());

        let (removed, need_cleanup) = {
            let Some(val) = self.get_mut(key) else {
                return Ok(0);
            };
            let zset = val.as_zset_mut()?;
            let mut removed = 0;

            for member in members {
                if zset.remove(member) {
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
        key: &[u8],
        increment: f64,
        member: &[u8],
    ) -> Result<f64, TypeError> {
        let zset = self.get_zset_mut(None, key)?;

        let new_score = match zset.score(member) {
            Some(current) => current + increment,
            None => increment,
        };

        zset.insert(new_score, Bytes::copy_from_slice(member));
        Ok(new_score)
    }
}
