use crate::value::Value::Hash;
use std::collections::BTreeMap;

use bytes::Bytes;

use crate::{error::TypeError, value::RedisType, OxidArt, NO_EXPIRY};

impl OxidArt {
    /// Get or create a hash at the given key, ensuring type correctness.
    fn get_btree_map_mut<'a>(
        &'a mut self,
        ttl: Option<u64>,
        key: &[u8],
    ) -> Result<&'a mut BTreeMap<Bytes, Bytes>, TypeError> {
        let now = self.now;
        let node_key = self.ensure_key(key);
        let node = self.get_node_mut(node_key);

        match node.get_value_mut(now) {
            Some(Hash(_)) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                node.val = Some((Hash(BTreeMap::new()), ttl.unwrap_or(NO_EXPIRY)));
            }
        };

        let val = node.get_value_mut(now).unwrap();
        let Hash(map) = val else { unreachable!() };
        Ok(map)
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

        let map = self.get_btree_map_mut(ttl, key)?;
        let mut added = 0;

        for (field, value) in field_values {
            if map.insert(field.clone(), value.clone()).is_none() {
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
        let map = val.as_hash()?;
        Ok(map.get(field).cloned())
    }

    /// HGETALL - get all field-value pairs in a hash.
    /// Returns a flat vector: [field1, value1, field2, value2, ...]
    pub fn cmd_hgetall(&mut self, key: &[u8]) -> Result<Vec<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(Vec::new());
        };
        let map = val.as_hash()?;

        let mut result = Vec::with_capacity(map.len() * 2);
        for (field, value) in map.iter() {
            result.push(field.clone());
            result.push(value.clone());
        }
        Ok(result)
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
            let map = val.as_hash_mut()?;
            let mut deleted = 0;

            for field in fields {
                if map.remove(field).is_some() {
                    deleted += 1;
                }
            }
            (deleted, map.is_empty())
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
        let map = val.as_hash()?;
        Ok(map.contains_key(field))
    }

    /// HLEN - get the number of fields in a hash.
    pub fn cmd_hlen(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(0);
        };
        let map = val.as_hash()?;
        Ok(map.len() as u32)
    }

    /// HKEYS - get all field names in a hash.
    pub fn cmd_hkeys(&mut self, key: &[u8]) -> Result<Vec<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(Vec::new());
        };
        let map = val.as_hash()?;
        Ok(map.keys().cloned().collect())
    }

    /// HVALS - get all values in a hash.
    pub fn cmd_hvals(&mut self, key: &[u8]) -> Result<Vec<Bytes>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(Vec::new());
        };
        let map = val.as_hash()?;
        Ok(map.values().cloned().collect())
    }

    /// HMGET - get the values of multiple hash fields.
    /// Returns a vector with the same length as fields, with None for missing fields.
    pub fn cmd_hmget(&mut self, key: &[u8], fields: &[Bytes]) -> Result<Vec<Option<Bytes>>, RedisType> {
        let Some(val) = self.get(key) else {
            return Ok(vec![None; fields.len()]);
        };
        let map = val.as_hash()?;
        Ok(fields.iter().map(|f| map.get(f).cloned()).collect())
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
        let map = self.get_btree_map_mut(None, key)?;

        let current = match map.get(field) {
            Some(bytes) => {
                let s = std::str::from_utf8(bytes).map_err(|_| TypeError::NotAInt)?;
                s.parse::<i64>().map_err(|_| TypeError::NotAInt)?
            }
            None => 0,
        };

        let new_val = current.checked_add(increment).ok_or(TypeError::NotAInt)?;
        map.insert(Bytes::copy_from_slice(field), Bytes::from(new_val.to_string()));
        Ok(new_val)
    }
}
