//! Commandes List sur les blocs chaînés de `crate::list` (ANALYSE_ART_UNIFIE.md §14).
//! Une List vide n'existe pas : la clé part avec son dernier élément.

use radixox_lib::shared_byte::SharedByte;

use crate::{
    OxidArt,
    list::{self, List},
    value::{RedisType, Tag, alloc_list_raw, list_mut, list_ref},
};

/// Erreur d'une commande List.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListError {
    WrongType(RedisType),
    /// LSET sur une clé absente.
    NoSuchKey,
    /// LSET hors des bornes.
    OutOfRange,
}

impl From<RedisType> for ListError {
    fn from(t: RedisType) -> Self {
        ListError::WrongType(t)
    }
}

/// Bout d'une List.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Left,
    Right,
}

/// Options de LPOS (validées par le parseur) : `rank ≠ 0`, `maxlen` 0 = sans limite.
#[derive(Debug, Clone, Copy)]
pub struct LPosOpts {
    pub rank: i64,
    pub count: Option<u64>,
    pub maxlen: u64,
}

/// Bornes `start..=stop` (négatives depuis la fin) ramenées dans `0..len` ; `None` : vide.
fn span(start: i64, stop: i64, len: u32) -> Option<(u32, u32)> {
    let len = len as i64;
    let start = if start < 0 { (len + start).max(0) } else { start };
    let stop = if stop < 0 { len + stop } else { stop.min(len - 1) };
    (start <= stop && start < len).then_some((start as u32, stop as u32))
}

/// Indice (négatif depuis la fin) ramené dans `0..len`.
fn index(i: i64, len: u32) -> Option<u32> {
    let i = if i < 0 { len as i64 + i } else { i };
    (0..len as i64).contains(&i).then_some(i as u32)
}

impl OxidArt {
    /// En-tête de la List à `key` : `None` si absente.
    fn list_idx(&mut self, key: &[u8]) -> Result<Option<u32>, RedisType> {
        match self.get_ref(key) {
            Some(v) => v.list_idx().map(Some),
            None => Ok(None),
        }
    }

    /// LPUSH / RPUSH (`only_existing` : LPUSHX / RPUSHX) : longueur après.
    pub fn cmd_lpush(&mut self, key: &[u8], end: End, vals: &[SharedByte], only_existing: bool) -> Result<u32, RedisType> {
        let idx = match self.list_idx(key)? {
            Some(idx) => idx,
            None if only_existing => return Ok(0),
            None => self
                .ensure_container(key, None, Tag::List, || alloc_list_raw(List::new()))
                .map_err(Tag::redis_type)?,
        };
        let l = list_mut(idx);
        for v in vals {
            match end {
                End::Left => l.push_front(v.clone()),
                End::Right => l.push_back(v.clone()),
            }
        }
        Ok(l.len())
    }

    /// LPOP / RPOP : jusqu'à `count` éléments (`None` : clé absente).
    pub fn cmd_lpop(&mut self, key: &[u8], end: End, count: u64) -> Result<Option<Vec<SharedByte>>, RedisType> {
        let Some(idx) = self.list_idx(key)? else { return Ok(None) };
        let l = list_mut(idx);
        let n = count.min(l.len() as u64) as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(match end {
                End::Left => l.pop_front(),
                End::Right => l.pop_back(),
            }
            .expect("élément"));
        }
        if l.is_empty() {
            self.del(key);
        }
        Ok(Some(out))
    }

    pub fn cmd_llen(&mut self, key: &[u8]) -> Result<u32, RedisType> {
        Ok(self.list_idx(key)?.map_or(0, |i| list_ref(i).len()))
    }

    /// LRANGE : depuis le bout le plus proche du début de la plage.
    pub fn cmd_lrange(&mut self, key: &[u8], start: i64, stop: i64) -> Result<Vec<SharedByte>, RedisType> {
        let Some(idx) = self.list_idx(key)? else { return Ok(Vec::new()) };
        let l = list_ref(idx);
        let Some((a, b)) = span(start, stop, l.len()) else { return Ok(Vec::new()) };
        let n = (b - a + 1) as usize;
        let mut out = Vec::with_capacity(n);
        let mut c = l.locate(a);
        while let Some(at) = c
            && out.len() < n
        {
            out.push(list::value(at).clone());
            c = list::next(at);
        }
        Ok(out)
    }

    pub fn cmd_lindex(&mut self, key: &[u8], i: i64) -> Result<Option<SharedByte>, RedisType> {
        let Some(idx) = self.list_idx(key)? else { return Ok(None) };
        let l = list_ref(idx);
        Ok(index(i, l.len()).and_then(|i| l.get(i)).cloned())
    }

    pub fn cmd_lset(&mut self, key: &[u8], i: i64, val: SharedByte) -> Result<(), ListError> {
        let idx = self.list_idx(key)?.ok_or(ListError::NoSuchKey)?;
        let l = list_mut(idx);
        let c = index(i, l.len()).and_then(|i| l.locate(i)).ok_or(ListError::OutOfRange)?;
        drop(l.replace(c, val));
        Ok(())
    }

    /// LINSERT : longueur après, -1 si le pivot manque, 0 si la clé manque.
    pub fn cmd_linsert(&mut self, key: &[u8], after: bool, pivot: &[u8], val: SharedByte) -> Result<i64, RedisType> {
        let Some(idx) = self.list_idx(key)? else { return Ok(0) };
        let l = list_mut(idx);
        let mut c = l.first_cursor();
        while let Some(at) = c {
            if list::value(at).as_slice() == pivot {
                l.insert(at, after, val);
                return Ok(l.len() as i64);
            }
            c = list::next(at);
        }
        Ok(-1)
    }

    /// LREM : nombre retiré.
    pub fn cmd_lrem(&mut self, key: &[u8], count: i64, val: &[u8]) -> Result<u32, RedisType> {
        let Some(idx) = self.list_idx(key)? else { return Ok(0) };
        let l = list_mut(idx);
        let n = l.remove_matching(count, |x| x == val);
        if l.is_empty() {
            self.del(key);
        }
        Ok(n)
    }

    pub fn cmd_ltrim(&mut self, key: &[u8], start: i64, stop: i64) -> Result<(), RedisType> {
        let Some(idx) = self.list_idx(key)? else { return Ok(()) };
        let l = list_mut(idx);
        match span(start, stop, l.len()) {
            Some((a, b)) => {
                let tail = l.len() - 1 - b;
                l.drop_end(a, true);
                l.drop_end(tail, false);
            }
            None => {
                self.del(key);
            }
        }
        Ok(())
    }

    /// LPOS : positions (depuis le début) des correspondances, au plus `count` (sans
    /// `count` : la première seulement).
    pub fn cmd_lpos(&mut self, key: &[u8], val: &[u8], o: LPosOpts) -> Result<Vec<u32>, RedisType> {
        let Some(idx) = self.list_idx(key)? else { return Ok(Vec::new()) };
        let l = list_ref(idx);
        let want = match o.count {
            None => 1,
            Some(0) => u64::MAX,
            Some(n) => n,
        };
        let mut skip = o.rank.unsigned_abs() - 1;
        let maxlen = if o.maxlen == 0 { u64::MAX } else { o.maxlen };
        let back = o.rank < 0;
        let mut out = Vec::new();
        let mut c = if back { l.last_cursor() } else { l.first_cursor() };
        let mut k = 0u64;
        while let Some(at) = c
            && k < maxlen
            && (out.len() as u64) < want
        {
            if list::value(at).as_slice() == val {
                if skip == 0 {
                    out.push(if back { l.len() - 1 - k as u32 } else { k as u32 });
                } else {
                    skip -= 1;
                }
            }
            k += 1;
            c = if back { list::prev(at) } else { list::next(at) };
        }
        Ok(out)
    }

    /// LMOVE / RPOPLPUSH : l'élément déplacé (`None` : source absente). Même clé :
    /// rotation.
    pub fn cmd_lmove(&mut self, src: &[u8], dst: &[u8], from: End, to: End) -> Result<Option<SharedByte>, RedisType> {
        let Some(si) = self.list_idx(src)? else { return Ok(None) };
        // la destination d'un autre type : erreur avant de rien retirer (comme Redis)
        if src != dst {
            self.list_idx(dst)?;
        }
        let l = list_mut(si);
        let v = match from {
            End::Left => l.pop_front(),
            End::Right => l.pop_back(),
        }
        .expect("élément");
        if src == dst {
            match to {
                End::Left => l.push_front(v.clone()),
                End::Right => l.push_back(v.clone()),
            }
            return Ok(Some(v));
        }
        if l.is_empty() {
            self.del(src);
        }
        self.cmd_lpush(dst, to, std::slice::from_ref(&v), false)?;
        Ok(Some(v))
    }
}
