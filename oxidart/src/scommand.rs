use crate::value::Value::Set;
use std::collections::BTreeSet;

use bytes::Bytes;

use crate::{NO_EXPIRY, OxidArt, error::TypeError, value::RedisType};

pub enum SPOPResult {
    Single(Option<Bytes>),
    Multiple(Vec<Bytes>),
}
impl OxidArt {
    fn get_btree_set_mut<'a>(
        &'a mut self,
        ttl: Option<u64>,
        key: &[u8],
    ) -> Result<&'a mut BTreeSet<Bytes>, TypeError> {
        let now = self.now;
        let node_key = self.ensure_key(key);
        let node = self.get_node_mut(node_key);

        match node.get_value_mut(now) {
            Some(Set(_)) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                node.val = Some((Set(BTreeSet::new()), ttl.unwrap_or(NO_EXPIRY)));
            }
        };

        let val = node.get_value_mut(now).unwrap();
        let Set(set) = val else { unreachable!() };
        Ok(set)
    }

    /// SPOP - remove and return one or more random members from a set.
    ///
    /// `count` parameter:
    /// - `None` => pop 1 element (returns Single)
    /// - `Some(bytes)` => parse as u32, pop that many elements (returns Multiple if count > 1)
    ///
    /// Returns error if count is not a valid positive u32.
    pub fn cmd_spop(&mut self, key: &[u8], count: Option<&[u8]>) -> Result<SPOPResult, TypeError> {
        let count = match count {
            Some(bytes) => match parse_u32(bytes) {
                Some(n) if n > 0 => n,
                _ => return Err(TypeError::NotAInt),
            },
            None => 1,
        };

        let set = self.get_btree_set_mut(None, key)?;

        if count == 1 {
            return Ok(SPOPResult::Single(set.pop_last()));
        }

        let mut res = Vec::with_capacity(count.min(set.len() as u32) as usize);
        for _ in 0..count {
            if let Some(val) = set.pop_last() {
                res.push(val);
            } else {
                break;
            }
        }

        Ok(SPOPResult::Multiple(res))
    }
    pub fn cmd_sadd(
        &mut self,
        key: &[u8],
        members: &[Bytes],
        ttl: Option<u64>,
    ) -> Result<u32, TypeError> {
        debug_assert!(!members.is_empty());

        let set = self.get_btree_set_mut(ttl, key)?;
        let mut count = 0;

        for member in members {
            if set.insert(member.clone()) {
                count += 1;
            }
        }

        Ok(count)
    }
    pub fn cmd_srem(&mut self, key: &[u8], members: &[Bytes]) -> Result<u32, RedisType> {
        debug_assert!(!members.is_empty());

        let (count, need_clean_up) = {
            let Some(val) = self.get_mut(key) else {
                return Ok(0);
            };
            let set = val.as_set_mut()?;
            let mut count = 0;

            for member in members {
                if set.remove(member) {
                    count += 1;
                }
            }
            (count, set.is_empty())
        };
        if need_clean_up {
            let _ = self.del(key);
        }

        Ok(count)
    }
    pub fn cmd_smembers(&mut self, key: &[u8]) -> Result<Vec<Bytes>, RedisType> {
        let res: Vec<Bytes> = {
            let Some(val) = self.get(key) else {
                return Ok(Vec::new());
            };
            let set = val.as_set()?;

            set.iter().cloned().collect()
        };
        if res.is_empty() {
            let _ = self.del(key);
        }
        Ok(res)
    }
    pub fn cmd_sismember(&mut self, key: &[u8], member: &[u8]) -> Result<bool, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(false);
        };
        let set = val.as_set()?;
        Ok(set.contains(member))
    }
    pub fn cmd_scard(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        let len = {
            let Some(val) = self.get(key) else {
                return Ok(0);
            };

            let set = val.as_set()?;
            set.len()
        };

        if len == 0 {
            let _ = self.del(key);
        }
        Ok(len as u32)
    }
}

/// Parse u32 from byte slice (ASCII digits only).
fn parse_u32(data: &[u8]) -> Option<u32> {
    if data.is_empty() {
        return None;
    }
    let mut res = 0u64;
    for &b in data {
        match b {
            n @ b'0'..=b'9' => {
                res = res.checked_mul(10)?;
                res = res.checked_add((n - b'0') as u64)?;
                if res > u32::MAX as u64 {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(res as u32)
}
