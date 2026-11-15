//! SSCAN / HSCAN : parcours ordonné d'un Set ou d'un Hash, repris strictement après une
//! position (SCAN_DESIGN.md, « HSCAN / SSCAN »).
//!
//! # Ordre
//!
//! Le trie range une entrée au premier slot libre du chemin de son hash : sa place
//! dépend de l'histoire (une suppression remonte une feuille). L'ordre du parcours ne
//! dépend donc pas des places mais du membre seul : **clé d'ordre** = le hash 64 bits lu
//! dans l'ordre de la descente (`h0`, l'octet du niveau 0, en poids fort :
//! `h.swap_bytes()`), puis les octets du membre pour départager deux hash égaux. Ordre
//! total, fixe pour un membre : une page reprend strictement après la dernière entrée
//! examinée, sans doublon ni oubli d'une entrée présente tout le long, quoi qu'il arrive
//! au trie entre deux pages (comme SCAN, qui reprend après la dernière clé).
//!
//! # Parcours
//!
//! Tout le sous-arbre d'un slot de radix `r` au niveau `d` partage les octets `h0..=hd`
//! du chemin ; l'entrée du slot lui-même aussi, mais ses octets suivants sont quelconques.
//! Dans un nœud, les slots sont pris par radix croissant (ils ne sont pas rangés : un
//! bitmap 256 bits des radix présents et une table radix → rang, construits en une passe
//! et lus à la demande, voir [`Order`]) ; l'entrée d'un slot à enfant **flotte** dans le
//! sous-arbre jusqu'à sa place (au plus 7 flottantes, une par niveau). Un groupe de radix
//! avant la position de reprise est sauté sans y descendre.
//!
//! La clé d'ordre d'une entrée (le hash, recalculé : le nœud n'en garde que l'octet du
//! niveau et l'octet de contrôle) n'est calculée que là où le chemin ne suffit pas à
//! placer l'entrée : slot à enfant (elle flotte), groupe de plus d'une entrée, groupe sur
//! le chemin de la position de reprise. Une feuille seule à son radix hors de ce chemin,
//! le cas courant, sort sans hash ; l'appelant calcule la clé de la dernière entrée de la
//! page ([`ValTrees::scan_order_key`]). Les membres sont lus de toute façon (MATCH,
//! réponse) : ils sont préchargés `AHEAD` slots à l'avance, les nœuds enfants deux slots à
//! l'avance (puis leurs premiers membres un slot à l'avance), comme les deux étages du
//! parcours de SCAN (`oxidart/src/krange.rs`).

#[allow(unused_imports)]
use crate::prelude::*;

use radixox_lib::shared_byte::SharedByte;
use radixox_lib::small_vec::SmallVec;

use crate::common::{DISPATCH_LEVELS, Slot};
use crate::overflow::NO_CHILD;
use crate::{ValTrees, prefetch_slot};

/// Slots d'avance pour le préchargement des membres.
const AHEAD: usize = 8;

/// Entrée vue par le parcours.
#[derive(Clone, Copy)]
pub(crate) enum At {
    Node(u32, Slot),
    Bucket(u32, usize),
}

/// Ce que le parcours lit d'un Set ou d'un Hash.
trait Src {
    /// Radix inline puis radix du débordement, dans l'ordre de `slot_at`.
    fn radixes(&self, idx: u32) -> (&[u8], &[u8]);
    fn slot_at(&self, idx: u32, n: usize) -> Slot;
    fn child(&self, idx: u32, s: Slot) -> u32;
    fn key(&self, idx: u32, s: Slot) -> &SharedByte;
    fn bucket_len(&self, b: u32) -> usize;
    fn bucket_key(&self, b: u32, i: usize) -> &SharedByte;
    fn trees(&self) -> &ValTrees;
    /// Premiers membres d'un nœud (déjà arrivé en cache).
    fn prefetch_entries(&self, idx: u32);
}

struct SetSrc<'t>(&'t ValTrees);
struct HashSrc<'t>(&'t ValTrees);

impl Src for SetSrc<'_> {
    fn radixes(&self, idx: u32) -> (&[u8], &[u8]) {
        self.0.set_node(idx).radixes()
    }
    fn slot_at(&self, idx: u32, n: usize) -> Slot {
        self.0.set_node(idx).slot_at(n)
    }
    fn child(&self, idx: u32, s: Slot) -> u32 {
        self.0.set_node(idx).child(s)
    }
    fn key(&self, idx: u32, s: Slot) -> &SharedByte {
        self.0.set_node(idx).key(s)
    }
    fn bucket_len(&self, b: u32) -> usize {
        self.0.set_collisions.get(b).len()
    }
    fn bucket_key(&self, b: u32, i: usize) -> &SharedByte {
        &self.0.set_collisions.get(b)[i]
    }
    fn trees(&self) -> &ValTrees {
        self.0
    }
    fn prefetch_entries(&self, idx: u32) {
        self.0.set_node(idx).prefetch_entries();
    }
}

impl Src for HashSrc<'_> {
    fn radixes(&self, idx: u32) -> (&[u8], &[u8]) {
        self.0.hash_node(idx).radixes()
    }
    fn slot_at(&self, idx: u32, n: usize) -> Slot {
        self.0.hash_node(idx).slot_at(n)
    }
    fn child(&self, idx: u32, s: Slot) -> u32 {
        self.0.hash_node(idx).child(s)
    }
    fn key(&self, idx: u32, s: Slot) -> &SharedByte {
        self.0.hash_node(idx).key(s)
    }
    fn bucket_len(&self, b: u32) -> usize {
        self.0.hash_collisions.get(b).len()
    }
    fn bucket_key(&self, b: u32, i: usize) -> &SharedByte {
        &self.0.hash_collisions.get(b)[i].0
    }
    fn trees(&self) -> &ValTrees {
        self.0
    }
    fn prefetch_entries(&self, idx: u32) {
        self.0.hash_node(idx).prefetch_entries();
    }
}

/// Entrée flottante : clé d'ordre, membre, emplacement.
type Float<'t> = (u64, &'t [u8], At);
/// Un groupe : les flottantes (≤ 7) et l'entrée du slot ; plus seulement dans un bucket
/// de collision.
type Group<'t> = SmallVec<8, Float<'t>>;

/// Slots d'un nœud par radix croissant, lus à la demande : bitmap des radix présents et
/// table radix → rang (≤ 256 slots, un radix au plus une fois par nœud). Construite en
/// une passe sur les octets de radix, sans tri ni liste : une page qui reprend au milieu
/// d'un gros nœud n'en lit que les slots qu'elle consomme.
struct Order {
    bits: [u64; 4],
    rank: [u8; 256],
}

impl Order {
    #[inline]
    fn new((inline, overflow): (&[u8], &[u8])) -> Self {
        let mut o = Order { bits: [0; 4], rank: [0; 256] };
        for (i, &r) in inline.iter().chain(overflow).enumerate() {
            o.bits[(r >> 6) as usize] |= 1 << (r & 63);
            o.rank[r as usize] = i as u8;
        }
        o
    }

    /// Premier radix présent ≥ `from` (`from` ≤ 256).
    #[inline]
    fn next(&self, from: usize) -> Option<u8> {
        let mut w = from >> 6;
        if w >= 4 {
            return None;
        }
        let mut bits = self.bits[w] & (!0u64 << (from & 63));
        loop {
            if bits != 0 {
                return Some((w * 64 + bits.trailing_zeros() as usize) as u8);
            }
            w += 1;
            if w == 4 {
                return None;
            }
            bits = self.bits[w];
        }
    }

    #[inline]
    fn after(&self, r: Option<u8>) -> Option<u8> {
        r.and_then(|r| self.next(r as usize + 1))
    }
}

struct Walk<'t, 'f, S, F> {
    src: &'t S,
    /// Position de reprise (exclue).
    from: Option<(u64, &'f [u8])>,
    /// Rend `false` pour arrêter.
    f: F,
}

impl<'t, S: Src, F: FnMut(&'t [u8], At) -> bool> Walk<'t, '_, S, F> {
    fn order_key(&self, member: &[u8]) -> u64 {
        self.src.trees().member_hash(member).0.swap_bytes()
    }

    /// Émet les entrées d'un groupe, triées ; `check` : groupe sur le chemin de `from`,
    /// seules les entrées après `from` sortent (hors du chemin, tout le groupe est
    /// après). `false` : arrêt demandé.
    fn emit(&mut self, group: &mut [Float<'t>], check: bool) -> bool {
        if group.len() > 1 {
            group.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        }
        for &(k, m, at) in group.iter() {
            if check && self.from.is_some_and(|(fk, fm)| (k, m) <= (fk, fm)) {
                continue;
            }
            if !(self.f)(m, at) {
                return false;
            }
        }
        true
    }

    /// Nœud `idx` au niveau `depth` ; `float` : entrées des ancêtres dont le hash
    /// prolonge le chemin, triées ; `bounded` : le chemin est celui de `from`.
    ///
    /// La clé d'ordre (un foldhash) n'est calculée que là où elle sert : entrée d'un
    /// slot à enfant (elle flotte), groupe de plus d'une entrée, groupe sur le chemin de
    /// `from`. Une feuille seule à son radix hors de ce chemin a déjà sa place par le
    /// parcours : c'est le cas courant.
    fn node(&mut self, idx: u32, depth: usize, float: &[Float<'t>], bounded: bool) -> bool {
        let src = self.src;
        let byte = |k: u64| (k >> (8 * (7 - depth))) as u8;
        let bound = bounded.then(|| byte(self.from.expect("bounded ⇒ from").0));
        // les groupes avant `from` sont sautés d'emblée
        let lo = bound.unwrap_or(0);
        let ord = Order::new(src.radixes(idx));
        let slot = |r: u8| src.slot_at(idx, ord.rank[r as usize] as usize);
        // `cur` : prochain slot ; `next` : celui d'après ; `ahead` : prochain membre à
        // précharger, AHEAD slots devant `cur`
        let mut cur = ord.next(lo as usize);
        let mut next = ord.after(cur);
        let mut ahead = cur;
        for _ in 0..AHEAD {
            let Some(r) = ahead else { break };
            src.key(idx, slot(r)).prefetch();
            ahead = ord.after(ahead);
        }
        let mut j = float.partition_point(|e| byte(e.0) < lo);
        let mut staged = NO_CHILD;
        loop {
            let r = match (cur, float.get(j)) {
                (Some(r), Some(e)) => r.min(byte(e.0)),
                (Some(r), None) => r,
                (None, Some(e)) => byte(e.0),
                (None, None) => return true,
            };
            let j_end = j + float[j..].iter().take_while(|e| byte(e.0) == r).count();
            let floats = &float[j..j_end];
            j = j_end;
            let deeper = bounded && Some(r) == bound;
            if cur != Some(r) {
                let mut group: Group<'t> = SmallVec::new();
                for &e in floats {
                    group.push(e);
                }
                if !self.emit(&mut group, deeper) {
                    return false;
                }
                continue;
            }
            let s = slot(r);
            cur = next;
            next = ord.after(next);
            // préchargement en deux étages (voir l'en-tête)
            if staged != NO_CHILD {
                src.prefetch_entries(staged);
                staged = NO_CHILD;
            }
            if let Some(a) = ahead {
                src.key(idx, slot(a)).prefetch();
                ahead = ord.after(ahead);
            }
            if let Some(nr) = next {
                let c = src.child(idx, slot(nr));
                if c != NO_CHILD && depth + 1 < DISPATCH_LEVELS {
                    prefetch_slot(&src.trees().nodes, c);
                    staged = c;
                }
            }
            let m: &'t [u8] = src.key(idx, s);
            let child = src.child(idx, s);
            if child == NO_CHILD && floats.is_empty() && !deeper {
                // feuille seule à son radix, hors du chemin de `from`
                if !(self.f)(m, At::Node(idx, s)) {
                    return false;
                }
                continue;
            }
            let mut group: Group<'t> = SmallVec::new();
            for &e in floats {
                group.push(e);
            }
            group.push((self.order_key(m), m, At::Node(idx, s)));
            let go = if child == NO_CHILD {
                self.emit(&mut group, deeper)
            } else {
                group.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
                if depth + 1 < DISPATCH_LEVELS {
                    self.node(child, depth + 1, &group, deeper)
                } else {
                    // octets de dispatch épuisés : bucket de collision
                    for b in 0..src.bucket_len(child) {
                        let m = src.bucket_key(child, b);
                        group.push((self.order_key(m), m, At::Bucket(child, b)));
                    }
                    self.emit(&mut group, deeper)
                }
            };
            if !go {
                return false;
            }
        }
    }
}

impl ValTrees {
    /// SSCAN : membres du Set `root` après `from` (clé d'ordre, membre), dans l'ordre
    /// du module ; `f(membre)` rend `false` pour arrêter. La position d'une entrée vue
    /// est `(scan_order_key(membre), membre)`.
    pub fn set_scan<'t>(&'t self, root: u32, from: Option<(u64, &[u8])>, mut f: impl FnMut(&'t SharedByte) -> bool) {
        let src = SetSrc(self);
        let mut w = Walk {
            src: &src,
            from,
            f: |_m: &[u8], at| f(match at {
                At::Node(n, s) => self.set_node(n).key(s),
                At::Bucket(b, i) => &self.set_collisions.get(b)[i],
            }),
        };
        if !self.set_node(root).is_empty() {
            w.node(root, 0, &[], from.is_some());
        }
    }

    /// HSCAN : comme [`set_scan`](Self::set_scan), `f(champ, valeur)`.
    pub fn hash_scan<'t>(
        &'t self,
        root: u32,
        from: Option<(u64, &[u8])>,
        mut f: impl FnMut(&'t SharedByte, &'t SharedByte) -> bool,
    ) {
        let src = HashSrc(self);
        let mut w = Walk {
            src: &src,
            from,
            f: |_m: &[u8], at| match at {
                At::Node(n, s) => {
                    let node = self.hash_node(n);
                    f(node.key(s), node.val(s))
                }
                At::Bucket(b, i) => {
                    let (fk, v) = &self.hash_collisions.get(b)[i];
                    f(fk, v)
                }
            },
        };
        if !self.hash_node(root).is_empty() {
            w.node(root, 0, &[], from.is_some());
        }
    }

    /// Clé d'ordre de SSCAN / HSCAN d'un membre (champ).
    pub fn scan_order_key(&self, member: &[u8]) -> u64 {
        self.member_hash(member).0.swap_bytes()
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use alloc::collections::BTreeSet;
    use radixox_lib::shared_byte::SharedByte;

    use crate::ValTrees;

    fn sb(s: &str) -> SharedByte {
        SharedByte::from_slice(s.as_bytes())
    }

    /// Pages de `count` jusqu'au bout : chaque membre une fois, dans l'ordre (clé, membre).
    fn scan_all(t: &ValTrees, root: u32, count: usize) -> Vec<(u64, Vec<u8>)> {
        let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
        loop {
            let from = out.last().map(|(k, m)| (*k, m.clone()));
            let mut page = Vec::new();
            t.set_scan(root, from.as_ref().map(|(k, m)| (*k, m.as_slice())), |m| {
                page.push((t.scan_order_key(m), m.as_slice().to_vec()));
                page.len() < count
            });
            if page.is_empty() {
                return out;
            }
            out.extend(page);
        }
    }

    fn check(t: &ValTrees, root: u32, members: &BTreeSet<String>) {
        for count in [1, 3, 64, 100_000] {
            let got = scan_all(t, root, count);
            assert!(got.windows(2).all(|w| w[0] < w[1]), "ordre strict (count {count})");
            let set: BTreeSet<String> = got.iter().map(|(_, m)| String::from_utf8(m.clone()).unwrap()).collect();
            assert_eq!(set.len(), got.len());
            assert_eq!(&set, members, "count {count}");
            for (k, m) in &got {
                assert_eq!(*k, t.scan_order_key(m));
            }
        }
    }

    #[test]
    fn set_scan_is_ordered_and_complete() {
        for mask in [u64::MAX, 0x0000_0000_00FF_FFFF, 0xFF00_0000_0000_00FF, 0x0303_0303_0303_0303] {
            let mut t = ValTrees::new();
            t.hash_mask = mask;
            let root = t.set_create();
            let mut members = BTreeSet::new();
            for i in 0..3000 {
                let m = format!("m{i}");
                t.set_add(root, sb(&m));
                members.insert(m);
            }
            check(&t, root, &members);
            // suppressions (remontées de feuilles), puis on reprend au milieu
            for i in (0..3000).step_by(3) {
                let m = format!("m{i}");
                t.set_remove(root, m.as_bytes());
                members.remove(&m);
            }
            check(&t, root, &members);
        }
    }

    /// Un scan coupé en deux, avec des écritures entre les pages : les membres présents
    /// tout le long sortent une fois exactement.
    #[test]
    fn set_scan_survives_writes_between_pages() {
        let mut t = ValTrees::new();
        t.hash_mask = 0x0F0F_0F0F_0F0F_0F0F;
        let root = t.set_create();
        for i in 0..2000 {
            t.set_add(root, sb(&format!("s{i}")));
        }
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut from: Option<(u64, Vec<u8>)> = None;
        let mut round = 0;
        loop {
            let mut page = Vec::new();
            t.set_scan(root, from.as_ref().map(|(k, m)| (*k, m.as_slice())), |m| {
                page.push((t.scan_order_key(m), m.as_slice().to_vec()));
                page.len() < 50
            });
            let Some(last) = page.last().cloned() else { break };
            seen.extend(page.into_iter().map(|(_, m)| m));
            from = Some(last);
            // écritures : on retire des membres instables, on en ajoute d'autres
            for i in 0..20 {
                t.set_remove(root, format!("s{}", 1000 + (round * 20 + i) % 1000).as_bytes());
                t.set_add(root, sb(&format!("new{round}:{i}")));
            }
            round += 1;
        }
        let mut uniq = seen.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), seen.len(), "aucun doublon");
        for i in 0..1000 {
            assert!(seen.contains(&format!("s{i}").into_bytes()), "s{i} stable, oublié");
        }
    }

    #[test]
    fn hash_scan_returns_pairs() {
        let mut t = ValTrees::new();
        t.hash_mask = 0x00FF_00FF_00FF_00FF;
        let root = t.hash_create();
        for i in 0..500 {
            t.hash_set(root, sb(&format!("f{i}")), sb(&format!("v{i}")));
        }
        let mut got = Vec::new();
        t.hash_scan(root, None, |f, v| {
            got.push((f.as_slice().to_vec(), v.as_slice().to_vec()));
            true
        });
        assert_eq!(got.len(), 500);
        for (f, v) in got {
            assert_eq!(&f[1..], &v[1..]);
        }
    }
}
