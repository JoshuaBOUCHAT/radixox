//! Pool des `SharedByte` en forme courte (MEMORY_ARCH.md §16.3, §16.4).
//!
//! Plage virtuelle réservée une fois, pages de 2 Mio (huge pages), blocs de 64 Kio.
//! Allocation par bump, un compteur de granules vivants par bloc, sans en-tête de bloc :
//! libérer = décrémenter ; un bloc dont le compteur tombe à 0 se réutilise tel quel ; une
//! page dont les 32 blocs sont libres depuis `RADIXOX_MEMORY_DECAY` secondes est rendue au
//! système au-delà de la réserve. Pas de GC ici (étape 6).
//!
//! Mono-thread : état global en `static mut` (ARCHITECTURE.md §1, `RUST_TEST_THREADS=1`).
//! Les tests construisent leurs propres [`Pool`], plus petits.

use std::collections::VecDeque;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

pub const BLOCK_SHIFT: u32 = 16;
/// Bloc : unité du bump et du compteur.
pub const BLOCK: usize = 1 << BLOCK_SHIFT;
/// Page : une huge page, unité de rendu au système.
pub const PAGE: usize = 2 << 20;
const BLOCKS_PER_PAGE: u32 = (PAGE / BLOCK) as u32;
/// M : au-delà, un emplacement passe par la fenêtre Best-Fit.
pub const MEDIUM: usize = 2048;
const WINDOW: usize = 8;
const NONE: u32 = u32::MAX;
/// Pages rendues au plus par appel de [`release_step`] (un `MADV_DONTNEED` de 2 Mio
/// coûte quelques dizaines de µs).
const RELEASE_BUDGET: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageKind {
    /// Au moins un bloc occupé ou ouvert.
    Active,
    /// 32 blocs libres, mémoire encore tenue ; dans `empty_pages`.
    Empty,
    /// Rendue par `MADV_DONTNEED` ; réutilisable comme une page neuve.
    Released,
}

#[derive(Clone, Copy)]
struct PageState {
    free_mask: u32,
    state: PageKind,
    /// Présente dans `pages_with_free` (suppression paresseuse : pas de doublon).
    in_stack: bool,
}

/// Curseur bump : bloc courant, `top`, `end` (adresses).
struct Cursor {
    block: u32,
    top: usize,
    end: usize,
}

/// Fenêtre Best-Fit des emplacements > M : 8 blocs ouverts, reste de chacun en granules
/// (0 = case vide) ; le `top` d'une case se déduit de son reste.
struct Window {
    block: [u32; WINDOW],
    rest: [u16; WINDOW],
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    /// Somme des emplacements vivants.
    pub live_bytes: u64,
    /// Blocs pris (occupés ou ouverts).
    pub held_blocks: u64,
    pub pages_active: u64,
    pub pages_empty: u64,
    pub pages_released: u64,
    /// Pages neuves ou rendues reprises dans la boucle : faute de page synchrone (v1).
    pub sync_faults: u64,
    /// Allocations repliées en externe, plage pleine.
    pub range_full: u64,
}

pub struct Pool {
    base: usize,
    nb_pages: u32,
    /// Granules vivants, un `u16` par bloc.
    live: *mut u16,
    /// Bloc courant d'un curseur : un compteur à 0 ne le libère pas.
    open: *mut u64,
    maps: [(*mut u8, usize); 3],
    pages: Vec<PageState>,
    next_page: u32,
    pages_with_free: Vec<u32>,
    empty_pages: VecDeque<(u32, Instant)>,
    released_pages: Vec<u32>,
    main: Cursor,
    medium: Window,
    decay: Duration,
    reserve_div: u64,
    live_bytes: u64,
    held_blocks: u64,
    sync_faults: u64,
    range_full: u64,
}

/// `mmap` anonyme `MAP_NORESERVE` : rien n'est engagé avant d'être touché.
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

impl Pool {
    /// Plage de `virt` octets (arrondie à la page). `None` si un `mmap` échoue.
    pub fn new(virt: usize, decay: Duration, reserve_div: u64) -> Option<Self> {
        let nb_pages = (virt / PAGE).clamp(1, (u32::MAX / BLOCKS_PER_PAGE) as usize) as u32;
        let virt = nb_pages as usize * PAGE;
        let nb_blocks = nb_pages as usize * BLOCKS_PER_PAGE as usize;
        let range = map(virt + PAGE)?;
        let base = (range as usize).next_multiple_of(PAGE);
        unsafe { libc::madvise(base as *mut _, virt, libc::MADV_HUGEPAGE) };
        let (live_len, open_len) = (nb_blocks * 2, nb_blocks.div_ceil(64) * 8);
        let unmap = |p: *mut u8, len| unsafe { libc::munmap(p as *mut _, len) };
        let Some(live) = map(live_len) else {
            unmap(range, virt + PAGE);
            return None;
        };
        let Some(open) = map(open_len) else {
            unmap(range, virt + PAGE);
            unmap(live, live_len);
            return None;
        };
        Some(Self {
            base,
            nb_pages,
            live: live as *mut u16,
            open: open as *mut u64,
            maps: [(range, virt + PAGE), (live, live_len), (open, open_len)],
            pages: Vec::new(),
            next_page: 0,
            pages_with_free: Vec::new(),
            empty_pages: VecDeque::new(),
            released_pages: Vec::new(),
            main: Cursor { block: NONE, top: 0, end: 0 },
            medium: Window { block: [NONE; WINDOW], rest: [0; WINDOW] },
            decay,
            reserve_div: reserve_div.max(1),
            live_bytes: 0,
            held_blocks: 0,
            sync_faults: 0,
            range_full: 0,
        })
    }

    pub fn stats(&self) -> Stats {
        let (empty, released) = (self.empty_pages.len() as u64, self.released_pages.len() as u64);
        Stats {
            live_bytes: self.live_bytes,
            held_blocks: self.held_blocks,
            pages_active: self.next_page as u64 - empty - released,
            pages_empty: empty,
            pages_released: released,
            sync_faults: self.sync_faults,
            range_full: self.range_full,
        }
    }

    /// Réserve : `max(2 pages, vivants / div)` (§16.6), gardée même vide.
    pub fn reserve(&self) -> u64 {
        (2 * PAGE as u64).max(self.live_bytes / self.reserve_div)
    }

    #[inline]
    fn block_addr(&self, b: u32) -> usize {
        self.base + ((b as usize) << BLOCK_SHIFT)
    }

    #[inline]
    fn live(&mut self, b: u32) -> &mut u16 {
        unsafe { &mut *self.live.add(b as usize) }
    }

    #[inline]
    fn is_open(&self, b: u32) -> bool {
        unsafe { *self.open.add(b as usize / 64) & (1 << (b % 64)) != 0 }
    }

    #[inline]
    fn set_open(&mut self, b: u32, on: bool) {
        let w = unsafe { &mut *self.open.add(b as usize / 64) };
        if on { *w |= 1 << (b % 64) } else { *w &= !(1 << (b % 64)) }
    }

    /// Emplacement de `size` octets (pair, ≤ [`BLOCK`]). Nul si la plage est pleine.
    #[inline]
    pub fn alloc(&mut self, size: usize) -> *mut u8 {
        debug_assert!(size % 2 == 0 && size <= BLOCK);
        if size > MEDIUM {
            return self.alloc_medium(size);
        }
        let c = &mut self.main;
        if c.top + size <= c.end {
            let p = c.top;
            c.top += size;
            let b = c.block;
            *self.live(b) += (size / 2) as u16;
            self.live_bytes += size as u64;
            return p as *mut u8;
        }
        self.alloc_main_slow(size)
    }

    #[cold]
    fn alloc_main_slow(&mut self, size: usize) -> *mut u8 {
        if self.main.block != NONE {
            self.close_block(self.main.block);
            self.main = Cursor { block: NONE, top: 0, end: 0 };
        }
        let Some(b) = self.take_block() else { return null_mut() };
        let a = self.block_addr(b);
        self.main = Cursor { block: b, top: a, end: a + BLOCK };
        self.alloc(size)
    }

    #[inline(never)]
    fn alloc_medium(&mut self, size: usize) -> *mut u8 {
        let g = (size / 2) as u16;
        let w = &self.medium;
        let mut best: Option<usize> = None;
        for i in 0..WINDOW {
            if w.rest[i] >= g && best.is_none_or(|j| w.rest[i] < w.rest[j]) {
                best = Some(i);
            }
        }
        let i = match best {
            Some(i) => i,
            None => {
                let Some(b) = self.take_block() else { return null_mut() };
                let w = &self.medium;
                let i = match w.rest.iter().position(|&r| r == 0) {
                    Some(i) => i,
                    None => {
                        let j = (0..WINDOW).min_by_key(|&j| w.rest[j]).unwrap();
                        self.close_case(j);
                        j
                    }
                };
                self.medium.block[i] = b;
                self.medium.rest[i] = (BLOCK / 2) as u16;
                i
            }
        };
        let b = self.medium.block[i];
        let p = self.block_addr(b) + BLOCK - self.medium.rest[i] as usize * 2;
        self.medium.rest[i] -= g;
        *self.live(b) += g;
        self.live_bytes += size as u64;
        if (self.medium.rest[i] as usize) * 2 < MEDIUM {
            self.close_case(i);
        }
        p as *mut u8
    }

    fn close_case(&mut self, i: usize) {
        let b = self.medium.block[i];
        self.medium.block[i] = NONE;
        self.medium.rest[i] = 0;
        self.close_block(b);
    }

    /// Le bloc quitte son curseur ; vide, il est libéré.
    fn close_block(&mut self, b: u32) {
        self.set_open(b, false);
        if *self.live(b) == 0 {
            self.release_block(b);
        }
    }

    /// Rend l'emplacement `ptr` de `size` octets.
    ///
    /// # Safety
    /// `ptr` vient de [`Pool::alloc`] de ce pool avec la même `size`, pas encore rendu.
    #[inline]
    pub unsafe fn free(&mut self, ptr: *mut u8, size: usize) {
        let addr = ptr as usize;
        let b = ((addr - self.base) >> BLOCK_SHIFT) as u32;
        let g = (size / 2) as u16;
        let live = self.live(b);
        debug_assert!(*live >= g);
        *live -= g;
        let now_empty = *live == 0;
        self.live_bytes -= size as u64;
        if self.is_open(b) {
            self.rewind(b, addr, size, now_empty);
        } else if now_empty {
            self.release_block(b);
        }
    }

    /// Objet d'un bloc encore ouvert : s'il était le dernier, le `top` recule ; si le
    /// bloc est vide, il repart de zéro.
    #[cold]
    fn rewind(&mut self, b: u32, addr: usize, size: usize, empty: bool) {
        if self.main.block == b {
            if empty {
                self.main.top = self.block_addr(b);
            } else if addr + size == self.main.top {
                self.main.top = addr;
            }
            return;
        }
        let end = self.block_addr(b) + BLOCK;
        for i in 0..WINDOW {
            if self.medium.block[i] == b {
                let top = end - self.medium.rest[i] as usize * 2;
                if empty {
                    self.medium.rest[i] = (BLOCK / 2) as u16;
                } else if addr + size == top {
                    self.medium.rest[i] += (size / 2) as u16;
                }
                return;
            }
        }
    }

    /// Un bloc libre, marqué ouvert : d'une page active trouée, sinon d'une page vide (la
    /// plus récente), rendue, puis neuve. `None` : plage pleine.
    fn take_block(&mut self) -> Option<u32> {
        while let Some(&p) = self.pages_with_free.last() {
            let ps = &mut self.pages[p as usize];
            if ps.state != PageKind::Active || ps.free_mask == 0 {
                ps.in_stack = false;
                self.pages_with_free.pop();
                continue;
            }
            let bit = ps.free_mask.trailing_zeros();
            ps.free_mask &= !(1 << bit);
            if ps.free_mask == 0 {
                ps.in_stack = false;
                self.pages_with_free.pop();
            }
            return Some(self.open_block(p * BLOCKS_PER_PAGE + bit));
        }
        let p = if let Some((p, _)) = self.empty_pages.pop_back() {
            p
        } else if let Some(p) = self.released_pages.pop() {
            self.sync_faults += 1;
            p
        } else if self.next_page < self.nb_pages {
            self.sync_faults += 1;
            self.pages.push(PageState { free_mask: 0, state: PageKind::Active, in_stack: false });
            self.next_page += 1;
            self.next_page - 1
        } else {
            self.range_full += 1;
            return None;
        };
        let ps = &mut self.pages[p as usize];
        ps.state = PageKind::Active;
        ps.free_mask = !1;
        if !ps.in_stack {
            ps.in_stack = true;
            self.pages_with_free.push(p);
        }
        Some(self.open_block(p * BLOCKS_PER_PAGE))
    }

    fn open_block(&mut self, b: u32) -> u32 {
        self.set_open(b, true);
        self.held_blocks += 1;
        b
    }

    /// Compteur à 0, bloc fermé : retour dans le masque de sa page.
    fn release_block(&mut self, b: u32) {
        self.held_blocks -= 1;
        let p = b / BLOCKS_PER_PAGE;
        let ps = &mut self.pages[p as usize];
        ps.free_mask |= 1 << (b % BLOCKS_PER_PAGE);
        if !ps.in_stack {
            ps.in_stack = true;
            self.pages_with_free.push(p);
        }
        if ps.free_mask == u32::MAX {
            ps.state = PageKind::Empty;
            self.empty_pages.push_back((p, Instant::now()));
        }
    }

    /// Rend au système les pages vides au-delà de la réserve et vides depuis `decay`, au
    /// plus `budget`. Vrai s'il en reste à rendre tout de suite.
    pub fn release_step(&mut self, now: Instant, budget: u32) -> bool {
        let reserve = self.reserve();
        let mut n = 0;
        while let Some(&(p, since)) = self.empty_pages.front() {
            if (self.empty_pages.len() * PAGE) as u64 <= reserve || now.saturating_duration_since(since) < self.decay {
                return false;
            }
            if n == budget {
                return true;
            }
            self.empty_pages.pop_front();
            let a = self.block_addr(p * BLOCKS_PER_PAGE);
            unsafe { libc::madvise(a as *mut _, PAGE, libc::MADV_DONTNEED) };
            self.pages[p as usize].state = PageKind::Released;
            self.released_pages.push(p);
            n += 1;
        }
        false
    }

    #[cfg(test)]
    fn page_kind(&self, p: u32) -> PageKind {
        self.pages[p as usize].state
    }

    #[cfg(test)]
    fn block_of(&self, ptr: *mut u8) -> u32 {
        ((ptr as usize - self.base) >> BLOCK_SHIFT) as u32
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for (p, len) in self.maps {
            unsafe { libc::munmap(p as *mut _, len) };
        }
    }
}

// ─── Pool global ──────────────────────────────────────────────────────────────

static mut POOL: Option<Pool> = None;
static mut DISABLED: bool = false;

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[cold]
fn init() -> Option<&'static mut Pool> {
    unsafe {
        if DISABLED {
            return None;
        }
        let gb: usize = env("RADIXOX_POOL_VIRT_GB", 256);
        let decay = Duration::from_secs(env("RADIXOX_MEMORY_DECAY", 10));
        let Some(pool) = Pool::new(gb << 30, decay, env("RADIXOX_GC_RESERVE_DIV", 16)) else {
            eprintln!("radixox: mmap de la plage du pool ({gb} Gio) refusé, valeurs allouées hors pool");
            DISABLED = true;
            return None;
        };
        (*&raw mut POOL).insert(pool).into()
    }
}

/// Emplacement du pool global. Nul : pool indisponible ou plage pleine, l'appelant
/// alloue en externe.
///
/// # Safety
/// Mono-thread ; `size` pair et ≤ [`BLOCK`].
#[inline]
pub unsafe fn alloc(size: usize) -> *mut u8 {
    match unsafe { &mut *&raw mut POOL } {
        Some(p) => p.alloc(size),
        None => init().map_or(null_mut(), |p| p.alloc(size)),
    }
}

/// # Safety
/// `ptr` vient de [`alloc`] avec la même `size`, pas encore rendu.
#[inline]
pub unsafe fn free(ptr: *mut u8, size: usize) {
    match unsafe { &mut *&raw mut POOL } {
        Some(p) => unsafe { p.free(ptr, size) },
        None => unreachable!("free hors pool"),
    }
}

/// Depuis la boucle, au repos (timer) : rend les pages vides en trop. Vrai s'il en
/// reste à rendre tout de suite.
pub fn release_step(now: Instant) -> bool {
    match unsafe { &mut *&raw mut POOL } {
        Some(p) => p.release_step(now, RELEASE_BUDGET),
        None => false,
    }
}

/// `None` tant que le pool n'a servi à rien (ou s'il est désactivé).
pub fn stats() -> Option<(Stats, u64)> {
    unsafe { (*&raw const POOL).as_ref().map(|p| (p.stats(), p.reserve())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(pages: usize) -> Pool {
        Pool::new(pages * PAGE, Duration::ZERO, 16).unwrap()
    }

    #[test]
    fn last_object_rewinds_top() {
        let mut p = pool(2);
        let a = p.alloc(100);
        let b = p.alloc(100);
        assert_eq!(b as usize, a as usize + 100);
        unsafe { p.free(b, 100) };
        assert_eq!(p.alloc(100), b, "le top a reculé");
        unsafe { p.free(a, 100) }; // pas le dernier : le top ne bouge pas
        assert_eq!(p.alloc(100) as usize, b as usize + 100);
        assert_eq!(p.stats().live_bytes, 200);
    }

    #[test]
    fn open_block_at_zero_restarts() {
        let mut p = pool(2);
        let a = p.alloc(100);
        let b = p.alloc(100);
        unsafe {
            p.free(a, 100);
            p.free(b, 100);
        }
        assert_eq!(p.alloc(100), a, "bloc ouvert vidé : le bump repart du début");
    }

    #[test]
    fn closed_block_at_zero_is_reused() {
        let mut p = pool(2);
        let per_block = BLOCK / MEDIUM;
        let first: Vec<_> = (0..per_block).map(|_| p.alloc(MEDIUM)).collect();
        let b0 = p.block_of(first[0]);
        assert!(first.iter().all(|&x| p.block_of(x) == b0));
        let next = p.alloc(MEDIUM); // ferme le bloc 0, ouvre le suivant
        assert_ne!(p.block_of(next), b0);
        assert_eq!(p.stats().held_blocks, 2);
        for &x in &first {
            unsafe { p.free(x, MEDIUM) };
        }
        assert_eq!(p.stats().held_blocks, 1, "bloc 0 libéré à 0");
        // Le curseur finit son bloc, puis reprend le bloc 0 (bit le plus bas).
        for _ in 1..per_block {
            p.alloc(MEDIUM);
        }
        let x = p.alloc(MEDIUM);
        assert_eq!(p.block_of(x), b0);
    }

    #[test]
    fn empty_page_then_released_then_reused() {
        let mut p = pool(5);
        let per_page = PAGE / MEDIUM;
        // 3 pages pleines, puis un objet sur la 4e (le curseur quitte la 3e).
        let objs: Vec<_> = (0..3 * per_page).map(|_| p.alloc(MEDIUM)).collect();
        let last = p.alloc(MEDIUM);
        assert_eq!(p.block_of(last) / BLOCKS_PER_PAGE, 3);
        for &x in &objs {
            unsafe { p.free(x, MEDIUM) };
        }
        let s = p.stats();
        assert_eq!((s.pages_active, s.pages_empty, s.pages_released), (1, 3, 0));
        assert_eq!(s.sync_faults, 4);
        // Réserve = 2 pages : une seule des 3 pages vides est rendue, la plus ancienne.
        assert!(!p.release_step(Instant::now(), 8));
        let s = p.stats();
        assert_eq!((s.pages_empty, s.pages_released), (2, 1));
        assert_eq!(p.page_kind(0), PageKind::Released);
        // Les pages vides passent avant la page rendue, la plus récente d'abord.
        let mut seen = Vec::new();
        for _ in 0..3 * per_page {
            let x = p.alloc(MEDIUM);
            let pg = p.block_of(x) / BLOCKS_PER_PAGE;
            if seen.last() != Some(&pg) {
                seen.push(pg);
            }
        }
        assert_eq!(seen, [3, 2, 1, 0]);
        assert_eq!(p.stats().sync_faults, 5, "seule la page rendue fait une faute");
    }

    #[test]
    fn release_waits_for_decay_and_budget() {
        let mut p = Pool::new(8 * PAGE, Duration::from_secs(3600), 16).unwrap();
        let per_page = PAGE / MEDIUM;
        let objs: Vec<_> = (0..6 * per_page).map(|_| p.alloc(MEDIUM)).collect();
        p.alloc(MEDIUM);
        for &x in &objs {
            unsafe { p.free(x, MEDIUM) };
        }
        assert!(!p.release_step(Instant::now(), 8), "pas encore assez vieilles");
        assert_eq!(p.stats().pages_released, 0);
        let later = Instant::now() + Duration::from_secs(3601);
        assert!(p.release_step(later, 2), "budget de 2 : il en reste");
        assert!(!p.release_step(later, 8));
        assert_eq!(p.stats().pages_released, 4); // 6 vides − réserve de 2
    }

    #[test]
    fn range_full_returns_null() {
        let mut p = pool(1);
        let n = PAGE / MEDIUM;
        for _ in 0..n {
            assert!(!p.alloc(MEDIUM).is_null());
        }
        assert!(p.alloc(MEDIUM).is_null());
        assert!(p.alloc(MEDIUM + 2).is_null());
        assert_eq!(p.stats().range_full, 2);
    }

    #[test]
    fn medium_window_best_fit() {
        let mut p = pool(2);
        let a = p.alloc(10_000);
        let ba = p.block_of(a);
        let a2 = p.alloc(40_000); // reste de A : 55 536 → A
        assert_eq!(p.block_of(a2), ba);
        let b = p.alloc(20_000); // reste de A : 15 536 < 20 000 → nouveau bloc B
        let bb = p.block_of(b);
        assert_ne!(bb, ba);
        // A (15 536) et B (45 536) conviennent : le plus petit reste gagne.
        let c = p.alloc(15_000);
        assert_eq!(p.block_of(c), ba);
        assert_eq!(c as usize, a2 as usize + 40_000);
        // Reste de A : 536 < M → case fermée ; les petits objets vont au curseur main.
        assert!(!p.is_open(ba));
        let x = p.alloc(15_000);
        assert_eq!(p.block_of(x), bb);
        // Fenêtre pleine : la case au plus petit reste est fermée pour un nouveau bloc.
        for _ in 0..8 {
            p.alloc(60_000);
        }
        assert_eq!(p.medium.block.iter().filter(|&&b| b != NONE).count(), WINDOW);
        assert!(p.medium.rest.iter().all(|&r| r as usize * 2 >= MEDIUM));
    }
}
