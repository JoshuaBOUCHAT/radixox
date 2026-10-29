//! Index membre → score de la grande forme du ZSet (ANALYSE_ART_UNIFIE.md §13.4) : pas
//! d'ordre, comme le dict de Redis, mais **sans table qui se redimensionne** (§5
//! d'ARCHITECTURE.md : pas de rehash O(n)).
//!
//! Trie sur le hash du membre, 4 bits par niveau (du haut vers le bas), à seaux :
//! - seau (128 o) : jusqu'à [`B`] entrées `{poignée u32, score u64}` + un octet de contrôle
//!   (empreinte du hash) ;
//! - répertoire : 16 cases ; une case pointe vers un seau ou un répertoire ; un seau peut
//!   couvrir une plage alignée de `2^span` cases (hachage extensible à l'intérieur du
//!   répertoire).
//!
//! Un seau plein se coupe en deux (sa plage aussi) ; s'il ne couvre qu'une case, il devient
//! un répertoire du niveau suivant. Le travail d'une insertion est borné (un seau, un
//! répertoire), jamais proportionnel à l'index. Après les 16 niveaux (64 bits identiques),
//! les seaux se chaînent. À la suppression, deux seaux jumeaux peu remplis fusionnent ; un
//! répertoire réduit à un seul seau disparaît.
//!
//! L'index ne connaît pas les membres : la recherche prend `eq(poignée)` (octets comparés
//! via la table des poignées), la coupe `rehash(poignée)` (hash recalculé du membre).

#[allow(unused_imports)]
use crate::prelude::*;
use radixox_lib::hislab::HiSlab;

/// Entrées par seau.
pub const B: usize = 9;
const FANOUT: usize = 16;
const LEVELS: usize = 16;
const NONE: u32 = u32::MAX;
/// Marque d'un répertoire dans le premier octet du nœud (celui d'un seau = `n` ≤ [`B`]).
const DIR: u8 = 0xFF;

#[derive(Clone, Copy)]
#[repr(C)]
struct Bucket {
    n: u8,
    ctrl: [u8; B],
    /// Seau suivant de la chaîne (au-delà des 16 niveaux seulement).
    next: u32,
    handles: [u32; B],
    /// Répertoire parent, seau précédent d'une chaîne, ou `NONE` (racine) : compactage.
    /// Dans les 4 octets que l'alignement des scores laissait libres.
    parent: u32,
    scores: [u64; B],
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Dir {
    tag: u8,
    /// Log2 de la longueur de la plage de chaque case.
    span: [u8; FANOUT],
    child: [u32; FANOUT],
    /// Répertoire parent, ou `NONE` (racine) : compactage.
    parent: u32,
}

#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub union INode {
    b: Bucket,
    d: Dir,
}

const _: () = assert!(size_of::<INode>() == 128);

static mut SLAB: Option<HiSlab<INode>> = None;

/// Accès à chaque nœud : toujours inliné. L'initialisation reste hors du chemin chaud
/// (avec elle dedans, l'accesseur n'était pas inliné : un vrai appel et un cadre de pile
/// de 248 o par nœud lu, ~13 % d'un ZADD).
#[inline(always)]
fn slab() -> &'static mut HiSlab<INode> {
    #[allow(static_mut_refs)]
    unsafe {
        match SLAB.as_mut() {
            Some(s) => s,
            None => init_slab(),
        }
    }
}

#[cold]
#[inline(never)]
fn init_slab() -> &'static mut HiSlab<INode> {
    #[allow(static_mut_refs)]
    unsafe {
        SLAB.insert(HiSlab::new(0, 1 << 26).expect("slab de l'index des ZSet"))
    }
}

/// Slab de l'index des ZSet : `(octets touchés, octets vivants, octets résidents)` ; zéros tant qu'il
/// n'existe pas.
pub(crate) fn slab_mem() -> (u64, u64, u64) {
    #[allow(static_mut_refs)]
    unsafe { SLAB.as_ref() }.map_or((0, 0, 0), crate::value::hislab_mem)
}

/// Voir [`crate::value::hislab_usage`].
pub(crate) fn slab_usage() -> (u64, u64) {
    #[allow(static_mut_refs)]
    unsafe { SLAB.as_ref() }.map_or((0, 0), crate::value::hislab_usage)
}

/// Voir [`crate::value::trim_tail`] ; 0 tant que la slab n'existe pas.
pub(crate) fn trim_tail() -> usize {
    #[allow(static_mut_refs)]
    unsafe { SLAB.as_ref() }.map_or(0, crate::value::trim_tail)
}

#[inline]
fn node(i: u32) -> &'static mut INode {
    unsafe { slab().get_unchecked_mut(i) }
}

#[inline]
fn is_dir(i: u32) -> bool {
    unsafe { node(i).d.tag == DIR }
}

#[inline]
fn bucket(i: u32) -> &'static mut Bucket {
    debug_assert!(!is_dir(i));
    unsafe { &mut node(i).b }
}

#[inline]
fn dir(i: u32) -> &'static mut Dir {
    debug_assert!(is_dir(i));
    unsafe { &mut node(i).d }
}

fn new_bucket() -> u32 {
    slab().insert(INode { b: Bucket { n: 0, ctrl: [0; B], next: NONE, handles: [0; B], parent: NONE, scores: [0; B] } })
}

fn new_dir(child: u32, span: u8) -> u32 {
    slab().insert(INode { d: Dir { tag: DIR, span: [span; FANOUT], child: [child; FANOUT], parent: NONE } })
}

/// Parent du nœud `i` (répertoire ou seau).
#[inline]
fn parent(i: u32) -> u32 {
    if is_dir(i) { dir(i).parent } else { bucket(i).parent }
}

#[inline]
fn set_parent(i: u32, p: u32) {
    if is_dir(i) { dir(i).parent = p } else { bucket(i).parent = p }
}

fn free_node(i: u32) {
    slab().remove(i);
}

/// Préchargement d'un nœud (chaînes AMAC).
#[inline]
pub fn prefetch(i: u32) {
    let p = slab().slot_ptr(i) as *const u8;
    radixox_lib::prefetch::prefetch_l1(p);
    radixox_lib::prefetch::prefetch_l1(unsafe { p.add(64) });
}

/// Nœuds de la slab (mesures).
pub fn slab_nodes() -> u32 {
    slab().len()
}

#[inline]
fn nibble(h: u64, level: usize) -> usize {
    ((h >> (60 - 4 * level)) & 0xF) as usize
}

#[inline]
fn fp(h: u64) -> u8 {
    h as u8
}

/// Emplacement d'une entrée : seau et rang.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub node: u32,
    pub i: u8,
}

/// En-tête de l'index (dans l'en-tête du ZSet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Index {
    root: u32,
    len: u32,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

/// Chemin d'une descente : `(répertoire, case)` par niveau.
type Path = [(u32, u8); LEVELS];

impl Index {
    pub const fn new() -> Self {
        Self { root: NONE, len: 0 }
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn root(&self) -> u32 {
        self.root
    }

    /// Seau de tête pour `h`, et la profondeur ; remplit `path`.
    #[inline]
    fn descend(&self, h: u64, path: &mut Path) -> (u32, usize) {
        let mut i = self.root;
        let mut level = 0;
        while is_dir(i) {
            let s = nibble(h, level);
            path[level] = (i, s as u8);
            i = dir(i).child[s];
            level += 1;
        }
        (i, level)
    }

    /// Un pas de descente pour les chaînes AMAC : du nœud `i` au niveau `level` vers le
    /// suivant ; `None` : `i` est un seau.
    #[inline]
    pub fn probe_step(i: u32, h: u64, level: usize) -> Option<u32> {
        is_dir(i).then(|| dir(i).child[nibble(h, level)])
    }

    /// Empreinte rangée dans le seau pour le hash `h`.
    #[inline]
    pub fn fingerprint(h: u64) -> u8 {
        fp(h)
    }

    /// Chaînes AMAC, au seau : premier candidat d'empreinte `f` à partir du rang `from`
    /// (maillon `b` seulement) : `(rang, poignée, score)`.
    #[inline]
    pub fn candidate(b: u32, f: u8, from: usize) -> Option<(usize, u32, u64)> {
        let bk = bucket(b);
        (from..bk.n as usize).find(|&i| bk.ctrl[i] == f).map(|i| (i, bk.handles[i], bk.scores[i]))
    }

    /// Entrée du membre de hash `h` (`eq` : la poignée désigne-t-elle ce membre ?).
    #[inline]
    pub fn find(&self, h: u64, eq: impl Fn(u32) -> bool) -> Option<Loc> {
        self.find_by(h, |x, _| eq(x))
    }

    /// Comme [`find`](Self::find), `eq(poignée, score)` (validation des entrées périmées
    /// après un retrait de plage, `ZLarge`).
    #[inline]
    pub fn find_by(&self, h: u64, eq: impl Fn(u32, u64) -> bool) -> Option<Loc> {
        if self.root == NONE {
            return None;
        }
        let mut b = self.root;
        let mut level = 0;
        while is_dir(b) {
            b = dir(b).child[nibble(h, level)];
            level += 1;
        }
        let f = fp(h);
        loop {
            let bk = bucket(b);
            for i in 0..bk.n as usize {
                if bk.ctrl[i] == f && eq(bk.handles[i], bk.scores[i]) {
                    return Some(Loc { node: b, i: i as u8 });
                }
            }
            if bk.next == NONE {
                return None;
            }
            b = bk.next;
        }
    }

    #[inline]
    pub fn entry(loc: Loc) -> (u32, u64) {
        let bk = bucket(loc.node);
        (bk.handles[loc.i as usize], bk.scores[loc.i as usize])
    }

    #[inline]
    pub fn set_score(loc: Loc, s: u64) {
        bucket(loc.node).scores[loc.i as usize] = s;
    }

    /// Ajoute `(poignée, score)` de hash `h`, membre absent.
    pub fn insert(&mut self, h: u64, handle: u32, score: u64, rehash: impl Fn(u32) -> u64) {
        self.len += 1;
        if self.root == NONE {
            self.root = new_bucket();
        }
        let mut path: Path = [(NONE, 0); LEVELS];
        loop {
            let (b, level) = self.descend(h, &mut path);
            if level == LEVELS {
                Self::chain_push(b, h, handle, score);
                return;
            }
            let bk = bucket(b);
            if (bk.n as usize) < B {
                let n = bk.n as usize;
                bk.ctrl[n] = fp(h);
                bk.handles[n] = handle;
                bk.scores[n] = score;
                bk.n += 1;
                return;
            }
            // Seau plein : un seau d'une seule case devient un répertoire, puis on coupe.
            let (d, dl, s) = if level == 0 {
                self.root = new_dir(b, 4);
                set_parent(b, self.root);
                (self.root, 0, nibble(h, 0))
            } else {
                let (p, ps) = path[level - 1];
                if dir(p).span[ps as usize] == 0 {
                    let d = new_dir(b, 4);
                    set_parent(d, p);
                    set_parent(b, d);
                    dir(p).child[ps as usize] = d;
                    (d, level, nibble(h, level))
                } else {
                    (p, level - 1, ps as usize)
                }
            };
            Self::split(d, s, dl, &rehash);
        }
    }

    /// Coupe le seau de la case `s` du répertoire `d` (niveau `dl`) : sa plage de `2^k`
    /// cases se divise, la moitié haute va à un seau neuf.
    fn split(d: u32, s: usize, dl: usize, rehash: &impl Fn(u32) -> u64) {
        let k = dir(d).span[s] as usize;
        debug_assert!(k > 0);
        let len = 1 << k;
        let base = s & !(len - 1);
        let half = len / 2;
        let old = dir(d).child[s];
        let nb = new_bucket();
        set_parent(nb, d);
        {
            let dr = dir(d);
            for c in base..base + len {
                dr.span[c] = (k - 1) as u8;
            }
            for c in base + half..base + len {
                dr.child[c] = nb;
            }
        }
        let src = *bucket(old);
        let (lo, hi) = (bucket(old) as *mut Bucket, bucket(nb) as *mut Bucket);
        unsafe {
            (*lo).n = 0;
            for i in 0..src.n as usize {
                let dst = if nibble(rehash(src.handles[i]), dl) & half != 0 { &mut *hi } else { &mut *lo };
                let n = dst.n as usize;
                dst.ctrl[n] = src.ctrl[i];
                dst.handles[n] = src.handles[i];
                dst.scores[n] = src.scores[i];
                dst.n += 1;
            }
        }
    }

    /// Ajout au-delà des 16 niveaux : premier maillon de la chaîne qui a de la place.
    fn chain_push(mut b: u32, h: u64, handle: u32, score: u64) {
        loop {
            let bk = bucket(b);
            if (bk.n as usize) < B {
                let n = bk.n as usize;
                bk.ctrl[n] = fp(h);
                bk.handles[n] = handle;
                bk.scores[n] = score;
                bk.n += 1;
                return;
            }
            if bk.next == NONE {
                let nb = new_bucket();
                set_parent(nb, b);
                bucket(b).next = nb;
            }
            b = bucket(b).next;
        }
    }

    /// Retire le membre de hash `h` ; rend `(poignée, score)`.
    pub fn remove(&mut self, h: u64, eq: impl Fn(u32) -> bool) -> Option<(u32, u64)> {
        self.remove_by(h, |x, _| eq(x))
    }

    /// Comme [`remove`](Self::remove), `eq(poignée, score)`.
    pub fn remove_by(&mut self, h: u64, eq: impl Fn(u32, u64) -> bool) -> Option<(u32, u64)> {
        if self.root == NONE {
            return None;
        }
        let mut path: Path = [(NONE, 0); LEVELS];
        let (head, level) = self.descend(h, &mut path);
        let f = fp(h);
        // maillon et rang
        let mut b = head;
        let at = loop {
            let bk = bucket(b);
            if let Some(i) = (0..bk.n as usize).find(|&i| bk.ctrl[i] == f && eq(bk.handles[i], bk.scores[i])) {
                break i;
            }
            if bk.next == NONE {
                return None;
            }
            b = bk.next;
        };
        let out = (bucket(b).handles[at], bucket(b).scores[at]);
        self.len -= 1;
        // Le trou prend la dernière entrée du dernier maillon.
        let (mut tail, mut before) = (head, NONE);
        while bucket(tail).next != NONE {
            before = tail;
            tail = bucket(tail).next;
        }
        let tb = *bucket(tail);
        let last = tb.n as usize - 1;
        let bk = bucket(b);
        bk.ctrl[at] = tb.ctrl[last];
        bk.handles[at] = tb.handles[last];
        bk.scores[at] = tb.scores[last];
        bucket(tail).n -= 1;
        if bucket(tail).n == 0 && before != NONE {
            free_node(tail);
            bucket(before).next = NONE;
        }
        if level < LEVELS || bucket(head).next == NONE {
            self.shrink(path, head, level);
        }
        Some(out)
    }

    /// Après un retrait dans le seau `b` (niveau `level`) : fusions de jumeaux, répertoires
    /// réduits à un seul seau retirés (le seau prend leur case, puis peut fusionner au
    /// niveau du dessus), index vide.
    fn shrink(&mut self, path: Path, b: u32, mut level: usize) {
        loop {
            if level == 0 {
                if bucket(b).n == 0 {
                    free_node(b);
                    self.root = NONE;
                }
                return;
            }
            let (d, s) = path[level - 1];
            let s = s as usize;
            loop {
                let dr = dir(d);
                let k = dr.span[s] as usize;
                if k == 4 {
                    break;
                }
                let len = 1 << k;
                let buddy = (s & !(len - 1)) ^ len;
                let other = dr.child[buddy];
                if dr.span[buddy] as usize != k || is_dir(other) {
                    break;
                }
                let (ba, bb) = (*bucket(b), *bucket(other));
                let (na, nb) = (ba.n as usize, bb.n as usize);
                if ba.next != NONE || bb.next != NONE || (na + nb > B / 2 && na != 0 && nb != 0) {
                    break;
                }
                let bk = bucket(b);
                for i in 0..nb {
                    bk.ctrl[na + i] = bb.ctrl[i];
                    bk.handles[na + i] = bb.handles[i];
                    bk.scores[na + i] = bb.scores[i];
                }
                bk.n += nb as u8;
                free_node(other);
                let dr = dir(d);
                let lo = (s & !(len - 1)).min(buddy);
                for c in lo..lo + 2 * len {
                    dr.span[c] = (k + 1) as u8;
                    dr.child[c] = b;
                }
            }
            if dir(d).span[s] != 4 {
                return;
            }
            // répertoire réduit à un seul seau
            free_node(d);
            if level == 1 {
                self.root = b;
                set_parent(b, NONE);
            } else {
                let (p, ps) = path[level - 2];
                dir(p).child[ps as usize] = b;
                set_parent(b, p);
            }
            level -= 1;
        }
    }

    /// Toutes les entrées `(poignée, score)`, chaque nœud une fois.
    pub fn for_each(&self, mut f: impl FnMut(u32, u64)) {
        fn rec(i: u32, f: &mut impl FnMut(u32, u64)) {
            if is_dir(i) {
                let dr = *dir(i);
                let mut s = 0;
                while s < FANOUT {
                    rec(dr.child[s], f);
                    s += 1 << dr.span[s];
                }
                return;
            }
            let mut b = i;
            while b != NONE {
                let bk = bucket(b);
                for j in 0..bk.n as usize {
                    f(bk.handles[j], bk.scores[j]);
                }
                b = bk.next;
            }
        }
        if self.root != NONE {
            rec(self.root, &mut f);
        }
    }

    /// Rend tous les nœuds.
    pub fn free_all(&mut self) {
        fn rec(i: u32) {
            if is_dir(i) {
                let dr = *dir(i);
                let mut s = 0;
                while s < FANOUT {
                    rec(dr.child[s]);
                    s += 1 << dr.span[s];
                }
            } else {
                let mut b = bucket(i).next;
                while b != NONE {
                    let next = bucket(b).next;
                    free_node(b);
                    b = next;
                }
            }
            free_node(i);
        }
        if self.root != NONE {
            rec(self.root);
        }
        *self = Self::new();
    }

    /// Libération reprenable : `stack` part de [`take_for_free`](Self::take_for_free) ; un
    /// nœud par unité de `budget`. Pile vide = fini.
    pub(crate) fn free_step(stack: &mut Vec<u32>, budget: &mut u32) {
        while *budget > 0
            && let Some(i) = stack.pop()
        {
            if is_dir(i) {
                let dr = *dir(i);
                let mut s = 0;
                while s < FANOUT {
                    stack.push(dr.child[s]);
                    s += 1 << dr.span[s];
                }
            } else if bucket(i).next != NONE {
                stack.push(bucket(i).next);
            }
            free_node(i);
            *budget -= 1;
        }
    }

    /// Pile de départ de [`free_step`](Self::free_step) ; l'index devient vide.
    pub(crate) fn take_for_free(&mut self) -> Vec<u32> {
        let stack = if self.root == NONE { Vec::new() } else { vec![self.root] };
        *self = Self::new();
        stack
    }

    /// `(seaux, répertoires)` (mesures).
    pub fn node_counts(&self) -> (u32, u32) {
        let mut acc = (0, 0);
        fn rec(i: u32, acc: &mut (u32, u32)) {
            if is_dir(i) {
                acc.1 += 1;
                let dr = *dir(i);
                let mut s = 0;
                while s < FANOUT {
                    rec(dr.child[s], acc);
                    s += 1 << dr.span[s];
                }
            } else {
                let mut b = i;
                while b != NONE {
                    acc.0 += 1;
                    b = bucket(b).next;
                }
            }
        }
        if self.root != NONE {
            rec(self.root, &mut acc);
        }
        acc
    }

    /// Vérificateur : plages alignées et homogènes, empreintes, chaînes seulement au
    /// dernier niveau, `len`, et chaque entrée retrouvée par son hash (routage).
    pub fn verify(&self, rehash: impl Fn(u32) -> u64) -> Result<(), String> {
        fn rec(i: u32, up: u32, level: usize, rehash: &impl Fn(u32) -> u64, n: &mut u32) -> Result<(), String> {
            if parent(i) != up {
                return Err(format!("nœud {i} : parent {} au lieu de {up}", parent(i)));
            }
            if is_dir(i) {
                if level == LEVELS {
                    return Err(format!("répertoire {i} au-delà du dernier niveau"));
                }
                let dr = *dir(i);
                let mut s = 0;
                while s < FANOUT {
                    let k = dr.span[s] as usize;
                    let len = 1 << k;
                    if k > 4 || s % len != 0 {
                        return Err(format!("répertoire {i} : plage {s} de 2^{k} mal alignée"));
                    }
                    for c in s..s + len {
                        if dr.child[c] != dr.child[s] || dr.span[c] as usize != k {
                            return Err(format!("répertoire {i} : plage {s} hétérogène"));
                        }
                    }
                    if is_dir(dr.child[s]) && k != 0 {
                        return Err(format!("répertoire {i} : sous-répertoire sur une plage de 2^{k}"));
                    }
                    rec(dr.child[s], i, level + 1, rehash, n)?;
                    s += len;
                }
                return Ok(());
            }
            let mut b = i;
            let mut link = 0;
            let mut prev = up;
            while b != NONE {
                let bk = *bucket(b);
                if link > 0 && bk.parent != prev {
                    return Err(format!("seau chaîné {b} : parent {} au lieu de {prev}", bk.parent));
                }
                prev = b;
                if bk.n as usize > B || (link > 0 && level < LEVELS) {
                    return Err(format!("seau {b} : n = {} ou chaîne au niveau {level}", bk.n));
                }
                for j in 0..bk.n as usize {
                    if fp(rehash(bk.handles[j])) != bk.ctrl[j] {
                        return Err(format!("seau {b} : empreinte de l'entrée {j}"));
                    }
                }
                *n += bk.n as u32;
                b = bk.next;
                link += 1;
            }
            Ok(())
        }
        if self.root == NONE {
            return if self.len == 0 { Ok(()) } else { Err("index vide avec len > 0".into()) };
        }
        let mut n = 0;
        rec(self.root, NONE, 0, &rehash, &mut n)?;
        if n != self.len {
            return Err(format!("len {} mais {n} entrées", self.len));
        }
        let mut err = None;
        self.for_each(|handle, _| {
            if err.is_none() && self.find(rehash(handle), |x| x == handle).is_none() {
                err = Some(format!("poignée {handle} introuvable par son hash"));
            }
        });
        err.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    #[allow(unused_imports)]
    use std::{dbg, eprintln, print, println, thread_local};
    use super::*;
    use std::collections::HashMap;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Hash de test : `mask` réduit l'entropie (collisions jusqu'aux 64 bits pour
    /// éprouver les chaînes).
    fn hasher(mask: u64) -> impl Fn(u32) -> u64 {
        move |h: u32| {
            let mut x = (h as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 29;
            x.wrapping_mul(0xBF58_476D_1CE4_E5B9) & mask
        }
    }

    #[test]
    fn random_against_hashmap() {
        for mask in [u64::MAX, 0xF000_0000_0000_00FF, 0x3] {
            let rehash = hasher(mask);
            let mut rng = Rng(0xD1B5_4A32_D192_ED03 ^ mask);
            let mut idx = Index::new();
            let mut model: HashMap<u32, u64> = HashMap::new();
            let mut keys: Vec<u32> = Vec::new();
            let mut next = 0u32;
            for step in 0..40_000 {
                let op = rng.below(10);
                if keys.is_empty() || op < 5 {
                    let h = next;
                    next += 1;
                    let s = rng.next();
                    assert!(idx.find(rehash(h), |x| x == h).is_none());
                    idx.insert(rehash(h), h, s, &rehash);
                    model.insert(h, s);
                    keys.push(h);
                } else if op < 8 {
                    let i = rng.below(keys.len() as u64) as usize;
                    let h = keys.swap_remove(i);
                    assert_eq!(idx.remove(rehash(h), |x| x == h), Some((h, model.remove(&h).unwrap())));
                    assert_eq!(idx.remove(rehash(h), |x| x == h), None);
                } else {
                    let h = keys[rng.below(keys.len() as u64) as usize];
                    let loc = idx.find(rehash(h), |x| x == h).unwrap();
                    assert_eq!(Index::entry(loc), (h, model[&h]));
                    let s = rng.next();
                    Index::set_score(loc, s);
                    model.insert(h, s);
                }
                if step % 1999 == 0 || (mask == 0x3 && step % 97 == 0) {
                    idx.verify(&rehash).unwrap();
                    let mut seen = HashMap::new();
                    idx.for_each(|h, s| {
                        seen.insert(h, s);
                    });
                    assert_eq!(seen, model);
                }
                // phases de vidage massif : les fusions et les retraits de répertoires
                if step % 10_000 == 9_999 {
                    while keys.len() > 50 {
                        let h = keys.pop().unwrap();
                        assert!(idx.remove(rehash(h), |x| x == h).is_some());
                        model.remove(&h);
                    }
                    idx.verify(&rehash).unwrap();
                    if mask == u64::MAX {
                        let (buckets, dirs) = idx.node_counts();
                        assert!(buckets + dirs <= 50, "{buckets} seaux, {dirs} répertoires pour 50 entrées");
                    }
                }
            }
            for h in keys.drain(..) {
                assert!(idx.remove(rehash(h), |x| x == h).is_some());
            }
            assert!(idx.is_empty());
            assert_eq!(idx.root(), NONE, "tout rendu");
        }
    }

    /// Remplissage des seaux sur des hash uniformes (~69 % attendu).
    #[test]
    fn bucket_fill() {
        let rehash = hasher(u64::MAX);
        let mut idx = Index::new();
        let n = 200_000u32;
        for h in 0..n {
            idx.insert(rehash(h), h, h as u64, &rehash);
        }
        idx.verify(&rehash).unwrap();
        let (buckets, dirs) = idx.node_counts();
        let fill = n as f64 / (buckets as f64 * B as f64);
        let per = (buckets + dirs) as f64 * 128.0 / n as f64;
        assert!(fill > 0.6, "remplissage {fill}");
        eprintln!("index : remplissage {fill:.3}, {buckets} seaux, {dirs} répertoires, {per:.1} o/entrée");
        assert!(per < 25.0, "{per} o/entrée");
        idx.free_all();
    }
}

/// Compactage de la slab de l'index (comme celle du B+tree) : les nœuds les plus hauts
/// descendent dans les trous les plus bas, puis la fin de la slab est rendue. Chaque nœud
/// connaît son parent : un répertoire (toutes les cases qui pointent le nœud sont
/// corrigées : un seau couvre une plage de cases), le seau précédent d'une chaîne (son
/// `next`), ou l'en-tête du ZSet pour la racine (table racine → en-tête, revérifiée). Les
/// enfants d'un répertoire et le maillon suivant d'un seau reçoivent le nouveau parent.
/// Une référence qui ne se vérifie pas (index décroché) : le nœud ne bouge pas.
///
/// Interdit pendant une save et tant que des libérations de ZSet attendent : la passe
/// s'arrête d'elle-même dans ce cas.
pub mod compact {
    use hashbrown::HashMap;

    use super::{FANOUT, INode, NONE, bucket, dir, is_dir, parent, set_parent, slab};
    use crate::zset::ZSet;

    const PAGE_NODES: u32 = ((2 << 20) / size_of::<INode>()) as u32;
    /// Coût fixe d'un déplacement (copie + références), en lignes de cache.
    const MOVE_COST: u32 = 6;

    #[derive(Debug)]
    pub struct ICompaction {
        target: u32,
        keep: u32,
        scan: u32,
        pub moved: u64,
        /// Racine de l'index → idx de l'en-tête du ZSet.
        roots: HashMap<u32, u32>,
    }

    pub fn holes_bytes() -> u64 {
        let s = slab();
        let span = s.last_occupied().map_or(0, |l| l + 1);
        (span - s.len()) as u64 * size_of::<INode>() as u64
    }

    fn roots() -> HashMap<u32, u32> {
        let mut m = HashMap::new();
        crate::value::zset_slab_mut().for_each_occupied(|idx, z| {
            if let ZSet::Large(l) = z
                && l.index.root != NONE
            {
                m.insert(l.index.root, idx);
            }
        });
        m
    }

    fn blocked() -> bool {
        crate::zset::save::is_saving() || crate::zset::lazy::pending() > 0
    }

    pub fn want(force: bool) -> Option<ICompaction> {
        if blocked() {
            return None;
        }
        let s = slab();
        let span = s.last_occupied()? + 1;
        let slab_bytes = span as u64 * size_of::<INode>() as u64;
        let threshold = if slab_bytes > 256 << 20 { 4 << 20 } else { 3 << 20 };
        if !force && holes_bytes() <= threshold {
            return None;
        }
        let keep = if force { 0 } else { PAGE_NODES };
        let target = if force { s.len() } else { (s.len() + keep).next_multiple_of(PAGE_NODES) };
        (target < span).then(|| ICompaction { target, keep, scan: span, moved: 0, roots: roots() })
    }

    fn header_of(c: &mut ICompaction, root: u32) -> Option<&'static mut super::Index> {
        let mut hit = c.roots.get(&root).copied();
        if hit.is_none() {
            c.roots = roots();
            hit = c.roots.get(&root).copied();
        }
        match crate::value::zset_slab_mut().get_mut(hit?)? {
            ZSet::Large(l) if l.index.root == root => Some(&mut l.index),
            _ => None,
        }
    }

    /// La référence vers `src` existe-t-elle bien (parent qui la tient) ?
    fn referenced(c: &mut ICompaction, src: u32, up: u32) -> bool {
        if up == NONE {
            return header_of(c, src).is_some();
        }
        if is_dir(up) {
            dir(up).child.contains(&src)
        } else {
            bucket(up).next == src
        }
    }

    pub fn step(c: &mut ICompaction, budget: u32) -> Option<(u64, usize)> {
        if blocked() {
            return Some((c.moved, 0));
        }
        let mut spent = 0;
        while spent < budget {
            let Some(src) = slab().prev_occupied(c.scan).filter(|&s| s >= c.target) else { return Some(finish(c)) };
            c.scan = src;
            let up = parent(src);
            if !referenced(c, src, up) {
                spent += 1;
                continue;
            }
            // SAFETY: rempli par `move_to` tout de suite, ou rendu.
            let dst = unsafe { slab().reserve_lowest() };
            if dst >= src || dst >= c.target {
                unsafe { slab().unreserve(dst) };
                return Some(finish(c));
            }
            // SAFETY: `src` occupé, `dst` réservé, aucune référence vivante dans la slab.
            unsafe { slab().move_to(src, dst) };
            let mut fixed = 1;
            if up == NONE {
                header_of(c, src).expect("en-tête vérifié").root = dst;
                let z = c.roots.remove(&src).expect("racine connue");
                c.roots.insert(dst, z);
            } else if is_dir(up) {
                for ch in dir(up).child.iter_mut().filter(|ch| **ch == src) {
                    *ch = dst;
                }
            } else {
                bucket(up).next = dst;
            }
            if is_dir(dst) {
                let dr = *dir(dst);
                let mut s = 0;
                while s < FANOUT {
                    set_parent(dr.child[s], dst);
                    fixed += 1;
                    s += 1 << dr.span[s];
                }
            } else if bucket(dst).next != NONE {
                set_parent(bucket(dst).next, dst);
                fixed += 1;
            }
            c.moved += 1;
            spent += MOVE_COST + fixed;
        }
        None
    }

    fn finish(c: &ICompaction) -> (u64, usize) {
        let s = slab();
        let last = s.last_occupied().map_or(0, |l| l + 1);
        let from = last.max(s.len() + c.keep);
        // SAFETY: rien n'est occupé à partir de `last` ≤ `from`.
        (c.moved, unsafe { s.release_above(from) })
    }
}
