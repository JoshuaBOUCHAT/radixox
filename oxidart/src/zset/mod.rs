//! ZSet (ANALYSE_ART_UNIFIE.md §13) : petite forme en colonnes, grande forme = index
//! valart + B+tree compté + membres à poignée (MEMORY_ARCH.md §19).

pub(crate) mod amac;
pub mod btree;
pub mod index;
pub mod large;
pub(crate) mod lazy;
pub(crate) mod save;
pub mod small;

use std::cmp::Ordering;

pub use large::{Added, ZLarge};
pub use small::ZSmall;

/// Un ZSet : petite forme tant qu'elle suffit, puis grande, sans retour (§13.2). Possède
/// sa mémoire (bloc, membres, nœuds) : `Drop` la rend.
pub enum ZSet {
    Small(ZSmall),
    Large(ZLarge),
}

impl Default for ZSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ZSet {
    pub fn new() -> Self {
        Self::Small(ZSmall::new())
    }

    /// Depuis des `(score, membre)` triés, sans doublon de membre : petite forme si elle
    /// suffit, sinon grande, feuilles pleines (rechargement). `None` : un membre en double
    /// ou la table des poignées pleine.
    pub fn from_sorted<'a>(entries: impl ExactSizeIterator<Item = (u64, &'a [u8])> + Clone) -> Option<Self> {
        if entries.len() <= small::MAX_MEMBERS && entries.clone().all(|(_, m)| m.len() <= small::MAX_MEMBER_LEN) {
            let mut z = ZSmall::new();
            for (s, m) in entries {
                if z.find(m).is_some() {
                    z.free();
                    return None;
                }
                z.insert(s, m);
            }
            return Some(Self::Small(z));
        }
        ZLarge::from_sorted(entries).map(Self::Large)
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Small(z) => z.len(),
            Self::Large(z) => z.len() as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Score encodé de `m`.
    pub fn score(&self, m: &[u8]) -> Option<u64> {
        match self {
            Self::Small(z) => z.get(m),
            Self::Large(z) => z.get(m).map(|(_, s)| s),
        }
    }

    /// Ajoute `m` ou change son score (encodé). `None` : table des poignées pleine
    /// (le ZSet est inchangé).
    pub fn add(&mut self, s: u64, m: &[u8]) -> Option<Added> {
        let small = match self {
            Self::Large(z) => return z.add(s, m),
            Self::Small(z) => z,
        };
        if let Some(i) = small.find(m) {
            let old = small.score(i);
            if old == s {
                return Some(Added::Unchanged);
            }
            small.set_score(i, s);
            return Some(Added::Updated(old));
        }
        if small.insert(s, m) {
            return Some(Added::New);
        }
        // Seuil dépassé : passage en grande forme.
        let mut large = ZLarge::from_sorted(small.iter())?;
        let r = large.add(s, m);
        if r.is_none() {
            large.release();
            return None;
        }
        drop(std::mem::replace(self, Self::Large(large)));
        r
    }

    /// Retire `m` ; rend son score.
    pub fn remove(&mut self, m: &[u8]) -> Option<u64> {
        match self {
            Self::Small(z) => z.remove(m),
            Self::Large(z) => z.remove(m),
        }
    }

    /// Rang de `m` (croissant).
    pub fn rank(&self, m: &[u8]) -> Option<usize> {
        match self {
            Self::Small(z) => z.find(m),
            Self::Large(z) => z.rank(m).map(|r| r as usize),
        }
    }

    /// Premier rang dont l'entrée n'est pas avant la sonde (`probe(score, membre)`).
    pub fn lower_bound(&self, probe: impl FnMut(u64, &[u8]) -> Ordering) -> usize {
        match self {
            Self::Small(z) => z.lower_bound(probe),
            Self::Large(z) => z.lower_bound(probe) as usize,
        }
    }

    /// `(score, membre)` à partir du rang `r`, croissants.
    pub fn iter_from(&self, r: usize) -> ZIter<'_> {
        match self {
            Self::Small(z) => ZIter::Small(z.iter_from(r)),
            Self::Large(z) => ZIter::Large(z.iter_from(r as u32)),
        }
    }

    /// `(score, membre)` du rang `r` vers le début.
    pub fn iter_back_from(&self, r: usize) -> ZIterBack<'_> {
        match self {
            Self::Small(z) => ZIterBack::Small(z.iter_from(0).take(r + 1).rev()),
            Self::Large(z) => ZIterBack::Large(z.iter_back_from(r as u32)),
        }
    }

    pub fn iter(&self) -> ZIter<'_> {
        self.iter_from(0)
    }

    /// Retire le contenu d'une grande forme (le ZSet reste vide, sans rien à rendre).
    pub(crate) fn take_large(&mut self) -> Option<ZLarge> {
        match self {
            Self::Large(z) => Some(std::mem::take(z)),
            Self::Small(_) => None,
        }
    }

    /// Rend tout (comme `drop`).
    pub fn free(self) {
        drop(self)
    }

    /// Octets tenus (bloc de la petite forme ; en-tête, nœuds et membres de la grande ne
    /// sont pas comptés ici).
    pub fn small_block(&mut self) -> Option<(&mut *mut u8, usize)> {
        match self {
            Self::Small(z) => {
                let size = z.block_bytes();
                Some((&mut z.0, size))
            }
            Self::Large(_) => None,
        }
    }

    /// Vérificateur (grande forme : arbre, index, table).
    pub fn verify(&self) -> Result<(), String> {
        match self {
            Self::Small(_) => Ok(()),
            Self::Large(z) => z.verify(),
        }
    }
}

/// Le contenu, `(score, membre)` dans l'ordre (pas les adresses).
impl std::fmt::Debug for ZSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter().map(|(s, m)| (decode_score(s), String::from_utf8_lossy(m)))).finish()
    }
}

impl Drop for ZSet {
    fn drop(&mut self) {
        match self {
            Self::Small(z) => z.release(),
            Self::Large(z) => z.release(),
        }
    }
}

/// Copie profonde (`Value` possédée, image de save).
impl Clone for ZSet {
    fn clone(&self) -> Self {
        match self {
            Self::Small(z) => Self::Small(z.deep_clone()),
            Self::Large(z) => {
                let it = z.iter().collect::<Vec<_>>();
                Self::Large(ZLarge::from_sorted(it.into_iter()).expect("table des poignées pleine"))
            }
        }
    }
}

impl PartialEq for ZSet {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

/// Parcours croissant.
pub enum ZIter<'a> {
    Small(small::Iter<'a>),
    Large(large::Range),
}

impl<'a> Iterator for ZIter<'a> {
    type Item = (u64, &'a [u8]);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Small(i) => i.next(),
            Self::Large(i) => i.next(),
        }
    }
}

/// Parcours décroissant.
pub enum ZIterBack<'a> {
    Small(std::iter::Rev<std::iter::Take<small::Iter<'a>>>),
    Large(large::Range),
}

impl<'a> Iterator for ZIterBack<'a> {
    type Item = (u64, &'a [u8]);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Small(i) => i.next(),
            Self::Large(i) => i.next(),
        }
    }
}

/// `f64` → `u64` dont l'ordre est celui de `total_cmp` (bijection exacte sur les bits) ;
/// `-0.0` devient `0.0` (Valkey : même score). NaN refusé avant d'arriver ici.
#[inline]
pub fn encode_score(f: f64) -> u64 {
    debug_assert!(!f.is_nan());
    let b = if f == 0.0 { 0 } else { f.to_bits() };
    if b >> 63 == 1 { !b } else { b | 1 << 63 }
}

#[inline]
pub fn decode_score(u: u64) -> f64 {
    f64::from_bits(if u >> 63 == 1 { u & !(1 << 63) } else { !u })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zcommand::InnerZCommand;
    use radixox_lib::shared_byte::SharedByte;

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

    fn same(z: &ZSet, o: &InnerZCommand) {
        assert_eq!(z.len(), o.len());
        let got: Vec<(f64, Vec<u8>)> = z.iter().map(|(s, m)| (decode_score(s), m.to_vec())).collect();
        let want: Vec<(f64, Vec<u8>)> = o.iter().map(|(s, m)| (s.into_inner(), m.to_vec())).collect();
        assert_eq!(got, want);
    }

    /// Différentiel contre le ZSet actuel (oracle, comme oxidart-legacy pour l'arbre) :
    /// ajouts, mises à jour, retraits, scores, rangs, parcours dans les deux sens, passage
    /// petite → grande par le nombre et par la longueur d'un membre, ex-aequo massifs.
    #[test]
    fn differential_against_inner_zcommand() {
        crate::value::init_slabs();
        for (seed, universe, scores) in [(1u64, 100u64, 5u64), (2, 400, 1000), (3, 3000, 1), (4, 20_000, 1 << 30)] {
            let mut rng = Rng(0xA076_1D64_78BD_642F ^ seed);
            let mut z = ZSet::new();
            let mut o = InnerZCommand::new();
            let member = |i: u64| -> Vec<u8> {
                // un membre sur 997 dépasse 64 o : passage par la longueur
                let len = if i % 997 == 13 { 80 } else { 3 + (i % 17) as usize };
                let mut m = format!("m{i}:").into_bytes();
                m.resize(len.max(m.len()), b'x');
                m
            };
            for step in 0..30_000u64 {
                let m = member(rng.below(universe));
                let f = if scores == 1 { 0.0 } else { (rng.below(scores) as f64 - (scores / 2) as f64) * 0.5 };
                let s = encode_score(f);
                if rng.below(10) < 7 {
                    let new = o.insert(f, SharedByte::from_slice(&m));
                    let r = z.add(s, &m).unwrap();
                    assert_eq!(r == Added::New, new, "membre {:?}", String::from_utf8_lossy(&m));
                } else {
                    let had = o.remove(SharedByte::from_slice(&m));
                    assert_eq!(z.remove(&m).is_some(), had);
                }
                assert_eq!(z.score(&m).map(decode_score), o.score(SharedByte::from_slice(&m)));
                if step % 1013 == 0 {
                    same(&z, &o);
                    z.verify().unwrap();
                    let all: Vec<(u64, Vec<u8>)> = z.iter().map(|(s, m)| (s, m.to_vec())).collect();
                    for _ in 0..20 {
                        if all.is_empty() {
                            break;
                        }
                        let r = rng.below(all.len() as u64) as usize;
                        assert_eq!(z.rank(&all[r].1), Some(r));
                        let fwd: Vec<_> = z.iter_from(r).take(5).map(|(s, m)| (s, m.to_vec())).collect();
                        assert_eq!(fwd, all[r..(r + 5).min(all.len())]);
                        let back: Vec<_> = z.iter_back_from(r).take(5).map(|(s, m)| (s, m.to_vec())).collect();
                        let want: Vec<_> = all[r.saturating_sub(4)..=r].iter().rev().cloned().collect();
                        assert_eq!(back, want);
                        let x = all[r].0;
                        let lb = z.lower_bound(|es, _| if x <= es { Ordering::Less } else { Ordering::Greater });
                        assert_eq!(lb, all.partition_point(|e| e.0 < x));
                    }
                }
            }
            same(&z, &o);
            z.verify().unwrap();
            if universe >= 400 {
                assert!(matches!(z, ZSet::Large(_)), "grande forme attendue");
            }
            z.free();
        }
        radixox_lib::handle::verify().unwrap();
        assert_eq!(radixox_lib::handle::stats().0, 0, "toutes les poignées rendues");
    }

    /// Le GC déplace les membres pendant que le ZSet vit : la table suit, rien ne change.
    #[test]
    fn members_survive_gc_evacuation() {
        crate::value::init_slabs();
        let mut z = ZSet::new();
        for i in 0..5000u32 {
            z.add(encode_score((i % 7) as f64), format!("membre:{i}").as_bytes()).unwrap();
        }
        for i in (0..5000u32).step_by(2) {
            z.remove(format!("membre:{i}").as_bytes()).unwrap();
        }
        let before: Vec<(u64, Vec<u8>)> = z.iter().map(|(s, m)| (s, m.to_vec())).collect();
        if radixox_lib::pool::gc_begin(radixox_lib::pool::Mark::All, std::time::Instant::now()) > 0 {
            let mut at = Some(0);
            let mut moved = 0;
            while let Some(from) = at {
                at = radixox_lib::handle::gc_step(from, usize::MAX, &mut moved);
            }
            radixox_lib::pool::gc_unmark_all();
            assert!(!radixox_lib::pool::ENABLED || moved > 0, "des membres ont bougé");
        }
        radixox_lib::handle::verify().unwrap();
        z.verify().unwrap();
        let after: Vec<(u64, Vec<u8>)> = z.iter().map(|(s, m)| (s, m.to_vec())).collect();
        assert_eq!(before, after);
        z.free();
    }

    #[test]
    fn score_encoding_keeps_order() {
        let v = [f64::NEG_INFINITY, -1e300, -2.5, -1.0, -f64::MIN_POSITIVE, 0.0, 5e-324, 1.0, 2.5, 1e300, f64::INFINITY];
        for w in v.windows(2) {
            assert!(encode_score(w[0]) < encode_score(w[1]), "{} < {}", w[0], w[1]);
        }
        for x in v {
            assert_eq!(decode_score(encode_score(x)), x);
        }
        assert_eq!(encode_score(-0.0), encode_score(0.0));
    }
}
