//! Primitives des commandes de clés et de strings du lot 1 (COMMANDS_PLAN.md §3) :
//! valeur string avec son échéance, écriture qui garde le TTL, échéance absolue sous
//! condition (EXPIRE NX|XX|GT|LT, EXPIREAT), RENAME qui déplace la valeur (la poignée
//! d'un Hash, d'un Set, d'un ZSet ou d'une List passe telle quelle : rien n'est copié),
//! RANDOMKEY. Toutes les mutations passent par `set_raw` / `remove_loc` /
//! `set_loc_exp` (barrière de save).

#[allow(unused_imports)]
use crate::prelude::*;
use radixox_lib::shared_byte::SharedByte;

use crate::node::EntryRef;
use crate::value::{RawVal, RedisType, Tag, Value};
use crate::{OxidArt, expired};

pub use radixox_lib::cmd::ExpireCond;

impl OxidArt {
    /// Valeur string vivante de `key` (entier rendu en décimal) et son échéance.
    /// `Err` : la clé tient un autre type.
    pub fn get_str_exp(&mut self, key: &[u8]) -> Result<Option<(SharedByte, Option<u64>)>, RedisType> {
        let Some(loc) = self.live_loc(key) else {
            return Ok(None);
        };
        let (tag, val, exp) = self.loc_value(loc);
        // SAFETY: le tag indique le champ actif de l'union.
        let s = unsafe {
            match tag {
                Tag::Bytes => (*val.bytes).clone(),
                Tag::Int => SharedByte::from_slice(val.integer.to_string().as_bytes()),
                t => return Err(t.redis_type()),
            }
        };
        Ok(Some((s, exp)))
    }

    /// Type de la valeur vivante de `key` (`RedisType::None` : absente), sans la copier.
    pub fn type_of(&mut self, key: &[u8]) -> RedisType {
        self.get_ref(key).map_or(RedisType::None, |v| v.tag.redis_type())
    }

    /// Pose `val` à `key`, échéance absolue `exp` (secondes Unix, `None` : sans TTL).
    pub fn set_at(&mut self, key: &[u8], val: Value, exp: Option<u64>) {
        self.set_raw(key, RawVal::from_value(val), exp, None);
    }

    /// Échéance absolue de `key` : `Err(())` si absente, `Ok(None)` sans TTL.
    pub fn expire_time(&mut self, key: &[u8]) -> Result<Option<u64>, ()> {
        let loc = self.live_loc(key).ok_or(())?;
        Ok(self.loc_value(loc).2)
    }

    /// Nouvelle échéance `at` (secondes Unix) sous condition. `passed` : l'échéance est
    /// déjà passée, la clé est supprimée (si la condition tient). Rend `true` si la clé
    /// existe et que la condition tient (réponse 1 de Redis).
    pub fn expire_at(&mut self, key: &[u8], at: u64, passed: bool, cond: ExpireCond) -> bool {
        let Some(loc) = self.live_loc(key) else {
            return false;
        };
        let cur = self.loc_value(loc).2;
        let ok = match (cond, cur) {
            (ExpireCond::Always, _) => true,
            (ExpireCond::Nx, c) => c.is_none(),
            (ExpireCond::Xx, c) => c.is_some(),
            // sans TTL = infini : jamais dépassé, toujours plus proche
            (ExpireCond::Gt, None) => false,
            (ExpireCond::Gt, Some(c)) => at > c,
            (ExpireCond::Lt, None) => true,
            (ExpireCond::Lt, Some(c)) => at < c,
        };
        if !ok {
            return false;
        }
        if passed {
            if self.window.is_some() {
                self.remove_in_window(loc);
                self.touch(key);
            } else {
                drop(self.remove_loc(loc));
            }
        } else {
            self.set_loc_exp(loc, Some(at));
        }
        true
    }

    /// TOUCH : la clé existe-t-elle (vivante) ? Compte l'accès (LRU / LFU).
    pub fn touch_key(&mut self, key: &[u8]) -> bool {
        let Some(_v) = self.get_ref(key) else {
            return false;
        };
        #[cfg(feature = "eviction")]
        crate::value::touch_raw(_v.tag, _v.val);
        true
    }

    /// RENAME (`nx` : RENAMENX). `None` : `src` absente. `Some(false)` : RENAMENX et
    /// `dst` existe. La valeur et son échéance passent de `src` à `dst` sans copie ;
    /// l'ancienne valeur de `dst` (tout type) est libérée.
    pub fn rename(&mut self, src: &[u8], dst: &[u8], nx: bool) -> Option<bool> {
        let loc = self.live_loc(src)?;
        if src == dst {
            return Some(!nx);
        }
        if nx && self.get_ref(dst).is_some() {
            return Some(false);
        }
        // `src` cherchée à nouveau : `get_ref(dst)` a pu retirer une `dst` expirée et
        // changer la forme de l'arbre.
        let loc = if nx { self.live_loc(src).expect("src vivante") } else { loc };
        // Jamais dans une fenêtre AMAC (RENAME n'a pas de chemin à indice) : la
        // suppression peut changer la forme.
        debug_assert!(self.window.is_none());
        let exp = self.loc_value(loc).2;
        let raw = self.remove_loc(loc);
        self.set_raw(dst, raw, exp, None);
        Some(true)
    }

    /// RANDOMKEY : une clé vivante au hasard (descente aléatoire : pas uniforme, comme
    /// l'échantillonnage de Redis n'est pas exact non plus). `None` si la base est vide
    /// ou si les tirages ne tombent que sur des clés expirées.
    pub fn random_key(&mut self) -> Option<Vec<u8>> {
        use radixox_lib::rng;
        if self.key_count == 0 {
            return None;
        }
        let now = self.now;
        let mut path = Vec::new();
        for _ in 0..64 {
            path.clear();
            let mut idx = self.root_idx;
            let found = loop {
                let node = self.node(idx);
                let own = node.has_val() as usize;
                let n = own + node.len();
                if n == 0 {
                    break None;
                }
                let k = rng::below(n as u64) as usize;
                if k < own {
                    break (!expired(node.exp_and_radix.exp(), now)).then_some(());
                }
                let e = k - own;
                let r = node.radix_at(e);
                match node.entry_ref(e) {
                    EntryRef::Leaf { ttl, sfx, .. } => {
                        path.push(r);
                        if let Some(sfx) = sfx {
                            path.extend_from_slice(sfx);
                        }
                        break (!expired(ttl, now)).then_some(());
                    }
                    EntryRef::Child(c) => {
                        path.push(r);
                        path.extend_from_slice(&self.node(c).prefix);
                        idx = c;
                    }
                }
            };
            if found.is_some() {
                return Some(path);
            }
        }
        None
    }
}

/// Tirage de SRANDMEMBER / HRANDFIELD parmi `len` éléments (`len > 0`), comme Redis :
/// `count` négatif, avec répétitions ; positif, distincts, au plus `len`. `one` tire un
/// élément au hasard (descente aléatoire, pas forcément uniforme), `all` les énumère,
/// `id` est l'identité d'un élément (membre, champ).
pub(crate) fn sample<'a, T: Copy>(
    len: usize,
    count: i64,
    one: impl Fn(u64) -> T,
    all: impl FnOnce(&mut Vec<T>),
    id: impl Fn(&T) -> &'a [u8],
) -> Vec<T> {
    use radixox_lib::rng;
    if count < 0 {
        return (0..count.unsigned_abs()).map(|_| one(rng::u64())).collect();
    }
    let count = count as usize;
    let shuffled = |mut v: Vec<T>| {
        // Fisher-Yates partiel : les `count` premiers
        for i in 0..count.min(v.len()) {
            let j = i + rng::below((v.len() - i) as u64) as usize;
            v.swap(i, j);
        }
        v.truncate(count);
        v
    };
    if count >= len {
        let mut v = Vec::with_capacity(len);
        all(&mut v);
        return v;
    }
    if count * 3 >= len {
        let mut v = Vec::with_capacity(len);
        all(&mut v);
        return shuffled(v);
    }
    // peu d'éléments parmi beaucoup : tirages sans doublon, puis repli sur l'énumération
    // si la descente aléatoire tombe trop souvent sur les mêmes
    let mut seen = alloc::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(count);
    for _ in 0..count.saturating_mul(16).saturating_add(64) {
        let x = one(rng::u64());
        if seen.insert(id(&x)) {
            out.push(x);
            if out.len() == count {
                return out;
            }
        }
    }
    let mut v = Vec::with_capacity(len);
    all(&mut v);
    shuffled(v)
}
