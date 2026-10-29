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

use radixox_lib::cmd::{LexBound, ScoreBound, ZRangeBy};

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

    /// Grande forme sur un B+tree déjà construit (voir [`ZLarge::from_tree`]).
    pub(crate) fn from_tree(tree: btree::BTree) -> Option<Self> {
        ZLarge::from_tree(tree).map(Self::Large)
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
        self.upsert(m, |_| Some(s))
    }

    /// Ajoute ou met à jour `m` en une seule recherche : `f(ancien score)` (encodés) donne
    /// le nouveau, ou `None` pour ne rien changer (`Added::Unchanged`). `None` : table des
    /// poignées pleine (le ZSet est inchangé).
    pub fn upsert(&mut self, m: &[u8], f: impl FnOnce(Option<u64>) -> Option<u64>) -> Option<Added> {
        let small = match self {
            Self::Large(z) => return z.upsert(m, f),
            Self::Small(z) => z,
        };
        if let Some(i) = small.find(m) {
            let old = small.score(i);
            let Some(s) = f(Some(old)).filter(|&s| s != old) else { return Some(Added::Unchanged) };
            small.set_score(i, s);
            return Some(Added::Updated(old));
        }
        let Some(s) = f(None) else { return Some(Added::Unchanged) };
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

    /// Petite forme : place pour ces `(score, membre)` d'un coup (voir [`ZSmall::reserve`]).
    pub fn reserve<M: AsRef<[u8]>>(&mut self, pairs: &[(f64, M)]) {
        if let Self::Small(z) = self
            && pairs.len() > 1
            && pairs.iter().all(|(_, m)| m.as_ref().len() <= small::MAX_MEMBER_LEN)
        {
            let bytes = pairs.iter().map(|(_, m)| m.as_ref().len()).sum();
            z.reserve(pairs.len(), bytes, pairs.iter().map(|(s, _)| encode_score(*s)));
        }
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

    /// Rangs `[lo, hi)` des entrées de score dans `[min, max]` (bornes exclues si `excl`).
    pub fn score_range(&self, min: ScoreBound, max: ScoreBound) -> (usize, usize) {
        let (a, b) = (encode_score(min.value), encode_score(max.value));
        let (lo, hi) = match self {
            Self::Large(z) => (z.score_bound(a, min.excl) as usize, z.score_bound(b, !max.excl) as usize),
            Self::Small(_) => (
                self.lower_bound(|s, _| if min.excl { a < s } else { a <= s }.then_some(Ordering::Less).unwrap_or(Ordering::Greater)),
                self.lower_bound(|s, _| if max.excl { b <= s } else { b < s }.then_some(Ordering::Less).unwrap_or(Ordering::Greater)),
            ),
        };
        (lo, hi.max(lo))
    }

    /// Rangs `[lo, hi)` des membres dans `[min, max]` lexicographique (comme Redis, sens
    /// défini seulement si tous les scores sont égaux).
    pub fn lex_range(&self, min: LexBound<'_>, max: LexBound<'_>) -> (usize, usize) {
        let first_not_before = |b: LexBound<'_>, upper: bool| -> usize {
            match b {
                LexBound::Min => 0,
                LexBound::Max => self.len(),
                // borne basse incluse / haute exclue : premier membre ≥ x
                LexBound::Incl(x) if !upper => self.lower_bound(|_, m| if x <= m { Ordering::Less } else { Ordering::Greater }),
                LexBound::Excl(x) if upper => self.lower_bound(|_, m| if x <= m { Ordering::Less } else { Ordering::Greater }),
                // basse exclue / haute incluse : premier membre > x
                LexBound::Incl(x) | LexBound::Excl(x) => self.lower_bound(|_, m| if x < m { Ordering::Less } else { Ordering::Greater }),
            }
        };
        let (lo, hi) = (first_not_before(min, false), first_not_before(max, true));
        (lo, hi.max(lo))
    }

    /// Rangs `[lo, hi)` de `start..=stop` (négatifs depuis la fin), comme Redis.
    pub fn rank_range(&self, start: i64, stop: i64) -> (usize, usize) {
        let len = self.len() as i64;
        let start = if start < 0 { (len + start).max(0) } else { start };
        let stop = if stop < 0 { len + stop } else { stop.min(len - 1) };
        if start > stop || start >= len { (0, 0) } else { (start as usize, stop as usize + 1) }
    }

    /// Les rangs `[lo, hi)` d'une plage.
    pub fn resolve(&self, range: ZRangeBy<'_>) -> (usize, usize) {
        match range {
            ZRangeBy::Rank(a, b) => self.rank_range(a, b),
            ZRangeBy::Score(a, b) => self.score_range(a, b),
            ZRangeBy::Lex(a, b) => self.lex_range(a, b),
        }
    }

    /// Membres des rangs `[lo, hi)`, copiés (avant de les retirer).
    pub fn members_in(&self, lo: usize, hi: usize) -> Vec<Vec<u8>> {
        self.iter_from(lo).take(hi.saturating_sub(lo)).map(|(_, m)| m.to_vec()).collect()
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

/// Texte d'un score, comme `f64::to_string` (même sortie), en `SharedByte` (réponses
/// construites en `Frame`).
#[inline]
pub fn format_score(f: f64) -> radixox_lib::shared_byte::SharedByte {
    with_score_text(f, |t| radixox_lib::shared_byte::SharedByte::from_slice(t))
}

/// Texte d'un score, comme `f64::to_string` (même sortie), passé à `k` depuis un tampon
/// sur la pile : ni `String` ni `SharedByte` sur le chemin des réponses encodées. Entiers
/// (rangs, horodatages, compteurs, les plus fréquents) écrits à la main.
#[inline]
pub fn with_score_text<R>(f: f64, k: impl FnOnce(&[u8]) -> R) -> R {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        let mut buf = [0u8; 20];
        let (neg, mut v) = (f < 0.0, f.abs() as u64);
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        if neg {
            i -= 1;
            buf[i] = b'-';
        }
        return k(&buf[i..]);
    }
    float_text(f, k)
}

/// Flottant non entier : chiffres les plus courts par zmij (≤ 17 significatifs, arrondi au
/// pair entre deux candidats, comme Valkey), en forme décimale sans exposant comme `Display`, dans 64 octets sur la pile ; au-delà
/// (exposants extrêmes écrits en entier), `to_string`.
#[inline(never)]
fn float_text<R>(f: f64, k: impl FnOnce(&[u8]) -> R) -> R {
    if !f.is_finite() {
        return k(if f > 0.0 { b"inf" } else { b"-inf" });
    }
    let mut rb = zmij::Buffer::new();
    let s = rb.format_finite(f).as_bytes();
    // cas courant : déjà décimal sans exposant (`123.456`, `0.00123`), tel quel
    if !s.contains(&b'e') && !s.ends_with(b".0") {
        return k(s);
    }
    let (neg, s) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, s),
    };
    // `mantisse[e exposant]`, mantisse `d+[.d+]`
    let (mant, exp) = match s.iter().position(|&c| c == b'e') {
        Some(e) => (&s[..e], std::str::from_utf8(&s[e + 1..]).unwrap().parse::<i32>().unwrap()),
        None => (s, 0),
    };
    let mut digits = [0u8; 24];
    let (mut n, mut int_len) = (0, None);
    for &c in mant {
        if c == b'.' {
            int_len = Some(n);
        } else {
            digits[n] = c;
            n += 1;
        }
    }
    // position de la virgule dans `digits`, zéros de tête et de queue retirés
    let mut point = int_len.unwrap_or(n) as i32 + exp;
    let lead = digits[..n].iter().take_while(|&&c| c == b'0').count();
    let d = &digits[lead..n];
    point -= lead as i32;
    let d = &d[..d.len() - d.iter().rev().take_while(|&&c| c == b'0').count()];
    let len = neg as usize
        + if point <= 0 {
            2 + (-point) as usize + d.len()
        } else if point as usize >= d.len() {
            point as usize
        } else {
            d.len() + 1
        };
    if len > 64 {
        return k(f.to_string().as_bytes());
    }
    let mut buf = [0u8; 64];
    let mut i = 0;
    let mut put = |b: &[u8]| {
        buf[i..i + b.len()].copy_from_slice(b);
        i += b.len();
    };
    if neg {
        put(b"-");
    }
    if point <= 0 {
        put(b"0.");
        for _ in 0..-point {
            put(b"0");
        }
        put(d);
    } else if point as usize >= d.len() {
        put(d);
        for _ in d.len()..point as usize {
            put(b"0");
        }
    } else {
        put(&d[..point as usize]);
        put(b".");
        put(&d[point as usize..]);
    }
    k(&buf[..i])
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

    /// Retraits de grandes plages (`ZLarge::remove_range`) : feuilles du milieu décrochées,
    /// entrées d'index périmées tant que le nettoyage (`lazy::step`) n'est pas passé ;
    /// membres retirés puis rajoutés, retirés, lus ; ZSet supprimé avec son cimetière.
    #[test]
    fn range_removal_with_stale_index() {
        use crate::value::{alloc_zset, free_zset, zset_mut, zset_ref};
        use std::collections::BTreeMap;
        crate::value::init_slabs();
        for (seed, scores) in [(21u64, 1u64 << 30), (22, 50), (23, 1)] {
            let mut rng = Rng(0xD6E8_FEB8_6659_FD93 ^ seed);
            let idx = alloc_zset(ZSet::new());
            let mut model: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
            let member = |i: u64| format!("membre-{i}").into_bytes();
            for round in 0..80 {
                for _ in 0..rng.below(3000) {
                    let (m, sc) = (member(rng.below(40_000)), encode_score(rng.below(scores) as f64));
                    zset_mut(idx).add(sc, &m).unwrap();
                    model.insert(m, sc);
                }
                let len = zset_ref(idx).len() as u32;
                if len > 0 && rng.below(2) == 0 {
                    let lo = rng.below(len as u64) as u32;
                    let hi = lo + 1 + rng.below((len - lo) as u64) as u32;
                    let gone: Vec<Vec<u8>> = zset_ref(idx).members_in(lo as usize, hi as usize);
                    let ZSet::Large(z) = zset_mut(idx) else { panic!("grande forme attendue") };
                    assert_eq!(z.remove_range(idx, lo, hi), hi - lo);
                    for m in gone {
                        model.remove(&m);
                    }
                }
                // retirés : absents ; rajoutés : présents avec leur nouveau score
                for _ in 0..200 {
                    let m = member(rng.below(40_000));
                    match rng.below(4) {
                        0 => {
                            let sc = encode_score(rng.below(scores) as f64);
                            zset_mut(idx).add(sc, &m).unwrap();
                            model.insert(m, sc);
                        }
                        1 => assert_eq!(zset_mut(idx).remove(&m), model.remove(&m), "round {round}"),
                        _ => assert_eq!(zset_ref(idx).score(&m), model.get(&m).copied(), "round {round}"),
                    }
                }
                if rng.below(3) == 0 {
                    lazy::step(rng.below(5000) as u32);
                }
                zset_ref(idx).verify().unwrap();
                assert_eq!(zset_ref(idx).len(), model.len());
                let got: Vec<(u64, Vec<u8>)> = zset_ref(idx).iter().map(|(s, m)| (s, m.to_vec())).collect();
                let mut want: Vec<(u64, Vec<u8>)> = model.iter().map(|(m, &s)| (s, m.clone())).collect();
                want.sort();
                assert_eq!(got, want, "round {round}");
            }
            // supprimé avant la fin du nettoyage : tout est rendu
            free_zset(idx);
            while lazy::step(1 << 20) {}
            assert_eq!(lazy::pending(), 0);
        }
    }

    /// Compactage de la slab du B+tree : grands ZSet (racines, feuilles de bout, nœuds
    /// internes), une partie supprimée ; passe forcée, puis invariants (parents compris) et
    /// contenus gardés, slab rétrécie.
    #[test]
    fn btree_compaction_keeps_zsets() {
        use crate::value::{alloc_zset, free_zset, zset_mut, zset_ref};
        use std::collections::BTreeMap;
        crate::value::init_slabs();
        let mut rng = Rng(0x2F6B_1D4E_9A3C_7051);
        let member = |z: u64, i: u64| format!("z{z}-membre-{i}").into_bytes();
        let mut zs: Vec<(u32, BTreeMap<Vec<u8>, u64>)> = Vec::new();
        for z in 0..24u64 {
            let idx = alloc_zset(ZSet::new());
            let mut model = BTreeMap::new();
            for _ in 0..(200 + rng.below(6000)) {
                let (m, sc) = (member(z, rng.below(8000)), encode_score(rng.below(500) as f64));
                zset_mut(idx).add(sc, &m).unwrap();
                model.insert(m, sc);
            }
            zs.push((idx, model));
        }
        // trous : un ZSet sur trois supprimé, des plages retirées ailleurs
        let mut kept = Vec::new();
        for (k, (idx, mut model)) in zs.into_iter().enumerate() {
            if k % 3 == 0 {
                free_zset(idx);
                continue;
            }
            let len = zset_ref(idx).len() as u32;
            if let ZSet::Large(z) = zset_mut(idx)
                && len > 600
            {
                let (lo, hi) = (len / 4, len / 2);
                let gone = zset_ref(idx).members_in(lo as usize, hi as usize);
                let ZSet::Large(z2) = zset_mut(idx) else { unreachable!() };
                let _ = z;
                z2.remove_range(idx, lo, hi);
                for m in gone {
                    model.remove(&m);
                }
            }
            kept.push((idx, model));
        }
        while lazy::step(1 << 20) {}
        let span_before = btree::compact::holes_bytes();
        let mut c = btree::compact::want(true).expect("passe");
        while btree::compact::step(&mut c, 64).is_none() {}
        assert!(c.moved > 0);
        assert!(btree::compact::holes_bytes() < span_before.max(1), "trous {} (avant {span_before})", btree::compact::holes_bytes());
        // puis l'index
        let index_before = index::compact::holes_bytes();
        let mut c = index::compact::want(true).expect("passe de l'index");
        while index::compact::step(&mut c, 64).is_none() {}
        assert!(c.moved > 0);
        assert!(index::compact::holes_bytes() < index_before.max(1));
        for (idx, model) in &kept {
            zset_ref(*idx).verify().unwrap();
            for (m, &sc) in model {
                assert_eq!(zset_ref(*idx).score(m), Some(sc));
            }
            let got: Vec<(u64, Vec<u8>)> = zset_ref(*idx).iter().map(|(s, m)| (s, m.to_vec())).collect();
            let mut want: Vec<(u64, Vec<u8>)> = model.iter().map(|(m, &s)| (s, m.clone())).collect();
            want.sort();
            assert_eq!(got, want);
            // encore modifiable après déplacement
            zset_mut(*idx).add(encode_score(1.5), b"apres-compactage").unwrap();
            zset_ref(*idx).verify().unwrap();
        }
        for (idx, _) in kept {
            free_zset(idx);
        }
        while lazy::step(1 << 20) {}
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
    fn score_format_matches_display() {
        for f in [0.0, 1.0, -1.0, 42.0, -987654321.0, 1e14, 999999999999999.0, 1.5, -2.25, 1e15, 1e300, f64::INFINITY, f64::NEG_INFINITY, 1e-7, 0.1, 1.0 / 3.0, -2.0 / 3.0, 1e-20, 1e-300, 5e-324, f64::MAX, 1e16 + 0.5] {
            assert_eq!(format_score(f).as_slice(), f.to_string().as_bytes(), "{f}");
        }
        // égalité entre deux plus courts : zmij arrondit au pair, comme Valkey
        assert_eq!(format_score(1658206780088562.25).as_slice(), b"1658206780088562.2");
        // bits tirés au hasard, tous les exposants : mêmes chiffres les plus courts que
        // `to_string` (relus à l'identique, même longueur, sans exposant)
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..200_000 {
            let f = f64::from_bits(rng.next());
            if !f.is_nan() {
                let (got, want) = (format_score(f), f.to_string());
                let got = std::str::from_utf8(got.as_slice()).unwrap();
                assert_eq!(got.parse::<f64>().unwrap(), f, "{want}");
                assert_eq!(got.len(), want.len(), "{want} : {got}");
                assert!(!got.contains('e'), "{got}");
            }
        }
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

#[cfg(test)]
mod breakdown {
    /// Décomposition mémoire (mesure, `--ignored`) : 1000 ZSet × 1000 membres de 15 o.
    #[test]
    #[ignore]
    fn memory_breakdown() {
        crate::value::init_slabs();
        let mut zs: Vec<super::ZSet> = (0..1000).map(|_| super::ZSet::new()).collect();
        let pool0 = radixox_lib::pool::live_bytes();
        for (k, z) in zs.iter_mut().enumerate() {
            for j in 0..1000u32 {
                let _ = k;
                z.add(super::encode_score((j * 7 % 1000) as f64), format!("member:{j:07}").as_bytes()).unwrap();
            }
        }
        let n = 1_000_000f64;
        let (h, table) = radixox_lib::handle::stats();
        let pool = radixox_lib::pool::live_bytes() - pool0;
        let bt = super::btree::slab_nodes() as f64 * 512.0;
        let ix = super::index::slab_nodes() as f64 * 128.0;
        let (mut leaves, mut internals, mut buckets, mut dirs) = (0, 0, 0, 0);
        for z in &zs {
            if let super::ZSet::Large(l) = z {
                let (a, b) = l.tree.node_counts();
                let (c, d) = l.index.node_counts();
                (leaves, internals, buckets, dirs) = (leaves + a, internals + b, buckets + c, dirs + d);
            }
        }
        eprintln!("poignées {h}, table {:.1} o/m, objets (pool) {:.1} o/m", table as f64 / n, pool as f64 / n);
        eprintln!(
            "B+tree {:.1} o/m ({leaves} feuilles, remplissage {:.2}, {internals} internes) ; index {:.1} o/m ({buckets} seaux, remplissage {:.2}, {dirs} rép.)",
            bt / n,
            n / (leaves as f64 * 41.0),
            ix / n,
            n / (buckets as f64 * 9.0)
        );
        eprintln!("total {:.1} o/m", (table as f64 + pool as f64 + bt + ix) / n);
    }
}
