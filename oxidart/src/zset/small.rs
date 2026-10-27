//! Petite forme du ZSet (ANALYSE_ART_UNIFIE.md §13.3) : au plus [`MAX_MEMBERS`] membres
//! d'au plus [`MAX_MEMBER_LEN`] octets, dans un seul bloc en colonnes.
//!
//! ```text
//! [n u8][largeur u8][cap u8][pad][octets utilisés u16][octets réservés u16]
//! [scores : largeur × cap][longueurs : u8 × cap][octets des membres]
//! ```
//!
//! - Scores triés croissants, largeur fixe par bloc : entiers signés sur 2 ou 4 o si tous
//!   les scores le permettent, sinon 8 o (`u64` ordonné, [`super::encode_score`]). La
//!   largeur ne fait que grandir. Recherche dichotomique par score.
//! - Ordre `(score, membre)` ; à score égal, ordre lexicographique des octets.
//! - Décalage d'un membre = somme des longueurs avant lui.
//! - Un seul propriétaire (la valeur du nœud oxidart) : objet de structure du pool, déplacé
//!   d'un bloc par le GC, copié en profondeur par l'image de save.
//!
//! L'API prend et rend des scores encodés (`u64` ordonné).

use std::cmp::Ordering;
use std::ptr;

use radixox_lib::pool;

use super::{decode_score, encode_score};

/// Au-delà, le ZSet passe en grande forme (seuils de Redis, `zset-max-listpack-*`).
pub const MAX_MEMBERS: usize = 128;
pub const MAX_MEMBER_LEN: usize = 64;
const HEADER: usize = 8;

/// Largeur des scores pour `s` seul : 2 ou 4 si entier qui y tient, sinon 8.
#[inline]
fn width_for(s: u64) -> u8 {
    let f = decode_score(s);
    if f.fract() != 0.0 || !f.is_finite() {
        8
    } else if (i16::MIN as f64..=i16::MAX as f64).contains(&f) {
        2
    } else if (i32::MIN as f64..=i32::MAX as f64).contains(&f) {
        4
    } else {
        8
    }
}

#[inline]
unsafe fn hdr(p: *const u8) -> (usize, usize, usize, usize, usize) {
    unsafe {
        (
            *p as usize,
            *p.add(1) as usize,
            *p.add(2) as usize,
            (p.add(4) as *const u16).read() as usize,
            (p.add(6) as *const u16).read() as usize,
        )
    }
}

#[inline]
fn bytes_for(width: usize, cap: usize, bcap: usize) -> usize {
    HEADER + cap * (width + 1) + bcap
}

/// Bloc d'un petit ZSet. Possède son bloc ; [`ZSmall::free`] le rend (pas de `Drop` :
/// il vit dans une union de valeur).
#[derive(Debug)]
#[repr(transparent)]
pub struct ZSmall(pub(crate) *mut u8);

impl ZSmall {
    pub fn new() -> Self {
        Self::alloc(2, 0, 0, 0, 0)
    }

    fn alloc(width: usize, cap: usize, bcap: usize, n: usize, used: usize) -> Self {
        let size = bytes_for(width, cap, bcap).next_multiple_of(8);
        let bcap = size - bytes_for(width, cap, 0);
        let p = unsafe { pool::struct_alloc(size) };
        unsafe {
            *p = n as u8;
            *p.add(1) = width as u8;
            *p.add(2) = cap as u8;
            *p.add(3) = 0;
            (p.add(4) as *mut u16).write(used as u16);
            (p.add(6) as *mut u16).write(bcap as u16);
        }
        Self(p)
    }

    /// Taille du bloc (GC, mesures).
    #[inline]
    pub fn block_bytes(&self) -> usize {
        let (_, w, cap, _, bcap) = unsafe { hdr(self.0) };
        bytes_for(w, cap, bcap)
    }

    #[inline]
    pub fn len(&self) -> usize {
        unsafe { *self.0 as usize }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn width(&self) -> usize {
        unsafe { *self.0.add(1) as usize }
    }

    #[inline]
    fn cap(&self) -> usize {
        unsafe { *self.0.add(2) as usize }
    }

    #[inline]
    fn lens_ptr(&self) -> *mut u8 {
        unsafe { self.0.add(HEADER + self.cap() * self.width()) }
    }

    #[inline]
    fn bytes_ptr(&self) -> *mut u8 {
        unsafe { self.lens_ptr().add(self.cap()) }
    }

    #[inline]
    fn lens(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.lens_ptr(), self.len()) }
    }

    /// Score encodé du rang `i`.
    #[inline]
    pub fn score(&self, i: usize) -> u64 {
        debug_assert!(i < self.len());
        unsafe { read_score(self.0.add(HEADER), self.width(), i) }
    }

    /// Décalage des octets du membre `i`.
    #[inline]
    fn offset(&self, i: usize) -> usize {
        self.lens()[..i].iter().map(|&l| l as usize).sum()
    }

    /// Membre du rang `i`.
    #[inline]
    pub fn member(&self, i: usize) -> &[u8] {
        let off = self.offset(i);
        let l = self.lens()[i] as usize;
        unsafe { std::slice::from_raw_parts(self.bytes_ptr().add(off), l) }
    }

    /// `(score, membre)` dans l'ordre, à partir du rang `from`.
    pub fn iter_from(&self, from: usize) -> Iter<'_> {
        let (n, used) = (self.len(), unsafe { hdr(self.0) }.3);
        let from = from.min(n);
        Iter { z: self, i: from, off: self.offset(from), j: n, end: used }
    }

    pub fn iter(&self) -> Iter<'_> {
        self.iter_from(0)
    }

    /// Rang du membre `m` : balayage des longueurs, octets comparés aux seuls candidats
    /// de la bonne longueur.
    pub fn find(&self, m: &[u8]) -> Option<usize> {
        let base = self.bytes_ptr();
        let mut off = 0;
        for (i, &l) in self.lens().iter().enumerate() {
            let l = l as usize;
            if l == m.len() && unsafe { std::slice::from_raw_parts(base.add(off), l) } == m {
                return Some(i);
            }
            off += l;
        }
        None
    }

    /// Score encodé de `m`.
    pub fn get(&self, m: &[u8]) -> Option<u64> {
        self.find(m).map(|i| self.score(i))
    }

    /// Premier rang dont l'entrée n'est pas avant la sonde ; `probe(s, membre)` = la sonde
    /// comparée à l'entrée. Dichotomie sur les scores (les octets ne sont lus qu'aux
    /// ex-aequo).
    pub fn lower_bound(&self, mut probe: impl FnMut(u64, &[u8]) -> Ordering) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if probe(self.score(mid), self.member(mid)) == Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Rang d'insertion de `(s, m)`.
    fn position(&self, s: u64, m: &[u8]) -> usize {
        self.lower_bound(|es, em| (s, m).cmp(&(es, em)))
    }

    /// Ajoute `(s, m)`, `m` absent. Faux si le bloc ne peut plus le prendre (plus de
    /// [`MAX_MEMBERS`] ou membre trop long) : l'appelant passe en grande forme.
    pub fn insert(&mut self, s: u64, m: &[u8]) -> bool {
        debug_assert!(self.find(m).is_none());
        let (n, w, cap, used, bcap) = unsafe { hdr(self.0) };
        if n >= MAX_MEMBERS || m.len() > MAX_MEMBER_LEN {
            return false;
        }
        let pos = self.position(s, m);
        let nw = w.max(width_for(s) as usize);
        if nw == w && n < cap && used + m.len() <= bcap {
            unsafe {
                let sc = self.0.add(HEADER);
                ptr::copy(sc.add(pos * w), sc.add((pos + 1) * w), (n - pos) * w);
                write_score(sc, w, pos, s);
                let lens = self.lens_ptr();
                let off = self.offset(pos);
                ptr::copy(lens.add(pos), lens.add(pos + 1), n - pos);
                *lens.add(pos) = m.len() as u8;
                let b = self.bytes_ptr();
                ptr::copy(b.add(off), b.add(off + m.len()), used - off);
                ptr::copy_nonoverlapping(m.as_ptr(), b.add(off), m.len());
                *self.0 = (n + 1) as u8;
                (self.0.add(4) as *mut u16).write((used + m.len()) as u16);
            }
            return true;
        }
        // Réécriture du bloc : plus de place ou scores plus larges.
        let ncap = if n < cap { cap } else { (n + 1 + n / 4).min(MAX_MEMBERS) };
        let need = used + m.len();
        let nbcap = if need <= bcap { bcap } else { need + need / 4 };
        let new = Self::alloc(nw, ncap, nbcap, n + 1, need);
        unsafe {
            let (src, dst) = (self.0.add(HEADER), new.0.add(HEADER));
            for i in 0..pos {
                write_score(dst, nw, i, read_score(src, w, i));
            }
            write_score(dst, nw, pos, s);
            for i in pos..n {
                write_score(dst, nw, i + 1, read_score(src, w, i));
            }
            let (ol, nl) = (self.lens_ptr(), new.lens_ptr());
            ptr::copy_nonoverlapping(ol, nl, pos);
            *nl.add(pos) = m.len() as u8;
            ptr::copy_nonoverlapping(ol.add(pos), nl.add(pos + 1), n - pos);
            let off = self.offset(pos);
            let (ob, nb) = (self.bytes_ptr(), new.bytes_ptr());
            ptr::copy_nonoverlapping(ob, nb, off);
            ptr::copy_nonoverlapping(m.as_ptr(), nb.add(off), m.len());
            ptr::copy_nonoverlapping(ob.add(off), nb.add(off + m.len()), used - off);
        }
        std::mem::replace(self, new).free();
        true
    }

    /// Prépare la place de `n_add` membres de `bytes_add` octets en tout, scores tenant
    /// sur `width` octets : une seule réécriture du bloc au lieu d'une par ajout (un ZADD
    /// à plusieurs membres ; sans ça, chaque croissance laisse un trou dans le pool jusqu'au
    /// GC). Sans effet si la place y est déjà, ou si le seuil serait dépassé.
    pub fn reserve(&mut self, n_add: usize, bytes_add: usize, scores: impl Iterator<Item = u64>) {
        let (n, w, cap, used, bcap) = unsafe { hdr(self.0) };
        let nw = scores.fold(w, |acc, s| acc.max(width_for(s) as usize));
        let ncap = (n + n_add).min(MAX_MEMBERS);
        let nbcap = used + bytes_add;
        if (ncap <= cap && nbcap <= bcap && nw == w) || n + n_add > MAX_MEMBERS {
            return;
        }
        let new = Self::alloc(nw, ncap.max(cap), nbcap.max(bcap), n, used);
        unsafe {
            let (src, dst) = (self.0.add(HEADER), new.0.add(HEADER));
            for i in 0..n {
                write_score(dst, nw, i, read_score(src, w, i));
            }
            ptr::copy_nonoverlapping(self.lens_ptr(), new.lens_ptr(), n);
            ptr::copy_nonoverlapping(self.bytes_ptr(), new.bytes_ptr(), used);
        }
        std::mem::replace(self, new).free();
    }

    /// Retire le rang `i` (le bloc garde sa place).
    pub fn remove_at(&mut self, i: usize) {
        let (n, w, _, used, _) = unsafe { hdr(self.0) };
        debug_assert!(i < n);
        let off = self.offset(i);
        let l = self.lens()[i] as usize;
        unsafe {
            let sc = self.0.add(HEADER);
            ptr::copy(sc.add((i + 1) * w), sc.add(i * w), (n - i - 1) * w);
            let lens = self.lens_ptr();
            ptr::copy(lens.add(i + 1), lens.add(i), n - i - 1);
            let b = self.bytes_ptr();
            ptr::copy(b.add(off + l), b.add(off), used - off - l);
            *self.0 = (n - 1) as u8;
            (self.0.add(4) as *mut u16).write((used - l) as u16);
        }
    }

    /// Retire `m` ; rend son score.
    pub fn remove(&mut self, m: &[u8]) -> Option<u64> {
        let i = self.find(m)?;
        let s = self.score(i);
        self.remove_at(i);
        Some(s)
    }

    /// Change le score du rang `i` (retrait puis insertion, jamais de passage de forme).
    pub fn set_score(&mut self, i: usize, s: u64) {
        let m = self.member(i).to_vec();
        self.remove_at(i);
        let ok = self.insert(s, &m);
        debug_assert!(ok);
    }

    /// Copie profonde (image de save, `Value` possédée).
    pub fn deep_clone(&self) -> Self {
        let size = self.block_bytes();
        let p = unsafe { pool::struct_alloc(size) };
        unsafe { ptr::copy_nonoverlapping(self.0, p, size) };
        Self(p)
    }

    /// Rend le bloc ; `self` ne doit plus servir.
    pub fn release(&mut self) {
        let size = self.block_bytes();
        unsafe { pool::struct_free(self.0, size) };
        self.0 = ptr::null_mut();
    }

    /// Rend le bloc.
    pub fn free(mut self) {
        self.release();
    }
}

/// Itérateur des deux bouts : rang et décalage de chaque bout.
pub struct Iter<'a> {
    z: &'a ZSmall,
    i: usize,
    off: usize,
    j: usize,
    end: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (u64, &'a [u8]);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.i == self.j {
            return None;
        }
        let l = self.z.lens()[self.i] as usize;
        let m = unsafe { std::slice::from_raw_parts(self.z.bytes_ptr().add(self.off), l) };
        let s = self.z.score(self.i);
        self.i += 1;
        self.off += l;
        Some((s, m))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.j - self.i, Some(self.j - self.i))
    }
}

impl DoubleEndedIterator for Iter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.i == self.j {
            return None;
        }
        self.j -= 1;
        let l = self.z.lens()[self.j] as usize;
        self.end -= l;
        let m = unsafe { std::slice::from_raw_parts(self.z.bytes_ptr().add(self.end), l) };
        Some((self.z.score(self.j), m))
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl Default for ZSmall {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
unsafe fn read_score(sc: *const u8, w: usize, i: usize) -> u64 {
    unsafe {
        match w {
            2 => encode_score((sc.add(i * 2) as *const i16).read_unaligned() as f64),
            4 => encode_score((sc.add(i * 4) as *const i32).read_unaligned() as f64),
            _ => (sc.add(i * 8) as *const u64).read_unaligned(),
        }
    }
}

#[inline]
unsafe fn write_score(sc: *mut u8, w: usize, i: usize, s: u64) {
    unsafe {
        match w {
            2 => (sc.add(i * 2) as *mut i16).write_unaligned(decode_score(s) as i16),
            4 => (sc.add(i * 4) as *mut i32).write_unaligned(decode_score(s) as i32),
            _ => (sc.add(i * 8) as *mut u64).write_unaligned(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn check(z: &ZSmall, model: &[(u64, Vec<u8>)]) {
        let got: Vec<(u64, Vec<u8>)> = z.iter().map(|(s, m)| (s, m.to_vec())).collect();
        assert_eq!(got, model);
        for (i, (s, m)) in model.iter().enumerate() {
            assert_eq!(z.find(m), Some(i));
            assert_eq!(z.get(m), Some(*s));
        }
        let back: Vec<(u64, Vec<u8>)> = z.iter().rev().map(|(s, m)| (s, m.to_vec())).collect();
        assert!(back.iter().rev().eq(model.iter()));
    }

    /// Contre un `Vec` trié : scores entiers petits, grands, flottants (largeur qui
    /// grandit), ex-aequo, membres vides et au maximum, remplissage jusqu'au seuil.
    #[test]
    fn random_against_sorted_vec() {
        for seed in 1..=6u64 {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
            let mut z = ZSmall::new();
            let mut model: Vec<(u64, Vec<u8>)> = Vec::new();
            for step in 0..4000 {
                let f = match rng.below(10) {
                    0..=4 => (rng.below(20) as f64) - 10.0,
                    5 | 6 => rng.below(1 << 20) as f64 - (1 << 19) as f64,
                    7 if seed > 3 => (rng.next() >> 11) as f64 * 1e-3,
                    8 if seed > 3 => -1e300,
                    _ => 0.0,
                };
                let s = encode_score(f);
                let m: Vec<u8> = {
                    let len = match rng.below(20) {
                        0 => 0,
                        1 => MAX_MEMBER_LEN,
                        _ => 1 + rng.below(3) as usize,
                    };
                    (0..len).map(|_| b'a' + rng.below(4) as u8).collect()
                };
                match model.iter().position(|(_, x)| *x == m) {
                    Some(i) if rng.below(2) == 0 => {
                        assert_eq!(z.remove(&m), Some(model[i].0));
                        model.remove(i);
                    }
                    Some(i) => {
                        let zi = z.find(&m).unwrap();
                        z.set_score(zi, s);
                        model.remove(i);
                        let p = model.partition_point(|(es, em)| (*es, em.as_slice()) < (s, m.as_slice()));
                        model.insert(p, (s, m));
                    }
                    None => {
                        let ok = z.insert(s, &m);
                        assert_eq!(ok, model.len() < MAX_MEMBERS);
                        if ok {
                            let p = model.partition_point(|(es, em)| (*es, em.as_slice()) < (s, m.as_slice()));
                            model.insert(p, (s, m));
                        }
                    }
                }
                if step % 7 == 0 {
                    // place préparée (ZADD à plusieurs membres), parfois avec des scores
                    // plus larges que le bloc
                    let k = rng.below(6) as usize;
                    let wide = [encode_score(0.25), encode_score(70_000.0), encode_score(1.0)];
                    z.reserve(k, k * rng.below(40) as usize, wide.into_iter().take(rng.below(4) as usize));
                    check(&z, &model);
                }
                if step % 50 == 0 {
                    check(&z, &model);
                    // borne par score seul
                    let x = encode_score(rng.below(20) as f64 - 10.0);
                    let lb = z.lower_bound(|es, _| if x <= es { Ordering::Less } else { Ordering::Greater });
                    assert_eq!(lb, model.partition_point(|(es, _)| *es < x));
                }
            }
            check(&z, &model);
            let c = z.deep_clone();
            check(&c, &model);
            c.free();
            z.free();
        }
    }

    #[test]
    fn widths_and_size() {
        let mut z = ZSmall::new();
        for i in 0..10u8 {
            assert!(z.insert(encode_score(i as f64), format!("member:{i:08}").as_bytes()));
        }
        assert_eq!(z.width(), 2);
        // 10 membres de ~15 o, scores sur 2 o : ~18 o/membre + en-tête et marge
        assert!(z.block_bytes() <= 8 + 10 * 18 + 64, "{}", z.block_bytes());
        z.insert(encode_score(70_000.0), b"x");
        assert_eq!(z.width(), 4);
        z.insert(encode_score(0.5), b"y");
        assert_eq!(z.width(), 8);
        assert_eq!(decode_score(z.get(b"x").unwrap()), 70_000.0);
        assert_eq!(decode_score(z.score(0)), 0.0);
        assert!(!z.insert(1, &[0; MAX_MEMBER_LEN + 1]), "membre trop long");
        z.free();
    }
}
