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
//! Les tests construisent leurs propres [`Large`], plus petits.

use std::collections::VecDeque;
use std::ptr::null_mut;
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
    let p = unsafe {
        libc::mmap(
            null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    (p != libc::MAP_FAILED).then_some(p as *mut u8)
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
}

/// Tas de granules + slabs, sur une plage.
pub struct Large {
    base: usize,
    nb: u32,
    top: u32,
    tag: *mut u32,
    next: *mut u32,
    prev: *mut u32,
    /// Granule → slab qui le contient (granules de slab seulement).
    owner: *mut u32,
    maps: [(*mut u8, usize); 5],
    /// 0 : suites propres (rendues), 1 : sales.
    idx: [Index; 2],
    /// Suites sales à rendre : (début, libérée à).
    fifo: VecDeque<(u32, Instant)>,
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
        let lens = [nb as usize * GRANULE, side, side, side, side];
        let mut maps = [(null_mut(), 0); 5];
        for (i, &len) in lens.iter().enumerate() {
            let Some(p) = map(len) else {
                for &(p, len) in &maps[..i] {
                    unsafe { libc::munmap(p as *mut _, len) };
                }
                return None;
            };
            maps[i] = (p, len);
        }
        unsafe { libc::madvise(maps[0].0 as *mut _, lens[0], libc::MADV_NOHUGEPAGE) };
        Some(Self {
            base: maps[0].0 as usize,
            nb,
            top: 0,
            tag: maps[1].0 as *mut u32,
            next: maps[2].0 as *mut u32,
            prev: maps[3].0 as *mut u32,
            owner: maps[4].0 as *mut u32,
            maps,
            idx: [Index::EMPTY, Index::EMPTY],
            fifo: VecDeque::new(),
            decay,
            held: 0,
            dirty: 0,
            released: 0,
            slabs: Slabs::new(),
        })
    }

    #[inline]
    pub fn owns(&self, a: usize) -> bool {
        a.wrapping_sub(self.base) < self.nb as usize * GRANULE
    }

    pub fn stats(&self) -> Stats {
        Stats {
            held_bytes: self.held * GRANULE as u64,
            dirty_bytes: self.dirty * GRANULE as u64,
            released_bytes: self.released * GRANULE as u64,
            top_bytes: self.top as u64 * GRANULE as u64,
            slab_held_bytes: self.slabs.held_slabs * SLAB as u64,
            slab_live_bytes: self.slabs.live_bytes,
        }
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
            return Some(a);
        }
        if n > self.nb - self.top {
            return None;
        }
        let a = self.top;
        self.top += n;
        self.set_run(a, n, 0);
        self.held += n as u64;
        Some(a)
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
                    self.dirty += ln as u64;
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
                libc::madvise((self.base + s as usize * GRANULE) as *mut _, len as usize * GRANULE, libc::MADV_DONTNEED)
            };
            self.dirty -= len as u64;
            self.released += len as u64;
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
        if size >= HEAP_MIN {
            let n = size.div_ceil(GRANULE);
            if n > LEN as usize {
                return null_mut();
            }
            return self.alloc_granules(n as u32).map_or(null_mut(), |g| self.addr(g));
        }
        self.slab_alloc(size)
    }

    /// # Safety
    /// `ptr` vient de [`Large::alloc`] de ce tas avec la même `size`, pas encore rendu.
    pub unsafe fn free(&mut self, ptr: *mut u8, size: usize) {
        if size >= HEAP_MIN {
            let g = self.granule_of(ptr);
            self.free_granules(g, size.div_ceil(GRANULE) as u32, Instant::now());
        } else {
            unsafe { self.slab_free(ptr, size) }
        }
    }

    fn slab_alloc(&mut self, size: usize) -> *mut u8 {
        let c = class_of(size.max(1));
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

    unsafe fn slab_free(&mut self, ptr: *mut u8, size: usize) {
        let id = unsafe { *self.owner.add(self.granule_of(ptr) as usize) };
        debug_assert_eq!(self.slabs.slab(id).class as usize, class_of(size.max(1)));
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
        for &(p, len) in &self.maps {
            unsafe { libc::munmap(p as *mut _, len) };
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
    slabs: Vec<Slab>,
    free_ids: Vec<u32>,
    /// Slabs d'une classe avec de la place.
    partial: [u32; CLASSES.len()],
    held_slabs: u64,
    live_bytes: u64,
}

impl Slabs {
    fn new() -> Self {
        Self { slabs: Vec::new(), free_ids: Vec::new(), partial: [NONE; CLASSES.len()], held_slabs: 0, live_bytes: 0 }
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
                (self.slabs.len() - 1) as u32
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

#[allow(static_mut_refs)]
#[inline]
fn global() -> Option<&'static mut Large> {
    unsafe {
        if let Some(l) = LARGE.as_mut() {
            return Some(l);
        }
        if DISABLED {
            return None;
        }
        let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let gb = env("RADIXOX_LARGE_VIRT_GB", 64) as usize;
        let decay = Duration::from_secs(env("RADIXOX_MEMORY_DECAY", 10));
        match Large::new(gb << 30, decay) {
            Some(l) => {
                LARGE = Some(l);
                LARGE.as_mut()
            }
            None => {
                eprintln!("radixox: mmap de la plage des grands objets ({gb} Gio) refusé, repli sur malloc");
                DISABLED = true;
                None
            }
        }
    }
}

/// Objet de `size` octets (aligné sur 8) ; la libc si la plage est pleine ou absente.
///
/// # Safety
/// Mono-thread ; libérer par [`free`] avec la même taille.
pub unsafe fn malloc(size: usize) -> *mut u8 {
    if let Some(l) = global() {
        let p = l.alloc(size);
        if !p.is_null() {
            return p;
        }
    }
    unsafe { libc::malloc(size) as *mut u8 }
}

/// # Safety
/// `ptr` vient de [`malloc`] avec cette `size`, pas encore rendu.
pub unsafe fn free(ptr: *mut u8, size: usize) {
    #[allow(static_mut_refs)]
    match unsafe { LARGE.as_mut() } {
        Some(l) if l.owns(ptr as usize) => unsafe { l.free(ptr, size) },
        _ => unsafe { libc::free(ptr as *mut _) },
    }
}

/// Appelé par la boucle avec le rendu du pool. Vrai s'il en reste à rendre tout de suite.
pub fn release_step(now: Instant) -> bool {
    #[allow(static_mut_refs)]
    unsafe { LARGE.as_mut() }.is_some_and(|l| l.release_step(now))
}

pub fn stats() -> Option<Stats> {
    #[allow(static_mut_refs)]
    unsafe { LARGE.as_ref() }.map(Large::stats)
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
}
