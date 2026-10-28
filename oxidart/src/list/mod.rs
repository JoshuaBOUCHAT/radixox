//! List (ANALYSE_ART_UNIFIE.md §14) : blocs chaînés de [`CAP`] cases dans une slab à eux,
//! pierres tombales pour les retraits au milieu.
//!
//! ```text
//! en-tête (slab des List)        bloc (256 o)                    bloc
//! first ──────────────────────▶ prev | next | dead | lo | hi ◀─▶ …
//! last, len                     cases [lo, hi) : SharedByte
//! ```
//!
//! Invariants d'un bloc chaîné : `lo < hi`, les cases `lo` et `hi − 1` vivantes (une pierre
//! tombale au bord fait reculer le bord aussitôt) ; les bits de `dead` hors de `[lo, hi)`
//! sont nuls. Un pop n'a donc jamais de morte à sauter, et `first` / `last` ne sont jamais
//! vides.

use std::mem::MaybeUninit;

use radixox_lib::hislab::HiSlab;
use radixox_lib::shared_byte::SharedByte;

pub mod compact;
pub mod lazy;
pub mod save;

/// Cases par bloc : 30 (bloc de 256 o) ; mesures : `--cfg radixox_list128` (14 cases,
/// 128 o), `--cfg radixox_list512` (61 cases, 512 o, masque sur 64 bits).
#[cfg(not(any(radixox_list128, radixox_list512)))]
pub const CAP: usize = 30;
#[cfg(radixox_list128)]
pub const CAP: usize = 14;
#[cfg(radixox_list512)]
pub const CAP: usize = 61;
#[cfg(not(radixox_list512))]
type Mask = u32;
#[cfg(radixox_list512)]
type Mask = u64;
const _: () = assert!(CAP <= Mask::BITS as usize);
const NONE: u32 = u32::MAX;
/// Bout de chaîne (`prev` du premier bloc, `next` du dernier) : `HEAD | idx` désigne
/// l'en-tête de la List dans sa slab (compactage : un bloc de bout déplacé corrige
/// l'en-tête) ; `NONE` (qui a aussi le bit) : List hors de la slab (détachée, en cours de
/// libération), dont les blocs de bout ne bougent pas.
const HEAD: u32 = 1 << 31;

#[inline(always)]
fn is_end(x: u32) -> bool {
    x & HEAD != 0
}
/// Sous ce nombre de vivants, un bloc tente une fusion avec un voisin.
const MERGE_BELOW: u32 = (CAP / 4) as u32;

#[repr(C, align(64))]
pub struct Block {
    prev: u32,
    next: u32,
    /// Bit `i` : case `i` morte (dans `[lo, hi)` seulement).
    dead: Mask,
    lo: u8,
    hi: u8,
    slots: [MaybeUninit<SharedByte>; CAP],
}

const _: () = assert!(size_of::<Block>() % 64 == 0 && size_of::<Block>() < 16 + 8 * CAP + 64);

impl Block {
    /// Cases occupées `[lo, hi)`, mortes comprises.
    #[inline]
    fn range(&self) -> Mask {
        ((1u64 << self.hi) - (1u64 << self.lo)) as Mask
    }

    #[inline]
    fn live_mask(&self) -> Mask {
        self.range() & !self.dead
    }

    #[inline]
    pub(crate) fn live(&self) -> u32 {
        self.live_mask().count_ones()
    }

    /// # Safety
    /// Case vivante.
    #[inline]
    unsafe fn at(&self, i: usize) -> &SharedByte {
        unsafe { self.slots[i].assume_init_ref() }
    }

    /// Valeurs vivantes, dans l'ordre.
    pub(crate) fn values(&self) -> impl Iterator<Item = &SharedByte> + '_ {
        Bits(self.live_mask()).map(|i| unsafe { self.at(i) })
    }

    pub(crate) fn next_block(&self) -> u32 {
        self.next
    }

    /// Valeurs vivantes, modifiables (GC).
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut SharedByte> + '_ {
        let m = self.live_mask();
        self.slots.iter_mut().enumerate().filter(move |(i, _)| m & (1 << i) != 0).map(|(_, s)| unsafe { s.assume_init_mut() })
    }

    /// Case de la `k`-ième vivante (`k < live`).
    #[inline]
    fn nth(&self, k: u32) -> usize {
        let mut m = self.live_mask();
        for _ in 0..k {
            m &= m - 1;
        }
        m.trailing_zeros() as usize
    }

    /// Range les vivantes à partir de la case `at`, sans morte. Rend leur nombre.
    fn compact_to(&mut self, at: usize) -> usize {
        let mut tmp: [MaybeUninit<SharedByte>; CAP] = [const { MaybeUninit::uninit() }; CAP];
        let mut n = 0;
        for i in Bits(self.live_mask()) {
            tmp[n] = MaybeUninit::new(unsafe { self.slots[i].assume_init_read() });
            n += 1;
        }
        debug_assert!(at + n <= CAP);
        for (k, v) in tmp.into_iter().take(n).enumerate() {
            self.slots[at + k] = v;
        }
        (self.lo, self.hi, self.dead) = (at as u8, (at + n) as u8, 0);
        n
    }

    /// Bord bas : recule sur les mortes (invariant).
    #[inline]
    fn fix_lo(&mut self) {
        while self.lo < self.hi && self.dead & (1 << self.lo) != 0 {
            self.dead &= !(1 << self.lo);
            self.lo += 1;
        }
    }

    #[inline]
    fn fix_hi(&mut self) {
        while self.lo < self.hi && self.dead & (1 << (self.hi - 1)) != 0 {
            self.dead &= !(1 << (self.hi - 1));
            self.hi -= 1;
        }
    }
}

/// Bits à 1 d'un masque, croissants.
struct Bits(Mask);

impl Iterator for Bits {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        (self.0 != 0).then(|| {
            let i = self.0.trailing_zeros() as usize;
            self.0 &= self.0 - 1;
            i
        })
    }
}

impl DoubleEndedIterator for Bits {
    #[inline]
    fn next_back(&mut self) -> Option<usize> {
        (self.0 != 0).then(|| {
            let i = (Mask::BITS - 1 - self.0.leading_zeros()) as usize;
            self.0 &= !(1 << i);
            i
        })
    }
}

// ─── Slab des blocs ──────────────────────────────────────────────────────────

static mut SLAB: Option<HiSlab<Block>> = None;

#[inline(always)]
fn slab() -> &'static mut HiSlab<Block> {
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
fn init_slab() -> &'static mut HiSlab<Block> {
    #[allow(static_mut_refs)]
    unsafe {
        // 16 Gio virtuels, quelle que soit la taille de bloc
        SLAB.insert(HiSlab::new(0, ((16usize << 30) / size_of::<Block>()) as u32).expect("slab des blocs de List"))
    }
}

/// Slab des blocs de List : `(octets touchés, octets vivants, octets résidents)` ; zéros tant qu'il
/// n'existe pas.
pub(crate) fn slab_mem() -> (u64, u64, u64) {
    #[allow(static_mut_refs)]
    unsafe { SLAB.as_ref() }.map_or((0, 0, 0), crate::value::hislab_mem)
}

/// Voir [`crate::value::trim_tail`] ; 0 tant que la slab n'existe pas.
pub(crate) fn trim_tail() -> usize {
    #[allow(static_mut_refs)]
    unsafe { SLAB.as_ref() }.map_or(0, crate::value::trim_tail)
}

/// Lecture d'un bloc. Mono-thread ; jamais deux références au même bloc.
#[inline(always)]
pub(crate) fn block(i: u32) -> &'static Block {
    unsafe { slab().get_unchecked(i) }
}

/// Point de passage de toute écriture d'un bloc : barrière de save.
#[inline(always)]
fn block_mut(i: u32) -> &'static mut Block {
    save::before_block_write(i);
    unsafe { slab().get_unchecked_mut(i) }
}

fn new_block(lo: u8, prev: u32, next: u32) -> u32 {
    slab().insert(Block { prev, next, dead: 0, lo, hi: lo, slots: [const { MaybeUninit::uninit() }; CAP] })
}

/// Libère un bloc vidé (point de passage, comme [`block_mut`]).
fn free_block(i: u32) {
    save::before_block_write(i);
    // vidé : rien à détruire (`MaybeUninit`)
    slab().remove(i);
}

/// Bitmap de présence de la slab (vue T0 de la save).
pub(crate) fn slab_occupancy(buf: &mut Vec<u64>) {
    slab().copy_occupancy(buf);
}

/// Préchargement d'un bloc (en-tête et premières cases).
#[inline]
pub fn prefetch(i: u32) {
    radixox_lib::prefetch::prefetch_l1(slab().slot_ptr(i) as *const u8);
}

/// Blocs de la slab (mesures).
pub fn slab_blocks() -> u32 {
    slab().len()
}

/// GC : visite les valeurs des blocs `from..` de la slab, au plus `budget` blocs. Rend le
/// bloc où reprendre.
pub(crate) fn gc_visit(from: u32, budget: u32, f: &mut impl FnMut(&mut SharedByte)) -> Option<u32> {
    let s = slab();
    let mut at = from;
    for _ in 0..budget {
        let i = s.next_occupied(at)?;
        // pas de barrière : le GC change l'adresse des octets, pas la valeur (la save lit
        // les octets par le bloc vivant ou par son image, qui tient sa propre référence)
        for v in unsafe { s.get_unchecked_mut(i) }.values_mut() {
            f(v);
        }
        at = i + 1;
    }
    Some(at)
}

pub(crate) fn slot_span() -> u32 {
    slab().slot_span()
}

// ─── Position ────────────────────────────────────────────────────────────────

/// Position d'un élément : bloc, case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub block: u32,
    pub slot: u8,
}

/// Élément suivant (vers la fin).
#[inline]
pub fn next(c: Cursor) -> Option<Cursor> {
    let b = block(c.block);
    let above = b.live_mask() & !((2u64 << c.slot) - 1) as Mask;
    if above != 0 {
        return Some(Cursor { block: c.block, slot: above.trailing_zeros() as u8 });
    }
    (!is_end(b.next)).then(|| Cursor { block: b.next, slot: block(b.next).lo })
}

/// Élément précédent (vers le début).
#[inline]
pub fn prev(c: Cursor) -> Option<Cursor> {
    let b = block(c.block);
    let below = b.live_mask() & ((1u64 << c.slot) - 1) as Mask;
    if below != 0 {
        return Some(Cursor { block: c.block, slot: (Mask::BITS - 1 - below.leading_zeros()) as u8 });
    }
    (!is_end(b.prev)).then(|| Cursor { block: b.prev, slot: block(b.prev).hi - 1 })
}

/// Valeur en `c`.
#[inline]
pub fn value(c: Cursor) -> &'static SharedByte {
    unsafe { block(c.block).at(c.slot as usize) }
}

// ─── En-tête ─────────────────────────────────────────────────────────────────

/// En-tête d'une List (slab des List). Une List vide n'existe pas côté commandes (la clé
/// part avec son dernier élément) mais l'en-tête la sait représenter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct List {
    pub(crate) first: u32,
    pub(crate) last: u32,
    len: u32,
    /// Idx de l'en-tête dans la slab des List (`NONE` : hors de la slab).
    id: u32,
}

impl Default for List {
    fn default() -> Self {
        Self::new()
    }
}

impl List {
    pub const fn new() -> Self {
        Self { first: NONE, last: NONE, len: 0, id: NONE }
    }

    /// Marque des bouts de chaîne.
    #[inline]
    fn end(&self) -> u32 {
        if self.id == NONE { NONE } else { HEAD | self.id }
    }

    /// L'en-tête est (`id`) ou n'est plus (`NONE`) à cet idx de la slab des List : bouts
    /// remarqués.
    pub(crate) fn set_id(&mut self, id: u32) {
        self.id = id;
        if self.first != NONE {
            let e = self.end();
            block_mut(self.first).prev = e;
            block_mut(self.last).next = e;
        }
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn first_block(&self) -> u32 {
        self.first
    }

    pub fn last_block(&self) -> u32 {
        self.last
    }

    pub fn from_values(values: impl IntoIterator<Item = SharedByte>) -> Self {
        let mut l = Self::new();
        for v in values {
            l.push_back(v);
        }
        l
    }

    // ─── Bouts ───────────────────────────────────────────────────────────────

    pub fn push_back(&mut self, v: SharedByte) {
        self.len += 1;
        if self.last == NONE {
            // premier bloc : au milieu, de la place des deux côtés
            let b = new_block(CAP as u8 / 2, self.end(), self.end());
            (self.first, self.last) = (b, b);
        } else if block(self.last).hi as usize == CAP {
            let lb = block(self.last);
            // de la place à l'autre bout : décaler plutôt qu'ouvrir un bloc
            if lb.lo as usize + lb.dead.count_ones() as usize >= CAP / 4 {
                block_mut(self.last).compact_to(0);
            } else {
                let b = new_block(0, self.last, self.end());
                block_mut(self.last).next = b;
                self.last = b;
            }
        }
        let b = block_mut(self.last);
        b.slots[b.hi as usize] = MaybeUninit::new(v);
        b.hi += 1;
    }

    pub fn push_front(&mut self, v: SharedByte) {
        self.len += 1;
        if self.first == NONE {
            let b = new_block(CAP as u8 / 2, self.end(), self.end());
            (self.first, self.last) = (b, b);
        } else if block(self.first).lo == 0 {
            let fb = block(self.first);
            let free = CAP - fb.hi as usize + fb.dead.count_ones() as usize;
            if free >= CAP / 4 {
                let n = fb.live() as usize;
                block_mut(self.first).compact_to(CAP - n);
            } else {
                let b = new_block(CAP as u8, self.end(), self.first);
                block_mut(self.first).prev = b;
                self.first = b;
            }
        }
        let b = block_mut(self.first);
        b.lo -= 1;
        b.slots[b.lo as usize] = MaybeUninit::new(v);
    }

    pub fn pop_front(&mut self) -> Option<SharedByte> {
        if self.first == NONE {
            return None;
        }
        let i = self.first;
        let b = block_mut(i);
        let v = unsafe { b.slots[b.lo as usize].assume_init_read() };
        b.lo += 1;
        b.fix_lo();
        self.len -= 1;
        if b.lo == b.hi {
            self.unlink(i);
        }
        Some(v)
    }

    pub fn pop_back(&mut self) -> Option<SharedByte> {
        if self.last == NONE {
            return None;
        }
        let i = self.last;
        let b = block_mut(i);
        b.hi -= 1;
        let v = unsafe { b.slots[b.hi as usize].assume_init_read() };
        b.fix_hi();
        self.len -= 1;
        if b.lo == b.hi {
            self.unlink(i);
        }
        Some(v)
    }

    /// Décroche et libère le bloc `i`, vidé.
    fn unlink(&mut self, i: u32) {
        let (prev, next) = (block(i).prev, block(i).next);
        if is_end(prev) {
            self.first = if is_end(next) { NONE } else { next };
        } else {
            block_mut(prev).next = next;
        }
        if is_end(next) {
            self.last = if is_end(prev) { NONE } else { prev };
        } else {
            block_mut(next).prev = prev;
        }
        free_block(i);
    }

    pub fn front(&self) -> Option<&'static SharedByte> {
        (self.first != NONE).then(|| value(Cursor { block: self.first, slot: block(self.first).lo }))
    }

    pub fn back(&self) -> Option<&'static SharedByte> {
        (self.last != NONE).then(|| value(Cursor { block: self.last, slot: block(self.last).hi - 1 }))
    }

    // ─── Positions ───────────────────────────────────────────────────────────

    /// Position de l'élément de rang `i` (`i < len`) : depuis le bout le plus proche, des
    /// blocs entiers sautés par leurs vivants.
    pub fn locate(&self, i: u32) -> Option<Cursor> {
        if i >= self.len {
            return None;
        }
        if i < self.len / 2 {
            let (mut b, mut k) = (self.first, i);
            loop {
                let n = block(b).live();
                if k < n {
                    return Some(Cursor { block: b, slot: block(b).nth(k) as u8 });
                }
                k -= n;
                b = block(b).next;
            }
        }
        let (mut b, mut k) = (self.last, self.len - 1 - i);
        loop {
            let n = block(b).live();
            if k < n {
                return Some(Cursor { block: b, slot: block(b).nth(n - 1 - k) as u8 });
            }
            k -= n;
            b = block(b).prev;
        }
    }

    pub fn get(&self, i: u32) -> Option<&'static SharedByte> {
        self.locate(i).map(value)
    }

    /// Remplace la valeur en `c` ; rend l'ancienne.
    pub fn replace(&mut self, c: Cursor, v: SharedByte) -> SharedByte {
        let b = block_mut(c.block);
        std::mem::replace(unsafe { b.slots[c.slot as usize].assume_init_mut() }, v)
    }

    pub fn first_cursor(&self) -> Option<Cursor> {
        (self.first != NONE).then(|| Cursor { block: self.first, slot: block(self.first).lo })
    }

    pub fn last_cursor(&self) -> Option<Cursor> {
        (self.last != NONE).then(|| Cursor { block: self.last, slot: block(self.last).hi - 1 })
    }

    /// Valeurs du début à la fin.
    pub fn iter(&self) -> impl Iterator<Item = &'static SharedByte> {
        std::iter::successors(self.first_cursor(), |&c| next(c)).map(value)
    }

    /// Valeurs de la fin au début.
    pub fn iter_rev(&self) -> impl Iterator<Item = &'static SharedByte> {
        std::iter::successors(self.last_cursor(), |&c| prev(c)).map(value)
    }

    // ─── Milieu ──────────────────────────────────────────────────────────────

    /// Retire l'élément en `c` (pierre tombale), sans fusion. Le bloc peut disparaître.
    fn kill(&mut self, c: Cursor) -> SharedByte {
        let b = block_mut(c.block);
        let s = c.slot as usize;
        let v = unsafe { b.slots[s].assume_init_read() };
        b.dead |= 1 << s;
        b.fix_lo();
        b.fix_hi();
        self.len -= 1;
        if b.lo == b.hi {
            self.unlink(c.block);
        }
        v
    }

    /// Retire l'élément en `c` ; rend sa valeur. Les curseurs pris avant ne valent plus.
    pub fn remove(&mut self, c: Cursor) -> SharedByte {
        let v = self.kill(c);
        self.maybe_merge(c.block);
        v
    }

    /// Bloc `i` (s'il existe encore) sous [`MERGE_BELOW`] vivants : fusion avec un voisin
    /// qui a la place.
    fn maybe_merge(&mut self, i: u32) {
        if !slab().is_occupied(i) || block(i).live() >= MERGE_BELOW {
            return;
        }
        let b = block(i);
        let n = b.live();
        if !is_end(b.prev) && block(b.prev).live() + n <= CAP as u32 {
            self.absorb(b.prev, i);
        } else if !is_end(b.next) && block(b.next).live() + n <= CAP as u32 {
            self.absorb(i, b.next);
        }
    }

    /// Verse les vivantes de `right` à la fin de `left` (voisins), puis libère `right`.
    fn absorb(&mut self, left: u32, right: u32) {
        let at = {
            let l = block_mut(left);
            let n = l.live() as usize;
            let r = block(right).live() as usize;
            // le moins de déplacements : garder la place de `left` si la suite tient
            if l.dead != 0 || l.lo as usize + n + r > CAP { l.compact_to(0) as u8 } else { l.hi }
        };
        let r = block_mut(right);
        let mut k = at as usize;
        let mut moved: [MaybeUninit<SharedByte>; CAP] = [const { MaybeUninit::uninit() }; CAP];
        let mut m = 0;
        for i in Bits(r.live_mask()) {
            moved[m] = MaybeUninit::new(unsafe { r.slots[i].assume_init_read() });
            m += 1;
        }
        let next = r.next;
        (r.lo, r.hi, r.dead) = (0, 0, 0);
        let l = block_mut(left);
        for v in moved.into_iter().take(m) {
            l.slots[k] = v;
            k += 1;
        }
        l.hi = k as u8;
        l.next = next;
        if is_end(next) { self.last = left } else { block_mut(next).prev = left }
        free_block(right);
    }

    /// Insère `v` avant (`after` faux) ou après l'élément en `c`. Les curseurs pris avant
    /// ne valent plus.
    pub fn insert(&mut self, c: Cursor, after: bool, v: SharedByte) {
        let i = c.block;
        let b = block_mut(i);
        // rang de `c` parmi les vivantes du bloc, puis bloc compacté
        let k = (b.live_mask() & ((1u64 << c.slot) - 1) as Mask).count_ones() as usize + after as usize;
        let n = b.live() as usize;
        if n == CAP {
            // plein : coupé en deux, la moitié droite dans un bloc neuf
            let half = CAP / 2;
            let mut right: [MaybeUninit<SharedByte>; CAP] = [const { MaybeUninit::uninit() }; CAP];
            b.compact_to(0);
            for j in half..CAP {
                right[j - half] = MaybeUninit::new(unsafe { b.slots[j].assume_init_read() });
            }
            b.hi = half as u8;
            let next = b.next;
            let r = new_block(0, i, next);
            block_mut(i).next = r;
            if is_end(next) { self.last = r } else { block_mut(next).prev = r }
            let rb = block_mut(r);
            for (j, v) in right.into_iter().take(CAP - half).enumerate() {
                rb.slots[j] = v;
            }
            rb.hi = (CAP - half) as u8;
            return if k <= half { self.insert_at(i, k, v) } else { self.insert_at(r, k - half, v) };
        }
        self.insert_at(i, k, v);
    }

    /// Insère `v` au rang `k` des vivantes du bloc `i`, qui a de la place.
    fn insert_at(&mut self, i: u32, k: usize, v: SharedByte) {
        let b = block_mut(i);
        let n = b.live() as usize;
        debug_assert!(n < CAP && k <= n);
        // sans morte, sur place ; puis décalage du côté le plus court qui a de la place
        if b.dead != 0 {
            b.compact_to(b.lo as usize);
        }
        let pos = b.lo as usize + k;
        if (b.hi as usize) < CAP && (k >= n / 2 || b.lo == 0) {
            for j in (pos..b.hi as usize).rev() {
                b.slots[j + 1] = MaybeUninit::new(unsafe { b.slots[j].assume_init_read() });
            }
            b.hi += 1;
            b.slots[pos] = MaybeUninit::new(v);
        } else {
            for j in b.lo as usize..pos {
                b.slots[j - 1] = MaybeUninit::new(unsafe { b.slots[j].assume_init_read() });
            }
            b.lo -= 1;
            b.slots[pos - 1] = MaybeUninit::new(v);
        }
        self.len += 1;
    }

    /// Retire, du début (`count > 0`), de la fin (`count < 0`) ou partout (`0`), au plus
    /// `|count|` éléments pour lesquels `hit` est vrai. Rend le nombre retiré.
    pub fn remove_matching(&mut self, count: i64, mut hit: impl FnMut(&[u8]) -> bool) -> u32 {
        let max = if count == 0 { u64::MAX } else { count.unsigned_abs() };
        let back = count < 0;
        let mut touched: Vec<u32> = Vec::new();
        let mut removed = 0u64;
        let mut c = if back { self.last_cursor() } else { self.first_cursor() };
        while let Some(at) = c
            && removed < max
        {
            // le suivant avant de toucher au bloc (il peut disparaître)
            c = if back { prev(at) } else { next(at) };
            if hit(value(at).as_slice()) {
                drop(self.kill(at));
                removed += 1;
                if touched.last() != Some(&at.block) {
                    touched.push(at.block);
                }
            }
        }
        // fusions après le parcours : elles déplacent des cases
        for b in touched {
            self.maybe_merge(b);
        }
        removed as u32
    }

    /// Détache les `n` premiers (`front`) ou derniers éléments en une List à part, sans
    /// les toucher : la coupe est trouvée depuis le bout le plus proche (O(min(n, len − n)
    /// / CAP)), la chaîne de blocs entiers passe d'un coup à la List rendue, et seules les
    /// cases du bloc coupé (< CAP) sont retirées sur place. À rendre par
    /// [`lazy::release`] (libération par tranches).
    pub fn detach_end(&mut self, n: u32, front: bool) -> List {
        let n = n.min(self.len);
        if n == 0 {
            return List::new();
        }
        if n == self.len {
            let id = self.id;
            let mut out = std::mem::replace(self, List::new());
            self.id = id;
            out.set_id(NONE);
            return out;
        }
        // rang du premier élément gardé (devant) ou du premier retiré (derrière)
        let c = self.locate(if front { n } else { self.len - n }).expect("coupe");
        let b = block(c.block);
        let below = (b.live_mask() & ((1u64 << c.slot) - 1) as Mask).count_ones();
        let mut out = List::new();
        if front {
            // blocs avant `c` : détachés ; `below` cases de `c` : retirées sur place
            if !is_end(b.prev) {
                let last = b.prev;
                (out.first, out.last, out.len) = (self.first, last, n - below);
                block_mut(self.first).prev = NONE;
                block_mut(last).next = NONE;
                block_mut(c.block).prev = self.end();
                self.first = c.block;
            }
            self.len -= n - below;
            for _ in 0..below {
                drop(self.pop_front());
            }
        } else {
            let from = b.live() - below;
            if !is_end(b.next) {
                let first = b.next;
                (out.first, out.last, out.len) = (first, self.last, n - from);
                block_mut(first).prev = NONE;
                block_mut(self.last).next = NONE;
                block_mut(c.block).next = self.end();
                self.last = c.block;
            }
            self.len -= n - from;
            for _ in 0..from {
                drop(self.pop_back());
            }
        }
        out
    }

    /// Retire les `n` premiers (`front`) ou derniers éléments, par blocs entiers quand ils
    /// y passent.
    pub fn drop_end(&mut self, mut n: u32, front: bool) {
        while n > 0 && self.first != NONE {
            let i = if front { self.first } else { self.last };
            let live = block(i).live();
            if live <= n {
                let b = block_mut(i);
                for s in Bits(b.live_mask()) {
                    unsafe { b.slots[s].assume_init_drop() };
                }
                (b.lo, b.hi, b.dead) = (0, 0, 0);
                self.len -= live;
                n -= live;
                self.unlink(i);
            } else {
                for _ in 0..n {
                    drop(if front { self.pop_front() } else { self.pop_back() });
                }
                n = 0;
            }
        }
    }

    /// Libère tout (valeurs comprises). Liste vide ensuite.
    pub fn clear(&mut self) {
        self.drop_end(self.len, true);
    }

    /// Libère au plus `budget` blocs depuis le début ; vrai quand tout est rendu.
    pub fn clear_step(&mut self, budget: u32) -> bool {
        for _ in 0..budget {
            if self.first == NONE {
                return true;
            }
            let live = block(self.first).live();
            self.drop_end(live, true);
        }
        self.first == NONE
    }

    /// Vérifie les invariants (tests).
    pub fn verify(&self) -> Result<(), String> {
        if self.first == NONE {
            return if self.last == NONE && self.len == 0 { Ok(()) } else { Err(format!("vide incohérente : {self:?}")) };
        }
        let (mut b, mut prev_b, mut n) = (self.first, self.end(), 0u32);
        while !is_end(b) {
            let x = block(b);
            if x.prev != prev_b {
                return Err(format!("bloc {b} : prev {} au lieu de {prev_b}", x.prev));
            }
            if x.lo >= x.hi || x.hi as usize > CAP {
                return Err(format!("bloc {b} : bornes {}..{}", x.lo, x.hi));
            }
            if x.dead & !x.range() != 0 || x.dead & (1 << x.lo) != 0 || x.dead & (1 << (x.hi - 1)) != 0 {
                return Err(format!("bloc {b} : mortes {:b} hors bornes {}..{}", x.dead, x.lo, x.hi));
            }
            n += x.live();
            prev_b = b;
            b = x.next;
        }
        if prev_b != self.last {
            return Err(format!("dernier {prev_b} au lieu de {}", self.last));
        }
        if b != self.end() {
            return Err(format!("bout {b:x} au lieu de {:x}", self.end()));
        }
        if n != self.len {
            return Err(format!("{n} vivants au lieu de {}", self.len));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

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

    fn sb(x: u64) -> SharedByte {
        SharedByte::from_slice(format!("v{x}"))
    }

    fn same(l: &List, m: &VecDeque<SharedByte>) {
        l.verify().unwrap();
        assert_eq!(l.len() as usize, m.len());
        assert!(l.iter().map(|v| v.as_slice()).eq(m.iter().map(|v| v.as_slice())));
        assert!(l.iter_rev().map(|v| v.as_slice()).eq(m.iter().rev().map(|v| v.as_slice())));
    }

    #[test]
    fn random_against_vecdeque() {
        let mut r = Rng(0x9e3779b97f4a7c15);
        for round in 0..20 {
            let mut l = List::new();
            let mut m: VecDeque<SharedByte> = VecDeque::new();
            // peu de valeurs distinctes : des doublons pour remove_matching
            let vals = 4 + round * 3;
            for step in 0..4000 {
                match r.below(14) {
                    0..=2 => {
                        let v = sb(r.below(vals));
                        l.push_back(v.clone());
                        m.push_back(v);
                    }
                    3..=5 => {
                        let v = sb(r.below(vals));
                        l.push_front(v.clone());
                        m.push_front(v);
                    }
                    6 => assert_eq!(l.pop_front().map(|v| v.to_vec()), m.pop_front().map(|v| v.to_vec())),
                    7 => assert_eq!(l.pop_back().map(|v| v.to_vec()), m.pop_back().map(|v| v.to_vec())),
                    8 if !m.is_empty() => {
                        let i = r.below(m.len() as u64) as u32;
                        assert_eq!(l.get(i).unwrap().as_slice(), m[i as usize].as_slice());
                        let c = l.locate(i).unwrap();
                        assert_eq!(l.remove(c).as_slice(), m.remove(i as usize).unwrap().as_slice());
                    }
                    9 if !m.is_empty() => {
                        let i = r.below(m.len() as u64) as u32;
                        let after = r.below(2) == 0;
                        let v = sb(1000 + step);
                        let c = l.locate(i).unwrap();
                        l.insert(c, after, v.clone());
                        m.insert(i as usize + after as usize, v);
                    }
                    10 if !m.is_empty() => {
                        let i = r.below(m.len() as u64) as u32;
                        let v = sb(2000 + step);
                        let c = l.locate(i).unwrap();
                        drop(l.replace(c, v.clone()));
                        m[i as usize] = v;
                    }
                    11 => {
                        let target = sb(r.below(vals));
                        let count = r.below(5) as i64 - 2;
                        let got = l.remove_matching(count, |x| x == target.as_slice());
                        let mut want = 0;
                        let max = if count == 0 { usize::MAX } else { count.unsigned_abs() as usize };
                        if count >= 0 {
                            let mut i = 0;
                            while i < m.len() && want < max {
                                if m[i].as_slice() == target.as_slice() { m.remove(i); want += 1 } else { i += 1 }
                            }
                        } else {
                            let mut i = m.len();
                            while i > 0 && want < max {
                                i -= 1;
                                if m[i].as_slice() == target.as_slice() {
                                    m.remove(i);
                                    want += 1;
                                }
                            }
                        }
                        assert_eq!(got as usize, want);
                    }
                    12 => {
                        let cap = if r.below(4) == 0 { 400 } else { 40 };
                        let n = r.below(cap) as u32;
                        let front = r.below(2) == 0;
                        if r.below(2) == 0 {
                            l.drop_end(n, front);
                        } else {
                            let mut d = l.detach_end(n, front);
                            d.verify().unwrap();
                            assert!(d.len() <= n.min(m.len() as u32));
                            let want: Vec<Vec<u8>> = if front {
                                m.iter().take(n as usize).map(|v| v.to_vec()).collect()
                            } else {
                                m.iter().skip(m.len().saturating_sub(n as usize)).map(|v| v.to_vec()).collect()
                            };
                            let got: Vec<Vec<u8>> = d.iter().map(|v| v.to_vec()).collect();
                            // la List détachée tient les blocs entiers ; le reste du bloc coupé
                            // est parti sur place : ses éléments sont une fin de `want`
                            if front {
                                assert_eq!(got[..], want[..got.len()]);
                            } else {
                                assert_eq!(got[..], want[want.len() - got.len()..]);
                            }
                            d.clear();
                        }
                        for _ in 0..n.min(m.len() as u32) {
                            if front { m.pop_front(); } else { m.pop_back(); }
                        }
                    }
                    _ => {}
                }
                if step % 97 == 0 {
                    same(&l, &m);
                }
            }
            same(&l, &m);
            l.clear();
            assert!(l.is_empty());
            l.verify().unwrap();
        }
    }

    /// Compactage : List réelles (en-têtes dans leur slab), une sur deux supprimée, passe
    /// forcée ; contenus et invariants gardés, slab rétrécie. Une List en cours de
    /// libération (bouts `NONE`) ne bouge pas.
    #[test]
    fn compaction_keeps_lists() {
        use crate::value::{alloc_list_raw, free_list, list_ref};
        crate::value::init_slabs();
        let mut r = Rng(0x5851_F42D_4C95_7F2D);
        let mut lists: Vec<(u32, VecDeque<SharedByte>)> = Vec::new();
        for i in 0..400u64 {
            let n = 1 + r.below(300);
            let vals: VecDeque<SharedByte> = (0..n).map(|j| sb(i * 1000 + j)).collect();
            lists.push((alloc_list_raw(List::from_values(vals.iter().cloned())), vals));
        }
        // trous : une List sur deux part (tout de suite : petites)
        let mut kept = Vec::new();
        for (k, (idx, vals)) in lists.into_iter().enumerate() {
            if k % 2 == 0 { free_list(idx) } else { kept.push((idx, vals)) }
        }
        // une grande en cours de libération : bouts `NONE`, ne doit pas bouger
        let big = alloc_list_raw(List::from_values((0..2000).map(sb)));
        free_list(big);
        let before = slab().len();
        let span_before = slab().last_occupied().unwrap() + 1;
        let mut c = compact::want(true).expect("passe");
        while compact::step(&mut c, 64).is_none() {}
        assert_eq!(slab().len(), before, "aucun bloc perdu");
        for (idx, vals) in &kept {
            let l = list_ref(*idx);
            l.verify().unwrap();
            assert!(l.iter().map(|v| v.as_slice()).eq(vals.iter().map(|v| v.as_slice())));
        }
        while lazy::step(1 << 20) {}
        let mut c = compact::want(true).expect("passe");
        while compact::step(&mut c, 64).is_none() {}
        let span_after = slab().last_occupied().unwrap() + 1;
        assert!(span_after < span_before, "fin de slab {span_after} (avant {span_before})");
        assert_eq!(span_after, slab().len(), "plus de trou sous la fin");
        for (idx, vals) in kept {
            let l = list_ref(idx);
            l.verify().unwrap();
            assert!(l.iter().map(|v| v.as_slice()).eq(vals.iter().map(|v| v.as_slice())));
            free_list(idx);
        }
        while lazy::step(1 << 20) {}
    }

    #[test]
    fn queue_stays_in_one_block() {
        let before = slab_blocks();
        let mut l = List::new();
        for i in 0..100_000u64 {
            l.push_front(sb(i));
            if l.len() > 10 {
                drop(l.pop_back());
            }
        }
        assert_eq!(slab_blocks() - before, 1, "une file courte tient dans un bloc");
        l.clear();
    }

    #[test]
    fn sparse_removals_merge() {
        let mut l = List::from_values((0..3000).map(sb));
        // garde un élément sur 10 : sans fusion, ~1 bloc par 3 éléments
        let mut k = 0u64;
        l.remove_matching(0, |_| {
            k += 1;
            k % 10 != 0
        });
        l.verify().unwrap();
        assert_eq!(l.len(), 300);
        let mut blocks = 0;
        let mut b = l.first_block();
        while !is_end(b) {
            blocks += 1;
            b = block(b).next_block();
        }
        assert!(blocks <= 300 / MERGE_BELOW as usize + 2, "{blocks} blocs pour 300 éléments");
        l.clear();
    }
}
