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

use crate::node::{Ahead, Cursor, EntryRef};
use crate::value::{RedisType, Tag, ValUnion};
use crate::{OxidArt, PrefixHit, expired};

/// Valeur d'une entrée de range, empruntée à l'arbre.
#[derive(Clone, Copy, Debug)]
pub enum RangeVal<'a> {
    Bytes(&'a SharedByte),
    Int(i64),
}

/// Valeur string vivante, par référence (`None` : expirée, ou pas une string).
#[inline]
pub(crate) fn str_val(tag: Tag, val: &ValUnion, exp: Option<u64>, now: u64) -> Option<RangeVal<'_>> {
    if expired(exp, now) {
        return None;
    }
    match tag {
        // SAFETY: le tag indique le champ actif de l'union.
        Tag::Bytes => Some(RangeVal::Bytes(unsafe { &val.bytes })),
        Tag::Int => Some(RangeVal::Int(unsafe { val.integer })),
        _ => None,
    }
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

/// Étage de pile : un nœud dont les entrées (déjà triées dans le nœud)
/// restent à visiter à partir du rang `next`.
struct Frame {
    idx: u32,
    /// Nombre d'entrées du nœud.
    n: i32,
    /// Longueur de la clé du nœud (préfixe inclus).
    key_len: u32,
    /// Prochain rang à visiter (croissant, ou décroissant en KREVRANGE ;
    /// négatif = fini).
    next: i32,
    /// Radix de l'entrée sur le chemin de `from` : elle seule reste bornée.
    /// `NO_BOUND` : aucune (u16 plutôt qu'`Option<u8>` : comparaison en une
    /// instruction dans la boucle).
    bound: u16,
    /// KREVRANGE : la valeur du nœud sort après ses entrées.
    self_pending: bool,
    /// Enfant préchargé à l'étage 1 au pas précédent (`NO_CHILD` sinon) :
    /// l'étage 2 le reprend sans redécoder l'entrée.
    staged: u32,
}

const NO_CHILD: u32 = u32::MAX;
const NO_BOUND: u16 = 256;

/// Nombre d'entrées regardées en avance pour précharger les nœuds enfants.
const PREFETCH_AHEAD: i32 = 2;

/// Copie de `s` à l'adresse `d` sans appel à `memcpy` pour les morceaux
/// courts (≤ 32 octets) : copies chevauchantes de 16, 8 ou 4 octets.
///
/// # Safety
/// `d` est valide en écriture sur `s.len()` octets et ne chevauche pas `s`.
#[inline(always)]
unsafe fn copy_short(d: *mut u8, s: &[u8]) {
    let n = s.len();
    let p = s.as_ptr();
    unsafe {
        if n > 32 {
            std::ptr::copy_nonoverlapping(p, d, n);
        } else if n >= 16 {
            (d as *mut u128).write_unaligned((p as *const u128).read_unaligned());
            (d.add(n - 16) as *mut u128).write_unaligned((p.add(n - 16) as *const u128).read_unaligned());
        } else if n >= 8 {
            (d as *mut u64).write_unaligned((p as *const u64).read_unaligned());
            (d.add(n - 8) as *mut u64).write_unaligned((p.add(n - 8) as *const u64).read_unaligned());
        } else if n >= 4 {
            (d as *mut u32).write_unaligned((p as *const u32).read_unaligned());
            (d.add(n - 4) as *mut u32).write_unaligned((p.add(n - 4) as *const u32).read_unaligned());
        } else {
            for i in 0..n {
                *d.add(i) = *p.add(i);
            }
        }
    }
}

/// `extend_from_slice` avec [`copy_short`] : une réservation, pas d'appel de
/// bibliothèque pour les clés et suffixes courts (deux `memcpy` par entrée
/// coûtaient ~1 600 instructions par page de 64).
#[inline(always)]
fn extend_short(v: &mut Vec<u8>, s: &[u8]) {
    v.reserve(s.len());
    // SAFETY: `s.len()` octets réservés après `len`.
    unsafe {
        copy_short(v.as_mut_ptr().add(v.len()), s);
        v.set_len(v.len() + s.len());
    }
}

/// Clé courante = `key[..key_len]` + `radix` + `sfx`, en une réservation.
#[inline(always)]
fn set_tail(key: &mut Vec<u8>, key_len: usize, radix: u8, sfx: &[u8]) {
    key.truncate(key_len);
    key.reserve(1 + sfx.len());
    // SAFETY: `1 + sfx.len()` octets réservés après `key_len`.
    unsafe {
        let d = key.as_mut_ptr().add(key_len);
        *d = radix;
        copy_short(d.add(1), sfx);
        key.set_len(key_len + 1 + sfx.len());
    }
}

/// Buffers de travail réutilisés d'un appel à l'autre (par thread).
#[derive(Default)]
struct Scratch {
    key: Vec<u8>,
    stack: Vec<Frame>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::default();
}

/// Ce que le parcours fait des clés rencontrées (KRANGE : strings avec
/// valeur ; SCAN : clés de tout type, filtrées, avec budget).
trait Sink<'a> {
    /// Une clé avec valeur, du bon côté de `from` (vivante ou non).
    fn emit(&mut self, key: &[u8], tag: Tag, val: &'a ValUnion, exp: Option<u64>, now: u64);
    /// Le parcours s'arrête dès que c'est vrai (testé après chaque entrée).
    fn full(&self) -> bool;
}

struct RangeSink<'a> {
    out: RangeOut<'a>,
    count: usize,
}

impl<'a> Sink<'a> for RangeSink<'a> {
    /// Ajoute la clé si sa valeur est une string vivante.
    #[inline]
    fn emit(&mut self, key: &[u8], tag: Tag, val: &'a ValUnion, exp: Option<u64>, now: u64) {
        let Some(v) = str_val(tag, val, exp, now) else {
            return;
        };
        if let RangeVal::Bytes(b) = v {
            b.prefetch();
        }
        extend_short(&mut self.out.keys, key);
        self.out.entries.push((self.out.keys.len() as u32, v));
    }

    #[inline]
    fn full(&self) -> bool {
        self.out.len() >= self.count
    }
}

struct Walker<'a, 'f, S> {
    art: &'a OxidArt,
    from: &'f [u8],
    rev: bool,
    key: Vec<u8>,
    stack: Vec<Frame>,
    sink: S,
}

impl<'a, S: Sink<'a>> Walker<'a, '_, S> {
    #[inline]
    fn emit(&mut self, tag: Tag, val: &'a ValUnion, exp: Option<u64>) {
        self.sink.emit(&self.key, tag, val, exp, self.art.now);
    }

    /// La clé `self.key` est-elle du bon côté (strict) de `from` ?
    #[inline]
    fn past_from(&self) -> bool {
        if self.rev {
            self.key.as_slice() < self.from
        } else {
            self.key.as_slice() > self.from
        }
    }

    /// Entre dans `idx` ; `self.key` contient sa clé jusqu'au radix inclus
    /// (préfixe inclus aussi si `prefix_in_key`). `bounded` : le nœud est sur
    /// le chemin de `from`.
    fn enter(&mut self, idx: u32, bounded: bool, prefix_in_key: bool) {
        let art = self.art;
        let node = art.node(idx);
        if !prefix_in_key {
            extend_short(&mut self.key, &node.prefix);
        }
        let key_len = self.key.len();
        let n = node.len() as i32;

        let mut self_ok = true;
        let mut bound = NO_BOUND;
        // rangs à visiter : [first, n) en croissant, [0, first] en décroissant
        let mut first = if self.rev { n - 1 } else { 0 };
        if bounded {
            let f = self.from;
            if f.starts_with(&self.key) {
                let eq = f.len() == key_len;
                // nœud ≤ from ; nœud < from sauf égalité
                self_ok = self.rev && !eq;
                if eq {
                    // toutes les entrées sont > from
                    first = if self.rev { -1 } else { 0 };
                } else {
                    let b = f[key_len];
                    bound = b as u16;
                    first = match node.search(b) {
                        Ok(e) => e as i32,
                        Err(e) if self.rev => e as i32 - 1,
                        Err(e) => e as i32,
                    };
                }
            } else if (self.key.as_slice() > f) == self.rev {
                // Sous-arbre entièrement du mauvais côté de `from`.
                self.key.truncate(key_len - if prefix_in_key { 0 } else { node.prefix.len() });
                return;
            }
            // Sinon : entièrement du bon côté, plus de borne.
        }

        if self_ok && !self.rev && node.has_val() {
            self.emit(node.tag, &node.val, node.exp_and_radix.exp());
        }
        self.stack.push(Frame {
            idx,
            n,
            key_len: key_len as u32,
            next: first,
            bound,
            self_pending: self_ok && self.rev && node.has_val(),
            staged: NO_CHILD,
        });
    }

    /// Parcours. Une boucle interne par visite de nœud, avec le curseur, le
    /// rang et l'état d'anticipation en variables locales (registres) : on ne
    /// la quitte que pour entrer dans un enfant ou en fin de nœud. Garder le
    /// curseur dans `self` coûtait plus qu'il ne rapportait (lectures bloquées
    /// derrière ses écritures, chevauchement de lignes).
    fn run(&mut self) {
        let art = self.art;
        let step = if self.rev { -1 } else { 1 };
        'frames: while !self.sink.full() {
            let Some(f) = self.stack.last() else {
                return;
            };
            let (idx, n, key_len, bound) = (f.idx, f.n, f.key_len as usize, f.bound);
            let (mut e, mut staged) = (f.next, f.staged);
            if e < 0 || e >= n {
                let f = self.stack.pop().expect("non vide");
                self.key.truncate(f.key_len as usize);
                if f.self_pending {
                    let node = art.node(f.idx);
                    self.emit(node.tag, &node.val, node.exp_and_radix.exp());
                }
                continue;
            }
            let node = art.node(idx);
            let mut cur = Cursor::new(node, e as usize);
            loop {
                // Préchargement en deux étages. Sur des clés aléatoires, les
                // nœuds du dernier niveau n'ont que 2 ou 3 feuilles : anticiper
                // dans le nœud courant ne couvre presque rien, et les suffixes
                // d'un enfant ne sont connus qu'une fois l'enfant arrivé. Donc :
                // - étage 1, entrée e + 2 : les deux lignes de l'enfant (adresses
                //   connues par l'idx), ou le suffixe d'une feuille ;
                // - étage 2, entrée e + 1 : l'enfant préchargé au pas précédent
                //   (`staged`) est là ; sa page et les suffixes de ses feuilles
                //   partent, avec tout le sous-arbre de l'entrée e comme avance.
                if staged != NO_CHILD {
                    art.node(staged).prefetch_leaves();
                    staged = NO_CHILD;
                }
                let ahead = e + step * PREFETCH_AHEAD;
                if (0..n).contains(&ahead) {
                    // SAFETY: l'arbre n'est pas modifié pendant le parcours (`&OxidArt`).
                    match unsafe { cur.ahead(node, ahead as usize) } {
                        Ahead::Child(c) => {
                            let child = art.node(c);
                            radixox_lib::prefetch::prefetch_l1(child as *const _ as *const u8);
                            child.prefetch_body();
                            staged = c;
                        }
                        Ahead::Sfx(sfx) => sfx.prefetch(),
                        Ahead::Nothing => {}
                    }
                }
                // SAFETY: idem ; `cur` est le curseur de `node`.
                let (r, entry) = unsafe { cur.get(node, e as usize) };
                e += step;
                let bounded = bound == r as u16;
                match entry {
                    EntryRef::Child(c) => {
                        set_tail(&mut self.key, key_len, r, &[]);
                        let f = self.stack.last_mut().expect("non vide");
                        (f.next, f.staged) = (e, staged);
                        self.enter(c, bounded, false);
                        continue 'frames;
                    }
                    EntryRef::Leaf { tag, val, ttl, sfx } => {
                        set_tail(&mut self.key, key_len, r, sfx.map_or(&[], |s| s));
                        if !bounded || self.past_from() {
                            self.emit(tag, val, ttl);
                        }
                    }
                }
                if e < 0 || e >= n || self.sink.full() {
                    let f = self.stack.last_mut().expect("non vide");
                    (f.next, f.staged) = (e, staged);
                    continue 'frames;
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
        let cap = count.min(4096);
        let sink = RangeSink {
            out: RangeOut {
                keys: Vec::with_capacity(cap * (prefix.len() + 32)),
                entries: Vec::with_capacity(cap),
            },
            count,
        };
        self.range_walk(prefix, from, rev, sink, |_| ()).0.out
    }

    /// Comme [`krange`](Self::krange), mais rend un range possédé qui survit
    /// à l'emprunt de l'arbre (valeurs à refcount +1, aucune allocation de plus).
    pub fn krange_owned(&self, prefix: &[u8], from: &[u8], count: usize, rev: bool) -> OwnedRange {
        self.krange(prefix, from, count, rev).into_owned()
    }

    /// Parcours commun à KRANGE et SCAN. `on_stop` reçoit la clé courante si
    /// le parcours s'est arrêté sur `full()` avant la fin (c'est alors la
    /// dernière clé passée au sink) ; le booléen rendu dit si tout le préfixe
    /// a été parcouru.
    fn range_walk<'a, S: Sink<'a>, R>(
        &'a self,
        prefix: &[u8],
        from: &[u8],
        rev: bool,
        sink: S,
        on_stop: impl FnOnce(&[u8]) -> R,
    ) -> (S, Option<R>) {
        let sc = SCRATCH.with(|s| std::mem::take(&mut *s.borrow_mut()));
        let mut w = Walker {
            art: self,
            from,
            rev,
            key: sc.key,
            stack: sc.stack,
            sink,
        };
        match self.find_prefix(prefix, &mut w.key) {
            PrefixHit::None => {}
            PrefixHit::Node(idx) => {
                w.enter(idx, !from.is_empty(), true);
                w.run();
            }
            PrefixHit::Leaf { node, radix } => {
                if let Some(EntryRef::Leaf { tag, val, ttl, .. }) = self.node(node).get(radix)
                    && (from.is_empty() || w.past_from())
                {
                    w.emit(tag, val, ttl);
                }
            }
        }
        // Pile vide : le préfixe est épuisé (sortie normale de `run`, ou
        // préfixe absent / réduit à une feuille).
        let stopped = (!w.stack.is_empty()).then(|| on_stop(&w.key));
        let Walker { mut key, mut stack, sink, .. } = w;
        key.clear();
        stack.clear();
        SCRATCH.with(|s| *s.borrow_mut() = Scratch { key, stack });
        (sink, stopped)
    }

    /// Une page de SCAN : clés de tout type sous `prefix`, en ordre croissant,
    /// strictement après `from`, retenues par `keep(clé, type)`.
    ///
    /// S'arrête après `count` clés retenues ou `budget` clés examinées
    /// (expirées et rejetées comprises : un MATCH qui ne retient presque rien
    /// ne bloque pas le serveur). [`ScanPage::next`] est la clé de reprise
    /// (la dernière examinée), `None` quand le préfixe est épuisé.
    pub fn scan_keys(
        &self,
        prefix: &[u8],
        from: &[u8],
        count: usize,
        budget: usize,
        keep: impl FnMut(&[u8], RedisType) -> bool,
    ) -> ScanPage {
        let sink = ScanSink {
            page: ScanPage::default(),
            count: count.max(1),
            budget: budget.max(1),
            keep,
        };
        let (sink, next) = self.range_walk(prefix, from, false, sink, <[u8]>::to_vec);
        let mut page = sink.page;
        page.next = next;
        page
    }
}

/// Résultat de [`OxidArt::scan_keys`] : clés concaténées + fins.
#[derive(Default, Debug)]
pub struct ScanPage {
    keys: Vec<u8>,
    ends: Vec<u32>,
    /// Clé après laquelle reprendre ; `None` : parcours terminé.
    pub next: Option<Vec<u8>>,
}

impl ScanPage {
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> + '_ {
        let mut start = 0;
        self.ends.iter().map(move |&end| {
            let k = &self.keys[start..end as usize];
            start = end as usize;
            k
        })
    }
}

struct ScanSink<F> {
    page: ScanPage,
    count: usize,
    budget: usize,
    keep: F,
}

impl<'a, F: FnMut(&[u8], RedisType) -> bool> Sink<'a> for ScanSink<F> {
    #[inline]
    fn emit(&mut self, key: &[u8], tag: Tag, _val: &'a ValUnion, exp: Option<u64>, now: u64) {
        self.budget -= 1;
        if expired(exp, now) || !(self.keep)(key, tag.redis_type()) {
            return;
        }
        extend_short(&mut self.page.keys, key);
        self.page.ends.push(self.page.keys.len() as u32);
    }

    #[inline]
    fn full(&self) -> bool {
        self.page.ends.len() >= self.count || self.budget == 0
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

    fn xorshift(mut x: u64) -> impl FnMut() -> u64 {
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        }
    }

    /// Scan complet page par page (reprise sur `next`) : toutes les clés du
    /// préfixe retenues par `keep`, dans l'ordre, sans doublon, quels que
    /// soient `count` et `budget` ; expirées jamais rendues.
    #[test]
    fn scan_pages_match_btreemap() {
        use crate::value::RedisType;
        let mut art = OxidArt::new();
        art.set_now(1000);
        let mut model: BTreeMap<Vec<u8>, RedisType> = BTreeMap::new();
        let mut rnd = xorshift(0x9E37_79B9_7F4A_7C15);
        let alphabet = b"ab:019z";
        for i in 0..3000 {
            let len = 1 + rnd() as usize % 7;
            let mut k: Vec<u8> = (0..len).map(|_| alphabet[rnd() as usize % alphabet.len()]).collect();
            if i % 3 == 0 {
                k.splice(0..0, b"user:".iter().copied());
            }
            art.del(&k);
            model.remove(&k);
            match i % 7 {
                0 => {
                    let _ = art.cmd_hset(&k, &[(SharedByte::from_str("f"), SharedByte::from_str("v"))], None);
                    model.insert(k, RedisType::Hash);
                }
                1 => {
                    // expirée au moment du scan : jamais rendue
                    art.set_ttl(SharedByte::from_slice(&k), std::time::Duration::from_secs(5), Value::Int(1));
                }
                2 => {
                    art.set(SharedByte::from_slice(&k), Value::Int(i));
                    model.insert(k, RedisType::String);
                }
                _ => {
                    art.set(SharedByte::from_slice(&k), Value::String(SharedByte::from_str("v")));
                    model.insert(k, RedisType::String);
                }
            }
        }
        art.set_now(2000);
        let prefixes: [&[u8]; 5] = [b"", b"u", b"user:", b"user:1", b"zzzz"];
        for prefix in prefixes {
            for (count, budget) in [(1, 1), (10, 1000), (7, 3), (100, 50), (5000, 100_000)] {
                for only_hash in [false, true] {
                    let keep = |_: &[u8], t: RedisType| !only_hash || t == RedisType::Hash;
                    let want: Vec<Vec<u8>> = model
                        .iter()
                        .filter(|(k, t)| k.starts_with(prefix) && keep(k, **t))
                        .map(|(k, _)| k.clone())
                        .collect();
                    let mut got = Vec::new();
                    let mut from = Vec::new();
                    let mut pages = 0;
                    loop {
                        let page = art.scan_keys(prefix, &from, count, budget, keep);
                        assert!(page.len() <= count);
                        got.extend(page.iter().map(<[u8]>::to_vec));
                        pages += 1;
                        assert!(pages <= 2 * model.len() + 2, "le scan ne termine pas");
                        match page.next {
                            Some(n) => {
                                assert!(n.as_slice() > from.as_slice(), "la reprise doit avancer");
                                from = n;
                            }
                            None => break,
                        }
                    }
                    assert_eq!(got, want, "préfixe {prefix:?}, count {count}, budget {budget}, hash {only_hash}");
                }
            }
        }
    }

    /// Écritures entre les pages : une clé présente du début à la fin du scan
    /// sort exactement une fois ; aucune clé ne sort deux fois.
    #[test]
    fn scan_survives_writes_between_pages() {
        let mut art = OxidArt::new();
        let mut rnd = xorshift(0xD1B5_4A32_D192_ED03);
        let key = |rnd: &mut dyn FnMut() -> u64| -> Vec<u8> {
            let len = 1 + rnd() as usize % 6;
            (0..len).map(|_| b"abc:01"[rnd() as usize % 6]).collect()
        };
        let mut live = std::collections::BTreeSet::new();
        for _ in 0..2000 {
            let k = key(&mut rnd);
            art.set(SharedByte::from_slice(&k), Value::String(SharedByte::from_str("v")));
            live.insert(k);
        }
        let initial = live.clone();
        let mut touched = std::collections::BTreeSet::new();
        let mut seen = Vec::new();
        let mut from = Vec::new();
        loop {
            let page = art.scan_keys(b"", &from, 13, 40, |_, _| true);
            seen.extend(page.iter().map(<[u8]>::to_vec));
            let Some(n) = page.next else { break };
            from = n;
            for _ in 0..20 {
                let k = key(&mut rnd);
                if rnd() % 2 == 0 {
                    art.set(SharedByte::from_slice(&k), Value::String(SharedByte::from_str("w")));
                    live.insert(k.clone());
                } else {
                    art.del(&k);
                    live.remove(&k);
                }
                touched.insert(k);
            }
        }
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "ordre strict, donc sans doublon");
        for k in initial.difference(&touched) {
            assert!(seen.binary_search(k).is_ok(), "clé stable manquante : {k:?}");
        }
    }
    /// Fan-outs qui forcent les quatre formes de nœud (≤ 8, ≤ 16, ≤ 32,
    /// jusqu'à 256 entrées) : pages dans les deux sens, `from` présent,
    /// absent, entre deux clés, comparées à une BTreeMap. Couvre le suivi
    /// pas à pas des radix des formes larges et le passage nœud → page.
    #[test]
    fn krange_all_node_shapes_match_btreemap() {
        use crate::node::Kind;
        let mut rnd = xorshift(0xA076_1D64_78BD_642F);
        for fanout in [5usize, 12, 20, 30, 200, 256] {
            let mut art = OxidArt::new();
            let mut model = BTreeMap::new();
            // 2 niveaux de fan-out `fanout` (octets étalés sur 0..=255) sous
            // un préfixe, quelques clés plus longues, et une valeur sur un nœud.
            let byte = |i: usize| (i * 255 / fanout.max(2).saturating_sub(1).max(1)).min(255) as u8;
            for a in 0..fanout {
                for b in 0..(fanout.min(40)) {
                    if rnd() % 3 == 0 {
                        continue;
                    }
                    let mut k = vec![b'p', byte(a), byte(b)];
                    if rnd() % 5 == 0 {
                        k.extend_from_slice(b"-suffixe-long-sur-le-tas");
                    }
                    let v = format!("{a}-{b}").into_bytes();
                    art.set(SharedByte::from_slice(&k), Value::String(SharedByte::from_slice(&v)));
                    model.insert(k, v);
                }
            }
            let mut kinds = std::collections::BTreeSet::new();
            let mut todo = vec![art.root_idx];
            while let Some(idx) = todo.pop() {
                let node = art.node(idx);
                kinds.insert(node.kind() as u8);
                todo.extend(node.iter().filter_map(|(_, e)| match e {
                    crate::node::EntryRef::Child(c) => Some(c),
                    _ => None,
                }));
            }
            let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
            for t in 0..300 {
                let count = [1, 3, 17, 64, 1000][t % 5];
                let from: Vec<u8> = match t % 4 {
                    0 => Vec::new(),
                    1 => keys[rnd() as usize % keys.len()].clone(),
                    2 => {
                        let mut k = keys[rnd() as usize % keys.len()].clone();
                        k.push(0);
                        k
                    }
                    _ => vec![b'p', (rnd() % 256) as u8],
                };
                for rev in [false, true] {
                    let got: Vec<(Vec<u8>, Vec<u8>)> =
                        art.krange(b"p", &from, count, rev).iter().map(|(k, v)| (k.to_vec(), as_bytes(v))).collect();
                    assert_eq!(got, expected(&model, b"p", &from, count, rev), "fanout {fanout}, from {from:?}, count {count}, rev {rev}");
                }
            }
            if fanout >= 200 {
                assert!(kinds.contains(&(Kind::Wide256 as u8)), "fanout {fanout} : formes {kinds:?}");
            }
            if fanout == 20 || fanout == 30 {
                assert!(kinds.contains(&(Kind::Wide32 as u8)), "fanout {fanout} : formes {kinds:?}");
            }
            if fanout == 12 {
                assert!(kinds.contains(&(Kind::Overflow8 as u8)), "fanout {fanout} : formes {kinds:?}");
            }
        }
    }
}
