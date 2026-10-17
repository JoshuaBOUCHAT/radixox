//! Nœud 128 o à feuilles inline, quatre formes selon le nombre d'entrées.
//!
//! | forme       | entrées | métadonnées                          | données          |
//! |-------------|---------|--------------------------------------|------------------|
//! | `Inline`    | ≤ 8     | nœud : radixs, tags, maps u16        | nœud             |
//! | `Overflow8` | ≤ 16    | idem, les maps couvrent nœud + page  | nœud + page      |
//! | `Wide32`    | ≤ 32    | nœud : bitmap radix, tags, maps u32  | page             |
//! | `Wide256`   | ≤ 256   | nœud : bitmap radix ; page : maps+tags | page           |
//!
//! Entrées triées par radix, numérotées par rang `e`. Un segment (nœud, page)
//! range ses cellules ainsi, tout s'adressant par popcount des maps sous `e` :
//!
//! ```text
//! cells : [idx0 idx1 … | libre | … feuille1 | feuille0]
//! feuille : [sfx?][ttl?][val]       val toujours en fin de record
//! ```
//!
//! Petites formes : le nœud garde le plus long préfixe d'entrées qui tient
//! (≤ 8 entrées, ≤ 64 o), la page prend le reste (≤ 8 entrées).
//!
//! Page : taille = classe mimalloc du besoin (on paie la classe de toute façon),
//! réduite seulement quand le besoin passe sous ¾ du bloc. Changements de forme
//! avec hystérésis aussi (montée à 17 / 33 entrées, descente à 12 / 24).

use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};

use radixox_lib::prefetch::prefetch_l1;
use radixox_lib::raw_alloc::{free as mi_free, malloc as mi_malloc};

use crate::ExpAndRadix;
use crate::compact_path::CompactPath;
use crate::value::{Tag, ValUnion, drop_raw};

const SMALL_ENTRIES: usize = 8;
const NODE_CELLS: usize = 64;
/// radixs[8] + tags[8] en tête de page `Overflow8`.
const SMALL_PAGE_HEADER: usize = 16;
/// child, ttl, sfx en 3 × 256 bits en tête de page `Wide256`.
const WIDE_MAPS: usize = 96;
pub(crate) const SMALL_MAX: usize = 16;
pub(crate) const WIDE32_MAX: usize = 32;
/// Descente `Wide32` → petite forme à ce nombre d'entrées (si ça tient).
const WIDE32_DOWN: usize = 12;
/// Descente `Wide256` → `Wide32`.
const WIDE256_DOWN: usize = 24;

/// Cible du thin ptr sans page : le préchargement se fait sans test,
/// sur une ligne toujours chaude. Jamais écrite.
#[repr(C, align(64))]
struct Trash([u8; 64]);
static TRASH: Trash = Trash([0; 64]);

#[inline]
fn trash() -> NonNull<u8> {
    NonNull::from(&TRASH).cast()
}

/// Classe de taille mimalloc : 8 o par 8 o jusqu'à 64, puis 4 classes par
/// doublement (80, 96, 112, 128, 160, …).
pub(crate) fn alloc_class(size: usize) -> usize {
    if size <= 64 {
        return size.div_ceil(8) * 8;
    }
    let words = size.div_ceil(8);
    let b = usize::BITS - 1 - (words - 1).leading_zeros();
    let step = 1usize << (b - 2);
    words.div_ceil(step) * step * 8
}

#[inline]
fn round8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Inline = 0,
    Overflow8 = 1,
    Wide32 = 2,
    Wide256 = 3,
}

impl Kind {
    #[inline]
    fn is_small(self) -> bool {
        matches!(self, Kind::Inline | Kind::Overflow8)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SmallKeys {
    radixs: [u8; SMALL_ENTRIES],
    tags: [Tag; SMALL_ENTRIES],
}

/// Petites formes : radixs + tags. `Wide32` : maps child, ttl, sfx (u32).
#[repr(C)]
union Keys {
    small: SmallKeys,
    maps: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WideMeta {
    radix_bits: [u64; 4],
    /// `Wide32` seulement.
    tags: [Tag; WIDE32_MAX],
}

/// Petites formes : cellules. Formes larges : métadonnées.
#[repr(C)]
union Body {
    cells: [u64; NODE_CELLS / 8],
    wide: WideMeta,
}

#[repr(C, align(64))]
pub(crate) struct Node {
    // ── ligne 1 : tout ce que lit la descente ──
    /// Page, ou `TRASH`.
    ptr: NonNull<u8>,
    pub(crate) prefix: CompactPath,
    /// Self-val : clé = chemin du nœud exactement.
    pub(crate) val: ValUnion,
    pub(crate) exp_and_radix: ExpAndRadix,
    keys: Keys,
    pub(crate) parent_idx: u32,
    child_map: u16,
    ttl_map: u16,
    sfx_map: u16,
    kind: Kind,
    pub(crate) tag: Tag,
    /// Nombre d'entrées, sauf en `Wide256` (popcount de `radix_bits`).
    count: u8,
    n_in: u8,
    /// Taille du bloc de page en unités de 8 o (0 = pas de page).
    pg_block8: u16,
    // ── ligne 2 ──
    body: Body,
}

const _: () = assert!(size_of::<Node>() == 128);
const _: () = assert!(std::mem::offset_of!(Node, body) == 64);

// ─── Entrées ─────────────────────────────────────────────────────────────────

/// Feuille possédée (hors du nœud).
pub(crate) struct Leaf {
    pub(crate) tag: Tag,
    pub(crate) val: ValUnion,
    pub(crate) ttl: Option<u64>,
    pub(crate) sfx: Option<CompactPath>,
}

impl Drop for Leaf {
    fn drop(&mut self) {
        unsafe { drop_raw(self.tag, &mut self.val) };
    }
}

impl Leaf {
    /// Sort les morceaux sans libérer la valeur (la propriété passe à l'appelant).
    pub(crate) fn into_parts(self) -> (Tag, ValUnion, Option<u64>, Option<CompactPath>) {
        let me = ManuallyDrop::new(self);
        unsafe { (me.tag, ptr::read(&me.val), me.ttl, ptr::read(&me.sfx)) }
    }
}

pub(crate) enum Entry {
    Child(u32),
    Leaf(Leaf),
}

/// Vue empruntée d'une entrée. Les références sont invalidées par toute
/// mutation du nœud (les records bougent par memmove).
/// Résultat de [`Node::probe`].
pub(crate) enum Probe<'a> {
    Miss,
    /// Entrée lue dans le nœud (en cache avec lui).
    Here(EntryRef<'a>),
    /// Entrée de rang `e` dans la page ; adresse de sa cellule.
    Page(usize, *const u8),
}

pub(crate) enum EntryRef<'a> {
    Child(u32),
    Leaf {
        tag: Tag,
        val: &'a ValUnion,
        ttl: Option<u64>,
        sfx: Option<&'a CompactPath>,
    },
}

/// Entrée en transit : octets bruts, propriété manuelle.
#[derive(Clone, Copy)]
struct Raw {
    radix: u8,
    tag: Tag,
    child: bool,
    ttl: bool,
    sfx: bool,
    idx: u32,
    val: u64,
    exp: u64,
    path: u64,
}

impl Raw {
    #[inline]
    fn size(&self) -> usize {
        if self.child {
            4
        } else {
            8 * (1 + self.ttl as usize + self.sfx as usize)
        }
    }

    fn from_entry(radix: u8, entry: Entry) -> Self {
        let mut raw = Raw {
            radix,
            tag: Tag::None,
            child: false,
            ttl: false,
            sfx: false,
            idx: 0,
            val: 0,
            exp: 0,
            path: 0,
        };
        match entry {
            Entry::Child(idx) => {
                raw.child = true;
                raw.idx = idx;
            }
            Entry::Leaf(leaf) => {
                let leaf = ManuallyDrop::new(leaf);
                raw.tag = leaf.tag;
                raw.val = unsafe { ptr::read(&leaf.val as *const ValUnion as *const u64) };
                if let Some(exp) = leaf.ttl {
                    raw.ttl = true;
                    raw.exp = exp;
                }
                if let Some(sfx) = &leaf.sfx {
                    raw.sfx = true;
                    raw.path = unsafe { ptr::read(sfx as *const CompactPath as *const u64) };
                }
            }
        }
        raw
    }

    /// # Safety
    /// Consomme la propriété : `self` ne doit plus être relu.
    unsafe fn into_entry(self) -> Entry {
        if self.child {
            return Entry::Child(self.idx);
        }
        unsafe {
            Entry::Leaf(Leaf {
                tag: self.tag,
                val: ptr::read(&self.val as *const u64 as *const ValUnion),
                ttl: self.ttl.then_some(self.exp),
                sfx: self
                    .sfx
                    .then(|| ptr::read(&self.path as *const u64 as *const CompactPath)),
            })
        }
    }
}

// ─── Bitmaps ─────────────────────────────────────────────────────────────────

/// Map indexée par rang : bit `i` = entrée `i`.
trait Bits: Copy {
    fn bit(&self, i: usize) -> bool;
    /// Bits à 1 sous `i` (exclu).
    fn pop_below(&self, i: usize) -> usize;
    /// Insère `bit` en `i`, décale les bits ≥ `i` vers le haut.
    fn insert(&mut self, i: usize, bit: bool);
    /// Retire le bit `i`, décale les bits > `i` vers le bas.
    fn remove(&mut self, i: usize);
}

#[inline]
fn mask64(i: usize) -> u64 {
    if i >= 64 { !0 } else { (1u64 << i) - 1 }
}

macro_rules! bits_int {
    ($t:ty) => {
        impl Bits for $t {
            #[inline]
            fn bit(&self, i: usize) -> bool {
                (*self as u64) >> i & 1 == 1
            }
            #[inline]
            fn pop_below(&self, i: usize) -> usize {
                (*self as u64 & mask64(i)).count_ones() as usize
            }
            #[inline]
            fn insert(&mut self, i: usize, bit: bool) {
                let x = *self as u64;
                let m = mask64(i);
                *self = ((x & m) | ((x & !m) << 1) | ((bit as u64) << i)) as $t;
            }
            #[inline]
            fn remove(&mut self, i: usize) {
                let x = *self as u64;
                let m = mask64(i);
                *self = ((x & m) | ((x >> 1) & !m)) as $t;
            }
        }
    };
}
bits_int!(u16);
bits_int!(u32);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct U256([u64; 4]);

impl U256 {
    #[inline]
    fn pop(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }

    #[inline]
    fn set(&mut self, i: usize, bit: bool) {
        let (w, b) = (i >> 6, i & 63);
        self.0[w] = self.0[w] & !(1 << b) | ((bit as u64) << b);
    }

    /// Position du `k`-ième bit à 1 (select).
    fn select(&self, mut k: usize) -> usize {
        for (w, &word) in self.0.iter().enumerate() {
            let p = word.count_ones() as usize;
            if k < p {
                let mut x = word;
                for _ in 0..k {
                    x &= x - 1;
                }
                return w * 64 + x.trailing_zeros() as usize;
            }
            k -= p;
        }
        unreachable!("select hors bornes")
    }
}

impl Bits for U256 {
    #[inline]
    fn bit(&self, i: usize) -> bool {
        self.0[i >> 6] >> (i & 63) & 1 == 1
    }

    #[inline]
    fn pop_below(&self, i: usize) -> usize {
        let w = i >> 6;
        let mut n = 0;
        for k in 0..w.min(4) {
            n += self.0[k].count_ones() as usize;
        }
        if w < 4 {
            n += (self.0[w] & mask64(i & 63)).count_ones() as usize;
        }
        n
    }

    fn insert(&mut self, i: usize, bit: bool) {
        let (w, b) = (i >> 6, i & 63);
        for k in (w + 1..4).rev() {
            self.0[k] = (self.0[k] << 1) | (self.0[k - 1] >> 63);
        }
        let x = self.0[w];
        let m = mask64(b);
        self.0[w] = (x & m) | ((x & !m) << 1) | ((bit as u64) << b);
    }

    fn remove(&mut self, i: usize) {
        let (w, b) = (i >> 6, i & 63);
        let x = self.0[w];
        let m = mask64(b);
        let carry = if w < 3 { self.0[w + 1] & 1 } else { 0 };
        self.0[w] = (x & m) | ((x >> 1) & !m & !(1 << 63)) | (carry << 63);
        for k in w + 1..4 {
            let carry = if k < 3 { self.0[k + 1] & 1 } else { 0 };
            self.0[k] = (self.0[k] >> 1) | (carry << 63);
        }
    }
}

// ─── Segment ─────────────────────────────────────────────────────────────────

/// Vue temporaire d'un segment. Les maps sont des copies locales (bit 0 =
/// première entrée du segment) qui suivent les insert/remove de la vue ;
/// l'appelant les réécrit là où elles vivent.
struct Seg<M: Bits> {
    /// Nul dans les formes larges (radix donné par le bitmap).
    radixs: *mut u8,
    tags: *mut Tag,
    cells: *mut u8,
    cap: usize,
    n: usize,
    max_n: usize,
    c: M,
    t: M,
    s: M,
}

impl<M: Bits> Seg<M> {
    #[inline]
    fn leaf_bytes_below(&self, i: usize) -> usize {
        8 * (i - self.c.pop_below(i) + self.t.pop_below(i) + self.s.pop_below(i))
    }

    #[inline]
    fn leaf_bytes(&self) -> usize {
        self.leaf_bytes_below(self.n)
    }

    #[inline]
    fn kid_bytes(&self) -> usize {
        4 * self.c.pop_below(self.n)
    }

    #[inline]
    fn used(&self) -> usize {
        self.kid_bytes() + self.leaf_bytes()
    }

    #[inline]
    fn fits(&self, size: usize) -> bool {
        self.n < self.max_n && self.used() + size <= self.cap
    }

    #[inline]
    fn size_of(&self, i: usize) -> usize {
        if self.c.bit(i) {
            4
        } else {
            8 * (1 + self.t.bit(i) as usize + self.s.bit(i) as usize)
        }
    }

    /// Fin (exclue) du record de la feuille `i`.
    #[inline]
    fn leaf_end(&self, i: usize) -> usize {
        self.cap - self.leaf_bytes_below(i)
    }

    #[inline]
    fn child_off(&self, i: usize) -> usize {
        4 * self.c.pop_below(i)
    }

    /// `radix` vaut 0 dans les formes larges : l'appelant le complète.
    unsafe fn read(&self, i: usize) -> Raw {
        unsafe {
            let mut raw = Raw {
                radix: if self.radixs.is_null() {
                    0
                } else {
                    *self.radixs.add(i)
                },
                tag: *self.tags.add(i),
                child: self.c.bit(i),
                ttl: self.t.bit(i),
                sfx: self.s.bit(i),
                idx: 0,
                val: 0,
                exp: 0,
                path: 0,
            };
            if raw.child {
                raw.idx = (self.cells.add(self.child_off(i)) as *const u32).read();
            } else {
                let end = self.leaf_end(i);
                raw.val = (self.cells.add(end - 8) as *const u64).read();
                if raw.ttl {
                    raw.exp = (self.cells.add(end - 16) as *const u64).read();
                }
                if raw.sfx {
                    raw.path = (self.cells.add(end - raw.size()) as *const u64).read();
                }
            }
            raw
        }
    }

    unsafe fn entry_ref<'a>(&self, i: usize) -> EntryRef<'a> {
        unsafe {
            if self.c.bit(i) {
                return EntryRef::Child((self.cells.add(self.child_off(i)) as *const u32).read());
            }
            let end = self.leaf_end(i);
            EntryRef::Leaf {
                tag: *self.tags.add(i),
                val: &*(self.cells.add(end - 8) as *const ValUnion),
                ttl: self
                    .t
                    .bit(i)
                    .then(|| (self.cells.add(end - 16) as *const u64).read()),
                sfx: self
                    .s
                    .bit(i)
                    .then(|| &*(self.cells.add(end - self.size_of(i)) as *const CompactPath)),
            }
        }
    }

    unsafe fn insert(&mut self, i: usize, raw: &Raw) {
        debug_assert!(i <= self.n && self.fits(raw.size()));
        unsafe {
            if !self.radixs.is_null() {
                ptr::copy(self.radixs.add(i), self.radixs.add(i + 1), self.n - i);
                *self.radixs.add(i) = raw.radix;
            }
            ptr::copy(self.tags.add(i), self.tags.add(i + 1), self.n - i);
            *self.tags.add(i) = raw.tag;
            if raw.child {
                let off = self.child_off(i);
                let end = self.kid_bytes();
                ptr::copy(self.cells.add(off), self.cells.add(off + 4), end - off);
                (self.cells.add(off) as *mut u32).write(raw.idx);
            } else {
                let end = self.leaf_end(i);
                let start = self.cap - self.leaf_bytes();
                let size = raw.size();
                ptr::copy(
                    self.cells.add(start),
                    self.cells.add(start - size),
                    end - start,
                );
                (self.cells.add(end - 8) as *mut u64).write(raw.val);
                if raw.ttl {
                    (self.cells.add(end - 16) as *mut u64).write(raw.exp);
                }
                if raw.sfx {
                    (self.cells.add(end - size) as *mut u64).write(raw.path);
                }
            }
        }
        self.c.insert(i, raw.child);
        self.t.insert(i, raw.ttl);
        self.s.insert(i, raw.sfx);
        self.n += 1;
    }

    unsafe fn remove(&mut self, i: usize) -> Raw {
        debug_assert!(i < self.n);
        unsafe {
            let raw = self.read(i);
            if raw.child {
                let off = self.child_off(i);
                let end = self.kid_bytes();
                ptr::copy(self.cells.add(off + 4), self.cells.add(off), end - off - 4);
            } else {
                let end = self.leaf_end(i);
                let start = self.cap - self.leaf_bytes();
                let size = raw.size();
                ptr::copy(
                    self.cells.add(start),
                    self.cells.add(start + size),
                    end - size - start,
                );
            }
            if !self.radixs.is_null() {
                ptr::copy(self.radixs.add(i + 1), self.radixs.add(i), self.n - i - 1);
            }
            ptr::copy(self.tags.add(i + 1), self.tags.add(i), self.n - i - 1);
            self.c.remove(i);
            self.t.remove(i);
            self.s.remove(i);
            self.n -= 1;
            raw
        }
    }
}

/// Position de `radix` parmi les `n` premiers octets, en une passe SWAR.
#[inline]
fn swar_find(radixs: [u8; 8], n: usize, radix: u8) -> Option<usize> {
    let word = u64::from_le_bytes(radixs);
    let x = word ^ (0x0101_0101_0101_0101u64 * radix as u64);
    let mut z = x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080;
    if n < 8 {
        z &= (1u64 << (8 * n)) - 1;
    }
    (z != 0).then(|| z.trailing_zeros() as usize / 8)
}

/// Emplacement physique d'une entrée, tiré de son segment.
struct Loc {
    tag: *mut Tag,
    cells: *mut u8,
    /// Fin du record (feuille).
    leaf_end: usize,
    /// Décalage de l'idx (enfant).
    child_off: usize,
    child: bool,
    ttl: bool,
}

impl Loc {
    #[inline]
    unsafe fn of<M: Bits>(seg: &Seg<M>, i: usize) -> Self {
        let child = seg.c.bit(i);
        Loc {
            tag: unsafe { seg.tags.add(i) },
            cells: seg.cells,
            leaf_end: if child { 0 } else { seg.leaf_end(i) },
            child_off: if child { seg.child_off(i) } else { 0 },
            child,
            ttl: seg.t.bit(i),
        }
    }
}

/// Placement en petite forme pour une liste de tailles : (entrées dans le
/// nœud, octets de la page). `None` si la page dépasse 8 entrées.
fn plan(sizes: &[u8]) -> Option<(usize, usize)> {
    if sizes.len() > SMALL_MAX {
        return None;
    }
    let mut n_in = 0;
    let mut used = 0;
    while n_in < sizes.len() && n_in < SMALL_ENTRIES && used + sizes[n_in] as usize <= NODE_CELLS {
        used += sizes[n_in] as usize;
        n_in += 1;
    }
    if sizes.len() - n_in > SMALL_ENTRIES {
        return None;
    }
    Some((n_in, sizes[n_in..].iter().map(|&s| s as usize).sum()))
}

// ─── Node ────────────────────────────────────────────────────────────────────

impl Node {
    pub(crate) fn new(prefix: CompactPath, parent_idx: u32, parent_radix: u8) -> Self {
        Self {
            ptr: trash(),
            prefix,
            val: ValUnion { integer: 0 },
            exp_and_radix: ExpAndRadix::no_expiracy(parent_radix),
            keys: Keys {
                small: SmallKeys {
                    radixs: [0; SMALL_ENTRIES],
                    tags: [Tag::None; SMALL_ENTRIES],
                },
            },
            parent_idx,
            child_map: 0,
            ttl_map: 0,
            sfx_map: 0,
            kind: Kind::Inline,
            tag: Tag::None,
            count: 0,
            n_in: 0,
            pg_block8: 0,
            body: Body {
                cells: [0; NODE_CELLS / 8],
            },
        }
    }

    #[inline]
    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self.kind {
            Kind::Wide256 => self.radix_bits().pop(),
            _ => self.count as usize,
        }
    }

    /// Précharge la seconde ligne du nœud (cellules ou métadonnées larges).
    #[inline(always)]
    pub(crate) fn prefetch_body(&self) {
        prefetch_l1(unsafe { (self as *const Node as *const u8).add(64) });
    }

    /// Précharge la page sans test : sans page, `ptr` vise `TRASH`.
    #[inline]
    pub(crate) fn prefetch_page(&self) {
        prefetch_l1(self.ptr.as_ptr());
    }

    #[inline]
    fn block(&self) -> usize {
        self.pg_block8 as usize * 8
    }

    #[inline]
    fn small_keys(&self) -> &SmallKeys {
        debug_assert!(self.kind.is_small());
        unsafe { &self.keys.small }
    }

    #[inline]
    fn radix_bits(&self) -> U256 {
        debug_assert!(!self.kind.is_small());
        U256(unsafe { self.body.wide.radix_bits })
    }

    #[inline]
    fn set_radix_bits(&mut self, bits: U256) {
        self.body.wide.radix_bits = bits.0;
    }

    // ── segments ──

    /// # Safety
    /// `this` valide ; écrire via le `Seg` exige un `this` issu d'un `&mut`.
    #[inline]
    unsafe fn node_seg_raw(this: *mut Node) -> Seg<u16> {
        unsafe {
            let n = (*this).n_in as usize;
            let m = mask64(n) as u16;
            Seg {
                radixs: ptr::addr_of_mut!((*this).keys.small.radixs) as *mut u8,
                tags: ptr::addr_of_mut!((*this).keys.small.tags) as *mut Tag,
                cells: ptr::addr_of_mut!((*this).body.cells) as *mut u8,
                cap: NODE_CELLS,
                n,
                max_n: SMALL_ENTRIES,
                c: (*this).child_map & m,
                t: (*this).ttl_map & m,
                s: (*this).sfx_map & m,
            }
        }
    }

    #[inline]
    fn node_seg(&self) -> Seg<u16> {
        unsafe { Self::node_seg_raw(self as *const Node as *mut Node) }
    }

    #[inline]
    fn node_seg_mut(&mut self) -> Seg<u16> {
        unsafe { Self::node_seg_raw(self) }
    }

    #[inline]
    fn page_seg(&self) -> Seg<u16> {
        let start = self.n_in as usize;
        let n = self.count as usize - start;
        let m = mask64(n) as u16;
        let p = self.ptr.as_ptr();
        Seg {
            radixs: p,
            tags: unsafe { p.add(SMALL_ENTRIES) } as *mut Tag,
            cells: unsafe { p.add(SMALL_PAGE_HEADER) },
            cap: self.block().saturating_sub(SMALL_PAGE_HEADER),
            n,
            max_n: SMALL_ENTRIES,
            c: (self.child_map >> start) & m,
            t: (self.ttl_map >> start) & m,
            s: (self.sfx_map >> start) & m,
        }
    }

    /// # Safety
    /// Comme `node_seg_raw` (les tags vivent dans le nœud).
    #[inline]
    unsafe fn wide32_seg_raw(this: *mut Node) -> Seg<u32> {
        unsafe {
            let maps = (*this).keys.maps;
            Seg {
                radixs: ptr::null_mut(),
                tags: ptr::addr_of_mut!((*this).body.wide.tags) as *mut Tag,
                cells: (*this).ptr.as_ptr(),
                cap: (*this).block(),
                n: (*this).count as usize,
                max_n: WIDE32_MAX,
                c: maps[0],
                t: maps[1],
                s: maps[2],
            }
        }
    }

    #[inline]
    fn wide32_store(&mut self, seg: &Seg<u32>) {
        self.keys.maps = [seg.c, seg.t, seg.s, 0];
        self.count = seg.n as u8;
    }

    /// Segment `Wide256` avec une zone de tags de `tags_len` octets.
    fn wide256_seg(&self, tags_len: usize) -> Seg<U256> {
        let p = self.ptr.as_ptr();
        unsafe {
            let maps = p as *const [u64; 4];
            let head = WIDE_MAPS + tags_len;
            Seg {
                radixs: ptr::null_mut(),
                tags: p.add(WIDE_MAPS) as *mut Tag,
                cells: p.add(head),
                cap: self.block() - head,
                n: self.len(),
                max_n: 256,
                c: U256(maps.read()),
                t: U256(maps.add(1).read()),
                s: U256(maps.add(2).read()),
            }
        }
    }

    fn wide256_store(&mut self, seg: &Seg<U256>) {
        unsafe {
            let maps = self.ptr.as_ptr() as *mut [u64; 4];
            maps.write(seg.c.0);
            maps.add(1).write(seg.t.0);
            maps.add(2).write(seg.s.0);
        }
    }

    // ── lecture ──

    /// Rang de `radix`, ou rang d'insertion (`Err`).
    #[inline]
    pub(crate) fn search(&self, radix: u8) -> Result<usize, usize> {
        if !self.kind.is_small() {
            let bits = self.radix_bits();
            let e = bits.pop_below(radix as usize);
            return if bits.bit(radix as usize) {
                Ok(e)
            } else {
                Err(e)
            };
        }
        let n_in = self.n_in as usize;
        let count = self.count as usize;
        let radixs = self.small_keys().radixs;
        if n_in > 0 && radix <= radixs[n_in - 1] {
            return match swar_find(radixs, n_in, radix) {
                Some(i) => Ok(i),
                None => Err(radixs[..n_in].iter().take_while(|&&r| r < radix).count()),
            };
        }
        let page_n = count - n_in;
        if page_n == 0 {
            return Err(count);
        }
        let page = unsafe { (self.ptr.as_ptr() as *const [u8; 8]).read() };
        match swar_find(page, page_n, radix) {
            Some(i) => Ok(n_in + i),
            None => Err(n_in + page[..page_n].iter().take_while(|&&r| r < radix).count()),
        }
    }

    /// La recherche de `radix` doit-elle lire l'en-tête de la page
    /// (radixs du débordement en petite forme) ?
    #[inline]
    pub(crate) fn search_needs_page(&self, radix: u8) -> bool {
        self.kind == Kind::Overflow8 && {
            let n_in = self.n_in as usize;
            n_in == 0 || radix > self.small_keys().radixs[n_in - 1]
        }
    }

    #[inline]
    pub(crate) fn find(&self, radix: u8) -> Option<usize> {
        self.search(radix).ok()
    }

    #[inline]
    pub(crate) fn radix_at(&self, e: usize) -> u8 {
        if !self.kind.is_small() {
            return self.radix_bits().select(e) as u8;
        }
        let n_in = self.n_in as usize;
        if e < n_in {
            self.small_keys().radixs[e]
        } else {
            unsafe { *self.ptr.as_ptr().add(e - n_in) }
        }
    }

    #[inline]
    pub(crate) fn entry_ref(&self, e: usize) -> EntryRef<'_> {
        unsafe {
            match self.kind {
                Kind::Inline | Kind::Overflow8 => {
                    let n_in = self.n_in as usize;
                    if e < n_in {
                        self.node_seg().entry_ref(e)
                    } else {
                        self.page_seg().entry_ref(e - n_in)
                    }
                }
                Kind::Wide32 => Self::wide32_seg_raw(self as *const Node as *mut Node).entry_ref(e),
                Kind::Wide256 => self.wide256_seg(round8(self.len())).entry_ref(e),
            }
        }
    }

    #[inline]
    pub(crate) fn get(&self, radix: u8) -> Option<EntryRef<'_>> {
        match self.probe(radix) {
            Probe::Miss => None,
            Probe::Here(entry) => Some(entry),
            Probe::Page(e, _) => Some(self.entry_ref(e)),
        }
    }

    /// Recherche de `radix` pour une descente : l'entrée si elle est déjà dans
    /// le nœud, sinon son rang et l'adresse de sa cellule dans la page (à
    /// précharger avant de la lire).
    #[inline(always)]
    pub(crate) fn probe(&self, radix: u8) -> Probe<'_> {
        if self.kind.is_small() {
            let n_in = self.n_in as usize;
            let radixs = self.small_keys().radixs;
            if n_in > 0 && radix <= radixs[n_in - 1] {
                return match swar_find(radixs, n_in, radix) {
                    Some(e) => Probe::Here(self.inline_entry(e)),
                    None => Probe::Miss,
                };
            }
        }
        let Some(e) = self.find(radix) else {
            return Probe::Miss;
        };
        match self.entry_addr(e) {
            Some(addr) => Probe::Page(e, addr),
            None => Probe::Here(self.entry_ref(e)),
        }
    }

    /// Entrée `e` des cellules du nœud (petite forme, `e < n_in`), lue
    /// directement sur les maps globales : les bits sous `e` sont tous dans
    /// le nœud, aucun segment à construire.
    #[inline(always)]
    fn inline_entry(&self, e: usize) -> EntryRef<'_> {
        debug_assert!(self.kind.is_small() && e < self.n_in as usize);
        let m = ((1u32 << e) - 1) as u16;
        let c = self.child_map;
        let cells = unsafe { self.body.cells.as_ptr() as *const u8 };
        unsafe {
            if c >> e & 1 == 1 {
                return EntryRef::Child((cells.add(4 * (c & m).count_ones() as usize) as *const u32).read());
            }
            let (t, sx) = (self.ttl_map, self.sfx_map);
            let leaves_below = e - (c & m).count_ones() as usize;
            let end = NODE_CELLS - 8 * (leaves_below + (t & m).count_ones() as usize + (sx & m).count_ones() as usize);
            let has_ttl = t >> e & 1 == 1;
            let has_sfx = sx >> e & 1 == 1;
            EntryRef::Leaf {
                tag: self.keys.small.tags[e],
                val: &*(cells.add(end - 8) as *const ValUnion),
                ttl: has_ttl.then(|| (cells.add(end - 16) as *const u64).read()),
                sfx: has_sfx.then(|| {
                    let size = 8 * (1 + has_ttl as usize + 1);
                    &*(cells.add(end - size) as *const CompactPath)
                }),
            }
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u8, EntryRef<'_>)> {
        (0..self.len()).map(|e| (self.radix_at(e), self.entry_ref(e)))
    }

    /// Emplacement physique de l'entrée de rang `e`.
    fn loc_mut(&mut self, e: usize) -> Loc {
        unsafe { Self::loc_raw(self, e) }
    }

    /// # Safety
    /// Comme `node_seg_raw` : écrire via le `Loc` exige un `this` issu d'un `&mut`.
    #[inline]
    unsafe fn loc_raw(this: *mut Node, e: usize) -> Loc {
        unsafe {
            match (*this).kind {
                Kind::Inline | Kind::Overflow8 => {
                    let n_in = (*this).n_in as usize;
                    if e < n_in {
                        Loc::of(&Self::node_seg_raw(this), e)
                    } else {
                        Loc::of(&(*this).page_seg(), e - n_in)
                    }
                }
                Kind::Wide32 => Loc::of(&Self::wide32_seg_raw(this), e),
                Kind::Wide256 => Loc::of(&(*this).wide256_seg(round8((*this).len())), e),
            }
        }
    }

    /// Valeur d'une feuille, en place. Invalide dès la prochaine mutation.
    pub(crate) fn leaf_val_mut(&mut self, radix: u8) -> Option<(&mut Tag, &mut ValUnion)> {
        let e = self.find(radix)?;
        let loc = self.loc_mut(e);
        if loc.child {
            return None;
        }
        unsafe { Some((&mut *loc.tag, &mut *(loc.cells.add(loc.leaf_end - 8) as *mut ValUnion))) }
    }

    /// Remplace valeur et TTL d'une feuille existante (l'ancienne valeur est
    /// libérée). En place si la présence du TTL ne change pas.
    /// `false` (et `val` non consommée… donc libérée) si `radix` n'est pas une feuille.
    pub(crate) fn update_leaf(&mut self, radix: u8, tag: Tag, val: ValUnion, ttl: Option<u64>) -> bool {
        let mut val = ManuallyDrop::new(val);
        let Some(e) = self.find(radix) else {
            unsafe { drop_raw(tag, &mut val) };
            return false;
        };
        let loc = self.loc_mut(e);
        if loc.child {
            unsafe { drop_raw(tag, &mut val) };
            return false;
        }
        unsafe {
            let slot = loc.cells.add(loc.leaf_end - 8) as *mut ValUnion;
            drop_raw(*loc.tag, &mut *slot);
            *loc.tag = tag;
            slot.write(ManuallyDrop::into_inner(val));
            if loc.ttl == ttl.is_some() {
                if let Some(exp) = ttl {
                    (loc.cells.add(loc.leaf_end - 16) as *mut u64).write(exp);
                }
                return true;
            }
        }
        self.resize_leaf_ttl(e, radix, ttl);
        true
    }

    /// Réécrit en place l'idx d'une entrée enfant. `false` si ce n'est pas un enfant.
    pub(crate) fn set_child_idx(&mut self, radix: u8, idx: u32) -> bool {
        let Some(e) = self.find(radix) else {
            return false;
        };
        let loc = self.loc_mut(e);
        if !loc.child {
            return false;
        }
        unsafe { (loc.cells.add(loc.child_off) as *mut u32).write(idx) };
        true
    }

    #[inline]
    pub(crate) fn has_val(&self) -> bool {
        self.tag != Tag::None
    }

    /// Une valeur du nœud (self-val ou feuille) porte-t-elle un TTL ? C'est la
    /// condition pour que le nœud soit tagué dans le slab (évicteur).
    pub(crate) fn has_any_ttl(&self) -> bool {
        if self.has_val() && self.exp_and_radix.does_expire() {
            return true;
        }
        match self.kind {
            Kind::Inline | Kind::Overflow8 => self.ttl_map != 0,
            Kind::Wide32 => unsafe { self.keys.maps[1] != 0 },
            Kind::Wide256 => self.wide256_seg(round8(self.len())).t != U256::default(),
        }
    }

    /// Adresse de la cellule de l'entrée `e` (idx d'enfant, ou valeur de la
    /// feuille), pour la précharger. `None` si elle est déjà dans le nœud.
    #[inline]
    pub(crate) fn entry_addr(&self, e: usize) -> Option<*const u8> {
        if self.kind.is_small() && e < self.n_in as usize {
            return None;
        }
        let loc = unsafe { Self::loc_raw(self as *const Node as *mut Node, e) };
        Some(unsafe {
            if loc.child {
                loc.cells.add(loc.child_off)
            } else {
                loc.cells.add(loc.leaf_end - 8)
            }
        })
    }

    /// Taille du bloc de page (0 sans page).
    #[inline]
    pub(crate) fn page_block(&self) -> usize {
        self.block()
    }

    // ── page ──

    /// Taille d'en-tête de page de la forme courante.
    fn page_header(&self) -> usize {
        match self.kind {
            Kind::Inline | Kind::Overflow8 => SMALL_PAGE_HEADER,
            Kind::Wide32 => 0,
            Kind::Wide256 => WIDE_MAPS + round8(self.len()),
        }
    }

    /// (octets d'enfants, octets de feuilles) dans la page.
    fn page_usage(&self) -> (usize, usize) {
        if self.pg_block8 == 0 {
            return (0, 0);
        }
        match self.kind {
            Kind::Inline | Kind::Overflow8 => {
                let s = self.page_seg();
                (s.kid_bytes(), s.leaf_bytes())
            }
            Kind::Wide32 => {
                let s = unsafe { Self::wide32_seg_raw(self as *const Node as *mut Node) };
                (s.kid_bytes(), s.leaf_bytes())
            }
            Kind::Wide256 => {
                let s = self.wide256_seg(round8(self.len()));
                (s.kid_bytes(), s.leaf_bytes())
            }
        }
    }

    /// Réalloue la page à `new_block` octets (0 = libère) : en-tête et enfants
    /// restent au début, les feuilles restent collées à la fin.
    fn page_resize(&mut self, new_block: usize) {
        let old_block = self.block();
        if new_block == old_block {
            return;
        }
        let head = if old_block > 0 { self.page_header() } else { 0 };
        let (kids, leaves) = self.page_usage();
        unsafe {
            let new_ptr = if new_block == 0 {
                debug_assert!(kids + leaves == 0);
                trash()
            } else {
                let p = NonNull::new(mi_malloc(new_block)).expect("page alloc");
                if old_block > 0 {
                    debug_assert!(head + kids + leaves <= new_block);
                    let old = self.ptr.as_ptr();
                    ptr::copy_nonoverlapping(old, p.as_ptr(), head + kids);
                    ptr::copy_nonoverlapping(
                        old.add(old_block - leaves),
                        p.as_ptr().add(new_block - leaves),
                        leaves,
                    );
                }
                p
            };
            if old_block > 0 {
                mi_free(self.ptr.as_ptr());
            }
            self.ptr = new_ptr;
        }
        self.pg_block8 = (new_block / 8) as u16;
    }

    /// Garantit un bloc d'au moins `need` octets (classe mimalloc).
    fn page_grow(&mut self, need: usize) {
        if need > self.block() {
            self.page_resize(alloc_class(need));
        }
    }

    /// Réduit la page si le besoin passe sous ¾ du bloc ; la libère si vide
    /// en petite forme.
    fn page_trim(&mut self) {
        if self.pg_block8 == 0 {
            return;
        }
        let (kids, leaves) = self.page_usage();
        if self.kind.is_small() && kids + leaves == 0 {
            self.page_resize(0);
            self.kind = Kind::Inline;
            return;
        }
        let need = alloc_class(self.page_header() + kids + leaves);
        if need * 4 <= self.block() * 3 {
            self.page_resize(need);
        }
    }

    // ── petites formes ──

    fn sizes(&self, out: &mut [u8; SMALL_MAX + 1]) -> usize {
        let n = self.count as usize;
        for (e, s) in out.iter_mut().enumerate().take(n) {
            *s = if self.child_map.bit(e) {
                4
            } else {
                8 * (1 + self.ttl_map.bit(e) as u8 + self.sfx_map.bit(e) as u8)
            };
        }
        n
    }

    /// Dernière entrée du nœud → tête de page. Maps globales inchangées.
    fn spill_one(&mut self) {
        let mut page = self.page_seg();
        let mut node = self.node_seg_mut();
        unsafe {
            let raw = node.remove(node.n - 1);
            page.insert(0, &raw);
        }
        self.n_in -= 1;
    }

    /// Tête de page → fin du nœud.
    fn pull_one(&mut self) {
        let mut page = self.page_seg();
        let mut node = self.node_seg_mut();
        unsafe {
            let raw = page.remove(0);
            node.insert(node.n, &raw);
        }
        self.n_in += 1;
    }

    /// Insère au rang `e` en petite forme. `false` si le débordement devrait
    /// dépasser 8 entrées : l'appelant passe en `Wide32` (l'état reste valide,
    /// éventuellement avec quelques entrées déjà déversées).
    ///
    /// Sans débordement et avec de la place, insertion directe. Sinon on
    /// déverse la queue du nœud vers le débordement (croissance seulement),
    /// une entrée O(1) à la fois, agrandi à la demande.
    fn small_insert_at(&mut self, e: usize, raw: &Raw) -> bool {
        let size = raw.size();
        if self.pg_block8 == 0 && self.node_seg().fits(size) {
            unsafe { self.node_seg_mut().insert(e, raw) };
            self.n_in += 1;
            self.small_maps_insert(e, raw);
            return true;
        }
        while e < self.n_in as usize && !self.node_seg().fits(size) {
            let node = self.node_seg();
            if !self.page_room(node.size_of(node.n - 1)) {
                return false;
            }
            self.spill_one();
        }
        let n_in = self.n_in as usize;
        unsafe {
            if e <= n_in && self.node_seg().fits(size) {
                self.node_seg_mut().insert(e, raw);
                self.n_in += 1;
            } else {
                if !self.page_room(size) {
                    return false;
                }
                self.page_seg().insert(e - n_in, raw);
            }
        }
        self.small_maps_insert(e, raw);
        true
    }

    #[inline]
    fn small_maps_insert(&mut self, e: usize, raw: &Raw) {
        self.child_map.insert(e, raw.child);
        self.ttl_map.insert(e, raw.ttl);
        self.sfx_map.insert(e, raw.sfx);
        self.count += 1;
    }

    /// Place pour une entrée de `size` octets de plus dans le débordement
    /// (agrandi à la demande). `false` s'il a déjà 8 entrées.
    fn page_room(&mut self, size: usize) -> bool {
        if self.count as usize - self.n_in as usize >= SMALL_ENTRIES {
            return false;
        }
        let (kids, leaves) = self.page_usage();
        self.page_grow(SMALL_PAGE_HEADER + kids + leaves + size);
        self.kind = Kind::Overflow8;
        true
    }

    /// Retire le rang `e` puis remonte la tête de page tant qu'elle tient.
    fn small_remove_at(&mut self, e: usize) -> Raw {
        let n_in = self.n_in as usize;
        let raw = unsafe {
            if e < n_in {
                let raw = self.node_seg_mut().remove(e);
                self.n_in -= 1;
                raw
            } else {
                self.page_seg().remove(e - n_in)
            }
        };
        self.child_map.remove(e);
        self.ttl_map.remove(e);
        self.sfx_map.remove(e);
        self.count -= 1;
        while self.count > self.n_in {
            let size = unsafe { self.page_seg().read(0) }.size();
            if !self.node_seg().fits(size) {
                break;
            }
            self.pull_one();
        }
        raw
    }

    // ── formes larges ──

    fn wide32_insert_at(&mut self, e: usize, raw: &Raw) {
        let (kids, leaves) = self.page_usage();
        self.page_grow(kids + leaves + raw.size());
        unsafe {
            let mut seg = Self::wide32_seg_raw(self);
            seg.insert(e, raw);
            self.wide32_store(&seg);
        }
        let mut bits = self.radix_bits();
        bits.set(raw.radix as usize, true);
        self.set_radix_bits(bits);
    }

    fn wide32_remove_at(&mut self, e: usize) -> Raw {
        let mut raw = unsafe {
            let mut seg = Self::wide32_seg_raw(self);
            let raw = seg.remove(e);
            self.wide32_store(&seg);
            raw
        };
        raw.radix = self.radix_bits().select(e) as u8;
        let mut bits = self.radix_bits();
        bits.set(raw.radix as usize, false);
        self.set_radix_bits(bits);
        raw
    }

    fn wide256_insert_at(&mut self, e: usize, raw: &Raw) {
        let n = self.len();
        let (tags_old, tags_new) = (round8(n), round8(n + 1));
        let (kids, leaves) = self.page_usage();
        self.page_grow(WIDE_MAPS + tags_new + kids + leaves + raw.size());
        unsafe {
            if tags_new > tags_old {
                // la zone de tags gagne 8 o : les enfants glissent à droite
                let base = self.ptr.as_ptr().add(WIDE_MAPS);
                ptr::copy(base.add(tags_old), base.add(tags_new), kids);
            }
            let mut seg = self.wide256_seg(tags_new);
            seg.insert(e, raw);
            self.wide256_store(&seg);
        }
        let mut bits = self.radix_bits();
        bits.set(raw.radix as usize, true);
        self.set_radix_bits(bits);
    }

    fn wide256_remove_at(&mut self, e: usize) -> Raw {
        let n = self.len();
        let (tags_old, tags_new) = (round8(n), round8(n - 1));
        let mut raw = unsafe {
            let mut seg = self.wide256_seg(tags_old);
            let raw = seg.remove(e);
            self.wide256_store(&seg);
            if tags_new < tags_old {
                let base = self.ptr.as_ptr().add(WIDE_MAPS);
                ptr::copy(base.add(tags_old), base.add(tags_new), seg.kid_bytes());
            }
            raw
        };
        raw.radix = self.radix_bits().select(e) as u8;
        let mut bits = self.radix_bits();
        bits.set(raw.radix as usize, false);
        self.set_radix_bits(bits);
        raw
    }

    // ── changements de forme ──

    /// Vide le nœud de ses entrées (propriété transférée aux `Raw`) et le
    /// remet en `Inline` sans page.
    fn drain(&mut self) -> Vec<Raw> {
        let n = self.len();
        let mut out = Vec::with_capacity(n);
        for e in 0..n {
            let radix = self.radix_at(e);
            let mut raw = unsafe {
                match self.kind {
                    Kind::Inline | Kind::Overflow8 => {
                        let n_in = self.n_in as usize;
                        if e < n_in {
                            self.node_seg().read(e)
                        } else {
                            self.page_seg().read(e - n_in)
                        }
                    }
                    Kind::Wide32 => Self::wide32_seg_raw(self).read(e),
                    Kind::Wide256 => self.wide256_seg(round8(n)).read(e),
                }
            };
            raw.radix = radix;
            out.push(raw);
        }
        if self.pg_block8 > 0 {
            unsafe { mi_free(self.ptr.as_ptr()) };
        }
        self.ptr = trash();
        self.pg_block8 = 0;
        self.kind = Kind::Inline;
        self.keys = Keys {
            small: SmallKeys {
                radixs: [0; SMALL_ENTRIES],
                tags: [Tag::None; SMALL_ENTRIES],
            },
        };
        self.body = Body {
            cells: [0; NODE_CELLS / 8],
        };
        self.child_map = 0;
        self.ttl_map = 0;
        self.sfx_map = 0;
        self.count = 0;
        self.n_in = 0;
        out
    }

    /// Reconstruit dans la forme `kind` (le nœud sort de `drain`).
    fn rebuild(&mut self, kind: Kind, raws: &[Raw]) {
        let cells: usize = raws.iter().map(Raw::size).sum();
        match kind {
            Kind::Inline | Kind::Overflow8 => {
                for (e, raw) in raws.iter().enumerate() {
                    assert!(self.small_insert_at(e, raw), "rebuild : ne tient pas en petite forme");
                }
            }
            Kind::Wide32 => {
                self.kind = Kind::Wide32;
                self.keys = Keys { maps: [0; 4] };
                self.body = Body {
                    wide: WideMeta {
                        radix_bits: [0; 4],
                        tags: [Tag::None; WIDE32_MAX],
                    },
                };
                self.page_grow(cells);
                for (e, raw) in raws.iter().enumerate() {
                    self.wide32_insert_at(e, raw);
                }
            }
            Kind::Wide256 => {
                self.kind = Kind::Wide256;
                self.keys = Keys { maps: [0; 4] };
                self.body = Body {
                    wide: WideMeta {
                        radix_bits: [0; 4],
                        tags: [Tag::None; WIDE32_MAX],
                    },
                };
                self.page_grow(WIDE_MAPS + round8(raws.len()) + cells);
                unsafe { ptr::write_bytes(self.ptr.as_ptr(), 0, WIDE_MAPS) };
                for (e, raw) in raws.iter().enumerate() {
                    self.wide256_insert_at(e, raw);
                }
            }
        }
    }

    fn convert(&mut self, kind: Kind) {
        let raws = self.drain();
        self.rebuild(kind, &raws);
    }

    /// Descente de forme après une suppression ou un retrait de TTL.
    fn maybe_shrink_kind(&mut self) {
        match self.kind {
            Kind::Wide256 if self.len() <= WIDE256_DOWN => self.convert(Kind::Wide32),
            Kind::Wide32 if self.len() <= WIDE32_DOWN => {
                let mut sizes = [0u8; SMALL_MAX + 1];
                let seg = unsafe { Self::wide32_seg_raw(self) };
                for (i, s) in sizes.iter_mut().enumerate().take(seg.n) {
                    *s = seg.size_of(i) as u8;
                }
                if plan(&sizes[..seg.n]).is_some() {
                    self.convert(Kind::Inline);
                }
            }
            _ => {}
        }
    }

    // ── API ──

    /// Insère une entrée absente (panique si le radix est déjà là).
    pub(crate) fn insert(&mut self, radix: u8, entry: Entry) {
        let e = match self.search(radix) {
            Ok(_) => panic!("v2::Node::insert : radix {radix} déjà présent"),
            Err(e) => e,
        };
        let raw = Raw::from_entry(radix, entry);
        self.insert_raw(e, &raw);
        self.page_trim();
    }

    fn insert_raw(&mut self, e: usize, raw: &Raw) {
        if self.kind.is_small() {
            if self.small_insert_at(e, raw) {
                return;
            }
            self.convert(Kind::Wide32);
        }
        if self.kind == Kind::Wide32 && self.len() == WIDE32_MAX {
            self.convert(Kind::Wide256);
        }
        match self.kind {
            Kind::Wide32 => self.wide32_insert_at(e, raw),
            Kind::Wide256 => self.wide256_insert_at(e, raw),
            _ => unreachable!(),
        }
    }

    fn remove_raw(&mut self, e: usize) -> Raw {
        match self.kind {
            Kind::Inline | Kind::Overflow8 => self.small_remove_at(e),
            Kind::Wide32 => self.wide32_remove_at(e),
            Kind::Wide256 => self.wide256_remove_at(e),
        }
    }

    pub(crate) fn remove(&mut self, radix: u8) -> Option<Entry> {
        let e = self.find(radix)?;
        let mut raw = self.remove_raw(e);
        raw.radix = radix;
        self.page_trim();
        self.maybe_shrink_kind();
        Some(unsafe { raw.into_entry() })
    }

    /// Pose ou retire le TTL d'une feuille (le record change de taille).
    /// `false` si le radix n'est pas une feuille.
    pub(crate) fn set_ttl(&mut self, radix: u8, ttl: Option<u64>) -> bool {
        let Some(e) = self.find(radix) else {
            return false;
        };
        let loc = self.loc_mut(e);
        if loc.child {
            return false;
        }
        if loc.ttl == ttl.is_some() {
            if let Some(exp) = ttl {
                unsafe { (loc.cells.add(loc.leaf_end - 16) as *mut u64).write(exp) };
            }
            return true;
        }
        self.resize_leaf_ttl(e, radix, ttl);
        true
    }

    /// Le record de la feuille `e` gagne ou perd sa cellule TTL.
    fn resize_leaf_ttl(&mut self, e: usize, radix: u8, ttl: Option<u64>) {
        let mut raw = self.remove_raw(e);
        raw.radix = radix;
        raw.ttl = ttl.is_some();
        raw.exp = ttl.unwrap_or(0);
        self.insert_raw(e, &raw);
        self.page_trim();
        self.maybe_shrink_kind();
    }

    /// Vérifie toutes les invariants de forme (tests).
    #[cfg(test)]
    pub(crate) fn check(&self) {
        let n = self.len();
        for e in 1..n {
            assert!(self.radix_at(e - 1) < self.radix_at(e), "radixs non triés");
        }
        assert_eq!(self.ptr == trash(), self.pg_block8 == 0);
        let (kids, leaves) = self.page_usage();
        if self.pg_block8 > 0 {
            let need = self.page_header() + kids + leaves;
            assert!(need <= self.block(), "page trop petite");
            assert_eq!(self.block(), alloc_class(self.block()), "bloc hors classe");
            assert!(alloc_class(need) * 4 > self.block() * 3, "page non réduite");
        }
        let check_tags = |tags: *const Tag, c: &dyn Fn(usize) -> bool, i: usize| {
            let tag = unsafe { *tags.add(i) };
            assert_eq!(c(i), tag == Tag::None, "tag incohérent");
        };
        match self.kind {
            Kind::Inline | Kind::Overflow8 => {
                assert!(n <= SMALL_MAX);
                let n_in = self.n_in as usize;
                assert!(n_in <= n && n_in <= SMALL_ENTRIES);
                let m = mask64(n) as u16;
                assert_eq!(self.child_map & !m, 0);
                assert_eq!(self.ttl_map & !m, 0);
                assert_eq!(self.sfx_map & !m, 0);
                assert_eq!(self.child_map & (self.ttl_map | self.sfx_map), 0);
                assert!(self.node_seg().used() <= NODE_CELLS);
                let mut sizes = [0u8; SMALL_MAX + 1];
                self.sizes(&mut sizes);
                let (want_in, want_page) = plan(&sizes[..n]).expect("page > 8 entrées");
                assert_eq!(n_in, want_in, "placement non canonique");
                assert_eq!(kids + leaves, want_page);
                assert_eq!(self.kind == Kind::Inline, self.pg_block8 == 0);
                let (node, page) = (self.node_seg(), self.page_seg());
                for i in 0..node.n {
                    check_tags(node.tags, &|i| node.c.bit(i), i);
                }
                for i in 0..page.n {
                    check_tags(page.tags, &|i| page.c.bit(i), i);
                }
            }
            Kind::Wide32 => {
                assert!(n <= WIDE32_MAX);
                if n <= WIDE32_DOWN {
                    let mut sizes = [0u8; SMALL_MAX + 1];
                    let seg = unsafe { Self::wide32_seg_raw(self as *const Node as *mut Node) };
                    for (i, s) in sizes.iter_mut().enumerate().take(n) {
                        *s = seg.size_of(i) as u8;
                    }
                    assert!(plan(&sizes[..n]).is_none(), "Wide32 aurait dû redescendre");
                }
                assert_eq!(self.count as usize, self.radix_bits().pop());
                let seg = unsafe { Self::wide32_seg_raw(self as *const Node as *mut Node) };
                let m = mask64(n) as u32;
                assert_eq!((seg.c | seg.t | seg.s) & !m, 0);
                assert_eq!(seg.c & (seg.t | seg.s), 0);
                for i in 0..n {
                    check_tags(seg.tags, &|i| seg.c.bit(i), i);
                }
            }
            Kind::Wide256 => {
                assert!(n > WIDE256_DOWN);
                let seg = self.wide256_seg(round8(n));
                for k in 0..4 {
                    assert_eq!(seg.c.0[k] & (seg.t.0[k] | seg.s.0[k]), 0);
                }
                assert_eq!(seg.c.pop_below(256), seg.c.pop_below(n));
                assert_eq!(seg.t.pop_below(256), seg.t.pop_below(n));
                assert_eq!(seg.s.pop_below(256), seg.s.pop_below(n));
                for i in 0..n {
                    check_tags(seg.tags, &|i| seg.c.bit(i), i);
                }
            }
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        for raw in self.drain() {
            if !raw.child {
                drop(unsafe { raw.into_entry() });
            }
        }
        unsafe { drop_raw(self.tag, &mut self.val) };
    }
}
