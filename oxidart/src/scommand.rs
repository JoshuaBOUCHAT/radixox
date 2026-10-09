use radixox_lib::shared_byte::SharedByte;

use crate::{
    OxidArt,
    error::TypeError,
    value::{RedisType, Tag, drop_raw, valart},
};

pub enum SPOPResult {
    Single(Option<SharedByte>),
    Multiple(Vec<SharedByte>),
}
impl OxidArt {
    /// Racine valart du Set à `key`, créé vide s'il n'existe pas (ou a expiré).
    fn get_or_create_set(&mut self, ttl: Option<u64>, key: &[u8]) -> Result<u32, TypeError> {
        let now = self.now;
        let node_key = self.ensure_key(key);
        let node = self.get_node_mut(node_key);

        match node.get_value_mut(now) {
            Some(v) if *v.tag == Tag::Set => return Ok(unsafe { v.val.idx }),
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {}
        }
        // Valeur expirée encore en place : la libérer, et ne pas hériter de
        // son TTL.
        if node.tag != Tag::None {
            unsafe { drop_raw(node.tag, &mut node.val) };
            node.exp_and_radix.set_no_expiracy();
        }
        let root = valart().set_create();
        node.tag = Tag::Set;
        node.val.idx = root;
        if let Some(ttl) = ttl {
            node.exp_and_radix.set_exp(ttl);
        }
        Ok(root)
    }

    /// Racine valart du Set à `key`, `None` si la clé n'existe pas.
    fn set_root(&mut self, key: &[u8]) -> Result<Option<u32>, RedisType> {
        match self.get_mut(key) {
            Some(val) => val.set_root().map(Some),
            None => Ok(None),
        }
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

        let Some(root) = self.set_root(key).map_err(|_| TypeError::ValueNotSet)? else {
            return Ok(if count == 1 {
                SPOPResult::Single(None)
            } else {
                SPOPResult::Multiple(Vec::new())
            });
        };
        let trees = valart();
        let popped: Vec<SharedByte> = (0..count)
            .map_while(|_| trees.set_pop(root, rand::random()))
            .collect();
        if trees.set_card(root) == 0 {
            let _ = self.del(key);
        }

        if count == 1 {
            return Ok(SPOPResult::Single(popped.into_iter().next()));
        }
        Ok(SPOPResult::Multiple(popped))
    }
    pub fn cmd_sadd(
        &mut self,
        key: &[u8],
        members: &[SharedByte],
        ttl: Option<u64>,
    ) -> Result<u32, TypeError> {
        debug_assert!(!members.is_empty());

        let root = self.get_or_create_set(ttl, key)?;
        let trees = valart();
        Ok(members
            .iter()
            .filter(|member| trees.set_add(root, (*member).clone()))
            .count() as u32)
    }
    pub fn cmd_srem(&mut self, key: &[u8], members: &[SharedByte]) -> Result<u32, RedisType> {
        debug_assert!(!members.is_empty());

        let Some(root) = self.set_root(key)? else {
            return Ok(0);
        };
        let trees = valart();
        let count = members
            .iter()
            .filter(|member| trees.set_remove(root, member))
            .count() as u32;
        if trees.set_card(root) == 0 {
            let _ = self.del(key);
        }
        Ok(count)
    }
    pub fn cmd_smembers(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(root) = self.set_root(key)? else {
            return Ok(Vec::new());
        };
        let trees = valart();
        let mut res = Vec::with_capacity(trees.set_card(root) as usize);
        trees.set_for_each(root, |m| res.push(m.clone()));
        Ok(res)
    }
    pub fn cmd_sismember(&mut self, key: &[u8], member: SharedByte) -> Result<bool, RedisType> {
        let Some(root) = self.set_root(key)? else {
            return Ok(false);
        };
        Ok(valart().set_contains(root, &member))
    }
    pub fn cmd_scard(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        let Some(root) = self.set_root(key)? else {
            return Ok(0);
        };
        Ok(valart().set_card(root))
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
