use crate::value::Value::ZSet;
use crate::zset_inner::ZSetInner;

use bytes::Bytes;

use crate::{error::TypeError, value::RedisType, OxidArt, NO_EXPIRY};

impl OxidArt {
    /// Get or create a zset at the given key, ensuring type correctness.
    fn get_zset_mut<'a>(
        &'a mut self,
        ttl: Option<u64>,
        key: &[u8],
    ) -> Result<&'a mut ZSetInner, TypeError> {
        let now = self.now;
        let node_key = self.ensure_key(key);
        let node = self.get_node_mut(node_key);

        match node.get_value_mut(now) {
            Some(ZSet(_)) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                node.val = Some((ZSet(Box::default()), ttl.unwrap_or(NO_EXPIRY)));
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

    /// ZSCORE - get the score of a member in a sorted set. O(1) via HashMap.
    pub fn cmd_zscore(&mut self, key: &[u8], member: &[u8]) -> Result<Option<f64>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(None);
        };
        let zset = val.as_zset()?;
        Ok(zset.score(member))
    }

    /// ZREM - remove one or more members from a sorted set. O(1) lookup + O(log n) remove.
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
