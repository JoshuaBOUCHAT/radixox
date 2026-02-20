use crate::value::Value::Hash;
use std::collections::BTreeMap;

use bytes::Bytes;

use crate::{NO_EXPIRY, OxidArt, error::TypeError, value::RedisType};

const THRESHOLD: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub enum InnerHCommand {
    Small(Vec<(Bytes, Bytes)>),
    Large(BTreeMap<Bytes, Bytes>),
}

impl InnerHCommand {
    pub(crate) fn new() -> Self {
        InnerHCommand::Small(Vec::new())
    }

    /// Insert or update a field. Returns true if newly inserted, false if updated.
    pub(crate) fn insert(&mut self, field: Bytes, value: Bytes) -> bool {
        match self {
            InnerHCommand::Small(vec) => {
                for (k, v) in vec.iter_mut() {
                    if k == &field {
                        *v = value;
                        return false;
                    }
                }
                if vec.len() >= THRESHOLD {
                    // Promote: build BTreeMap from existing entries + new one in one pass.
                    let mut map = BTreeMap::new();
                    for (k, v) in vec.drain(..) {
                        map.insert(k, v);
                    }
                    map.insert(field, value);
                    *self = InnerHCommand::Large(map);
                } else {
                    vec.push((field, value));
                }
                true
            }
            InnerHCommand::Large(map) => map.insert(field, value).is_none(),
        }
    }

    /// Remove and return the value of an arbitrary field (last for Small, first for Large).
    #[allow(dead_code)]
    pub(crate) fn pop(&mut self) -> Option<Bytes> {
        match self {
            InnerHCommand::Small(vec) => vec.pop().map(|(_, v)| v),
            InnerHCommand::Large(map) => {
                let key = map.keys().next()?.clone();
                map.remove(&key)
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            InnerHCommand::Small(v) => v.len(),
            InnerHCommand::Large(m) => m.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn get(&self, field: &[u8]) -> Option<&Bytes> {
        match self {
            InnerHCommand::Small(v) => v.iter().find(|(k, _)| k.as_ref() == field).map(|(_, v)| v),
            InnerHCommand::Large(m) => m.get(field),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn get_mut(&mut self, field: &[u8]) -> Option<&mut Bytes> {
        match self {
            InnerHCommand::Small(v) => v
                .iter_mut()
                .find(|(k, _)| k.as_ref() == field)
                .map(|(_, v)| v),
            InnerHCommand::Large(m) => m.get_mut(field),
        }
    }

    /// Remove a field and return its value.
    pub(crate) fn del(&mut self, field: &[u8]) -> Option<Bytes> {
        match self {
            InnerHCommand::Small(v) => {
                let pos = v.iter().position(|(k, _)| k.as_ref() == field)?;
                Some(v.swap_remove(pos).1)
            }
            InnerHCommand::Large(m) => m.remove(field),
        }
    }

    pub(crate) fn contains_key(&self, field: &[u8]) -> bool {
        self.get(field).is_some()
    }

    /// All field-value pairs as a flat vec [field1, val1, field2, val2, ...].
    pub(crate) fn all(&self) -> Vec<Bytes> {
        match self {
            InnerHCommand::Small(v) => {
                let mut result = Vec::with_capacity(v.len() * 2);
                for (k, val) in v {
                    result.push(k.clone());
                    result.push(val.clone());
                }
                result
            }
            InnerHCommand::Large(m) => {
                let mut result = Vec::with_capacity(m.len() * 2);
                for (k, val) in m {
                    result.push(k.clone());
                    result.push(val.clone());
                }
                result
            }
        }
    }

    pub(crate) fn keys(&self) -> Vec<Bytes> {
        match self {
            InnerHCommand::Small(v) => v.iter().map(|(k, _)| k.clone()).collect(),
            InnerHCommand::Large(m) => m.keys().cloned().collect(),
        }
    }

    pub(crate) fn values(&self) -> Vec<Bytes> {
        match self {
            InnerHCommand::Small(v) => v.iter().map(|(_, val)| val.clone()).collect(),
            InnerHCommand::Large(m) => m.values().cloned().collect(),
        }
    }
}

impl OxidArt {
    /// Get or create a hash at the given key, ensuring type correctness.
    fn get_hash_mut<'a>(
        &'a mut self,
        ttl: Option<u64>,
        key: &[u8],
    ) -> Result<&'a mut InnerHCommand, TypeError> {
        let now = self.now;
        let node_key = self.ensure_key(key);
        let node = self.get_node_mut(node_key);

        match node.get_value_mut(now) {
            Some(Hash(_)) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                node.val = Some((Hash(InnerHCommand::new()), ttl.unwrap_or(NO_EXPIRY)));
            }
        };

        let val = node.get_value_mut(now).unwrap();
        let Hash(inner) = val else { unreachable!() };
        Ok(inner)
    }

    /// HSET - set one or more field-value pairs in a hash.
    /// Returns the number of fields that were added (not updated).
    pub fn cmd_hset(
        &mut self,
        key: &[u8],
        field_values: &[(Bytes, Bytes)],
        ttl: Option<u64>,
    ) -> Result<u32, TypeError> {
        debug_assert!(!field_values.is_empty());

        let inner = self.get_hash_mut(ttl, key)?;
        let mut added = 0;

        for (field, value) in field_values {
            if inner.insert(field.clone(), value.clone()) {
                added += 1;
            }
        }

        Ok(added)
    }

    /// HGET - get the value of a hash field.
    pub fn cmd_hget(&mut self, key: &[u8], field: &[u8]) -> Result<Option<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(None);
        };
        let inner = val.as_hash()?;
        Ok(inner.get(field).cloned())
    }

    /// HGETALL - get all field-value pairs in a hash.
    /// Returns a flat vector: [field1, value1, field2, value2, ...]
    pub fn cmd_hgetall(&mut self, key: &[u8]) -> Result<Vec<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(Vec::new());
        };
        let inner = val.as_hash()?;
        Ok(inner.all())
    }

    /// HDEL - delete one or more hash fields.
    /// Returns the number of fields that were removed.
    /// Auto-deletes the key if the hash becomes empty.
    pub fn cmd_hdel(&mut self, key: &[u8], fields: &[Bytes]) -> Result<u32, RedisType> {
        debug_assert!(!fields.is_empty());

        let (deleted, need_cleanup) = {
            let Some(val) = self.get_mut(key) else {
                return Ok(0);
            };
            let inner = val.as_hash_mut()?;
            let mut deleted = 0;

            for field in fields {
                if inner.del(field).is_some() {
                    deleted += 1;
                }
            }
            (deleted, inner.is_empty())
        };

        if need_cleanup {
            let _ = self.del(key);
        }

        Ok(deleted)
    }

    /// HEXISTS - check if a field exists in a hash.
    pub fn cmd_hexists(&mut self, key: &[u8], field: &[u8]) -> Result<bool, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(false);
        };
        let inner = val.as_hash()?;
        Ok(inner.contains_key(field))
    }

    /// HLEN - get the number of fields in a hash.
    pub fn cmd_hlen(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(0);
        };
        let inner = val.as_hash()?;
        Ok(inner.len() as u32)
    }

    /// HKEYS - get all field names in a hash.
    pub fn cmd_hkeys(&mut self, key: &[u8]) -> Result<Vec<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(Vec::new());
        };
        let inner = val.as_hash()?;
        Ok(inner.keys())
    }

    /// HVALS - get all values in a hash.
    pub fn cmd_hvals(&mut self, key: &[u8]) -> Result<Vec<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(Vec::new());
        };
        let inner = val.as_hash()?;
        Ok(inner.values())
    }

    /// HMGET - get the values of multiple hash fields.
    /// Returns a vector with the same length as fields, with None for missing fields.
    pub fn cmd_hmget(
        &mut self,
        key: &[u8],
        fields: &[Bytes],
    ) -> Result<Vec<Option<Bytes>>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(vec![None; fields.len()]);
        };
        let inner = val.as_hash()?;
        Ok(fields.iter().map(|f| inner.get(f).cloned()).collect())
    }

    /// HINCRBY - increment a hash field by an integer value.
    /// If the field doesn't exist, it's set to 0 before the operation.
    /// Returns the new value after increment.
    pub fn cmd_hincrby(
        &mut self,
        key: &[u8],
        field: &[u8],
        increment: i64,
    ) -> Result<i64, TypeError> {
        let inner = self.get_hash_mut(None, key)?;

        let current = match inner.get(field) {
            Some(bytes) => {
                let s = std::str::from_utf8(bytes).map_err(|_| TypeError::NotAInt)?;
                s.parse::<i64>().map_err(|_| TypeError::NotAInt)?
            }
            None => 0,
        };

        let new_val = current.checked_add(increment).ok_or(TypeError::NotAInt)?;
        inner.insert(
            Bytes::copy_from_slice(field),
            Bytes::from(new_val.to_string()),
        );
        Ok(new_val)
    }
}
