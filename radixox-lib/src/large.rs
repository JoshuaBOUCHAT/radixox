//! Grands objets et objets épinglés, sans jemalloc (MEMORY_ARCH.md §18).
//!
//! Plage virtuelle à elle, granules de 4 Kio, sans huge pages : un trou se rend au
//! système sans casser de page de 2 Mio.
//!
//! - Tas de granules (§18.4) : un objet de `size` ≥ [`HEAP_MIN`] occupe une suite de
//!   `⌈size / 4 Kio⌉` granules. Suites libres indexées en TLSF (deux niveaux), étiquettes
//!   de bord dans un tableau parallèle, fusion avec les voisines libres.
//! - Rendu (§18.5) : une suite libérée est sale (pages tenues) ; [`release_step`] la rend
//!   par `MADV_DONTNEED` après `decay`, sans thread de fond.
//! - Petits objets épinglés (§18.6) : slabs de 64 Kio par classe, pris au tas.
//!
//! Mono-thread : état global en `static mut` (ARCHITECTURE.md §1, `RUST_TEST_THREADS=1`).
//! Sert aussi d'allocateur global du binaire ([`crate::raw_alloc::heap::RxAlloc`]).
//! Les tests construisent leurs propres [`Large`], plus petits.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

pub const GRANULE_SHIFT: u32 = 12;
/// Unité d'allocation et de rendu du tas.
pub const GRANULE: usize = 1 << GRANULE_SHIFT;
/// À partir de cette taille, une suite de granules ; en dessous, un slab par classe.
pub const HEAP_MIN: usize = 16 * 1024;
const SLAB_GRANULES: u32 = 16;
const SLAB: usize = SLAB_GRANULES as usize * GRANULE;
/// Octets rendus au plus par appel de [`release_step`] (`MADV_DONTNEED` coûte en
/// proportion des pages tenues).
const RELEASE_BUDGET: u64 = 16 << 20;
/// Taille de la file des suites sales en dessous de laquelle on ne la nettoie pas.
const FIFO_MIN: usize = 4096;

// Étiquette d'un granule de bord : premier (`START`) ou dernier granule d'une suite.
// Les granules intérieurs ne sont jamais lus ; une jonction effacée par une fusion est
// remise à 0, pour qu'une entrée périmée de la file de rendu ne passe pas pour un début.
const FREE: u32 = 1 << 31;
const DIRTY: u32 = 1 << 30;
const START: u32 = 1 << 29;
const LEN: u32 = START - 1;
const NONE: u32 = u32::MAX;

// TLSF : niveau 1 = log2 de la longueur, niveau 2 = 16 sous-classes ; en dessous de 16
// granules, une liste par longueur exacte (niveau 1 = 0).
const SL_BITS: u32 = 4;
const SL: u32 = 1 << SL_BITS;
const FL: usize = 32;

#[inline]
fn mapping(n: u32) -> (usize, usize) {
    if n < SL {
        (0, n as usize)
    } else {
        let f = 31 - n.leading_zeros();
        ((f - SL_BITS + 1) as usize, ((n >> (f - SL_BITS)) - SL) as usize)
    }
}

/// Première liste dont toutes les suites ont au moins `n` granules.
#[inline]
fn mapping_search(n: u32) -> (usize, usize) {
    if n < SL {
        return (0, n as usize);
    }
    let f = 31 - n.leading_zeros();
    mapping(n.saturating_add((1 << (f - SL_BITS)) - 1))
}

struct Index {
    fl_mask: u32,
    sl_mask: [u16; FL],
    heads: [[u32; SL as usize]; FL],
}

impl Index {
    const EMPTY: Index = Index { fl_mask: 0, sl_mask: [0; FL], heads: [[NONE; SL as usize]; FL] };

    /// Tête d'une liste d'au moins `n` granules.
    fn find(&self, n: u32) -> Option<u32> {
        let (fl, sl) = mapping_search(n);
        if fl >= FL {
            return None;
        }
        let bits = self.sl_mask[fl] as u32 & (u32::MAX << sl);
        let (fl, bits) = if bits != 0 {
            (fl, bits)
        } else {
            let fls = self.fl_mask & u32::MAX.checked_shl(fl as u32 + 1).unwrap_or(0);
            if fls == 0 {
                return None;
            }
            let fl = fls.trailing_zeros() as usize;
            (fl, self.sl_mask[fl] as u32)
        };
        Some(self.heads[fl][bits.trailing_zeros() as usize])
    }
}

fn map(len: usize) -> Option<*mut u8> {
    crate::sys::map_anon(len, true).ok()
}

/// Tableau à capacité fixe sur un mapping à lui (`MAP_NORESERVE` : seules les pages
/// écrites comptent). Les tables du tas ne passent jamais par l'allocateur global, qui est
/// le tas lui-même : pas de réentrance, pas de repli.
struct MapArr<T: Copy> {
    ptr: *mut T,
    cap: usize,
    len: usize,
    /// Plus haute longueur depuis le dernier rendu (pages peut-être résidentes).
    touched: usize,
    /// Registre des pages, par page de 4 Kio (`MADV_NOHUGEPAGE`).
    pages: crate::pages::Tranches,
}

impl<T: Copy> MapArr<T> {
    fn new(cap: usize) -> Option<Self> {
        let p = map(Self::bytes_for(cap))?;
        // Petites tables : sous THP `always`, la première écriture prendrait 2 Mio.
        unsafe { crate::sys::advise(p, Self::bytes_for(cap), crate::sys::Advice::LinuxNoHugepage) };
        Some(Self { ptr: p as *mut T, cap, len: 0, touched: 0, pages: crate::pages::Tranches::new(p as usize, crate::pages::PAGE) })
    }

    fn bytes_for(cap: usize) -> usize {
        (cap * size_of::<T>()).max(1)
    }

    #[inline]
    fn push(&mut self, v: T) {
        assert!(self.len < self.cap, "table du tas pleine");
        unsafe { self.ptr.add(self.len).write(v) };
        self.len += 1;
        if self.len > self.touched {
            self.touched = self.len;
            self.pages.ensure(self.len * size_of::<T>());
        }
    }

    #[inline]
    fn pop(&mut self) -> Option<T> {
        (self.len > 0).then(|| {
            self.len -= 1;
            unsafe { self.ptr.add(self.len).read() }
        })
    }

    /// Longueur ramenée à `len` ; les pages entières au-delà de `keep` éléments sont
    /// rendues si plus de `2 × keep` ont été touchés.
    fn truncate(&mut self, len: usize, keep: usize) {
        debug_assert!(len <= self.len);
        self.len = len;
        let keep = keep.max(len);
        if self.touched > 2 * keep {
            let from = (keep * size_of::<T>()).next_multiple_of(4096);
            let to = (self.touched * size_of::<T>()).next_multiple_of(4096);
            if to > from {
                unsafe { crate::sys::advise((self.ptr as *mut u8).add(from), to - from, crate::sys::Advice::LinuxDontNeed) };
            }
            self.touched = keep;
            self.pages.shrink(from);
        }
    }

    fn resident(&self) -> u64 {
        crate::resident::region(self.ptr as usize, self.touched * size_of::<T>(), Self::bytes_for(self.cap))
    }
}

impl<T: Copy> std::ops::Index<usize> for MapArr<T> {
    type Output = T;
    #[inline]
    fn index(&self, i: usize) -> &T {
        assert!(i < self.len);
        unsafe { &*self.ptr.add(i) }
    }
}

impl<T: Copy> std::ops::IndexMut<usize> for MapArr<T> {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut T {
        assert!(i < self.len);
        unsafe { &mut *self.ptr.add(i) }
    }
}

impl<T: Copy> Drop for MapArr<T> {
    fn drop(&mut self) {
        unsafe { crate::sys::unmap(self.ptr as *mut u8, Self::bytes_for(self.cap)) };
    }
}

/// File des suites sales : tableau linéaire, `head` avance au rendu, la compaction
/// ramène les entrées vivantes au début.
struct Fifo {
    a: MapArr<(u32, Instant)>,
    head: usize,
}

impl Fifo {
    #[inline]
    fn front(&self) -> Option<&(u32, Instant)> {
        (self.head < self.a.len).then(|| &self.a[self.head])
    }

    #[inline]
    fn pop_front(&mut self) {
        self.head += 1;
        if self.head == self.a.len {
            self.head = 0;
            self.a.len = 0;
        }
    }

    #[inline]
    fn push_back(&mut self, e: (u32, Instant)) {
        self.a.push(e);
    }

    /// Fin de la partie écrite (déclenche la compaction).
    #[inline]
    fn tail(&self) -> usize {
        self.a.len
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.a.len - self.head
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Granules alloués (objets du tas et slabs), en octets.
    pub held_bytes: u64,
    /// Suites libres sales (pages tenues, au plus : une fusion avec une suite propre la
    /// compte sale tout entière), en octets.
    pub dirty_bytes: u64,
    /// Rendus au système depuis le démarrage.
    pub released_bytes: u64,
    /// Sommet de la plage utilisée.
    pub top_bytes: u64,
    /// Slabs : granules tenus, et octets des objets vivants (taille de classe).
    pub slab_held_bytes: u64,
    pub slab_live_bytes: u64,
    /// Tables par granule (étiquette, listes, propriétaire : 4 × 4 o par granule jusqu'au
    /// sommet), sur des mappings à elles : pages résidentes (2 Mio chacune sous THP).
    pub meta_bytes: u64,
}

/// Tas de granules + slabs, sur une plage.
pub struct Large {
    base: usize,
    nb: u32,
    top: u32,
    tag: *mut u32,
    next: *mut u32,
    prev: *mut u32,
    /// Granule → slab qui le contient (granules de slab), `NONE` au début d'une suite du
    /// tas : la route d'un bloc se retrouve depuis son adresse ([`Large::route_of_ptr`]).
    owner: *mut u32,
    /// Granule → dernière position dans la file (compaction de la file seulement).
    last: *mut u32,
    maps: [(*mut u8, usize); 6],
    /// Registre des pages des tables par granule (`tag`, `next`, `prev`, `owner`, `last`),
    /// jusqu'au sommet. Les données sont comptées à part, au granule près : `held + dirty`
    /// (tout ce qui n'est pas propre sous `top`, MEMORY_ARCH.md §10.4).
    meta_pages: [crate::pages::Tranches; 5],
    /// 0 : suites propres (rendues), 1 : sales.
    idx: [Index; 2],
    /// Suites sales à rendre : (début, libérée à).
    fifo: Fifo,
    /// Longueur de la file qui déclenche son nettoyage ([`Large::compact_fifo`]).
    fifo_compact_at: usize,
    decay: Duration,
    held: u64,
    dirty: u64,
    released: u64,
    slabs: Slabs,
}

impl Large {
    /// Plage de `virt` octets (arrondie au granule, au plus 2^29 granules).
    pub fn new(virt: usize, decay: Duration) -> Option<Self> {
        let nb = (virt / GRANULE).clamp(SLAB_GRANULES as usize, LEN as usize) as u32;
        let side = nb as usize * 4;
        let lens = [nb as usize * GRANULE, side, side, side, side, side];
        let mut maps = [(null_mut(), 0); 6];
        let unmap = |maps: &[(*mut u8, usize)]| {
            for &(p, len) in maps {
                unsafe { crate::sys::unmap(p, len) };
            }
        };
        for (i, &len) in lens.iter().enumerate() {
            let Some(p) = map(len) else {
                unmap(&maps[..i]);
                return None;
            };
            maps[i] = (p, len);
        }
        // Bornes : un slab par 16 granules ; la file est compactée à 2 × le nombre de suites
        // libres (≤ nb / 2 + 1), et au moins à FIFO_MIN.
        let slabs = nb as usize / SLAB_GRANULES as usize + 1;
        let (Some(fifo), Some(slabs)) = (MapArr::new(nb as usize + FIFO_MIN + 2), Slabs::new(slabs)) else {
            unmap(&maps);
            return None;
        };
        // Données et tables en pages de 4 Kio : sous THP, chaque table par granule prenait
        // 2 à 4 Mio pour quelques Ko écrits (mesuré : 21 Mo pour les 5). Elles ne servent que
        // sur les chemins lents du tas.
        for &(p, len) in &maps {
            unsafe { crate::sys::advise(p, len, crate::sys::Advice::LinuxNoHugepage) };
        }
        Some(Self {
            base: maps[0].0 as usize,
            nb,
            top: 0,
            tag: maps[1].0 as *mut u32,
            next: maps[2].0 as *mut u32,
            prev: maps[3].0 as *mut u32,
            owner: maps[4].0 as *mut u32,
            last: maps[5].0 as *mut u32,
            meta_pages: std::array::from_fn(|i| crate::pages::Tranches::new(maps[i + 1].0 as usize, crate::pages::PAGE)),
            maps,
            idx: [Index::EMPTY, Index::EMPTY],
            fifo: Fifo { a: fifo, head: 0 },
            fifo_compact_at: FIFO_MIN,
            decay,
            held: 0,
            dirty: 0,
            released: 0,
            slabs,
        })
    }

    #[inline]
    pub fn owns(&self, a: usize) -> bool {
        a.wrapping_sub(self.base) < self.nb as usize * GRANULE
    }

    /// Octets résidents, données jusqu'au sommet et tables (`mincore`, pour `INFO`).
    pub fn resident(&self) -> u64 {
        let (p, len) = self.maps[0];
        crate::resident::region(p as usize, self.top as usize * GRANULE, len) + self.stats().meta_bytes
    }

    pub fn stats(&self) -> Stats {
        Stats {
            held_bytes: self.held * GRANULE as u64,
            dirty_bytes: self.dirty * GRANULE as u64,
            released_bytes: self.released * GRANULE as u64,
            top_bytes: self.top as u64 * GRANULE as u64,
            slab_held_bytes: self.slabs.held_slabs * SLAB as u64,
            slab_live_bytes: self.slabs.live_bytes,
            meta_bytes: self.maps[1..].iter().map(|&(p, len)| crate::resident::region(p as usize, self.top as usize * 4, len)).sum::<u64>()
                + self.fifo.a.resident()
                + self.slabs.slabs.resident()
                + self.slabs.free_ids.resident(),
        }
    }

    /// Part de ce tas dans le registre des pages : données (`held + dirty`), tables par
    /// granule, file des suites sales et tables des slabs.
    pub fn charged_bytes(&self) -> u64 {
        (self.held + self.dirty) * GRANULE as u64
            + self.meta_pages.iter().map(|t| t.bytes() as u64).sum::<u64>()
            + (self.fifo.a.pages.bytes() + self.slabs.slabs.pages.bytes() + self.slabs.free_ids.pages.bytes()) as u64
    }

    // ─── Étiquettes et listes ────────────────────────────────────────────────

    #[inline]
    fn tag(&self, g: u32) -> u32 {
        unsafe { *self.tag.add(g as usize) }
    }

    #[inline]
    fn set_tag(&mut self, g: u32, t: u32) {
        unsafe { *self.tag.add(g as usize) = t }
    }

    /// Étiquettes de bord de la suite `[s, s + n)`, état `state` (`FREE`, `DIRTY`).
    #[inline]
    fn set_run(&mut self, s: u32, n: u32, state: u32) {
        self.set_tag(s + n - 1, state | n);
        self.set_tag(s, state | START | n);
    }

    fn link(&mut self, s: u32, n: u32, dirty: bool) {
        let (fl, sl) = mapping(n);
        let ix = &mut self.idx[dirty as usize];
        let head = ix.heads[fl][sl];
        unsafe {
            *self.next.add(s as usize) = head;
            *self.prev.add(s as usize) = NONE;
            if head != NONE {
                *self.prev.add(head as usize) = s;
            }
        }
        ix.heads[fl][sl] = s;
        ix.sl_mask[fl] |= 1 << sl;
        ix.fl_mask |= 1 << fl;
        self.set_run(s, n, FREE | if dirty { DIRTY } else { 0 });
    }

    fn unlink(&mut self, s: u32, n: u32, dirty: bool) {
        let (fl, sl) = mapping(n);
        let (nx, pv) = unsafe { (*self.next.add(s as usize), *self.prev.add(s as usize)) };
        unsafe {
            if nx != NONE {
                *self.prev.add(nx as usize) = pv;
            }
            if pv != NONE {
                *self.next.add(pv as usize) = nx;
            }
        }
        let ix = &mut self.idx[dirty as usize];
        if pv == NONE {
            ix.heads[fl][sl] = nx;
            if nx == NONE {
                ix.sl_mask[fl] &= !(1 << sl);
                if ix.sl_mask[fl] == 0 {
                    ix.fl_mask &= !(1 << fl);
                }
            }
        }
    }

    // ─── Tas de granules ─────────────────────────────────────────────────────

    /// Suite de `n` granules (sale d'abord : déjà tenue, aucun défaut de page).
    fn alloc_granules(&mut self, n: u32) -> Option<u32> {
        self.alloc_granules_z(n).map(|(g, _)| g)
    }

    /// Comme [`Large::alloc_granules`], et si la suite est déjà à zéro : propre (rendue par
    /// `MADV_DONTNEED`) ou neuve (au-dessus de `top`, jamais écrite).
    fn alloc_granules_z(&mut self, n: u32) -> Option<(u32, bool)> {
        debug_assert!(n > 0);
        for dirty in [true, false] {
            let Some(s) = self.idx[dirty as usize].find(n) else { continue };
            let len = self.tag(s) & LEN;
            debug_assert!(len >= n);
            self.unlink(s, len, dirty);
            if dirty {
                self.dirty -= n as u64;
            }
            // pris à la fin : le reste garde son début, donc son entrée de la file de rendu
            let a = s + len - n;
            if len > n {
                self.link(s, len - n, dirty);
            }
            self.set_run(a, n, 0);
            self.held += n as u64;
            if !dirty {
                crate::pages::charge(n as usize * GRANULE);
            }
            return Some((a, !dirty));
        }
        if n > self.nb - self.top {
            return None;
        }
        let a = self.top;
        self.top += n;
        for t in &self.meta_pages {
            t.ensure(self.top as usize * 4);
        }
        self.set_run(a, n, 0);
        self.held += n as u64;
        crate::pages::charge(n as usize * GRANULE);
        Some((a, true))
    }

    /// Rend la suite `[s, s + n)` : sale, fusionnée avec ses voisines libres.
    fn free_granules(&mut self, s: u32, n: u32, now: Instant) {
        debug_assert_eq!(self.tag(s), START | n, "suite libérée inconnue");
        self.held -= n as u64;
        self.dirty += n as u64;
        let (mut start, mut len) = (s, n);
        if s > 0 {
            let t = self.tag(s - 1);
            if t & FREE != 0 {
                let ln = t & LEN;
                let d = t & DIRTY != 0;
                self.unlink(s - ln, ln, d);
                if !d {
                    // propre fusionnée dans une sale : comptée sale, donc au registre
                    self.dirty += ln as u64;
                    crate::pages::charge(ln as usize * GRANULE);
                }
                self.set_tag(s - 1, 0);
                start = s - ln;
                len += ln;
            }
        }
        let e = s + n;
        if e < self.top {
            let t = self.tag(e);
            if t & FREE != 0 {
                let rn = t & LEN;
                let d = t & DIRTY != 0;
                self.unlink(e, rn, d);
                if !d {
                    self.dirty += rn as u64;
                    crate::pages::charge(rn as usize * GRANULE);
                }
                self.set_tag(e, 0);
                len += rn;
            }
        }
        if start != s {
            self.set_tag(s, 0);
        }
        if s + n - 1 != start + len - 1 && s + n - 1 != start {
            self.set_tag(s + n - 1, 0);
        }
        self.link(start, len, true);
        self.fifo.push_back((start, now));
        if self.fifo.tail() >= self.fifo_compact_at {
            self.compact_fifo();
        }
    }

    /// Une entrée de la file par libération, réutilisation ou fusion comprise : sous un
    /// trafic soutenu, elle suit le rythme des libérations × `decay`, pas la mémoire
    /// (mesuré : 390 K entrées, 6 Mo dans la libc, pour un chargement de 140 Mo). Quand
    /// elle double, on n'y garde que les suites encore libres et sales, une entrée par
    /// suite (la dernière : `decay` court depuis la dernière libération), dans l'ordre :
    /// bornée par le nombre de suites libres. Les pages en trop sont rendues. Sur place,
    /// en deux passes : la première note dans `last` la dernière position de chaque suite.
    fn compact_fifo(&mut self) {
        let (tag, last) = (self.tag, self.last);
        // SAFETY: les entrées désignent des granules sous `top` (tables de la plage).
        let valid = |s: u32| unsafe { *tag.add(s as usize) } & (FREE | DIRTY | START) == FREE | DIRTY | START;
        let (h, t) = (self.fifo.head, self.fifo.tail());
        let a = &mut self.fifo.a;
        for i in h..t {
            let s = a[i].0;
            if valid(s) {
                unsafe { *last.add(s as usize) = i as u32 };
            }
        }
        let mut w = 0;
        for i in h..t {
            let e = a[i];
            if valid(e.0) && unsafe { *last.add(e.0 as usize) } == i as u32 {
                a[w] = e;
                w += 1;
            }
        }
        self.fifo_compact_at = (w * 2).max(FIFO_MIN);
        self.fifo.head = 0;
        self.fifo.a.truncate(w, self.fifo_compact_at);
    }

    /// Rend au système les suites sales libérées depuis `decay`. Vrai s'il en reste à
    /// rendre tout de suite (budget épuisé).
    pub fn release_step(&mut self, now: Instant) -> bool {
        let mut budget = RELEASE_BUDGET;
        while let Some(&(s, since)) = self.fifo.front() {
            if now.saturating_duration_since(since) < self.decay {
                return false;
            }
            if budget == 0 {
                return true;
            }
            self.fifo.pop_front();
            let t = self.tag(s);
            // périmée : réallouée, fusionnée, ou déjà rendue
            if t & (FREE | DIRTY | START) != FREE | DIRTY | START {
                continue;
            }
            let len = t & LEN;
            self.unlink(s, len, true);
            unsafe {
                crate::sys::advise((self.base + s as usize * GRANULE) as *mut u8, len as usize * GRANULE, crate::sys::Advice::LinuxDontNeed)
            };
            self.dirty -= len as u64;
            self.released += len as u64;
            crate::pages::credit(len as usize * GRANULE);
            self.link(s, len, false);
            budget = budget.saturating_sub(len as u64 * GRANULE as u64);
        }
        false
    }

    // ─── Objets ──────────────────────────────────────────────────────────────

    #[inline]
    fn granule_of(&self, p: *mut u8) -> u32 {
        ((p as usize - self.base) >> GRANULE_SHIFT) as u32
    }

    #[inline]
    fn addr(&self, g: u32) -> *mut u8 {
        (self.base + g as usize * GRANULE) as *mut u8
    }

    /// Objet de `size` octets, aligné sur 8 ; nul si la plage est pleine.
    pub fn alloc(&mut self, size: usize) -> *mut u8 {
        match Route::of(size, 8) {
            Some(r) => self.alloc_route(r),
            None => null_mut(),
        }
    }

    /// # Safety
    /// `ptr` vient de [`Large::alloc`] de ce tas avec la même `size`, pas encore rendu.
    pub unsafe fn free(&mut self, ptr: *mut u8, size: usize) {
        unsafe { self.free_route(ptr, Route::of(size, 8).expect("taille servie par le tas")) }
    }

    fn alloc_route(&mut self, r: Route) -> *mut u8 {
        self.alloc_route_z(r).0
    }

    /// Bloc et « déjà à zéro » (suite propre ou neuve ; jamais un emplacement de slab).
    fn alloc_route_z(&mut self, r: Route) -> (*mut u8, bool) {
        match r {
            Route::Heap(n) => self.alloc_granules_z(n).map_or((null_mut(), false), |(g, zero)| {
                unsafe { *self.owner.add(g as usize) = NONE };
                (self.addr(g), zero)
            }),
            Route::Slab(c) => (self.slab_alloc(c), false),
        }
    }

    /// Route d'un bloc servi, depuis son adresse : granule d'un slab (`owner`), sinon début
    /// d'une suite du tas (`owner == NONE`, posé à l'allocation).
    fn route_of_ptr(&self, p: *mut u8) -> Route {
        let g = self.granule_of(p);
        match unsafe { *self.owner.add(g as usize) } {
            NONE => Route::Heap(self.tag(g) & LEN),
            id => Route::Slab(self.slabs.slab(id).class as usize),
        }
    }

    /// Octets utilisables d'un bloc servi (taille de classe, ou suite entière).
    fn usable_of(&self, p: *mut u8) -> usize {
        match self.route_of_ptr(p) {
            Route::Heap(n) => n as usize * GRANULE,
            Route::Slab(c) => CLASSES[c] as usize,
        }
    }

    /// # Safety
    /// `ptr` vient de [`Large::alloc_route`] de ce tas avec la même route, pas encore rendu.
    unsafe fn free_route(&mut self, ptr: *mut u8, r: Route) {
        match r {
            Route::Heap(n) => self.free_granules(self.granule_of(ptr), n, Instant::now()),
            Route::Slab(c) => unsafe { self.slab_free(ptr, c) },
        }
    }

    fn slab_alloc(&mut self, c: usize) -> *mut u8 {
        let id = match self.slabs.partial[c] {
            NONE => {
                let Some(g) = self.alloc_granules(SLAB_GRANULES) else { return null_mut() };
                let id = self.slabs.create(g, c as u8);
                for i in 0..SLAB_GRANULES {
                    unsafe { *self.owner.add((g + i) as usize) = id };
                }
                id
            }
            id => id,
        };
        let base = self.addr(self.slabs.slab(id).start);
        self.slabs.take(id, base)
    }

    unsafe fn slab_free(&mut self, ptr: *mut u8, c: usize) {
        let id = unsafe { *self.owner.add(self.granule_of(ptr) as usize) };
        debug_assert_eq!(self.slabs.slab(id).class as usize, c);
        let base = self.addr(self.slabs.slab(id).start);
        if let Some(g) = unsafe { self.slabs.give(id, base, ptr) } {
            self.free_granules(g, SLAB_GRANULES, Instant::now());
        }
    }

    /// Vérifie les invariants (tests) : pavage de `[0, top)`, étiquettes, listes, comptes.
    #[cfg(test)]
    fn check(&self) {
        let (mut g, mut held, mut dirty, mut free_runs) = (0u32, 0u64, 0u64, 0usize);
        let mut prev_free = false;
        while g < self.top {
            let t = self.tag(g);
            assert!(t & START != 0, "granule {g} : pas un début de suite");
            let n = t & LEN;
            assert!(n > 0 && g + n <= self.top);
            assert_eq!(self.tag(g + n - 1) & !START, t & !START, "bords différents en {g}");
            if t & FREE != 0 {
                assert!(!prev_free, "deux suites libres voisines en {g}");
                free_runs += 1;
                if t & DIRTY != 0 {
                    dirty += n as u64;
                }
            } else {
                held += n as u64;
            }
            prev_free = t & FREE != 0;
            g += n;
        }
        assert_eq!(held, self.held);
        assert_eq!(dirty, self.dirty);
        let mut listed = 0;
        for (d, ix) in self.idx.iter().enumerate() {
            for fl in 0..FL {
                for sl in 0..SL as usize {
                    let mut s = ix.heads[fl][sl];
                    assert_eq!(s != NONE, ix.sl_mask[fl] & (1 << sl) != 0);
                    while s != NONE {
                        let t = self.tag(s);
                        assert_eq!(t & (FREE | START), FREE | START);
                        assert_eq!(t & DIRTY != 0, d == 1);
                        assert_eq!(mapping(t & LEN), (fl, sl));
                        listed += 1;
                        s = unsafe { *self.next.add(s as usize) };
                    }
                }
            }
        }
        assert_eq!(listed, free_runs);
    }
}

impl Drop for Large {
    fn drop(&mut self) {
        crate::pages::credit((self.held + self.dirty) as usize * GRANULE);
        for &(p, len) in &self.maps {
            unsafe { crate::sys::unmap(p, len) };
        }
    }
}

// ─── Slabs des petits objets épinglés (§18.6) ────────────────────────────────

/// Tailles de classe : multiples de 8 jusqu'à 128, de 16 jusqu'à 256, puis ×1,25 environ
/// (multiples de 16) jusqu'à [`HEAP_MIN`]. Un membre de ZSet de 15 o + 6 o d'en-tête : 24.
const CLASSES: [u32; 43] = {
    let mut t = [0u32; 43];
    let mut i = 0;
    while i < 16 {
        t[i] = 8 * (i as u32 + 1);
        i += 1;
    }
    while i < 24 {
        t[i] = 128 + 16 * (i as u32 - 15);
        i += 1;
    }
    let mut s = 256u32;
    while i < 43 {
        s = (s + s / 4).next_multiple_of(16);
        if s > HEAP_MIN as u32 {
            s = HEAP_MIN as u32;
        }
        t[i] = s;
        i += 1;
    }
    t
};

/// Où vit un bloc : suite de `n` granules (alignée sur 4 Kio), ou emplacement de la classe
/// `c`. Une classe sert un alignement qui la divise : ses emplacements sont à des multiples
/// de sa taille depuis un début de slab, lui-même aligné sur un granule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Heap(u32),
    Slab(usize),
}

impl Route {
    /// `None` : alignement au-delà d'un granule, ou plus grand que la plage.
    #[inline]
    fn of(size: usize, align: usize) -> Option<Route> {
        if align <= 8 && size < HEAP_MIN {
            return Some(Route::Slab(class_of(size.max(1))));
        }
        if align > GRANULE {
            return None;
        }
        let s = size.max(1).next_multiple_of(align);
        if s < HEAP_MIN {
            let c = class_of(s);
            if CLASSES[c] as usize % align == 0 {
                return Some(Route::Slab(c));
            }
        }
        let n = size.div_ceil(GRANULE);
        (n <= LEN as usize).then_some(Route::Heap(n as u32))
    }
}

#[inline]
fn class_of(size: usize) -> usize {
    debug_assert!(size > 0 && size < HEAP_MIN);
    if size <= 128 {
        return (size - 1) / 8;
    }
    if size <= 256 {
        return 16 + (size - 129) / 16;
    }
    CLASSES.partition_point(|&c| (c as usize) < size)
}

#[derive(Clone, Copy)]
struct Slab {
    start: u32,
    class: u8,
    used: u32,
    /// Premier emplacement libre (décalage), chaîné dans les emplacements ; `NONE` : aucun.
    free: u32,
    /// Début de la partie jamais servie.
    bump: u32,
    next: u32,
    prev: u32,
    in_partial: bool,
}

struct Slabs {
    slabs: MapArr<Slab>,
    free_ids: MapArr<u32>,
    /// Slabs d'une classe avec de la place.
    partial: [u32; CLASSES.len()],
    held_slabs: u64,
    live_bytes: u64,
}

impl Slabs {
    fn new(cap: usize) -> Option<Self> {
        Some(Self {
            slabs: MapArr::new(cap)?,
            free_ids: MapArr::new(cap)?,
            partial: [NONE; CLASSES.len()],
            held_slabs: 0,
            live_bytes: 0,
        })
    }

    #[inline]
    fn slab(&self, id: u32) -> &Slab {
        &self.slabs[id as usize]
    }

    fn create(&mut self, start: u32, class: u8) -> u32 {
        let s = Slab { start, class, used: 0, free: NONE, bump: 0, next: NONE, prev: NONE, in_partial: false };
        let id = match self.free_ids.pop() {
            Some(id) => {
                self.slabs[id as usize] = s;
                id
            }
            None => {
                self.slabs.push(s);
                (self.slabs.len - 1) as u32
            }
        };
        self.held_slabs += 1;
        self.partial_insert(id);
        id
    }

    fn partial_insert(&mut self, id: u32) {
        let c = self.slabs[id as usize].class as usize;
        let head = self.partial[c];
        if head != NONE {
            self.slabs[head as usize].prev = id;
        }
        let s = &mut self.slabs[id as usize];
        (s.next, s.prev, s.in_partial) = (head, NONE, true);
        self.partial[c] = id;
    }

    fn partial_remove(&mut self, id: u32) {
        let Slab { next, prev, class, .. } = self.slabs[id as usize];
        if next != NONE {
            self.slabs[next as usize].prev = prev;
        }
        if prev != NONE {
            self.slabs[prev as usize].next = next;
        } else {
            self.partial[class as usize] = next;
        }
        self.slabs[id as usize].in_partial = false;
    }

    /// Un emplacement du slab `id` (de la liste `partial`), dont les données sont à `base`.
    fn take(&mut self, id: u32, base: *mut u8) -> *mut u8 {
        let s = &mut self.slabs[id as usize];
        let size = CLASSES[s.class as usize];
        let off = if s.free != NONE {
            let off = s.free;
            s.free = unsafe { (base.add(off as usize) as *const u32).read() };
            off
        } else {
            let off = s.bump;
            s.bump += size;
            off
        };
        s.used += 1;
        self.live_bytes += size as u64;
        if s.free == NONE && s.bump as usize + size as usize > SLAB {
            self.partial_remove(id);
        }
        unsafe { base.add(off as usize) }
    }

    /// Rend `ptr` au slab `id` ; si le slab se vide, rend son premier granule (à libérer).
    ///
    /// # Safety
    /// `ptr` vient de [`Slabs::take`] sur ce slab, pas encore rendu.
    unsafe fn give(&mut self, id: u32, base: *mut u8, ptr: *mut u8) -> Option<u32> {
        let s = &mut self.slabs[id as usize];
        let size = CLASSES[s.class as usize];
        let off = (ptr as usize - base as usize) as u32;
        unsafe { (ptr as *mut u32).write(s.free) };
        s.free = off;
        s.used -= 1;
        self.live_bytes -= size as u64;
        if s.used == 0 {
            let start = s.start;
            if s.in_partial {
                self.partial_remove(id);
            }
            self.free_ids.push(id);
            self.held_slabs -= 1;
            return Some(start);
        }
        if !s.in_partial {
            self.partial_insert(id);
        }
        None
    }
}

// ─── Instance globale ────────────────────────────────────────────────────────

static mut LARGE: Option<Large> = None;
static mut DISABLED: bool = false;
/// Dans le tas : une allocation qui y reviendrait part en mapping direct ([`with`] rend
/// `None`). Ne devrait plus arriver : les tables du tas sont des [`MapArr`], et
/// l'initialisation lit l'environnement sans allouer.
static mut BUSY: bool = false;
/// La plage, lisible de tout thread ([`owns`]) : fixée une fois à l'initialisation.
static RANGE_BASE: AtomicUsize = AtomicUsize::new(0);
static RANGE_LEN: AtomicUsize = AtomicUsize::new(0);

/// `f` sur le tas global, créé au premier appel. `None` : déjà dedans, ou plage refusée.
#[allow(static_mut_refs)]
#[inline]
fn with<R>(f: impl FnOnce(&mut Large) -> R) -> Option<R> {
    unsafe {
        if BUSY {
            return None;
        }
        BUSY = true;
        let l = match LARGE.as_mut() {
            Some(l) => Some(l),
            None if DISABLED => None,
            None => init(),
        };
        let r = l.map(f);
        BUSY = false;
        r
    }
}

/// Variable d'environnement entière, lue sans allouer (`getenv` : le tas n'existe pas encore).
fn env_u64(name: &std::ffi::CStr, default: u64) -> u64 {
    let v = unsafe { libc::getenv(name.as_ptr()) };
    if v.is_null() {
        return default;
    }
    let v = unsafe { std::ffi::CStr::from_ptr(v) }.to_bytes();
    std::str::from_utf8(v).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Plage refusée (`vm.overcommit_memory = 2`, `ulimit -v`) : on retente en la divisant par
/// deux jusqu'à 1 Gio ; au-delà, tout part en mapping direct (lent, mais compté).
#[cold]
#[allow(static_mut_refs)]
unsafe fn init() -> Option<&'static mut Large> {
    let mut gb = env_u64(c"RADIXOX_LARGE_VIRT_GB", 64).max(1) as usize;
    let decay = Duration::from_secs(env_u64(c"RADIXOX_MEMORY_DECAY", 10));
    unsafe {
        loop {
            if let Some(l) = Large::new(gb << 30, decay) {
                RANGE_BASE.store(l.base, Relaxed);
                RANGE_LEN.store(l.nb as usize * GRANULE, Relaxed);
                LARGE = Some(l);
                return LARGE.as_mut();
            }
            if gb == 1 {
                crate::raw_alloc::direct::message(b"radixox: plage des grands objets refusee, mappings directs\n");
                DISABLED = true;
                return None;
            }
            gb /= 2;
        }
    }
}

/// `a` est dans la plage du tas global (de tout thread).
#[inline]
pub fn owns(a: usize) -> bool {
    a.wrapping_sub(RANGE_BASE.load(Relaxed)) < RANGE_LEN.load(Relaxed)
}

/// Objet de `size` octets (aligné sur 8) ; mapping direct si la plage est pleine ou absente.
///
/// # Safety
/// Mono-thread ; libérer par [`free`] avec la même taille.
pub unsafe fn malloc(size: usize) -> *mut u8 {
    match with(|l| l.alloc(size)) {
        Some(p) if !p.is_null() => p,
        _ => crate::raw_alloc::direct::alloc(size, 8),
    }
}

/// # Safety
/// `ptr` vient de [`malloc`] avec cette `size`, pas encore rendu.
pub unsafe fn free(ptr: *mut u8, size: usize) {
    if owns(ptr as usize) {
        with(|l| unsafe { l.free(ptr, size) }).expect("libération d'un objet du tas depuis le tas");
    } else {
        unsafe { crate::raw_alloc::direct::free(ptr) }
    }
}

/// Allocateur global ([`crate::raw_alloc::heap::RxAlloc`]) : bloc de `size` octets aligné
/// sur `align`. Nul (l'appelant prend la libc) : alignement au-delà de 4 Kio, plage pleine
/// ou absente, ou appel revenu du tas lui-même.
#[inline]
pub fn global_alloc(size: usize, align: usize) -> *mut u8 {
    let Some(r) = Route::of(size, align) else { return null_mut() };
    with(|l| l.alloc_route(r)).unwrap_or(null_mut())
}

/// Comme [`global_alloc`], mis à zéro. Une suite propre ou neuve l'est déjà : pas
/// d'écriture, ses pages restent non résidentes (un gros `calloc` ne coûte que ce qu'on
/// en écrit).
pub fn global_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
    let Some(r) = Route::of(size, align) else { return null_mut() };
    let (p, zero) = with(|l| l.alloc_route_z(r)).unwrap_or((null_mut(), false));
    if !p.is_null() && !zero {
        unsafe { std::ptr::write_bytes(p, 0, size) };
    }
    p
}

/// # Safety
/// `ptr` vient de [`global_alloc`] avec ces `size` et `align` ([`owns`] vrai), pas encore
/// rendu ; thread du tas.
#[inline]
pub unsafe fn global_free(ptr: *mut u8, size: usize, align: usize) {
    let r = Route::of(size, align).expect("route d'un bloc servi");
    with(|l| unsafe { l.free_route(ptr, r) }).expect("libération d'un bloc du tas depuis le tas");
}

/// Octets utilisables d'un bloc du tas ([`owns`] vrai), retrouvés depuis son adresse.
pub fn usable_size(ptr: *mut u8) -> usize {
    with(|l| l.usable_of(ptr)).expect("taille d'un bloc du tas depuis le tas")
}

/// Rend un bloc du tas sans connaître sa taille (`free` du C).
///
/// # Safety
/// `ptr` vient de [`global_alloc`] ([`owns`] vrai), pas encore rendu ; thread du tas.
pub unsafe fn free_ptr(ptr: *mut u8) {
    with(|l| unsafe { l.free_route(ptr, l.route_of_ptr(ptr)) }).expect("libération d'un bloc du tas depuis le tas");
}

/// Le bloc du tas `ptr` sert aussi `new` (alignement `align`) sans bouger.
pub fn ptr_fits(ptr: *mut u8, new: usize, align: usize) -> bool {
    Route::of(new, align).is_some_and(|r| with(|l| l.route_of_ptr(ptr) == r).unwrap_or(false))
}

/// Un bloc servi pour `(old, align)` sert aussi `new` sans bouger.
#[inline]
pub fn global_fits(old: usize, new: usize, align: usize) -> bool {
    Route::of(old, align).is_some_and(|r| Route::of(new, align) == Some(r))
}

/// Appelé par la boucle avec le rendu du pool. Vrai s'il en reste à rendre tout de suite.
#[allow(static_mut_refs)]
pub fn release_step(now: Instant) -> bool {
    // jamais créé : rien à rendre (et ne pas le créer ici)
    if unsafe { LARGE.is_none() } {
        return false;
    }
    with(|l| l.release_step(now)).unwrap_or(false)
}

pub fn stats() -> Option<Stats> {
    #[allow(static_mut_refs)]
    unsafe { LARGE.as_ref() }.map(Large::stats)
}

/// Octets comptés par le registre mais réutilisables sans nouvelle page : suites libres
/// sales (tenues) et emplacements libres des slabs, en O(1). MEMORY_ARCH.md §10.5.
pub fn reusable_bytes() -> u64 {
    #[allow(static_mut_refs)]
    unsafe { LARGE.as_ref() }.map_or(0, |l| {
        l.dirty * GRANULE as u64 + (l.slabs.held_slabs * SLAB as u64).saturating_sub(l.slabs.live_bytes)
    })
}

/// Voir [`Large::resident`] ; 0 sans tas.
pub fn resident() -> u64 {
    #[allow(static_mut_refs)]
    unsafe { LARGE.as_ref() }.map_or(0, Large::resident)
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

    #[test]
    fn mapping_covers_sizes() {
        for n in 1..100_000u32 {
            let (fl, sl) = mapping(n);
            let (sfl, ssl) = mapping_search(n);
            assert!((sfl, ssl) >= (fl, sl));
            // toute suite de la liste de recherche a au moins n granules
            let lo = if sfl == 0 { ssl as u32 } else { (SL + ssl as u32) << (sfl - 1) };
            assert!(lo >= n, "n={n} : liste ({sfl},{ssl}) commence à {lo}");
        }
    }

    #[test]
    fn classes_increase_to_heap_min() {
        assert_eq!(CLASSES[23], 256);
        for w in CLASSES.windows(2) {
            assert!(w[0] < w[1] || w[1] == HEAP_MIN as u32);
        }
        assert_eq!(*CLASSES.last().unwrap(), HEAP_MIN as u32);
        for size in 1..HEAP_MIN {
            let c = class_of(size);
            assert!(CLASSES[c] as usize >= size);
            assert!(c == 0 || CLASSES[c - 1] as usize <= size || CLASSES[c - 1] == CLASSES[c]);
        }
    }

    /// Allocations et libérations aléatoires contre un modèle ; contenu vérifié ;
    /// invariants du tas après chaque étape.
    #[test]
    fn random_against_model() {
        let c0 = crate::pages::charged();
        let mut l = Large::new(32 << 30, Duration::ZERO).unwrap();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut live: Vec<(*mut u8, usize, u8)> = Vec::new();
        for step in 0..20_000 {
            if live.is_empty() || rng.below(100) < 55 {
                let size = match rng.below(4) {
                    0 => 1 + rng.below(256) as usize,
                    1 => 257 + rng.below(HEAP_MIN as u64 - 257) as usize,
                    2 => HEAP_MIN + rng.below(256 << 10) as usize,
                    _ => (1 + rng.below(8) as usize) << 20,
                };
                let p = l.alloc(size);
                assert!(!p.is_null());
                assert_eq!(p as usize % 8, 0);
                let b = (step % 251) as u8;
                unsafe {
                    p.write(b);
                    p.add(size - 1).write(b);
                }
                live.push((p, size, b));
            } else {
                let i = rng.below(live.len() as u64) as usize;
                let (p, size, b) = live.swap_remove(i);
                unsafe {
                    assert_eq!(p.read(), b);
                    assert_eq!(p.add(size - 1).read(), b);
                    l.free(p, size);
                }
            }
            if step % 64 == 0 {
                l.check();
                assert_eq!(crate::pages::charged() - c0, l.charged_bytes(), "registre, pas {step}");
            }
            if step % 997 == 0 {
                l.release_step(Instant::now() + Duration::from_secs(1));
                l.check();
            }
        }
        for (p, size, b) in live.drain(..) {
            unsafe {
                assert_eq!(p.read(), b);
                l.free(p, size);
            }
        }
        l.check();
        assert_eq!(l.held, 0);
        assert_eq!(l.slabs.live_bytes, 0);
        while l.release_step(Instant::now() + Duration::from_secs(1)) {}
        l.check();
        assert_eq!(l.dirty, 0);
        assert_eq!(crate::pages::charged() - c0, l.charged_bytes());
        // Les données rendues ne comptent plus : il ne reste que les tables (4 o par granule
        // jusqu'au sommet, chacune à une page près) et les petites tables des slabs.
        let tables = 5 * (l.top as u64 * 4 + 2 * crate::pages::PAGE as u64);
        assert!(l.charged_bytes() <= tables + (1 << 20), "{} > {tables}", l.charged_bytes());
        drop(l);
        assert_eq!(crate::pages::charged(), c0, "tout rendu à la destruction");
    }

    /// Sans objet vivant, tout est rendu : une seule suite propre.
    #[test]
    fn everything_released_coalesces() {
        let mut l = Large::new(256 << 20, Duration::ZERO).unwrap();
        let ps: Vec<_> = (0..64).map(|i| (l.alloc(HEAP_MIN + i * 5000), HEAP_MIN + i * 5000)).collect();
        for &(p, s) in ps.iter().rev().step_by(2) {
            unsafe { l.free(p, s) };
        }
        for &(p, s) in ps.iter().rev().skip(1).step_by(2) {
            unsafe { l.free(p, s) };
        }
        while l.release_step(Instant::now() + Duration::from_secs(1)) {}
        l.check();
        let t = l.tag(0);
        assert_eq!(t & (FREE | DIRTY | START), FREE | START);
        assert_eq!(t & LEN, l.top);
    }

    /// 64 Kio + en-tête : 17 granules (68 Kio), pas une classe de 80 Kio.
    #[test]
    fn sixty_four_kib_rounding() {
        let mut l = Large::new(64 << 20, Duration::ZERO).unwrap();
        let p = l.alloc((64 << 10) + 6);
        assert!(!p.is_null());
        assert_eq!(l.stats().held_bytes, 68 << 10);
        unsafe { l.free(p, (64 << 10) + 6) };
    }

    /// Le decay retient le rendu ; passé, la mémoire part.
    #[test]
    fn decay_holds_then_releases() {
        let mut l = Large::new(64 << 20, Duration::from_secs(10)).unwrap();
        let p = l.alloc(1 << 20);
        unsafe { l.free(p, 1 << 20) };
        let now = Instant::now();
        assert!(!l.release_step(now));
        assert_eq!(l.stats().dirty_bytes, 1 << 20);
        l.release_step(now + Duration::from_secs(11));
        assert_eq!(l.stats().dirty_bytes, 0);
        assert_eq!(l.stats().released_bytes, 1 << 20);
        l.check();
    }

    /// Va-et-vient d'objets ≥ 16 Kio (tampons qui grandissent) : la file des suites sales
    /// reste bornée par les suites libres, pas par le nombre de libérations, et le rendu
    /// marche toujours après nettoyage.
    #[test]
    fn churn_keeps_fifo_bounded() {
        let mut l = Large::new(256 << 20, Duration::from_secs(10)).unwrap();
        let keep: Vec<_> = (0..8).map(|_| l.alloc(64 << 10)).collect();
        for i in 0..200_000usize {
            let size = (16 << 10) + (i % 7) * 8192;
            let p = l.alloc(size);
            unsafe { l.free(p, size) };
        }
        assert!(l.fifo.len() < 2 * FIFO_MIN, "file : {} entrées", l.fifo.len());
        assert!(l.fifo.a.touched <= 4 * FIFO_MIN, "touchées : {}", l.fifo.a.touched);
        l.check();
        for &p in &keep {
            unsafe { l.free(p, 64 << 10) };
        }
        l.release_step(Instant::now() + Duration::from_secs(11));
        assert_eq!(l.stats().dirty_bytes, 0, "tout le sale est rendu après nettoyage");
        assert_eq!(l.stats().held_bytes, 0);
        l.check();
    }
}
