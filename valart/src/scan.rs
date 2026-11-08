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
//! Dans un nœud, les slots sont pris par radix croissant (ils ne sont pas rangés : tri
//! sur place, ≤ 256) ; l'entrée d'un slot à enfant **flotte** dans le sous-arbre jusqu'à
//! sa place (au plus 7 flottantes, une par niveau). Un groupe de radix avant la position
//! de reprise est sauté sans y descendre.
//!
//! Il faut le hash de chaque entrée vue : il est recalculé (le nœud n'en garde que
//! l'octet du niveau et l'octet de contrôle). Les membres sont donc lus de toute façon :
//! ils sont préchargés `AHEAD` slots à l'avance, les nœuds enfants deux slots à
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
    fn len(&self, idx: u32) -> usize;
    fn slot_at(&self, idx: u32, n: usize) -> Slot;
    fn radix(&self, idx: u32, s: Slot) -> u8;
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
    fn len(&self, idx: u32) -> usize {
        self.0.set_node(idx).len()
    }
    fn slot_at(&self, idx: u32, n: usize) -> Slot {
        self.0.set_node(idx).slot_at(n)
    }
    fn radix(&self, idx: u32, s: Slot) -> u8 {
        self.0.set_node(idx).radix_of(s)
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
    fn len(&self, idx: u32) -> usize {
        self.0.hash_node(idx).len()
    }
    fn slot_at(&self, idx: u32, n: usize) -> Slot {
        self.0.hash_node(idx).slot_at(n)
    }
    fn radix(&self, idx: u32, s: Slot) -> u8 {
        self.0.hash_node(idx).radix_of(s)
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

/// Au-delà de cette taille, les slots d'un nœud sont rangés par une table radix → rang
/// (O(256 + n), sans comparaison) plutôt que triés.
const SORT_MAX: usize = 16;

struct Walk<'t, 'f, S, F> {
    src: &'t S,
    /// Position de reprise (exclue).
    from: Option<(u64, &'f [u8])>,
    /// Rend `false` pour arrêter.
    f: F,
}

impl<'t, S: Src, F: FnMut(u64, &'t [u8], At) -> bool> Walk<'t, '_, S, F> {
    fn order_key(&self, member: &[u8]) -> u64 {
        self.src.trees().member_hash(member).0.swap_bytes()
    }

    /// Émet les entrées d'un groupe, triées, après `from`. `false` : arrêt demandé.
    fn emit(&mut self, group: &mut [Float<'t>]) -> bool {
        if group.len() == 1 {
            let (k, m, at) = group[0];
            return self.from.is_some_and(|(fk, fm)| (k, m) <= (fk, fm)) || (self.f)(k, m, at);
        }
        group.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        for &(k, m, at) in group.iter() {
            if self.from.is_some_and(|(fk, fm)| (k, m) <= (fk, fm)) {
                continue;
            }
            if !(self.f)(k, m, at) {
                return false;
            }
        }
        true
    }

    /// Nœud `idx` au niveau `depth` ; `float` : entrées des ancêtres dont le hash
    /// prolonge le chemin, triées ; `bounded` : le chemin est celui de `from`.
    fn node(&mut self, idx: u32, depth: usize, float: &[Float<'t>], bounded: bool) -> bool {
        let src = self.src;
        let n = src.len(idx);
        let byte = |k: u64| (k >> (8 * (7 - depth))) as u8;
        let bound = bounded.then(|| byte(self.from.expect("bounded ⇒ from").0));
        let lo = bound.unwrap_or(0);
        // slots de radix ≥ `lo` (les groupes avant `from` sont sautés d'emblée), par
        // radix croissant
        let mut slots: SmallVec<16, (u8, Slot)> = SmallVec::new();
        if n <= SORT_MAX {
            for i in 0..n {
                let s = src.slot_at(idx, i);
                let r = src.radix(idx, s);
                if r >= lo {
                    slots.push((r, s));
                }
            }
            slots.sort_unstable_by_key(|&(r, _)| r);
        } else {
            let mut rank = [u16::MAX; 256];
            for i in 0..n {
                rank[src.radix(idx, src.slot_at(idx, i)) as usize] = i as u16;
            }
            for (r, &i) in rank.iter().enumerate().skip(lo as usize) {
                if i != u16::MAX {
                    slots.push((r as u8, src.slot_at(idx, i as usize)));
                }
            }
        }
        let mut i = 0;
        let mut j = float.partition_point(|e| byte(e.0) < lo);
        for s in slots.iter().skip(i).take(AHEAD) {
            src.key(idx, s.1).prefetch();
        }
        let mut staged = NO_CHILD;
        while i < slots.len() || j < float.len() {
            let r = match (slots.get(i), float.get(j)) {
                (Some(&(r, _)), Some(e)) => r.min(byte(e.0)),
                (Some(&(r, _)), None) => r,
                (None, Some(e)) => byte(e.0),
                (None, None) => unreachable!(),
            };
            let j_end = j + float[j..].iter().take_while(|e| byte(e.0) == r).count();
            let mut group: Group<'t> = SmallVec::new();
            for &e in &float[j..j_end] {
                group.push(e);
            }
            j = j_end;
            let deeper = bounded && Some(r) == bound;
            let Some(&(sr, slot)) = slots.get(i).filter(|&&(sr, _)| sr == r) else {
                if !self.emit(&mut group) {
                    return false;
                }
                continue;
            };
            debug_assert_eq!(sr, r);
            i += 1;
            // préchargement en deux étages (voir l'en-tête)
            if staged != NO_CHILD {
                src.prefetch_entries(staged);
                staged = NO_CHILD;
            }
            if let Some(&(_, s)) = slots.get(i + AHEAD - 1) {
                src.key(idx, s).prefetch();
            }
            if let Some(&(_, s)) = slots.get(i + 1) {
                let c = src.child(idx, s);
                if c != NO_CHILD && depth + 1 < DISPATCH_LEVELS {
                    prefetch_slot(&src.trees().nodes, c);
                    staged = c;
                }
            }
            let m: &'t [u8] = src.key(idx, slot);
            group.push((self.order_key(m), m, At::Node(idx, slot)));
            let child = src.child(idx, slot);
            let go = if child == NO_CHILD {
                self.emit(&mut group)
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
                    self.emit(&mut group)
                }
            };
            if !go {
                return false;
            }
        }
        true
    }
}

impl ValTrees {
    /// SSCAN : membres du Set `root` après `from` (clé d'ordre, membre), dans l'ordre
    /// du module ; `f(clé d'ordre, membre)` rend `false` pour arrêter.
    pub fn set_scan<'t>(&'t self, root: u32, from: Option<(u64, &[u8])>, mut f: impl FnMut(u64, &'t SharedByte) -> bool) {
        let src = SetSrc(self);
        let mut w = Walk {
            src: &src,
            from,
            f: |k, _m: &[u8], at| f(k, match at {
                At::Node(n, s) => self.set_node(n).key(s),
                At::Bucket(b, i) => &self.set_collisions.get(b)[i],
            }),
        };
        if !self.set_node(root).is_empty() {
            w.node(root, 0, &[], from.is_some());
        }
    }

    /// HSCAN : comme [`set_scan`](Self::set_scan), `f(clé d'ordre, champ, valeur)`.
    pub fn hash_scan<'t>(
        &'t self,
        root: u32,
        from: Option<(u64, &[u8])>,
        mut f: impl FnMut(u64, &'t SharedByte, &'t SharedByte) -> bool,
    ) {
        let src = HashSrc(self);
        let mut w = Walk {
            src: &src,
            from,
            f: |k, _m: &[u8], at| match at {
                At::Node(n, s) => {
                    let node = self.hash_node(n);
                    f(k, node.key(s), node.val(s))
                }
                At::Bucket(b, i) => {
                    let (fk, v) = &self.hash_collisions.get(b)[i];
                    f(k, fk, v)
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
            t.set_scan(root, from.as_ref().map(|(k, m)| (*k, m.as_slice())), |k, m| {
                page.push((k, m.as_slice().to_vec()));
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
            t.set_scan(root, from.as_ref().map(|(k, m)| (*k, m.as_slice())), |k, m| {
                page.push((k, m.as_slice().to_vec()));
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
        t.hash_scan(root, None, |_, f, v| {
            got.push((f.as_slice().to_vec(), v.as_slice().to_vec()));
            true
        });
        assert_eq!(got.len(), 500);
        for (f, v) in got {
            assert_eq!(&f[1..], &v[1..]);
        }
    }
}
