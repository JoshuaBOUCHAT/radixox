use radixox_lib::shared_byte::SharedByte;

use crate::{
    OxidArt,
    error::TypeError,
    value::{RedisType, Tag, ValUnion, valart},
};

impl OxidArt {
    /// Racine valart du Hash à `key`, créé vide s'il n'existe pas (ou a expiré).
    fn get_or_create_hash(&mut self, ttl: Option<u64>, key: &[u8]) -> Result<u32, TypeError> {
        let node_key = self.ensure_key(key);
        // Lecture d'abord : un Hash existant ne modifie pas le nœud oxidart.
        if let Some(v) = self.get_node(node_key).get_value_ref(self.now) {
            return v.hash_root().map_err(|_| TypeError::ValueNotSet);
        }
        let root = valart().hash_create();
        self.install_value_raw(node_key, Tag::Hash, ValUnion { idx: root }, ttl);
        Ok(root)
    }

    /// Racine valart du Hash à `key`, `None` si la clé n'existe pas.
    fn hash_root(&mut self, key: &[u8]) -> Result<Option<u32>, RedisType> {
        self.get_ref(key).map(|v| v.hash_root()).transpose()
    }

    /// HSET - set one or more field-value pairs in a hash.
    /// Returns the number of fields that were added (not updated).
    pub fn cmd_hset(
        &mut self,
        key: &[u8],
        field_values: &[(SharedByte, SharedByte)],
        ttl: Option<u64>,
    ) -> Result<u32, TypeError> {
        debug_assert!(!field_values.is_empty());

        let root = self.get_or_create_hash(ttl, key)?;
        let trees = valart();
        let mut added = 0;
        for (field, value) in field_values {
            if trees.hash_set(root, field.clone(), value.clone()) {
                added += 1;
            }
        }
        Ok(added)
    }

    /// HGET - get the value of a hash field.
    pub fn cmd_hget(&mut self, key: &[u8], field: &[u8]) -> Result<Option<SharedByte>, RedisType> {
        let Some(root) = self.hash_root(key)? else {
            return Ok(None);
        };
        Ok(valart().hash_get(root, field).cloned())
    }

    /// HGETALL - get all field-value pairs in a hash.
    /// Returns a flat vector: [field1, value1, field2, value2, ...]
    pub fn cmd_hgetall(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(root) = self.hash_root(key)? else {
            return Ok(Vec::new());
        };
        let trees = valart();
        let mut result = Vec::with_capacity(2 * trees.hash_count(root) as usize);
        trees.hash_for_each(root, |k, v| {
            result.push(k.clone());
            result.push(v.clone());
        });
        Ok(result)
    }

    /// HDEL - delete one or more hash fields.
    /// Returns the number of fields that were removed.
    /// Auto-deletes the key if the hash becomes empty.
    pub fn cmd_hdel(&mut self, key: &[u8], fields: &[SharedByte]) -> Result<u32, RedisType> {
        debug_assert!(!fields.is_empty());

        let Some(root) = self.hash_root(key)? else {
            return Ok(0);
        };
        let trees = valart();
        let deleted = fields
            .iter()
            .filter(|field| trees.hash_remove(root, field))
            .count() as u32;
        if trees.hash_count(root) == 0 {
            self.del(key);
        }
        Ok(deleted)
    }

    /// HEXISTS - check if a field exists in a hash.
    pub fn cmd_hexists(&mut self, key: &[u8], field: &[u8]) -> Result<bool, RedisType> {
        let Some(root) = self.hash_root(key)? else {
            return Ok(false);
        };
        Ok(valart().hash_contains(root, field))
    }

    /// HLEN - get the number of fields in a hash.
    pub fn cmd_hlen(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        let Some(root) = self.hash_root(key)? else {
            return Ok(0);
        };
        Ok(valart().hash_count(root))
    }

    /// HKEYS - get all field names in a hash.
    pub fn cmd_hkeys(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(root) = self.hash_root(key)? else {
            return Ok(Vec::new());
        };
        let trees = valart();
        let mut result = Vec::with_capacity(trees.hash_count(root) as usize);
        trees.hash_for_each(root, |k, _| result.push(k.clone()));
        Ok(result)
    }

    /// HVALS - get all values in a hash.
    pub fn cmd_hvals(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(root) = self.hash_root(key)? else {
            return Ok(Vec::new());
        };
        let trees = valart();
        let mut result = Vec::with_capacity(trees.hash_count(root) as usize);
        trees.hash_for_each(root, |_, v| result.push(v.clone()));
        Ok(result)
    }

    /// HMGET - get the values of multiple hash fields.
    /// Returns a vector with the same length as fields, with None for missing fields.
    pub fn cmd_hmget(
        &mut self,
        key: &[u8],
        fields: &[SharedByte],
    ) -> Result<Vec<Option<SharedByte>>, RedisType> {
        let Some(root) = self.hash_root(key)? else {
            return Ok(vec![None; fields.len()]);
        };
        let trees = valart();
        Ok(fields
            .iter()
            .map(|f| trees.hash_get(root, f).cloned())
            .collect())
    }

    /// HINCRBY - increment a hash field by an integer value.
    /// If the field doesn't exist, it's set to 0 before the operation.
    /// Returns the new value after increment.
    pub fn cmd_hincrby(
        &mut self,
        key: &[u8],
        field: SharedByte,
        increment: i64,
    ) -> Result<i64, TypeError> {
        let root = self.get_or_create_hash(None, key)?;
        let trees = valart();

        let current = match trees.hash_get(root, &field) {
            Some(bytes) => {
                let s = std::str::from_utf8(bytes).map_err(|_| TypeError::NotAInt)?;
                s.parse::<i64>().map_err(|_| TypeError::NotAInt)?
            }
            None => 0,
        };

        let new_val = current.checked_add(increment).ok_or(TypeError::NotAInt)?;
        trees.hash_set(root, field, SharedByte::from_slice(new_val.to_string()));
        Ok(new_val)
    }
}
