use radixox_lib::shared_byte::SharedByte;

use crate::{
    OxidArt,
    error::TypeError,
    small,
    value::{RawVal, RedisType, Tag, ValUnion, hash_into_raw, valart},
};

/// Hash vivant, en lecture : petit bloc ou arbre valart.
enum HashView<'a> {
    Small(&'a [small::Pair]),
    /// Racine, cardinal.
    Tree(u32, u32),
}

impl<'a> HashView<'a> {
    fn get(&self, field: &[u8]) -> Option<&'a SharedByte> {
        match *self {
            HashView::Small(e) => e.iter().find(|[f, _]| f.as_slice() == field).map(|[_, v]| v),
            HashView::Tree(root, _) => valart().hash_get(root, field),
        }
    }

    fn len(&self) -> u32 {
        match *self {
            HashView::Small(e) => e.len() as u32,
            HashView::Tree(_, card) => card,
        }
    }

    fn for_each(&self, mut f: impl FnMut(&SharedByte, &SharedByte)) {
        match *self {
            HashView::Small(e) => e.iter().for_each(|[k, v]| f(k, v)),
            HashView::Tree(root, _) => valart().hash_for_each(root, f),
        }
    }
}

/// Hash vivant, en écriture (obtenu par `value_mut` : le nœud est passé par
/// la barrière de save). Le petit bloc passe en valart au-delà de
/// `SMALL_MAX` champs.
struct HashMut<'a> {
    tag: &'a mut Tag,
    val: &'a mut ValUnion,
    /// Nœud oxidart qui tient la valeur : indice de la racine valart (compactage).
    node: u32,
}

impl HashMut<'_> {
    /// `true` si le champ est nouveau.
    fn set(&mut self, field: SharedByte, value: SharedByte) -> bool {
        unsafe {
            match *self.tag {
                Tag::Hash => self.tree_set(field, value),
                Tag::HashSmall => {
                    if let Some(i) = small::find::<2>(self.val.ptr, &field) {
                        // Écrasement sur place (MEMORY_ARCH.md §16.5) : l'image de save du
                        // bloc clone ses entrées (rc == 2), l'écrasement se refuse seul.
                        let cur = &mut small::entries_mut::<2>(self.val.ptr)[i][1];
                        if !cur.overwrite_from(&value) {
                            *cur = value;
                        }
                        return false;
                    }
                    if let Err([field, value]) = small::push::<2>(&mut self.val.ptr, [field, value]) {
                        self.to_tree();
                        return self.tree_set(field, value);
                    }
                    true
                }
                _ => unreachable!("HashMut sur un autre type"),
            }
        }
    }

    fn remove(&mut self, field: &[u8]) -> bool {
        unsafe {
            match *self.tag {
                Tag::Hash => {
                    let removed = valart().hash_remove(self.val.idx, field);
                    self.val.cont.card -= removed as u32;
                    removed
                }
                Tag::HashSmall => match small::find::<2>(self.val.ptr, field) {
                    Some(i) => {
                        drop(small::remove_at::<2>(self.val.ptr, i));
                        true
                    }
                    None => false,
                },
                _ => unreachable!("HashMut sur un autre type"),
            }
        }
    }

    fn len(&self) -> u32 {
        unsafe {
            match *self.tag {
                Tag::Hash => self.val.cont.card,
                _ => small::len(self.val.ptr) as u32,
            }
        }
    }

    fn get(&self, field: &[u8]) -> Option<&SharedByte> {
        unsafe {
            match *self.tag {
                Tag::Hash => valart().hash_get(self.val.idx, field),
                _ => small::entries::<2>(self.val.ptr)
                    .iter()
                    .find(|[f, _]| f.as_slice() == field)
                    .map(|[_, v]| v),
            }
        }
    }

    /// Champ posé dans l'arbre : cardinal et indice de la racine à jour.
    fn tree_set(&mut self, field: SharedByte, value: SharedByte) -> bool {
        unsafe {
            let added = valart().hash_set(self.val.idx, field, value);
            self.val.cont.card += added as u32;
            valart().set_hash_root_hint(self.val.idx, self.node);
            added
        }
    }

    /// Petit bloc → arbre valart (sans retour).
    fn to_tree(&mut self) {
        let root = valart().hash_create();
        let mut card = 0;
        for [f, v] in unsafe { small::into_entries::<2>(self.val.ptr) } {
            card += valart().hash_set(root, f, v) as u32;
        }
        valart().set_hash_root_hint(root, self.node);
        *self.tag = Tag::Hash;
        self.val.cont = crate::value::Cont { idx: root, card };
    }
}

impl OxidArt {
    /// Hash vivant à `key`, `None` si la clé n'existe pas.
    fn hash_view(&mut self, key: &[u8]) -> Result<Option<HashView<'_>>, RedisType> {
        let Some(v) = self.get_ref(key) else {
            return Ok(None);
        };
        // SAFETY: le tag indique le champ actif de l'union.
        unsafe {
            match v.tag {
                Tag::Hash => Ok(Some(HashView::Tree(v.val.idx, v.val.cont.card))),
                Tag::HashSmall => Ok(Some(HashView::Small(small::entries::<2>(v.val.ptr)))),
                t => Err(t.redis_type()),
            }
        }
    }

    /// Hash vivant à `key` en écriture ; créé vide (avec `ttl`) s'il n'existe
    /// pas.
    fn hash_mut(&mut self, key: &[u8], ttl: Option<u64>) -> Result<HashMut<'_>, TypeError> {
        match self.get_ref(key).map(|v| v.tag) {
            Some(Tag::Hash | Tag::HashSmall) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                let empty = RawVal::new(Tag::HashSmall, ValUnion { ptr: small::alloc::<2>(1) });
                self.set_raw(key, empty, ttl, None);
            }
        }
        let v = self.value_mut(key).expect("Hash vivant");
        Ok(HashMut { tag: v.tag, val: v.val, node: v.node })
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
        match self.get_ref(key).map(|v| v.tag) {
            Some(Tag::Hash | Tag::HashSmall) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                // Hash neuf construit d'un coup : une seule descente d'écriture
                let (tag, val) = hash_into_raw(field_values.to_vec());
                let added = match tag {
                    Tag::HashSmall => unsafe { small::len(val.ptr) as u32 },
                    _ => unsafe { val.cont.card },
                };
                let root = (tag == Tag::Hash).then(|| unsafe { val.idx });
                self.set_raw(key, RawVal::new(tag, val), ttl, None);
                if let Some(root) = root
                    && let Some(loc) = self.live_loc(key)
                {
                    valart().set_hash_root_hint(root, loc.node);
                }
                return Ok(added);
            }
        }
        let v = self.value_mut(key).expect("Hash vivant");
        let mut h = HashMut { tag: v.tag, val: v.val, node: v.node };
        Ok(field_values
            .iter()
            .filter(|(f, v)| h.set(f.clone(), v.clone()))
            .count() as u32)
    }

    /// HGET - get the value of a hash field.
    pub fn cmd_hget(&mut self, key: &[u8], field: &[u8]) -> Result<Option<SharedByte>, RedisType> {
        Ok(self.hash_view(key)?.and_then(|h| h.get(field).cloned()))
    }

    /// HGETALL - get all field-value pairs in a hash.
    /// Returns a flat vector: [field1, value1, field2, value2, ...]
    pub fn cmd_hgetall(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(h) = self.hash_view(key)? else {
            return Ok(Vec::new());
        };
        let mut result = Vec::with_capacity(2 * h.len() as usize);
        h.for_each(|k, v| {
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
        match self.get_ref(key).map(|v| v.tag) {
            None => return Ok(0),
            Some(Tag::Hash | Tag::HashSmall) => {}
            Some(t) => return Err(t.redis_type()),
        }
        let v = self.value_mut(key).expect("Hash vivant");
        let mut h = HashMut { tag: v.tag, val: v.val, node: v.node };
        let deleted = fields.iter().filter(|f| h.remove(f)).count() as u32;
        if h.len() == 0 {
            self.del(key);
        }
        Ok(deleted)
    }

    /// HEXISTS - check if a field exists in a hash.
    pub fn cmd_hexists(&mut self, key: &[u8], field: &[u8]) -> Result<bool, RedisType> {
        Ok(self.hash_view(key)?.is_some_and(|h| h.get(field).is_some()))
    }

    /// HLEN - get the number of fields in a hash.
    pub fn cmd_hlen(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        Ok(self.hash_view(key)?.map_or(0, |h| h.len()))
    }

    /// HKEYS - get all field names in a hash.
    pub fn cmd_hkeys(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(h) = self.hash_view(key)? else {
            return Ok(Vec::new());
        };
        let mut result = Vec::with_capacity(h.len() as usize);
        h.for_each(|k, _| result.push(k.clone()));
        Ok(result)
    }

    /// HVALS - get all values in a hash.
    pub fn cmd_hvals(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(h) = self.hash_view(key)? else {
            return Ok(Vec::new());
        };
        let mut result = Vec::with_capacity(h.len() as usize);
        h.for_each(|_, v| result.push(v.clone()));
        Ok(result)
    }

    /// HMGET - get the values of multiple hash fields.
    /// Returns a vector with the same length as fields, with None for missing fields.
    pub fn cmd_hmget(
        &mut self,
        key: &[u8],
        fields: &[SharedByte],
    ) -> Result<Vec<Option<SharedByte>>, RedisType> {
        let Some(h) = self.hash_view(key)? else {
            return Ok(vec![None; fields.len()]);
        };
        Ok(fields.iter().map(|f| h.get(f).cloned()).collect())
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
        let mut h = self.hash_mut(key, None)?;
        let current = match h.get(&field) {
            Some(bytes) => {
                let s = std::str::from_utf8(bytes).map_err(|_| TypeError::NotAInt)?;
                s.parse::<i64>().map_err(|_| TypeError::NotAInt)?
            }
            None => 0,
        };
        let new_val = current.checked_add(increment).ok_or(TypeError::NotAInt)?;
        h.set(field, SharedByte::from_slice(new_val.to_string()));
        Ok(new_val)
    }
}
