use radixox_lib::shared_byte::SharedByte;

use crate::{
    OxidArt,
    error::TypeError,
    small,
    value::{RawVal, RedisType, Tag, ValUnion, set_into_raw, valart},
};

pub enum SPOPResult {
    Single(Option<SharedByte>),
    Multiple(Vec<SharedByte>),
}

/// Set vivant, en lecture : petit bloc ou arbre valart.
enum SetView<'a> {
    Small(&'a [small::Member]),
    /// Racine, cardinal.
    Tree(u32, u32),
}

impl SetView<'_> {
    fn contains(&self, member: &[u8]) -> bool {
        match *self {
            SetView::Small(e) => e.iter().any(|[m]| m.as_slice() == member),
            SetView::Tree(root, _) => valart().set_contains(root, member),
        }
    }

    fn len(&self) -> u32 {
        match *self {
            SetView::Small(e) => e.len() as u32,
            SetView::Tree(_, card) => card,
        }
    }

    fn for_each(&self, mut f: impl FnMut(&SharedByte)) {
        match *self {
            SetView::Small(e) => e.iter().for_each(|[m]| f(m)),
            SetView::Tree(root, _) => valart().set_for_each(root, f),
        }
    }
}

/// Set vivant, en écriture (obtenu par `value_mut` : le nœud est passé par
/// la barrière de save). Le petit bloc passe en valart au-delà de
/// `SMALL_MAX` membres.
struct SetMut<'a> {
    tag: &'a mut Tag,
    val: &'a mut ValUnion,
    /// Nœud oxidart qui tient la valeur : indice de la racine valart (compactage).
    node: u32,
}

impl SetMut<'_> {
    /// `true` si le membre est nouveau.
    fn add(&mut self, member: SharedByte) -> bool {
        unsafe {
            match *self.tag {
                Tag::Set => self.tree_add(member),
                Tag::SetSmall => {
                    if small::find::<1>(self.val.ptr, &member).is_some() {
                        return false;
                    }
                    if let Err([member]) = small::push::<1>(&mut self.val.ptr, [member]) {
                        self.to_tree();
                        return self.tree_add(member);
                    }
                    true
                }
                _ => unreachable!("SetMut sur un autre type"),
            }
        }
    }

    fn remove(&mut self, member: &[u8]) -> bool {
        unsafe {
            match *self.tag {
                Tag::Set => {
                    let removed = valart().set_remove(self.val.idx, member);
                    self.val.cont.card -= removed as u32;
                    removed
                }
                Tag::SetSmall => match small::find::<1>(self.val.ptr, member) {
                    Some(i) => {
                        drop(small::remove_at::<1>(self.val.ptr, i));
                        true
                    }
                    None => false,
                },
                _ => unreachable!("SetMut sur un autre type"),
            }
        }
    }

    /// Retire et rend un membre au hasard.
    fn pop(&mut self, rnd: u64) -> Option<SharedByte> {
        unsafe {
            match *self.tag {
                Tag::Set => {
                    let m = valart().set_pop(self.val.idx, rnd);
                    self.val.cont.card -= m.is_some() as u32;
                    m
                }
                Tag::SetSmall => {
                    let n = small::len(self.val.ptr);
                    (n > 0).then(|| {
                        let [m] = small::remove_at::<1>(self.val.ptr, (rnd % n as u64) as usize);
                        m
                    })
                }
                _ => unreachable!("SetMut sur un autre type"),
            }
        }
    }

    fn len(&self) -> u32 {
        unsafe {
            match *self.tag {
                Tag::Set => self.val.cont.card,
                _ => small::len(self.val.ptr) as u32,
            }
        }
    }

    /// Membre posé dans l'arbre : cardinal et indice de la racine à jour.
    fn tree_add(&mut self, member: SharedByte) -> bool {
        unsafe {
            let added = valart().set_add(self.val.idx, member);
            self.val.cont.card += added as u32;
            valart().set_set_root_hint(self.val.idx, self.node);
            added
        }
    }

    /// Petit bloc → arbre valart (sans retour).
    fn to_tree(&mut self) {
        let root = valart().set_create();
        let mut card = 0;
        for [m] in unsafe { small::into_entries::<1>(self.val.ptr) } {
            card += valart().set_add(root, m) as u32;
        }
        valart().set_set_root_hint(root, self.node);
        *self.tag = Tag::Set;
        self.val.cont = crate::value::Cont { idx: root, card };
    }
}

impl OxidArt {
    /// Set vivant à `key`, `None` si la clé n'existe pas.
    fn set_view(&mut self, key: &[u8]) -> Result<Option<SetView<'_>>, RedisType> {
        let Some(v) = self.get_ref(key) else {
            return Ok(None);
        };
        // SAFETY: le tag indique le champ actif de l'union.
        unsafe {
            match v.tag {
                Tag::Set => Ok(Some(SetView::Tree(v.val.idx, v.val.cont.card))),
                Tag::SetSmall => Ok(Some(SetView::Small(small::entries::<1>(v.val.ptr)))),
                t => Err(t.redis_type()),
            }
        }
    }

    /// Set vivant à `key` en écriture, `None` si la clé n'existe pas.
    fn set_mut(&mut self, key: &[u8]) -> Result<Option<SetMut<'_>>, RedisType> {
        match self.get_ref(key).map(|v| v.tag) {
            None => return Ok(None),
            Some(Tag::Set | Tag::SetSmall) => {}
            Some(t) => return Err(t.redis_type()),
        }
        let v = self.value_mut(key).expect("Set vivant");
        Ok(Some(SetMut { tag: v.tag, val: v.val, node: v.node }))
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

        let Some(mut set) = self.set_mut(key).map_err(|_| TypeError::ValueNotSet)? else {
            return Ok(if count == 1 {
                SPOPResult::Single(None)
            } else {
                SPOPResult::Multiple(Vec::new())
            });
        };
        let popped: Vec<SharedByte> = (0..count).map_while(|_| set.pop(radixox_lib::rng::u64())).collect();
        if set.len() == 0 {
            self.del(key);
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
        match self.get_ref(key).map(|v| v.tag) {
            Some(Tag::Set | Tag::SetSmall) => {}
            Some(_) => return Err(TypeError::ValueNotSet),
            None => {
                // Set neuf construit d'un coup : une seule descente d'écriture
                let (tag, val) = set_into_raw(members.to_vec());
                let added = match tag {
                    Tag::SetSmall => unsafe { small::len(val.ptr) as u32 },
                    _ => unsafe { val.cont.card },
                };
                let root = (tag == Tag::Set).then(|| unsafe { val.idx });
                self.set_raw(key, RawVal::new(tag, val), ttl, None);
                if let Some(root) = root
                    && let Some(loc) = self.live_loc(key)
                {
                    valart().set_set_root_hint(root, loc.node);
                }
                return Ok(added);
            }
        }
        let v = self.value_mut(key).expect("Set vivant");
        let mut set = SetMut { tag: v.tag, val: v.val, node: v.node };
        Ok(members.iter().filter(|m| set.add((*m).clone())).count() as u32)
    }

    pub fn cmd_srem(&mut self, key: &[u8], members: &[SharedByte]) -> Result<u32, RedisType> {
        debug_assert!(!members.is_empty());
        let Some(mut set) = self.set_mut(key)? else {
            return Ok(0);
        };
        let count = members.iter().filter(|m| set.remove(m)).count() as u32;
        if set.len() == 0 {
            self.del(key);
        }
        Ok(count)
    }

    pub fn cmd_smembers(&mut self, key: &[u8]) -> Result<Vec<SharedByte>, RedisType> {
        let Some(set) = self.set_view(key)? else {
            return Ok(Vec::new());
        };
        let mut res = Vec::with_capacity(set.len() as usize);
        set.for_each(|m| res.push(m.clone()));
        Ok(res)
    }

    pub fn cmd_sismember(&mut self, key: &[u8], member: SharedByte) -> Result<bool, RedisType> {
        Ok(self.set_view(key)?.is_some_and(|s| s.contains(&member)))
    }

    pub fn cmd_scard(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        Ok(self.set_view(key)?.map_or(0, |s| s.len()))
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
