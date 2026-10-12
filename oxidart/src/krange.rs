//! KRANGE / KREVRANGE : les `count` premières clés (ordre lexicographique
//! d'octets, croissant ou décroissant) sous un préfixe, strictement après
//! (resp. avant) une clé curseur `from`.
//!
//! Parcours DFS itératif borné : O(profondeur + nœuds visités jusqu'à `count`).
//! - Pas de recherche « plus petit enfant ≥ b » (le SWAR ne sait que l'égalité) :
//!   chaque nœud visité copie ses enfants dans un buffer partagé et les trie
//!   une fois ; son étage de pile garde un curseur dans cette tranche.
//! - La borne `from` ne suit qu'un seul chemin : à chaque nœud on compare sa
//!   clé complète à `from`. Sous-arbre entièrement du bon côté → parcouru sans
//!   comparaison ; entièrement du mauvais côté → sauté ; préfixe de `from` →
//!   seul l'enfant de radix `from[len]` reste borné.
//! - Seules les valeurs string (Bytes, Int) sont rendues et comptées.
//! - Les valeurs sont rendues par référence (pas de clone, donc pas d'écriture
//!   du refcount dans le bloc tas) et préchargées au moment où on les trouve :
//!   l'encodage qui suit les trouve en cache.

use radixox_lib::shared_byte::SharedByte;

use crate::value::Tag;
use crate::{CompResult, OxidArt};

/// Valeur d'une entrée de range, empruntée à l'arbre.
#[derive(Clone, Copy, Debug)]
pub enum RangeVal<'a> {
    Bytes(&'a SharedByte),
    Int(i64),
}

/// Résultat d'un range : clés concaténées dans un seul buffer (aucune
/// allocation par clé) + (fin de la clé dans `keys`, valeur).
#[derive(Default, Debug)]
pub struct RangeOut<'a> {
    keys: Vec<u8>,
    entries: Vec<(u32, RangeVal<'a>)>,
}

impl<'a> RangeOut<'a> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], RangeVal<'a>)> + '_ {
        let mut start = 0;
        self.entries.iter().map(move |&(end, v)| {
            let k = &self.keys[start..end as usize];
            start = end as usize;
            (k, v)
        })
    }
}

/// Valeur possédée d'un range : même taille et alignement que [`RangeVal`],
/// ce qui permet la conversion sur place dans [`RangeOut::into_owned`].
#[derive(Clone, Debug)]
pub enum OwnedVal {
    Bytes(SharedByte),
    Int(i64),
}

/// Range possédé : survit à l'emprunt de l'arbre. Les clés restent dans un
/// seul buffer concaténé ; les valeurs sont des `SharedByte` à refcount +1.
#[derive(Default, Debug)]
pub struct OwnedRange {
    keys: Vec<u8>,
    entries: Vec<(u32, OwnedVal)>,
}

impl OwnedRange {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &OwnedVal)> + '_ {
        let mut start = 0;
        self.entries.iter().map(move |(end, v)| {
            let k = &self.keys[start..*end as usize];
            start = *end as usize;
            (k, v)
        })
    }
}

/// Distance de prefetch (en entrées) pour `into_owned`.
const OWNED_PREFETCH_AHEAD: usize = 8;

const _: () = {
    assert!(size_of::<(u32, RangeVal<'static>)>() == size_of::<(u32, OwnedVal)>());
    assert!(align_of::<(u32, RangeVal<'static>)>() == align_of::<(u32, OwnedVal)>());
};

impl RangeOut<'_> {
    /// Version possédée, sans nouvelle allocation : le buffer de clés est
    /// déplacé, et le vecteur d'entrées est converti sur place
    /// (`&SharedByte` → `SharedByte` à refcount +1). Le bloc de l'entrée
    /// `i + 8` est préchargé pendant qu'on incrémente le refcount de `i`.
    pub fn into_owned(self) -> OwnedRange {
        let mut entries = std::mem::ManuallyDrop::new(self.entries);
        let (ptr, len, cap) = (entries.as_mut_ptr(), entries.len(), entries.capacity());
        let dst = ptr as *mut (u32, OwnedVal);
        for i in 0..len {
            // SAFETY: i < len ; les cases ≥ i sont encore des `(u32, RangeVal)`
            // initialisées, les cases < i des `(u32, OwnedVal)` déjà écrites.
            // Même taille/alignement (const assert) : écrire la case i ne
            // touche ni i-1 ni i+1. Aucune panique possible entre lecture et
            // écriture (clone = refcount +1), donc pas d'état mixte observable.
            unsafe {
                if i + OWNED_PREFETCH_AHEAD < len
                    && let (_, RangeVal::Bytes(b)) = *ptr.add(i + OWNED_PREFETCH_AHEAD)
                {
                    b.prefetch();
                }
                let (end, v) = ptr.add(i).read();
                let owned = match v {
                    RangeVal::Bytes(b) => OwnedVal::Bytes(b.clone()),
                    RangeVal::Int(n) => OwnedVal::Int(n),
                };
                dst.add(i).write((end, owned));
            }
        }
        // SAFETY: même allocation, même taille/alignement d'élément, `len`
        // cases converties.
        let entries = unsafe { Vec::from_raw_parts(dst, len, cap) };
        OwnedRange { keys: self.keys, entries }
    }
}

/// Étage de pile : un nœud dont les enfants triés restant à visiter sont
/// `kids[lo..hi]`.
struct Frame {
    idx: u32,
    /// Longueur de la clé du nœud (compression incluse).
    key_len: u32,
    /// Début de la tranche du nœud dans `kids` (tronquée au dépilement).
    kids_base: u32,
    lo: u32,
    hi: u32,
    /// Étage paresseux du chemin vers `from` : on est descendu dans l'enfant
    /// de ce radix sans rien collecter ; ses frères au-delà (resp. en deçà)
    /// ne sont collectés et triés que si on remonte ici.
    lazy: Option<u8>,
    /// KREVRANGE : la valeur du nœud sort après ses enfants.
    self_pending: bool,
}

/// Buffers de travail réutilisés d'un appel à l'autre (par thread).
#[derive(Default)]
struct Scratch {
    key: Vec<u8>,
    kids: Vec<(u8, u32)>,
    stack: Vec<Frame>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::default();
}

struct Walker<'a, 'f> {
    art: &'a OxidArt,
    from: &'f [u8],
    rev: bool,
    count: usize,
    key: Vec<u8>,
    kids: Vec<(u8, u32)>,
    stack: Vec<Frame>,
    out: RangeOut<'a>,
}

impl<'a> Walker<'a, '_> {
    fn emit(&mut self, idx: u32, key_len: usize) {
        let Some(node) = self.art.try_get_node(idx) else {
            return;
        };
        let Some(v) = node.get_value_ref(self.art.now) else {
            return;
        };
        let val = match v.tag {
            // SAFETY: le tag indique le champ actif de l'union.
            Tag::Bytes => {
                let b: &'a SharedByte = unsafe { &v.val.bytes };
                b.prefetch();
                RangeVal::Bytes(b)
            }
            Tag::Int => RangeVal::Int(unsafe { v.val.integer }),
            _ => return,
        };
        self.out.keys.extend_from_slice(&self.key[..key_len]);
        self.out.entries.push((self.out.keys.len() as u32, val));
    }

    /// Pousse les enfants de `idx` acceptés par `keep` dans `kids`, triés.
    fn collect_sorted(&mut self, idx: u32, keep: impl Fn(u8) -> bool) {
        let base = self.kids.len();
        let kids = &mut self.kids;
        self.art.iter_all_children(idx, |r, c| {
            if keep(r) {
                kids.push((r, c));
            }
        });
        self.kids[base..].sort_unstable_by_key(|&(r, _)| r);
    }

    /// Entre dans `idx` ; `self.key` contient déjà sa clé jusqu'au radix inclus.
    fn enter(&mut self, idx: u32, bounded: bool) {
        let Some(node) = self.art.try_get_node(idx) else {
            return;
        };
        self.key.extend_from_slice(&node.compression);
        let key_len = self.key.len();

        let mut bound = None;
        let mut self_ok = true;
        let mut kids_ok = true;
        if bounded {
            let f = self.from;
            if f.starts_with(&self.key) {
                // Clé du nœud = préfixe de `from` (ou égale).
                let eq = f.len() == key_len;
                if self.rev {
                    // nœud < from (sauf égalité) ; enfants > from si égalité
                    self_ok = !eq;
                    kids_ok = !eq;
                } else {
                    // nœud ≤ from : exclu ; enfants tous > from si égalité
                    self_ok = false;
                }
                if !eq {
                    bound = Some(f[key_len]);
                }
            } else if (self.key.as_slice() > f) == self.rev {
                // Sous-arbre entièrement du mauvais côté de `from`.
                return;
            }
            // Sinon : entièrement du bon côté, plus de borne.
        }

        if self_ok && !self.rev {
            self.emit(idx, key_len);
        }

        let kids_base = self.kids.len() as u32;
        if let Some(b) = bound {
            // Chemin vers `from` : descente directe (recherche SWAR, comme un
            // GET), frères collectés seulement si on remonte ici.
            self.stack.push(Frame {
                idx,
                key_len: key_len as u32,
                kids_base,
                lo: kids_base,
                hi: kids_base,
                lazy: Some(b),
                self_pending: self_ok && self.rev,
            });
            if let Some(child) = self.art.find(idx, b) {
                self.key.push(b);
                self.enter(child, true);
            }
            return;
        }

        if kids_ok {
            self.collect_sorted(idx, |_| true);
        }
        self.stack.push(Frame {
            idx,
            key_len: key_len as u32,
            kids_base,
            lo: kids_base,
            hi: self.kids.len() as u32,
            lazy: None,
            self_pending: self_ok && self.rev,
        });
    }

    fn run(&mut self) {
        while self.out.len() < self.count {
            let Some(f) = self.stack.last_mut() else {
                return;
            };
            if f.lo == f.hi
                && let Some(b) = f.lazy.take()
            {
                // On remonte sur le chemin de `from` : frères strictement
                // après (resp. avant) `b`, triés.
                let idx = f.idx;
                debug_assert_eq!(self.kids.len(), f.kids_base as usize);
                if self.rev {
                    self.collect_sorted(idx, |r| r < b);
                } else {
                    self.collect_sorted(idx, |r| r > b);
                }
                let f = self.stack.last_mut().expect("non vide");
                f.hi = self.kids.len() as u32;
                continue;
            }
            let next = if f.lo < f.hi {
                if self.rev {
                    f.hi -= 1;
                    Some(self.kids[f.hi as usize])
                } else {
                    f.lo += 1;
                    Some(self.kids[f.lo as usize - 1])
                }
            } else {
                None
            };
            match next {
                Some((radix, child)) => {
                    self.key.truncate(f.key_len as usize);
                    self.key.push(radix);
                    self.enter(child, false);
                }
                None => {
                    let f = self.stack.pop().expect("non vide");
                    if f.self_pending {
                        self.emit(f.idx, f.key_len as usize);
                    }
                    self.kids.truncate(f.kids_base as usize);
                }
            }
        }
    }
}

impl OxidArt {
    /// Jusqu'à `count` entrées string sous `prefix`, en ordre lexicographique
    /// croissant (`rev = false`, clés > `from`) ou décroissant (`rev = true`,
    /// clés < `from`). `from` vide = depuis le début (resp. la fin) du préfixe.
    /// Les valeurs non string (hash, set, zset, list) sont sautées sans compter.
    pub fn krange<'a>(&'a self, prefix: &[u8], from: &[u8], count: usize, rev: bool) -> RangeOut<'a> {
        if count == 0 {
            return RangeOut::default();
        }
        let sc = SCRATCH.with(|s| std::mem::take(&mut *s.borrow_mut()));
        let cap = count.min(4096);
        let mut w = Walker {
            art: self,
            from,
            rev,
            count,
            key: sc.key,
            kids: sc.kids,
            stack: sc.stack,
            out: RangeOut {
                keys: Vec::with_capacity(cap * (prefix.len() + 16)),
                entries: Vec::with_capacity(cap),
            },
        };
        if let Some(idx) = self.locate_prefix(prefix, &mut w.key) {
            w.enter(idx, !from.is_empty());
            w.run();
        }
        let Walker { mut key, mut kids, mut stack, out, .. } = w;
        key.clear();
        kids.clear();
        stack.clear();
        SCRATCH.with(|s| *s.borrow_mut() = Scratch { key, kids, stack });
        out
    }

    /// Comme [`krange`](Self::krange), mais rend un range possédé qui survit
    /// à l'emprunt de l'arbre (valeurs à refcount +1, aucune allocation de plus).
    pub fn krange_owned(&self, prefix: &[u8], from: &[u8], count: usize, rev: bool) -> OwnedRange {
        self.krange(prefix, from, count, rev).into_owned()
    }

    /// Racine du sous-arbre des clés commençant par `prefix` ; `key` reçoit sa
    /// clé jusqu'au radix inclus (sans la compression du nœud).
    fn locate_prefix(&self, prefix: &[u8], key: &mut Vec<u8>) -> Option<u32> {
        let mut idx = self.root_idx;
        let mut cursor = 0;
        loop {
            if cursor == prefix.len() {
                return Some(idx);
            }
            let radix = prefix[cursor];
            idx = self.find(idx, radix)?;
            key.push(radix);
            let node = self.try_get_node(idx)?;
            cursor += 1;
            match node.compare_compression_key(&prefix[cursor..]) {
                CompResult::Final => return Some(idx),
                CompResult::Partial(common) => {
                    return (common == prefix.len() - cursor).then_some(idx);
                }
                CompResult::Path => {
                    key.extend_from_slice(&node.compression);
                    cursor += node.compression.len();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use radixox_lib::shared_byte::SharedByte;

    use super::{OwnedVal, RangeVal};
    use crate::{OxidArt, Value};

    fn as_bytes(v: RangeVal<'_>) -> Vec<u8> {
        match v {
            RangeVal::Bytes(b) => b.as_slice().to_vec(),
            RangeVal::Int(n) => n.to_string().into_bytes(),
        }
    }

    fn expected(
        model: &BTreeMap<Vec<u8>, Vec<u8>>,
        prefix: &[u8],
        from: &[u8],
        count: usize,
        rev: bool,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let it = model.iter().filter(|(k, _)| k.starts_with(prefix));
        let kv = |(k, v): (&Vec<u8>, &Vec<u8>)| (k.clone(), v.clone());
        if rev {
            it.rev()
                .filter(|(k, _)| from.is_empty() || k.as_slice() < from)
                .take(count)
                .map(kv)
                .collect()
        } else {
            it.filter(|(k, _)| from.is_empty() || k.as_slice() > from)
                .take(count)
                .map(kv)
                .collect()
        }
    }

    /// Comparaison à une BTreeMap : préfixes imbriqués (user:1 / user:10 /
    /// user:1:x), fort fan-out, hash intercalés (sautés), `from` présent ou
    /// absent, dans ou hors du préfixe, dans les deux sens.
    #[test]
    fn krange_matches_btreemap() {
        let mut art = OxidArt::new();
        let mut model = BTreeMap::new();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let alphabet = b"ab:019z";
        for i in 0..3000 {
            let len = 1 + rnd() as usize % 7;
            let mut k: Vec<u8> = (0..len).map(|_| alphabet[rnd() as usize % alphabet.len()]).collect();
            if i % 3 == 0 {
                k.splice(0..0, b"user:".iter().copied());
            }
            if i % 11 == 0 {
                // hash : présent dans l'arbre mais jamais rendu
                if !model.contains_key(&k) {
                    let _ = art.cmd_hset(&k, &[(SharedByte::from_str("f"), SharedByte::from_str("v"))], None);
                }
                continue;
            }
            if i % 13 == 0 {
                art.set(SharedByte::from_slice(&k), Value::Int(i));
                model.insert(k, i.to_string().into_bytes());
            } else {
                let v = format!("v{i}").into_bytes();
                art.set(SharedByte::from_slice(&k), Value::String(SharedByte::from_slice(&v)));
                model.insert(k, v);
            }
        }
        let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
        let prefixes: [&[u8]; 6] = [b"", b"u", b"user:", b"user:1", b"a:", b"zzzz"];
        for prefix in prefixes {
            for t in 0..120 {
                let from: Vec<u8> = match t % 4 {
                    0 => Vec::new(),
                    1 => keys[rnd() as usize % keys.len()].clone(),
                    2 => {
                        let mut f = keys[rnd() as usize % keys.len()].clone();
                        f.push(b'0'); // probablement absente
                        f
                    }
                    _ => (0..1 + rnd() as usize % 4).map(|_| alphabet[rnd() as usize % alphabet.len()]).collect(),
                };
                let count = [1, 3, 30, 10_000][t % 4];
                for rev in [false, true] {
                    let got: Vec<_> = art
                        .krange(prefix, &from, count, rev)
                        .iter()
                        .map(|(k, v)| (k.to_vec(), as_bytes(v)))
                        .collect();
                    let want = expected(&model, prefix, &from, count, rev);
                    assert_eq!(
                        got,
                        want,
                        "prefix={:?} from={:?} count={count} rev={rev}",
                        String::from_utf8_lossy(prefix),
                        String::from_utf8_lossy(&from)
                    );
                }
            }
        }
    }

    #[test]
    fn krange_owned_matches_borrowed() {
        let mut art = OxidArt::new();
        for i in 0..300 {
            let k = format!("k:{i}");
            if i % 4 == 0 {
                art.set(SharedByte::from_str(&k), Value::Int(-(i as i64)));
            } else {
                art.set(SharedByte::from_str(&k), Value::String(SharedByte::from_str(&format!("v{i}"))));
            }
        }
        for rev in [false, true] {
            let borrowed: Vec<_> = art.krange(b"k:", b"k:15", 100, rev).iter().map(|(k, v)| (k.to_vec(), as_bytes(v))).collect();
            let range = art.krange_owned(b"k:", b"k:15", 100, rev);
            // l'arbre change après coup : le range possédé reste valide
            art.set(SharedByte::from_str("k:149"), Value::String(SharedByte::from_str("écrasé")));
            art.del(b"k:150");
            let owned: Vec<_> = range
                .iter()
                .map(|(k, v)| {
                    let v = match v {
                        OwnedVal::Bytes(b) => b.as_slice().to_vec(),
                        OwnedVal::Int(n) => n.to_string().into_bytes(),
                    };
                    (k.to_vec(), v)
                })
                .collect();
            // avant k:15 : k:0, k:1, k:10..k:14, k:100..k:149 = 57 ; après : plafonné à 100
            assert_eq!(borrowed.len(), if rev { 57 } else { 100 });
            assert_eq!(owned, borrowed);
        }
    }

    /// Conversion sur place seule (sans arbre, donc exécutable sous Miri) :
    /// refcounts, prefetch en avance au-delà de 8 entrées, valeurs Int.
    #[test]
    fn into_owned_in_place() {
        let vals: Vec<SharedByte> = (0..20).map(|i| SharedByte::from_str(&format!("val{i}"))).collect();
        let mut out = super::RangeOut::default();
        for (i, v) in vals.iter().enumerate() {
            out.keys.extend_from_slice(format!("key{i}").as_bytes());
            let rv = if i % 5 == 4 { RangeVal::Int(-(i as i64)) } else { RangeVal::Bytes(v) };
            out.entries.push((out.keys.len() as u32, rv));
        }
        let cap = out.entries.capacity();
        let owned = out.into_owned();
        assert_eq!(owned.entries.capacity(), cap, "même allocation");
        for (i, (k, v)) in owned.iter().enumerate() {
            assert_eq!(k, format!("key{i}").as_bytes());
            match v {
                OwnedVal::Bytes(b) => {
                    assert_eq!(b.as_slice(), format!("val{i}").as_bytes());
                    assert_eq!(vals[i].rc(), 2);
                }
                OwnedVal::Int(n) => assert_eq!(*n, -(i as i64)),
            }
        }
        drop(owned);
        assert!(vals.iter().all(|v| v.rc() == 1));
    }

    /// Pagination par curseur : enchaîner les pages redonne tout, sans doublon.
    #[test]
    fn krange_pagination() {
        let mut art = OxidArt::new();
        for i in 0..500 {
            let k = format!("msg:{i:04}");
            art.set(SharedByte::from_str(&k), Value::String(SharedByte::from_str("x")));
        }
        for rev in [false, true] {
            let mut all = Vec::new();
            let mut from = Vec::new();
            loop {
                let page: Vec<Vec<u8>> = art.krange(b"msg:", &from, 37, rev).iter().map(|(k, _)| k.to_vec()).collect();
                if page.is_empty() {
                    break;
                }
                from = page.last().unwrap().clone();
                all.extend(page);
            }
            let mut want: Vec<Vec<u8>> = (0..500).map(|i| format!("msg:{i:04}").into_bytes()).collect();
            if rev {
                want.reverse();
            }
            assert_eq!(all, want);
        }
    }
}
