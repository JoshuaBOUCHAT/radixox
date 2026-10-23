//! Pool des `SharedByte` en forme courte (MEMORY_ARCH.md §16.3, §16.4, §16.6).
//!
//! Plage virtuelle réservée une fois, pages de 2 Mio (huge pages), blocs de 64 Kio.
//! Allocation par bump, un compteur de granules vivants par bloc, sans en-tête de bloc :
//! libérer = décrémenter ; un bloc dont le compteur tombe à 0 se réutilise tel quel ; une
//! page dont les 32 blocs sont libres depuis `RADIXOX_MEMORY_DECAY` secondes est rendue au
//! système au-delà de la réserve.
//!
//! GC niveau A (§16.6) : le pool décide quand une passe démarre et marque les blocs creux ;
//! le parcours des structures (oxidart) déplace chaque objet d'un bloc marqué vers le
//! curseur des survivants (`SharedByte::gc_relocate`) ; un bloc évacué tombe à 0 et se
//! libère par le chemin normal.
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
/// Curseurs et fenêtres : les allocations normales, les survivants du GC (§16.4).
const MAIN: usize = 0;
const SURVIVOR: usize = 1;
/// Une passe qui rend moins que ça suspend le déclenchement (§16.6).
const WEAK_PASS: u64 = 2 << 20;
const WEAK_PAUSE: Duration = Duration::from_secs(10);
/// Passes qu'un bloc marqué traverse sans se vider avant d'être démarqué.
const MAX_MARKED_PASSES: u8 = 2;

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
#[derive(Clone, Copy)]
struct Cursor {
    block: u32,
    top: usize,
    end: usize,
}

const NO_CURSOR: Cursor = Cursor { block: NONE, top: 0, end: 0 };

/// Fenêtre Best-Fit des emplacements > M : 8 blocs ouverts, reste de chacun en granules
/// (0 = case vide) ; le `top` d'une case se déduit de son reste.
#[derive(Clone, Copy)]
struct Window {
    block: [u32; WINDOW],
    rest: [u16; WINDOW],
}

const NO_WINDOW: Window = Window { block: [NONE; WINDOW], rest: [0; WINDOW] };

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

#[derive(Clone, Copy, Default, Debug)]
pub struct GcStats {
    pub running: bool,
    pub passes: u64,
    pub evacuated_objects: u64,
    pub evacuated_bytes: u64,
    /// Objets d'un bloc marqué tenus ailleurs (rc > 1) : pas déplacés.
    pub pinned: u64,
    /// Blocs démarqués après [`MAX_MARKED_PASSES`] passes sans se vider.
    pub unmarked_blocks: u64,
    /// Pages prises faute de bloc libre pendant une passe.
    pub fresh_pages: u64,
    pub last_pass_ms: u64,
    /// Blocs marqués en ce moment.
    pub marked_blocks: u64,
}

/// Fin d'une passe, pour le journal.
#[derive(Clone, Copy, Debug)]
pub struct PassReport {
    pub freed_bytes: u64,
    pub still_marked: u64,
    pub unmarked: u64,
    pub ms: u64,
}

pub struct Pool {
    base: usize,
    nb_pages: u32,
    /// Granules vivants, un `u16` par bloc.
    live: *mut u16,
    /// Bloc courant d'un curseur : un compteur à 0 ne le libère pas.
    open: *mut u64,
    /// Bloc à évacuer par la passe en cours (ou la suivante).
    marked: *mut u64,
    maps: [(*mut u8, usize); 4],
    pages: Vec<PageState>,
    next_page: u32,
    pages_with_free: Vec<u32>,
    empty_pages: VecDeque<(u32, Instant)>,
    released_pages: Vec<u32>,
    cursors: [Cursor; 2],
    windows: [Window; 2],
    decay: Duration,
    reserve_div: u64,
    live_bytes: u64,
    held_blocks: u64,
    sync_faults: u64,
    range_full: u64,
    /// Octets alloués par les commandes (survivants exclus), cumul : budget du GC.
    allocated: u64,
    /// Blocs marqués et passes déjà traversées.
    marked_list: Vec<(u32, u8)>,
    gc: GcStats,
    pass_started: Option<Instant>,
    /// Blocs marqués libérés depuis le début de la passe.
    freed_in_pass: u64,
    suspended_until: Option<Instant>,
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

#[inline]
fn bit(bits: *mut u64, b: u32) -> bool {
    unsafe { *bits.add(b as usize / 64) & (1 << (b % 64)) != 0 }
}

#[inline]
fn set_bit(bits: *mut u64, b: u32, on: bool) {
    let w = unsafe { &mut *bits.add(b as usize / 64) };
    if on { *w |= 1 << (b % 64) } else { *w &= !(1 << (b % 64)) }
}

impl Pool {
    /// Plage de `virt` octets (arrondie à la page). `None` si un `mmap` échoue.
    pub fn new(virt: usize, decay: Duration, reserve_div: u64) -> Option<Self> {
        let nb_pages = (virt / PAGE).clamp(1, (u32::MAX / BLOCKS_PER_PAGE) as usize) as u32;
        let virt = nb_pages as usize * PAGE;
        let nb_blocks = nb_pages as usize * BLOCKS_PER_PAGE as usize;
        let bits_len = nb_blocks.div_ceil(64) * 8;
        let lens = [virt + PAGE, nb_blocks * 2, bits_len, bits_len];
        let mut maps = [(null_mut(), 0); 4];
        for (i, &len) in lens.iter().enumerate() {
            let Some(p) = map(len) else {
                for &(p, len) in &maps[..i] {
                    unsafe { libc::munmap(p as *mut _, len) };
                }
                return None;
            };
            maps[i] = (p, len);
        }
        let base = (maps[0].0 as usize).next_multiple_of(PAGE);
        unsafe { libc::madvise(base as *mut _, virt, libc::MADV_HUGEPAGE) };
        Some(Self {
            base,
            nb_pages,
            live: maps[1].0 as *mut u16,
            open: maps[2].0 as *mut u64,
            marked: maps[3].0 as *mut u64,
            maps,
            pages: Vec::new(),
            next_page: 0,
            pages_with_free: Vec::new(),
            empty_pages: VecDeque::new(),
            released_pages: Vec::new(),
            cursors: [NO_CURSOR; 2],
            windows: [NO_WINDOW; 2],
            decay,
            reserve_div: reserve_div.max(1),
            live_bytes: 0,
            held_blocks: 0,
            sync_faults: 0,
            range_full: 0,
            allocated: 0,
            marked_list: Vec::new(),
            gc: GcStats::default(),
            pass_started: None,
            freed_in_pass: 0,
            suspended_until: None,
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

    pub fn gc_stats(&self) -> GcStats {
        GcStats { marked_blocks: self.marked_list.len() as u64, ..self.gc }
    }

    /// Réserve : `max(2 pages, vivants / div)` (§16.6), gardée même vide.
    pub fn reserve(&self) -> u64 {
        (2 * PAGE as u64).max(self.live_bytes / self.reserve_div)
    }

    /// Octets alloués par les commandes depuis la création (survivants exclus).
    pub fn allocated(&self) -> u64 {
        self.allocated
    }

    #[inline]
    fn block_addr(&self, b: u32) -> usize {
        self.base + ((b as usize) << BLOCK_SHIFT)
    }

    /// Bloc de l'adresse `a`, `None` hors de la plage.
    #[inline]
    fn block_of_addr(&self, a: usize) -> Option<u32> {
        let off = a.wrapping_sub(self.base);
        (off < self.nb_pages as usize * PAGE).then_some((off >> BLOCK_SHIFT) as u32)
    }

    #[inline]
    fn live(&mut self, b: u32) -> &mut u16 {
        unsafe { &mut *self.live.add(b as usize) }
    }

    #[inline]
    fn is_open(&self, b: u32) -> bool {
        bit(self.open, b)
    }

    /// L'objet d'en-tête `a` est dans un bloc marqué.
    #[inline]
    pub fn is_marked_addr(&self, a: usize) -> bool {
        self.block_of_addr(a).is_some_and(|b| bit(self.marked, b))
    }

    /// Emplacement de `size` octets (pair, ≤ [`BLOCK`]). Nul si la plage est pleine.
    #[inline]
    pub fn alloc(&mut self, size: usize) -> *mut u8 {
        self.allocated += size as u64;
        self.alloc_in(MAIN, size)
    }

    /// Destination du GC : curseur et fenêtre à part, les survivants restent denses.
    #[inline]
    pub fn alloc_survivor(&mut self, size: usize) -> *mut u8 {
        self.alloc_in(SURVIVOR, size)
    }

    #[inline(always)]
    fn alloc_in(&mut self, k: usize, size: usize) -> *mut u8 {
        debug_assert!(size % 2 == 0 && size <= BLOCK);
        if size > MEDIUM {
            return self.alloc_medium(k, size);
        }
        let c = &mut self.cursors[k];
        if c.top + size <= c.end {
            let p = c.top;
            c.top += size;
            let b = c.block;
            *self.live(b) += (size / 2) as u16;
            self.live_bytes += size as u64;
            return p as *mut u8;
        }
        self.alloc_bump_slow(k, size)
    }

    #[cold]
    fn alloc_bump_slow(&mut self, k: usize, size: usize) -> *mut u8 {
        if self.cursors[k].block != NONE {
            self.close_block(self.cursors[k].block);
            self.cursors[k] = NO_CURSOR;
        }
        let Some(b) = self.take_block() else { return null_mut() };
        let a = self.block_addr(b);
        self.cursors[k] = Cursor { block: b, top: a, end: a + BLOCK };
        self.alloc_in(k, size)
    }

    #[inline(never)]
    fn alloc_medium(&mut self, k: usize, size: usize) -> *mut u8 {
        let g = (size / 2) as u16;
        let w = &self.windows[k];
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
                let w = &self.windows[k];
                let i = match w.rest.iter().position(|&r| r == 0) {
                    Some(i) => i,
                    None => {
                        let j = (0..WINDOW).min_by_key(|&j| w.rest[j]).unwrap();
                        self.close_case(k, j);
                        j
                    }
                };
                self.windows[k].block[i] = b;
                self.windows[k].rest[i] = (BLOCK / 2) as u16;
                i
            }
        };
        let b = self.windows[k].block[i];
        let p = self.block_addr(b) + BLOCK - self.windows[k].rest[i] as usize * 2;
        self.windows[k].rest[i] -= g;
        *self.live(b) += g;
        self.live_bytes += size as u64;
        if (self.windows[k].rest[i] as usize) * 2 < MEDIUM {
            self.close_case(k, i);
        }
        p as *mut u8
    }

    fn close_case(&mut self, k: usize, i: usize) {
        let b = self.windows[k].block[i];
        self.windows[k].block[i] = NONE;
        self.windows[k].rest[i] = 0;
        self.close_block(b);
    }

    /// Ferme tous les curseurs et fenêtres (passe forcée des tests).
    fn close_cursors(&mut self) {
        for k in [MAIN, SURVIVOR] {
            if self.cursors[k].block != NONE {
                self.close_block(self.cursors[k].block);
                self.cursors[k] = NO_CURSOR;
            }
            for i in 0..WINDOW {
                if self.windows[k].rest[i] != 0 {
                    self.close_case(k, i);
                }
            }
        }
    }

    /// Le bloc quitte son curseur ; vide, il est libéré.
    fn close_block(&mut self, b: u32) {
        set_bit(self.open, b, false);
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

    /// L'emplacement `ptr` passe de `old` à `new` octets (écrasement sur place plus
    /// court) : la fin devient un trou, compté comme tel.
    ///
    /// # Safety
    /// `ptr` vivant, de taille `old` ≥ `new` > 0.
    #[inline]
    pub unsafe fn shrink(&mut self, ptr: *mut u8, old: usize, new: usize) {
        let b = ((ptr as usize - self.base) >> BLOCK_SHIFT) as u32;
        *self.live(b) -= ((old - new) / 2) as u16;
        self.live_bytes -= (old - new) as u64;
    }

    /// Objet d'un bloc encore ouvert : s'il était le dernier, le `top` recule ; si le
    /// bloc est vide, il repart de zéro.
    #[cold]
    fn rewind(&mut self, b: u32, addr: usize, size: usize, empty: bool) {
        let start = self.block_addr(b);
        for c in &mut self.cursors {
            if c.block == b {
                if empty {
                    c.top = start;
                } else if addr + size == c.top {
                    c.top = addr;
                }
                return;
            }
        }
        let end = start + BLOCK;
        for w in &mut self.windows {
            for i in 0..WINDOW {
                if w.block[i] == b {
                    let top = end - w.rest[i] as usize * 2;
                    if empty {
                        w.rest[i] = (BLOCK / 2) as u16;
                    } else if addr + size == top {
                        w.rest[i] += (size / 2) as u16;
                    }
                    return;
                }
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
            self.page_fault();
            p
        } else if self.next_page < self.nb_pages {
            self.page_fault();
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

    fn page_fault(&mut self) {
        self.sync_faults += 1;
        if self.gc.running {
            self.gc.fresh_pages += 1;
        }
    }

    fn open_block(&mut self, b: u32) -> u32 {
        set_bit(self.open, b, true);
        self.held_blocks += 1;
        b
    }

    /// Compteur à 0, bloc fermé : retour dans le masque de sa page (démarqué s'il l'était).
    /// En débogage, rempli de `0xDB` : un pointeur périmé lit un motif reconnaissable.
    fn release_block(&mut self, b: u32) {
        #[cfg(radixox_gc_debug)]
        unsafe {
            std::ptr::write_bytes(self.block_addr(b) as *mut u8, 0xDB, BLOCK)
        };
        if bit(self.marked, b) {
            set_bit(self.marked, b, false);
            self.freed_in_pass += 1;
        }
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

    // ─── GC niveau A (§16.6) ──────────────────────────────────────────────────

    /// Déclenchement : `Some(seuil de densité)` si une passe doit démarrer. Au repos dès
    /// 5 % de fragmentation (seuil 90 %) ; sous charge à 15 %, 8 Mio récupérables et moins
    /// de blocs libres que la réserve (seuil 75 %).
    pub fn gc_want(&self, idle: bool, now: Instant) -> Option<f64> {
        if self.gc.running || self.suspended_until.is_some_and(|t| now < t) || self.held_blocks == 0 {
            return None;
        }
        let held = self.held_blocks * BLOCK as u64;
        let frag = 1.0 - self.live_bytes as f64 / held as f64;
        if idle {
            return (frag > 0.05).then_some(0.90);
        }
        let recoverable = held - self.live_bytes;
        let pages_held = (self.next_page as u64 - self.released_pages.len() as u64) * BLOCKS_PER_PAGE as u64;
        let free = (pages_held - self.held_blocks) * BLOCK as u64;
        (frag > 0.15 && recoverable >= 8 << 20 && free < self.reserve()).then_some(0.75)
    }

    /// Début de passe : marque, par densité croissante, les blocs fermés sous `threshold`
    /// tant que leurs octets vivants tiennent dans la réserve (ceux encore marqués de la
    /// passe précédente comptent). `all` : tous les blocs non vides, curseurs fermés
    /// d'abord, sans limite de réserve (tests). Rend
    /// le nombre de blocs marqués ; à 0, la passe compte comme faible et rien ne démarre.
    pub fn gc_begin(&mut self, threshold: f64, all: bool, now: Instant) -> usize {
        if all {
            self.close_cursors(); // les blocs ouverts aussi : passe exhaustive
        }
        let limit = (threshold * (BLOCK / 2) as f64) as u16;
        let mut budget = self.reserve();
        let mut sum: u64 = self.marked_list.iter().map(|&(b, _)| unsafe { *self.live.add(b as usize) } as u64 * 2).sum();
        let mut cand = Vec::new();
        for p in 0..self.next_page {
            let ps = self.pages[p as usize];
            if ps.state != PageKind::Active {
                continue;
            }
            for i in 0..BLOCKS_PER_PAGE {
                let b = p * BLOCKS_PER_PAGE + i;
                let live = unsafe { *self.live.add(b as usize) };
                if ps.free_mask & (1 << i) == 0 && !self.is_open(b) && !bit(self.marked, b) && live > 0 && (all || live < limit) {
                    cand.push((live, b));
                }
            }
        }
        cand.sort_unstable();
        if all {
            budget = u64::MAX;
        }
        for (live, b) in cand {
            if sum + live as u64 * 2 > budget {
                break;
            }
            sum += live as u64 * 2;
            set_bit(self.marked, b, true);
            self.marked_list.push((b, 0));
        }
        if self.marked_list.is_empty() {
            self.suspended_until = Some(now + WEAK_PAUSE);
            return 0;
        }
        self.gc.running = true;
        self.freed_in_pass = 0;
        self.pass_started = Some(now);
        self.marked_list.len()
    }

    /// Fin de passe : un bloc encore marqué après [`MAX_MARKED_PASSES`] passes est
    /// démarqué ; une passe qui a rendu moins de 2 Mio suspend le déclenchement 10 s.
    pub fn gc_end_pass(&mut self, now: Instant) -> PassReport {
        let marked = self.marked;
        let mut unmarked = 0;
        self.marked_list.retain_mut(|(b, passes)| {
            if !bit(marked, *b) {
                return false; // vidé et libéré
            }
            *passes += 1;
            if *passes >= MAX_MARKED_PASSES {
                set_bit(marked, *b, false);
                unmarked += 1;
                return false;
            }
            true
        });
        let freed = self.freed_in_pass * BLOCK as u64;
        if freed < WEAK_PASS {
            self.suspended_until = Some(now + WEAK_PAUSE);
        }
        let ms = self.pass_started.take().map_or(0, |t| now.saturating_duration_since(t).as_millis() as u64);
        self.gc.running = false;
        self.gc.passes += 1;
        self.gc.unmarked_blocks += unmarked;
        self.gc.last_pass_ms = ms;
        PassReport { freed_bytes: freed, still_marked: self.marked_list.len() as u64, unmarked, ms }
    }

    /// Démarque tout, sans compter de passe (tests : passe forcée).
    pub fn gc_unmark_all(&mut self) {
        for &(b, _) in &self.marked_list {
            set_bit(self.marked, b, false);
        }
        self.marked_list.clear();
        self.gc.running = false;
        self.pass_started = None;
    }

    pub fn note_pinned(&mut self) {
        self.gc.pinned += 1;
    }

    pub fn note_evacuated(&mut self, size: usize) {
        self.gc.evacuated_objects += 1;
        self.gc.evacuated_bytes += size as u64;
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

/// Le pool global, s'il a déjà servi.
#[inline]
fn global() -> Option<&'static mut Pool> {
    unsafe { (*&raw mut POOL).as_mut() }
}

/// Emplacement du pool global. Nul : pool indisponible ou plage pleine, l'appelant
/// alloue en externe.
///
/// # Safety
/// Mono-thread ; `size` pair et ≤ [`BLOCK`].
#[inline]
pub unsafe fn alloc(size: usize) -> *mut u8 {
    match global() {
        Some(p) => p.alloc(size),
        None => init().map_or(null_mut(), |p| p.alloc(size)),
    }
}

/// # Safety
/// `ptr` vient de [`alloc`] avec la même `size`, pas encore rendu.
#[inline]
pub unsafe fn free(ptr: *mut u8, size: usize) {
    match global() {
        Some(p) => unsafe { p.free(ptr, size) },
        None => unreachable!("free hors pool"),
    }
}

/// # Safety
/// Voir [`Pool::shrink`] ; `ptr` vient de [`alloc`].
#[inline]
pub unsafe fn shrink(ptr: *mut u8, old: usize, new: usize) {
    match global() {
        Some(p) => unsafe { p.shrink(ptr, old, new) },
        None => unreachable!("shrink hors pool"),
    }
}

/// Destination d'un objet évacué. Nul : plage pleine, l'objet reste où il est.
pub fn alloc_survivor(size: usize) -> *mut u8 {
    global().map_or(null_mut(), |p| p.alloc_survivor(size))
}

/// L'objet d'en-tête `a` est à évacuer.
#[inline]
pub fn is_marked(a: usize) -> bool {
    global().is_some_and(|p| p.is_marked_addr(a))
}

pub fn note_pinned() {
    if let Some(p) = global() {
        p.note_pinned();
    }
}

pub fn note_evacuated(size: usize) {
    if let Some(p) = global() {
        p.note_evacuated(size);
    }
}

/// Depuis la boucle, au repos (timer) : rend les pages vides en trop. Vrai s'il en
/// reste à rendre tout de suite.
pub fn release_step(now: Instant) -> bool {
    global().is_some_and(|p| p.release_step(now, RELEASE_BUDGET))
}

/// Voir [`Pool::gc_want`] (`None` sans pool).
pub fn gc_want(idle: bool, now: Instant) -> Option<f64> {
    global().and_then(|p| p.gc_want(idle, now))
}

/// Voir [`Pool::gc_begin`] (0 sans pool).
pub fn gc_begin(threshold: f64, all: bool, now: Instant) -> usize {
    global().map_or(0, |p| p.gc_begin(threshold, all, now))
}

/// Voir [`Pool::gc_end_pass`].
pub fn gc_end_pass(now: Instant) -> Option<PassReport> {
    global().map(|p| p.gc_end_pass(now))
}

/// Voir [`Pool::gc_unmark_all`].
pub fn gc_unmark_all() {
    if let Some(p) = global() {
        p.gc_unmark_all();
    }
}

/// Octets alloués par les commandes (cumul), réserve : budget du GC sous charge.
pub fn gc_budget_inputs() -> Option<(u64, u64)> {
    global().map(|p| (p.allocated(), p.reserve()))
}

/// `None` tant que le pool n'a servi à rien (ou s'il est désactivé).
pub fn stats() -> Option<(Stats, u64)> {
    global().map(|p| (p.stats(), p.reserve()))
}

pub fn gc_stats() -> Option<GcStats> {
    global().map(|p| p.gc_stats())
}

/// Vérificateur (MEMORY_ARCH.md §16.8) : `slots` = les emplacements que tient l'arbre
/// (`SharedByte::pool_slot`). Chacun doit être dans la plage ; par bloc, leur somme ne
/// dépasse jamais le compteur `live` (l'égalité n'est pas exigée : des objets vivent hors
/// de l'arbre). Rend le total des octets recensés.
pub fn verify_census(slots: &[(usize, usize)]) -> Result<u64, String> {
    let Some(p) = global() else {
        return if slots.is_empty() { Ok(0) } else { Err("emplacements recensés sans pool".into()) };
    };
    let end = p.base + p.nb_pages as usize * PAGE;
    let mut per_block = std::collections::HashMap::<u32, u64>::new();
    let mut total = 0;
    for &(a, size) in slots {
        if a < p.base || a + size > end || a % 2 != 0 || size % 2 != 0 {
            return Err(format!("emplacement {a:#x} ({size} o) hors plage ou mal aligné"));
        }
        let b = ((a - p.base) >> BLOCK_SHIFT) as u32;
        if (a + size - 1 - p.base) >> BLOCK_SHIFT != b as usize {
            return Err(format!("emplacement {a:#x} ({size} o) à cheval sur deux blocs"));
        }
        *per_block.entry(b).or_default() += size as u64 / 2;
        total += size as u64;
    }
    for (b, g) in per_block {
        let live = unsafe { *p.live.add(b as usize) } as u64;
        if g > live {
            return Err(format!("bloc {b} : {g} granules recensés, compteur {live}"));
        }
    }
    Ok(total)
}

/// Octets vivants du pool global (0 sans pool).
pub fn live_bytes() -> u64 {
    global().map_or(0, |p| p.live_bytes)
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
        assert_eq!(p.windows[MAIN].block.iter().filter(|&&b| b != NONE).count(), WINDOW);
        assert!(p.windows[MAIN].rest.iter().all(|&r| r as usize * 2 >= MEDIUM));
    }

    /// Blocs fermés à moitié vides : marqués par densité croissante dans la limite de la
    /// réserve ; les survivants vont dans leur propre bloc ; un bloc évacué est libéré et
    /// démarqué ; un bloc qui ne se vide pas est démarqué après deux passes.
    #[test]
    fn gc_marks_evacuates_and_unmarks() {
        let mut p = pool(8);
        let per_block = BLOCK / 100;
        // 10 blocs pleins d'objets de 100 o, puis on en libère une part croissante.
        let objs: Vec<Vec<*mut u8>> = (0..10).map(|_| (0..per_block).map(|_| p.alloc(100)).collect()).collect();
        p.alloc(100); // ferme le 10e bloc
        for (i, blk) in objs.iter().enumerate() {
            for &x in blk.iter().skip(i % 5 + 1).step_by(2) {
                unsafe { p.free(x, 100) }; // ~moitié libérée, densités voisines
            }
        }
        let now = Instant::now();
        assert!(p.gc_want(true, now).is_some(), "au repos, fragmentation > 5 %");
        let n = p.gc_begin(0.90, false, now);
        assert!(n > 0 && n <= 10);
        // Évacuation à la main de tout ce qui reste dans les blocs marqués.
        let marked: Vec<u32> = p.marked_list.iter().map(|&(b, _)| b).collect();
        let held_before = p.stats().held_blocks;
        // Les objets vivants d'un bloc marqué : ceux qu'on n'a pas libérés.
        for (i, blk) in objs.iter().enumerate() {
            let freed: std::collections::HashSet<usize> =
                blk.iter().skip(i % 5 + 1).step_by(2).map(|&x| x as usize).collect();
            for &x in blk {
                if !freed.contains(&(x as usize)) && p.is_marked_addr(x as usize) {
                    let y = p.alloc_survivor(100);
                    assert!(!p.is_marked_addr(y as usize), "survivant hors des blocs marqués");
                    unsafe { p.free(x, 100) };
                }
            }
        }
        // n blocs rendus, au plus n/2 + 1 blocs de survivants (moitié pleins avant)
        assert!(p.stats().held_blocks <= held_before - n as u64 + (n as u64 / 2 + 1), "blocs marqués rendus");
        assert!(marked.iter().all(|&b| !bit(p.marked, b)), "libérés donc démarqués");
        let r = p.gc_end_pass(now);
        assert_eq!(r.freed_bytes, n as u64 * BLOCK as u64);
        assert_eq!(r.still_marked, 0);

        // Un bloc marqué qui ne se vide pas (objet épinglé) : démarqué après deux passes.
        let later = now + Duration::from_secs(60);
        let m = p.gc_begin(1.0, true, later);
        assert!(m > 0);
        assert_eq!(p.gc_end_pass(later).still_marked, m as u64);
        let r = p.gc_end_pass(later);
        assert_eq!((r.still_marked, r.unmarked), (0, m as u64));
        assert_eq!(p.gc_stats().marked_blocks, 0);
    }

    #[test]
    fn gc_under_load_needs_pressure_and_weak_pass_suspends() {
        let mut p = pool(16);
        let per_block = BLOCK / 100;
        let objs: Vec<*mut u8> = (0..200 * per_block).map(|_| p.alloc(100)).collect();
        p.alloc(100);
        for &x in objs.iter().step_by(2) {
            unsafe { p.free(x, 100) };
        }
        let now = Instant::now();
        // ~50 % de fragmentation, ~6 Mio récupérables : sous le seuil de 8 Mio.
        assert_eq!(p.gc_want(false, now), None);
        assert_eq!(p.gc_want(true, now), Some(0.90));
        // Rien sous 10 % de densité : passe faible, déclenchement suspendu 10 s.
        assert_eq!(p.gc_begin(0.10, false, now), 0);
        assert_eq!(p.gc_want(true, now + Duration::from_secs(5)), None);
        assert!(p.gc_want(true, now + Duration::from_secs(11)).is_some());
    }
}
