//! Valeur bitset (BITSET_DESIGN.md §10) : deux formes pour tout le bitset, classique
//! (`Vec<u64>`) ou compacte (B+tree de `radixox_lib::bitset`), et **bascule tranche par
//! tranche** de l'une à l'autre avec les deux formes vivantes en même temps.
//!
//! Pendant une bascule, l'ancienne forme fait foi sur `[c, ext)` (`c` : curseur, `ext` :
//! étendue de l'ancienne forme), la nouvelle sur `[0, c) ∪ [ext, 2³²)`. Chaque requête est
//! routée vers la forme dont le territoire contient l'offset ; un parcours dans l'ordre
//! enchaîne trois segments. Rien n'est effacé dans l'ancienne forme : ce qui est sous `c`
//! y est périmé, jamais lu, et part avec elle à la fin.
//!
//! En-têtes dans un HiSlab (la valeur oxidart porte l'idx), barrière de save par bitset
//! entier (`save.rs`).

#[allow(unused_imports)]
use crate::prelude::*;
use core::mem::MaybeUninit;

use radixox_lib::bitset::{self as tree, BitsetId, WordBits};
use radixox_lib::hislab::HiSlab;

pub(crate) mod op;
pub(crate) mod save;
#[cfg(test)]
mod test;

/// Fin de l'espace des offsets (exclue).
pub const END: u64 = 1 << 32;

/// Budget d'une tranche de bascule (1 par mot classique lu, 8 par offset inséré dans
/// l'arbre, 1 par offset recopié dans le tableau) : ~30 à 50 µs.
const STEP_BUDGET: u32 = 4096;

/// Budget d'une tranche. En test, petit (les bascules restent en cours d'une opération à
/// l'autre) et réglable.
#[cfg(not(test))]
#[inline(always)]
fn step_budget() -> u32 {
    STEP_BUDGET
}
#[cfg(test)]
pub(crate) static TEST_STEP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(24);
#[cfg(test)]
fn step_budget() -> u32 {
    TEST_STEP.load(core::sync::atomic::Ordering::Relaxed)
}

// ─── Coûts et règle de forme ──────────────────────────────────────────────────

/// Octets estimés de la forme compacte (~10,7 o par offset construit en bloc, ~13 en
/// insertions au hasard ; une feuille au minimum).
#[inline]
fn compact_cost(count: u64) -> u64 {
    (12 * count).max(64)
}

/// Octets de la forme classique couvrant `[0, bits)`.
#[inline]
fn classic_cost(bits: u64) -> u64 {
    bits.div_ceil(64) * 8
}

/// Hystérésis ×4 : classique si elle coûte au plus la moitié de la compacte, compacte si
/// elle coûte au plus la moitié de la classique.
#[inline]
fn want_classic(count: u64, hi: u64) -> bool {
    2 * classic_cost(hi) <= compact_cost(count)
}
#[inline]
fn want_compact(count: u64, bits: u64) -> bool {
    2 * compact_cost(count) <= classic_cost(bits)
}

// ─── Une forme ────────────────────────────────────────────────────────────────

pub(crate) enum Form {
    Compact(BitsetId),
    /// Bit `i` = bit `i % 64` du mot `i / 64`.
    Classic(Vec<u64>),
}

#[inline]
fn bit(words: &[u64], off: u64) -> bool {
    words.get((off >> 6) as usize).is_some_and(|w| w & (1 << (off & 63)) != 0)
}

/// Masque des bits `[from, to)` du mot `w` (`from < to`, même mot ou bornes de mot).
#[inline]
fn word_mask(w: u64, from: u64, to: u64) -> u64 {
    let base = w << 6;
    let lo = from.saturating_sub(base).min(64);
    let hi = to.saturating_sub(base).min(64);
    if lo >= hi {
        return 0;
    }
    let up = if hi == 64 { u64::MAX } else { (1u64 << hi) - 1 };
    up & (u64::MAX << lo)
}

impl Form {
    /// Étendue représentable sans agrandir : `[0, ext)`.
    #[inline]
    fn ext(&self) -> u64 {
        match self {
            Form::Compact(_) => END,
            Form::Classic(w) => w.len() as u64 * 64,
        }
    }

    #[inline]
    fn get(&self, off: u32) -> bool {
        match self {
            Form::Compact(id) => id.get(off),
            Form::Classic(w) => bit(w, off as u64),
        }
    }

    /// Met à 1, agrandit le tableau si besoin. Rend l'ancienne valeur.
    #[inline]
    fn set(&mut self, off: u32) -> bool {
        match self {
            Form::Compact(id) => id.set(off),
            Form::Classic(w) => {
                let i = (off >> 6) as usize;
                if i >= w.len() {
                    w.resize(i + 1, 0);
                }
                let m = 1u64 << (off & 63);
                let old = w[i] & m != 0;
                w[i] |= m;
                old
            }
        }
    }

    #[inline]
    fn clear(&mut self, off: u32) -> bool {
        match self {
            Form::Compact(id) => id.clear(off),
            Form::Classic(w) => match w.get_mut((off >> 6) as usize) {
                Some(x) => {
                    let m = 1u64 << (off & 63);
                    let old = *x & m != 0;
                    *x &= !m;
                    old
                }
                None => false,
            },
        }
    }

    /// Offsets à 1 de `[from, to)` dans l'ordre, jusqu'à ce que `f` rende `false` (rend
    /// alors `false`).
    fn scan(&self, from: u64, to: u64, f: &mut impl FnMut(u32) -> bool) -> bool {
        if from >= to {
            return true;
        }
        match self {
            Form::Compact(id) => {
                let mut stopped = false;
                id.for_each_from(from as u32, |k| {
                    if k as u64 >= to {
                        return false;
                    }
                    stopped = !f(k);
                    !stopped
                });
                !stopped
            }
            Form::Classic(w) => {
                let to = to.min(w.len() as u64 * 64);
                if from >= to {
                    return true;
                }
                for wi in (from >> 6)..=((to - 1) >> 6) {
                    let mut x = w[wi as usize] & word_mask(wi, from, to);
                    while x != 0 {
                        if !f(((wi << 6) | x.trailing_zeros() as u64) as u32) {
                            return false;
                        }
                        x &= x - 1;
                    }
                }
                true
            }
        }
    }

    /// OR des bits de `[from, to)` dans `buf`, qui couvre `[base, base + 64·len)` (`base`
    /// multiple de 64, `[from, to)` inclus dans ce que couvre `buf`).
    fn fill(&self, from: u64, to: u64, base: u64, buf: &mut [u64]) {
        if from >= to {
            return;
        }
        match self {
            Form::Classic(w) => {
                let to = to.min(w.len() as u64 * 64);
                if from >= to {
                    return;
                }
                for wi in (from >> 6)..=((to - 1) >> 6) {
                    buf[(wi - (base >> 6)) as usize] |= w[wi as usize] & word_mask(wi, from, to);
                }
            }
            Form::Compact(_) => {
                self.scan(from, to, &mut |k| {
                    let r = k as u64 - base;
                    buf[(r >> 6) as usize] |= 1 << (r & 63);
                    true
                });
            }
        }
    }

    /// Bits à 1 de `[from, to)`.
    fn count_in(&self, from: u64, to: u64) -> u64 {
        match self {
            Form::Classic(w) => {
                let to = to.min(w.len() as u64 * 64);
                if from >= to {
                    return 0;
                }
                let (a, b) = (from >> 6, (to - 1) >> 6);
                if a == b {
                    return (w[a as usize] & word_mask(a, from, to)).count_ones() as u64;
                }
                (w[a as usize] & word_mask(a, from, to)).count_ones() as u64
                    + tree::popcount_words(&w[a as usize + 1..b as usize])
                    + (w[b as usize] & word_mask(b, from, to)).count_ones() as u64
            }
            Form::Compact(_) => {
                let mut n = 0;
                self.scan(from, to, &mut |_| {
                    n += 1;
                    true
                });
                n
            }
        }
    }

    /// Premier bit à 0 de `[from, to)`.
    fn next_clear(&self, from: u64, to: u64) -> Option<u64> {
        if from >= to {
            return None;
        }
        match self {
            Form::Classic(w) => {
                let end = w.len() as u64 * 64;
                if from < end {
                    let last = (to.min(end) - 1) >> 6;
                    for wi in (from >> 6)..=last {
                        let x = !w[wi as usize] & word_mask(wi, from, to);
                        if x != 0 {
                            return Some((wi << 6) | x.trailing_zeros() as u64);
                        }
                    }
                }
                let x = from.max(end);
                (x < to).then_some(x)
            }
            Form::Compact(_) => {
                let mut x = from;
                self.scan(from, to, &mut |k| {
                    if k as u64 > x {
                        return false;
                    }
                    x = k as u64 + 1;
                    true
                });
                (x < to).then_some(x)
            }
        }
    }

    /// Plus grand offset à 1 (périmés compris pour une ancienne forme).
    fn last(&self) -> Option<u32> {
        match self {
            Form::Compact(id) => id.last(),
            Form::Classic(w) => {
                let i = w.iter().rposition(|&x| x != 0)?;
                Some(((i as u64) << 6 | (63 - w[i].leading_zeros()) as u64) as u32)
            }
        }
    }

    /// Libère la forme ; un arbre est rendu par tranches ([`tree::free_step`]).
    fn release(self) {
        if let Form::Compact(id) = self {
            id.destroy_lazy();
            // un petit arbre part tout de suite
            tree::free_step(64);
        }
    }
}

// ─── Le bitset ────────────────────────────────────────────────────────────────

pub(crate) struct Bitset {
    /// Nouvelle forme (la seule hors bascule).
    cur: Form,
    /// Ancienne forme pendant une bascule.
    old: Option<Form>,
    /// Curseur de la bascule : l'ancienne forme fait foi sur `[c, ext)`.
    c: u64,
    ext: u64,
    /// Bits à 1.
    count: u64,
    /// Dernier offset à 1 + 1 (0 : vide), exact.
    hi: u64,
    /// Dans la liste des bascules à avancer.
    queued: bool,
}

/// Image T0 d'un bitset pour la save : offsets ou mots, la plus petite.
pub(crate) enum Image {
    Offsets(Vec<u32>),
    Words(Vec<u64>),
}

impl Bitset {
    fn with(cur: Form, count: u64, hi: u64) -> Self {
        Self { cur, old: None, c: 0, ext: 0, count, hi, queued: false }
    }

    /// Bitset des offsets **strictement croissants** `offs`, dans la forme la moins chère.
    pub(crate) fn from_sorted(offs: &[u32]) -> Self {
        let count = offs.len() as u64;
        let hi = offs.last().map_or(0, |&l| l as u64 + 1);
        if classic_cost(hi) <= compact_cost(count) {
            let mut w = vec![0u64; hi.div_ceil(64) as usize];
            for &k in offs {
                w[(k >> 6) as usize] |= 1 << (k & 63);
            }
            Self::with(Form::Classic(w), count, hi)
        } else {
            Self::with(Form::Compact(BitsetId::from_sorted(offs)), count, hi)
        }
    }

    #[inline]
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// Dernier offset à 1.
    #[inline]
    pub(crate) fn last(&self) -> Option<u32> {
        self.hi.checked_sub(1).map(|l| l as u32)
    }

    /// Une bascule est en cours.
    #[inline]
    pub(crate) fn migrating(&self) -> bool {
        self.old.is_some()
    }

    /// Forme courante (tests, `OBJECT ENCODING`) : `true` en classique.
    pub(crate) fn is_classic(&self) -> bool {
        matches!(self.cur, Form::Classic(_))
    }

    #[inline]
    fn in_old(&self, off: u64) -> bool {
        self.old.is_some() && off >= self.c && off < self.ext
    }

    pub(crate) fn get(&self, off: u32) -> bool {
        if self.in_old(off as u64) {
            self.old.as_ref().unwrap().get(off)
        } else {
            self.cur.get(off)
        }
    }

    /// Met le bit à 1. Rend l'ancienne valeur. `step` : la bascule peut avancer.
    pub(crate) fn set(&mut self, off: u32, step: bool) -> bool {
        let o = off as u64;
        if self.old.is_none()
            && let Form::Classic(w) = &self.cur
            && o >= w.len() as u64 * 64
            && want_compact(self.count + 1, o + 1)
        {
            // au-delà du tableau, et loin : compacte plutôt que des Mio de zéros
            self.start(Form::Compact(BitsetId::create()), false);
        }
        let old = if self.in_old(o) { self.old.as_mut().unwrap().set(off) } else { self.cur.set(off) };
        if !old {
            self.count += 1;
            self.hi = self.hi.max(o + 1);
            self.after_write(step);
        }
        old
    }

    /// Met le bit à 0. Rend l'ancienne valeur. Un bitset vidé reste valide (à l'appelant
    /// de supprimer la clé).
    pub(crate) fn clear(&mut self, off: u32, step: bool) -> bool {
        let o = off as u64;
        let old = if self.in_old(o) { self.old.as_mut().unwrap().clear(off) } else { self.cur.clear(off) };
        if old {
            self.count -= 1;
            if o + 1 == self.hi {
                self.hi = self.find_last().map_or(0, |l| l as u64 + 1);
            }
            self.after_write(step);
        }
        old
    }

    /// Dernier offset à 1, recalculé depuis les formes.
    fn find_last(&self) -> Option<u32> {
        let cur = self.cur.last();
        match &self.old {
            None => cur,
            Some(old) => {
                // ce qui est sous `c` dans l'ancienne forme est périmé
                let o = old.last().filter(|&l| l as u64 >= self.c && (l as u64) < self.ext);
                cur.max(o)
            }
        }
    }

    fn after_write(&mut self, step: bool) {
        if self.old.is_some() {
            if step {
                self.step(step_budget());
            }
            return;
        }
        match &self.cur {
            Form::Compact(_) if want_classic(self.count, self.hi) => {
                let w = vec![0u64; self.hi.div_ceil(64) as usize];
                self.start(Form::Classic(w), step);
            }
            Form::Classic(w) if want_compact(self.count, w.len() as u64 * 64) => {
                self.start(Form::Compact(BitsetId::create()), step);
            }
            _ => {}
        }
    }

    /// Démarre une bascule vers `new` ; une première tranche tout de suite si `step` (une
    /// petite bascule finit là).
    fn start(&mut self, new: Form, step: bool) {
        debug_assert!(self.old.is_none());
        let old = core::mem::replace(&mut self.cur, new);
        self.ext = old.ext();
        self.c = 0;
        self.old = Some(old);
        if step {
            self.step(step_budget());
        }
    }

    /// Une tranche de bascule (au plus `budget`). Vrai quand la bascule est finie (ou s'il
    /// n'y en avait pas).
    pub(crate) fn step(&mut self, mut budget: u32) -> bool {
        let Some(old) = &self.old else { return true };
        let done = match (old, &mut self.cur) {
            (Form::Classic(w), Form::Compact(id)) => {
                let mut wi = (self.c >> 6) as usize;
                let end = w.len();
                while wi < end && budget > 0 {
                    let mut x = w[wi];
                    while x != 0 {
                        id.set(((wi << 6) as u64 | x.trailing_zeros() as u64) as u32);
                        x &= x - 1;
                        budget = budget.saturating_sub(8);
                    }
                    budget = budget.saturating_sub(1);
                    wi += 1;
                }
                self.c = (wi as u64) << 6;
                wi == end
            }
            (Form::Compact(id), Form::Classic(w)) => {
                let mut next = None;
                id.for_each_from(self.c as u32, |k| {
                    if budget == 0 {
                        next = Some(k);
                        return false;
                    }
                    let i = (k >> 6) as usize;
                    if i >= w.len() {
                        w.resize(i + 1, 0);
                    }
                    w[i] |= 1 << (k & 63);
                    budget -= 1;
                    true
                });
                match next {
                    Some(k) => {
                        self.c = k as u64;
                        false
                    }
                    None => true,
                }
            }
            _ => unreachable!("bascule entre deux formes identiques"),
        };
        if done {
            self.old.take().unwrap().release();
            self.c = 0;
            self.ext = 0;
        }
        done
    }

    /// Segments `(forme, from, to)` qui couvrent `[0, 2³²)` dans l'ordre.
    fn segments(&self) -> [(&Form, u64, u64); 3] {
        match &self.old {
            None => [(&self.cur, 0, END), (&self.cur, END, END), (&self.cur, END, END)],
            Some(old) => [(&self.cur, 0, self.c), (old, self.c, self.ext), (&self.cur, self.ext, END)],
        }
    }

    /// Offsets à 1 de `[from, to)` dans l'ordre, jusqu'à ce que `f` rende `false`.
    pub(crate) fn scan(&self, from: u64, to: u64, mut f: impl FnMut(u32) -> bool) -> bool {
        for (form, a, b) in self.segments() {
            if !form.scan(a.max(from), b.min(to), &mut f) {
                return false;
            }
        }
        true
    }

    /// Bits à 1 de `[from, to)`.
    pub(crate) fn count_in(&self, from: u64, to: u64) -> u64 {
        if from == 0 && to >= self.hi {
            return self.count;
        }
        self.segments().iter().map(|&(form, a, b)| form.count_in(a.max(from), b.min(to))).sum()
    }

    /// Premier bit à 1 de `[from, to)`.
    pub(crate) fn next_set(&self, from: u64, to: u64) -> Option<u32> {
        let mut r = None;
        self.scan(from, to, |k| {
            r = Some(k);
            false
        });
        r
    }

    /// Premier bit à 0 de `[from, to)`.
    pub(crate) fn next_clear(&self, from: u64, to: u64) -> Option<u64> {
        if from >= self.hi {
            return (from < to).then_some(from);
        }
        self.segments().iter().find_map(|&(form, a, b)| form.next_clear(a.max(from), b.min(to)))
    }

    /// Bits de `[base, base + 64·buf.len())` dans `buf` (écrasé), `base` multiple de 64.
    pub(crate) fn fill_chunk(&self, base: u64, buf: &mut [u64]) {
        buf.fill(0);
        let end = (base + 64 * buf.len() as u64).min(END);
        for (form, a, b) in self.segments() {
            form.fill(a.max(base), b.min(end), base, buf);
        }
    }

    /// Toutes les formes qui font foi sur `[from, to)` sont compactes.
    pub(crate) fn compact_on(&self, from: u64, to: u64) -> bool {
        self.segments()
            .iter()
            .all(|&(form, a, b)| a.max(from) >= b.min(to) || matches!(form, Form::Compact(_)))
    }

    /// Longueur en octets, à la Redis (déduite du contenu).
    #[inline]
    pub(crate) fn byte_len(&self) -> u64 {
        self.hi.div_ceil(8)
    }

    /// Tous les offsets à 1 dans l'ordre.
    pub(crate) fn to_vec(&self) -> Vec<u32> {
        let mut v = Vec::with_capacity(self.count as usize);
        self.scan(0, END, |k| {
            v.push(k);
            true
        });
        v
    }

    /// Image T0 (save) : la plus petite des deux représentations.
    pub(crate) fn image(&self) -> Image {
        if classic_cost(self.hi) <= 4 * self.count {
            let mut w = vec![0u64; self.hi.div_ceil(64) as usize];
            self.scan(0, END, |k| {
                w[(k >> 6) as usize] |= 1 << (k & 63);
                true
            });
            Image::Words(w)
        } else {
            Image::Offsets(self.to_vec())
        }
    }

    /// Libère les formes.
    pub(crate) fn release(self) {
        self.cur.release();
        if let Some(old) = self.old {
            old.release();
        }
    }

    /// Vérifie la cohérence (tests) : compte, `hi`, territoires.
    #[cfg(test)]
    pub(crate) fn check(&self) {
        let v = self.to_vec();
        assert!(v.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(v.len() as u64, self.count, "count");
        assert_eq!(v.last().map_or(0, |&l| l as u64 + 1), self.hi, "hi");
        if let Some(old) = &self.old {
            // la nouvelle forme n'a rien dans le territoire de l'ancienne
            let mut n = 0;
            self.cur.scan(self.c, self.ext, &mut |_| {
                n += 1;
                true
            });
            assert_eq!(n, 0, "nouvelle forme dans le territoire de l'ancienne");
            assert!(self.c <= self.ext && old.ext() >= self.ext.min(old.ext()));
        }
    }
}

impl Image {
    pub(crate) fn count(&self) -> u64 {
        match self {
            Image::Offsets(v) => v.len() as u64,
            Image::Words(w) => tree::popcount_words(w),
        }
    }

    /// Offsets `>= from` dans l'ordre, jusqu'à ce que `f` rende `false`.
    pub(crate) fn scan(&self, from: u64, mut f: impl FnMut(u32) -> bool) {
        match self {
            Image::Offsets(v) => {
                let i = v.partition_point(|&k| (k as u64) < from);
                for &k in &v[i..] {
                    if !f(k) {
                        return;
                    }
                }
            }
            Image::Words(w) => {
                let wi = (from >> 6) as usize;
                if wi >= w.len() {
                    return;
                }
                // premier mot masqué, puis les suivants par `WordBits`
                let mut y = w[wi] & (u64::MAX << (from & 63));
                while y != 0 {
                    if !f(((wi << 6) as u64 | y.trailing_zeros() as u64) as u32) {
                        return;
                    }
                    y &= y - 1;
                }
                let base = ((wi + 1) << 6) as u64;
                for k in WordBits::new(&w[wi + 1..]) {
                    if !f((base + k as u64) as u32) {
                        return;
                    }
                }
            }
        }
    }
}

// ─── Slab des en-têtes, liste des bascules ────────────────────────────────────

static mut SLAB: MaybeUninit<HiSlab<Bitset>> = MaybeUninit::uninit();
static SLAB_INIT: radixox_lib::sys::Once = radixox_lib::sys::Once::new();
/// En-têtes en bascule, avancés par [`migrate_step`] (entrées périmées tolérées).
static mut MIGRATING: Vec<u32> = Vec::new();

#[inline]
pub(crate) fn slab() -> &'static mut HiSlab<Bitset> {
    SLAB_INIT.call_once(|| unsafe {
        #[allow(static_mut_refs)]
        SLAB.write(HiSlab::new(0, 100_000_000).expect("bitset slab alloc"));
    });
    #[allow(static_mut_refs)]
    unsafe {
        SLAB.assume_init_mut()
    }
}

#[allow(static_mut_refs)]
fn migrating() -> &'static mut Vec<u32> {
    unsafe { &mut *&raw mut MIGRATING }
}

/// Nouveau bitset, rend son idx.
pub(crate) fn alloc(b: Bitset) -> u32 {
    let idx = slab().insert(b);
    note_migration(idx);
    idx
}

/// Bitset `idx` en lecture.
#[inline]
pub(crate) fn get(idx: u32) -> &'static Bitset {
    slab().get(idx).expect("en-tête de bitset")
}

/// Point de passage de toute modification d'un bitset : barrière de save.
#[inline]
pub(crate) fn get_mut(idx: u32) -> &'static mut Bitset {
    save::before_write(idx);
    slab().get_mut(idx).expect("en-tête de bitset")
}

/// Après une écriture sur `idx` : une bascule restée en cours entre dans la liste.
#[inline]
pub(crate) fn note_migration(idx: u32) {
    let b = slab().get_mut(idx).expect("en-tête de bitset");
    if b.migrating() && !b.queued {
        b.queued = true;
        migrating().push(idx);
    }
}

/// Libère le bitset `idx` ; pendant une save, plus tard (la vue T0 peut encore le lire).
pub(crate) fn free(idx: u32) {
    if save::defer(idx) {
        return;
    }
    if let Some(b) = slab().remove(idx) {
        b.release();
    }
}

/// Une tranche des bascules en cours et de la libération des arbres, au plus `budget`.
/// Rien pendant une save (une tranche capturerait l'image du bitset). Vrai s'il en reste.
pub(crate) fn background_step(mut budget: u32) -> bool {
    if save::is_saving() {
        return pending();
    }
    let m = migrating();
    while budget > 0
        && let Some(&idx) = m.last()
    {
        match slab().get_mut(idx) {
            Some(b) if b.queued && b.migrating() => {
                let used = budget.min(step_budget());
                if b.step(used) {
                    b.queued = false;
                    m.pop();
                }
                budget -= used;
            }
            // libéré, ou case réutilisée par un bitset qui n'est pas en bascule
            _ => {
                m.pop();
            }
        }
    }
    if m.is_empty() {
        *m = Vec::new();
    }
    tree::free_step(budget / 8);
    pending()
}

/// Des bascules ou des libérations d'arbres attendent.
pub(crate) fn pending() -> bool {
    !migrating().is_empty() || tree::free_pending()
}

/// `(octets touchés, octets vivants)` du slab des en-têtes.
pub(crate) fn header_usage() -> (u64, u64) {
    if !SLAB_INIT.is_completed() {
        return (0, 0);
    }
    crate::value::hislab_usage(slab())
}

/// `(octets touchés, octets vivants, octets résidents)` du slab des en-têtes.
pub(crate) fn header_mem() -> (u64, u64, u64) {
    if !SLAB_INIT.is_completed() {
        return (0, 0, 0);
    }
    crate::value::hislab_mem(slab())
}

/// Rognage de la fin du slab des en-têtes (voir `value::trim_tail`).
pub(crate) fn trim_tail() -> usize {
    if !SLAB_INIT.is_completed() {
        return 0;
    }
    crate::value::trim_tail(slab())
}

/// Bitmap de présence des en-têtes (vue T0 de la save).
pub(crate) fn slab_occupancy(buf: &mut Vec<u64>) {
    slab().copy_occupancy(buf);
}
