//! Objets à poignée (MEMORY_ARCH.md §19) : un objet référencé par plusieurs structures
//! (membre de ZSet : index + B+tree) reste déplaçable par le GC sans qu'on parcoure ces
//! structures. Elles gardent une poignée `u32` ; la table donne l'adresse ; l'objet porte
//! sa poignée dans son en-tête, ce qui permet d'évacuer un bloc en le parcourant.
//!
//! Deux formes d'objet :
//! - dans le pool (objet ≤ [`pool::MEDIUM`]) : `[longueur u16 : bit 15 = mort][poignée u32][octets]`,
//!   granule de 2 o, blocs des curseurs à poignée (bump seul) ;
//! - externe (plus grand, pool plein ou désactivé) : `[longueur u32][poignée u32][octets]`,
//!   allocateur des grands objets, jamais déplacé.
//!
//! La forme se lit à l'adresse : dans la plage du pool ou non.


use crate::pool;
use crate::raw_alloc::external;

/// Bit « mort » de la longueur d'un objet du pool.
pub(crate) const DEAD: u16 = 1 << 15;
/// En-tête d'un objet du pool.
pub(crate) const HEADER: usize = 6;
/// En-tête d'un objet externe.
const EXT_HEADER: usize = 8;
const NONE: u32 = u32::MAX;
/// Plus long membre dans le pool : l'objet entier tient dans [`pool::MEDIUM`].
pub const POOL_MAX_LEN: usize = pool::MEDIUM - HEADER;

/// Taille dans le pool d'un objet de `len` octets (granule de 2 o).
#[inline]
pub(crate) const fn obj_size(len: usize) -> usize {
    (HEADER + len + 1) & !1
}

/// Table poignée → adresse (§19.1) : plage `mmap` à elle, `u64` par poignée. Case libre :
/// `(poignée suivante << 1) | 1` (une adresse d'objet est paire).
pub struct Handles {
    slots: *mut u64,
    map_len: usize,
    cap: u32,
    /// Cases jamais servies au-delà.
    top: u32,
    free: u32,
    live: u32,
    /// Part comptée dans le registre des pages (THP permis : tranches de 2 Mio).
    pages: crate::pages::Tranches,
}

impl Handles {
    /// `cap` poignées au plus (`MAP_NORESERVE` : seules les cases touchées comptent).
    pub fn new(cap: u32) -> Option<Self> {
        let cap = cap.clamp(1, NONE - 1);
        let map_len = (cap as usize * 8).next_multiple_of(4096);
        crate::sys::map_anon(map_len, true).ok().map(|p| Self {
            slots: p as *mut u64,
            map_len,
            cap,
            top: 0,
            free: NONE,
            live: 0,
            pages: crate::pages::Tranches::new(p as usize, crate::pages::TRANCHE),
        })
    }

    /// Nouvelle poignée vers `addr` (paire). `None` : table pleine.
    #[inline]
    pub fn alloc(&mut self, addr: usize) -> Option<u32> {
        debug_assert!(addr % 2 == 0);
        let h = if self.free != NONE {
            let h = self.free;
            self.free = (unsafe { *self.slots.add(h as usize) } >> 1) as u32;
            h
        } else if self.top < self.cap {
            self.top += 1;
            self.pages.ensure(self.top as usize * 8);
            self.top - 1
        } else {
            return None;
        };
        unsafe { *self.slots.add(h as usize) = addr as u64 };
        self.live += 1;
        Some(h)
    }

    /// # Safety
    /// `h` vivante.
    #[inline]
    pub unsafe fn free(&mut self, h: u32) {
        debug_assert!(self.is_live(h));
        unsafe { *self.slots.add(h as usize) = ((self.free as u64) << 1) | 1 };
        self.free = h;
        self.live -= 1;
    }

    #[inline]
    pub fn get(&self, h: u32) -> usize {
        debug_assert!(self.is_live(h));
        unsafe { *self.slots.add(h as usize) as usize }
    }

    #[inline]
    pub fn set(&mut self, h: u32, addr: usize) {
        debug_assert!(self.is_live(h) && addr % 2 == 0);
        unsafe { *self.slots.add(h as usize) = addr as u64 };
    }

    /// Adresse de la case (préchargement).
    #[inline]
    pub fn slot_ptr(&self, h: u32) -> *const u64 {
        unsafe { self.slots.add(h as usize) }
    }

    pub fn is_live(&self, h: u32) -> bool {
        h < self.top && unsafe { *self.slots.add(h as usize) } & 1 == 0
    }

    pub fn live(&self) -> u32 {
        self.live
    }

    /// Octets touchés de la table.
    pub fn table_bytes(&self) -> u64 {
        self.top as u64 * 8
    }

    /// Octets résidents de la table (pages qui couvrent les cases touchées).
    pub fn table_resident(&self) -> u64 {
        crate::resident::region(self.slots as usize, self.top as usize * 8, self.map_len)
    }

    /// Poignées vivantes et leur adresse.
    pub fn for_each_live(&self, mut f: impl FnMut(u32, usize)) {
        for h in 0..self.top {
            let v = unsafe { *self.slots.add(h as usize) };
            if v & 1 == 0 {
                f(h, v as usize);
            }
        }
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe { crate::sys::unmap(self.slots as *mut u8, self.map_len) };
    }
}

// ─── Objets ───────────────────────────────────────────────────────────────────

/// Octets de l'objet d'adresse `a`, dans le pool `in_pool` ou externe.
///
/// # Safety
/// `a` est un objet vivant de la forme dite.
#[inline]
unsafe fn obj_bytes<'a>(a: usize, in_pool: bool) -> &'a [u8] {
    unsafe {
        if in_pool {
            let len = (a as *const u16).read() & !DEAD;
            std::slice::from_raw_parts((a + HEADER) as *const u8, len as usize)
        } else {
            let len = (a as *const u32).read();
            std::slice::from_raw_parts((a + EXT_HEADER) as *const u8, len as usize)
        }
    }
}

/// Écrit un objet `bytes` de poignée `h` : dans `pool` si possible, sinon en externe.
/// Rend l'adresse.
fn write_obj(pool: Option<&mut pool::Pool>, h: u32, bytes: &[u8]) -> usize {
    let len = bytes.len();
    if len <= POOL_MAX_LEN
        && let Some(p) = pool
    {
        let a = p.alloc_handled(obj_size(len));
        if !a.is_null() {
            unsafe {
                (a as *mut u16).write(len as u16);
                (a.add(2) as *mut u32).write_unaligned(h);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), a.add(HEADER), len);
            }
            return a as usize;
        }
    }
    let len32 = u32::try_from(len).expect("objet à poignée de plus de 4 Gio");
    unsafe {
        let a = external::malloc(EXT_HEADER + len);
        (a as *mut u32).write(len32);
        (a.add(4) as *mut u32).write(h);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), a.add(EXT_HEADER), len);
        a as usize
    }
}

/// Rend l'objet d'adresse `a`.
///
/// # Safety
/// `a` vient de [`write_obj`] avec ce `pool`, vivant.
unsafe fn free_obj(pool: Option<&mut pool::Pool>, a: usize) {
    unsafe {
        match pool {
            Some(p) if p.owns(a) => {
                let head = (a as *const u16).read();
                debug_assert!(head & DEAD == 0);
                (a as *mut u16).write(head | DEAD);
                p.free(a as *mut u8, obj_size(head as usize));
            }
            _ => {
                let len = (a as *const u32).read() as usize;
                external::free(a as *mut u8, EXT_HEADER + len);
            }
        }
    }
}

/// Un pool et sa table : ce que les tests construisent à part, et ce que tient le global.
pub struct Space<'a> {
    pub pool: Option<&'a mut pool::Pool>,
    pub table: &'a mut Handles,
}

impl Space<'_> {
    /// Nouvel objet, copie de `bytes`. `None` : table pleine.
    pub fn new_obj(&mut self, bytes: &[u8]) -> Option<u32> {
        let h = self.table.alloc(0)?;
        let a = write_obj(self.pool.as_deref_mut(), h, bytes);
        self.table.set(h, a);
        Some(h)
    }

    /// # Safety
    /// `h` vivante ; le résultat ne survit ni à une libération ni à une évacuation.
    pub unsafe fn bytes<'b>(&self, h: u32) -> &'b [u8] {
        let a = self.table.get(h);
        let in_pool = self.pool.as_deref().is_some_and(|p| p.owns(a));
        unsafe { obj_bytes(a, in_pool) }
    }

    /// # Safety
    /// `h` vivante ; plus aucune structure ne la garde.
    pub unsafe fn free(&mut self, h: u32) {
        let a = self.table.get(h);
        unsafe {
            free_obj(self.pool.as_deref_mut(), a);
            self.table.free(h);
        }
    }

    /// Voir [`pool::Pool::evacuate_handled`].
    pub fn evacuate(&mut self, from: u32, max_bytes: usize, moved: &mut usize) -> Option<u32> {
        match self.pool.as_deref_mut() {
            Some(p) => p.evacuate_handled(self.table, from, max_bytes, moved),
            None => None,
        }
    }

    /// Vérificateur (§19.6) : chaque case vivante pointe vers un objet dont l'en-tête porte
    /// cette poignée ; chaque objet vivant d'un bloc à poignée est pointé par sa case ; par
    /// bloc, les objets vivants ne dépassent pas le compteur du bloc.
    pub fn verify(&self) -> Result<(), String> {
        let pool = self.pool.as_deref();
        let mut err = None;
        let mut in_pool = 0u64;
        self.table.for_each_live(|h, a| {
            if err.is_some() {
                return;
            }
            let owned = pool.is_some_and(|p| p.owns(a));
            let back = unsafe {
                if owned {
                    in_pool += 1;
                    if (a as *const u16).read() & DEAD != 0 {
                        err = Some(format!("poignée {h} : objet {a:#x} mort"));
                        return;
                    }
                    ((a + 2) as *const u32).read_unaligned()
                } else {
                    ((a + 4) as *const u32).read()
                }
            };
            if back != h {
                err = Some(format!("poignée {h} : l'objet {a:#x} porte la poignée {back}"));
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        let Some(p) = pool else { return Ok(()) };
        let mut seen = 0u64;
        let mut per_block = std::collections::HashMap::<usize, u64>::new();
        p.handled_objects(|a, h, size| {
            if err.is_some() {
                return;
            }
            seen += 1;
            *per_block.entry(a >> pool::BLOCK_SHIFT).or_default() += size as u64 / 2;
            if !self.table.is_live(h) || self.table.get(h) != a {
                err = Some(format!("objet {a:#x} (poignée {h}) : la case ne pointe pas vers lui"));
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        if seen != in_pool {
            return Err(format!("{seen} objets vivants dans les blocs à poignée, {in_pool} cases vers le pool"));
        }
        for (blk, g) in per_block {
            let live = p.live_of_addr(blk << pool::BLOCK_SHIFT).unwrap_or(0);
            if g > live {
                return Err(format!("bloc {blk:#x} : {g} granules d'objets vivants, compteur {live}"));
            }
        }
        Ok(())
    }
}

// ─── Global ───────────────────────────────────────────────────────────────────

static mut HANDLES: Option<Handles> = None;

#[cold]
fn init_table() -> &'static mut Handles {
    let cap: u32 = std::env::var("RADIXOX_HANDLES_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(1 << 28);
    let t = Handles::new(cap).expect("radixox: mmap de la table des poignées refusé");
    unsafe { (*&raw mut HANDLES).insert(t) }
}

#[inline]
fn table() -> &'static mut Handles {
    match unsafe { (*&raw mut HANDLES).as_mut() } {
        Some(t) => t,
        None => init_table(),
    }
}

/// Le pool global des objets à poignée (absent sous `radixox_nopool` / `radixox_sysalloc`).
#[inline]
fn global_pool() -> Option<&'static mut pool::Pool> {
    if !pool::ENABLED {
        return None;
    }
    match pool::global() {
        Some(p) => Some(p),
        None => pool::init(),
    }
}

#[inline]
fn space() -> Space<'static> {
    Space { pool: if pool::ENABLED { pool::global() } else { None }, table: table() }
}

/// Nouvel objet global, copie de `bytes`. `None` : table pleine.
pub fn new_obj(bytes: &[u8]) -> Option<u32> {
    Space { pool: global_pool(), table: table() }.new_obj(bytes)
}

/// Octets de l'objet `h`.
///
/// # Safety
/// `h` vivante ; le résultat ne survit ni à sa libération ni à un pas du GC.
#[inline]
pub unsafe fn bytes(h: u32) -> &'static [u8] {
    unsafe { space().bytes(h) }
}

/// Adresse de la case de `h` (préchargement, chaînes AMAC).
#[inline]
pub fn slot_ptr(h: u32) -> *const u64 {
    table().slot_ptr(h)
}

/// Adresse de l'objet `h` (préchargement après la case).
#[inline]
pub fn addr(h: u32) -> usize {
    table().get(h)
}

/// # Safety
/// `h` vivante ; plus aucune structure ne la garde.
pub unsafe fn free(h: u32) {
    unsafe { space().free(h) }
}

/// Tranche d'évacuation des blocs à poignée marqués de la passe en cours ; voir
/// [`pool::Pool::evacuate_handled`].
pub fn gc_step(from: u32, max_bytes: usize, moved: &mut usize) -> Option<u32> {
    if unsafe { (*&raw const HANDLES).is_none() } {
        return None;
    }
    space().evacuate(from, max_bytes, moved)
}

/// Vérificateur global ; voir [`Space::verify`].
pub fn verify() -> Result<(), String> {
    if unsafe { (*&raw const HANDLES).is_none() } {
        return Ok(());
    }
    space().verify()
}

/// `(poignées vivantes, octets de la table)`.
/// Octets résidents de la table des poignées (0 sans table).
pub fn table_resident() -> u64 {
    unsafe { (*&raw const HANDLES).as_ref() }.map_or(0, Handles::table_resident)
}

pub fn stats() -> (u64, u64) {
    match unsafe { (*&raw const HANDLES).as_ref() } {
        Some(t) => (t.live() as u64, t.table_bytes()),
        None => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{Mark, PAGE, Pool};
    use std::time::{Duration, Instant};

    /// Générateur sans dépendance (xorshift).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn content(i: u64, len: usize) -> Vec<u8> {
        (0..len).map(|j| (i as usize * 31 + j) as u8).collect()
    }

    /// Modèle aléatoire (§19.6) : allouer, libérer, évacuer des blocs marqués ; après
    /// chaque étape, table et objets se désignent mutuellement, octets intacts, comptes
    /// du pool justes.
    #[test]
    fn handles_random_model() {
        let mut pool = Pool::new(64 * PAGE, Duration::ZERO, 16).unwrap();
        let mut table = Handles::new(1 << 20).unwrap();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // (poignée, graine du contenu, longueur)
        let mut live: Vec<(u32, u64, usize)> = Vec::new();
        let mut seed = 0u64;
        let check = |sp: &Space, live: &[(u32, u64, usize)]| {
            sp.verify().unwrap();
            for &(h, s, len) in live {
                assert_eq!(unsafe { sp.bytes(h) }, &content(s, len)[..], "poignée {h}");
            }
            assert_eq!(sp.table.live() as usize, live.len());
        };
        for round in 0..40 {
            {
                let mut sp = Space { pool: Some(&mut pool), table: &mut table };
                for _ in 0..3000 {
                    if live.is_empty() || rng.below(10) < 6 {
                        // surtout des petits, parfois au-delà du pool
                        let len = match rng.below(100) {
                            0 => POOL_MAX_LEN + 1 + rng.below(3000),
                            1..=5 => rng.below(POOL_MAX_LEN + 1),
                            _ => rng.below(40),
                        };
                        seed += 1;
                        let h = sp.new_obj(&content(seed, len)).unwrap();
                        live.push((h, seed, len));
                    } else {
                        let i = rng.below(live.len());
                        let (h, _, _) = live.swap_remove(i);
                        unsafe { sp.free(h) };
                    }
                }
                check(&sp, &live);
            }
            // une passe sur deux exhaustive (Mark::All ferme les curseurs)
            let n = pool.gc_begin(if round % 2 == 0 { Mark::All } else { Mark::Blocks(0.9) }, Instant::now());
            {
                let mut sp = Space { pool: Some(&mut pool), table: &mut table };
                let mut at = Some(0);
                let mut moved = 0;
                while let Some(from) = at {
                    // budget minuscule : reprise au rang rendu
                    at = sp.evacuate(from, moved + 1, &mut moved);
                    check(&sp, &live);
                }
            }
            if n > 0 {
                pool.gc_end_pass(Instant::now());
            }
            pool.gc_unmark_all();
            check(&Space { pool: Some(&mut pool), table: &mut table }, &live);
        }
        // tout libérer : le pool revient à zéro
        let mut sp = Space { pool: Some(&mut pool), table: &mut table };
        for (h, _, _) in live.drain(..) {
            unsafe { sp.free(h) };
        }
        check(&sp, &live);
        assert_eq!(pool.stats().live_bytes, 0);
    }

    /// Une passe complète vide entièrement les blocs à poignée creux : les survivants sont
    /// denses, et la case suit l'objet.
    #[test]
    fn evacuation_compacts_and_rewrites_slots() {
        let mut pool = Pool::new(8 * PAGE, Duration::ZERO, 16).unwrap();
        let mut table = Handles::new(1 << 16).unwrap();
        let hs: Vec<u32>;
        let before: Vec<usize>;
        let held;
        {
            let mut sp = Space { pool: Some(&mut pool), table: &mut table };
            hs = (0..20_000).map(|i| sp.new_obj(&content(i, 20)).unwrap()).collect();
            for &h in hs.iter().skip(1).step_by(2) {
                unsafe { sp.free(h) };
            }
            before = hs.iter().step_by(2).map(|&h| sp.table.get(h)).collect();
        }
        held = pool.stats().held_blocks;
        assert!(pool.gc_begin(Mark::All, Instant::now()) > 0);
        {
            let mut sp = Space { pool: Some(&mut pool), table: &mut table };
            let mut moved = 0;
            assert_eq!(sp.evacuate(0, usize::MAX, &mut moved), None);
            assert_eq!(moved, 10_000 * obj_size(20));
            sp.verify().unwrap();
            for (k, &h) in hs.iter().step_by(2).enumerate() {
                assert_ne!(sp.table.get(h), before[k], "déplacé");
                assert_eq!(unsafe { sp.bytes(h) }, &content(k as u64 * 2, 20)[..]);
            }
        }
        let r = pool.gc_end_pass(Instant::now());
        assert_eq!(r.still_marked, 0);
        // moitié vide avant : à peu près moitié moins de blocs après
        assert!(pool.stats().held_blocks <= held / 2 + 2, "{} → {}", held, pool.stats().held_blocks);
    }

    #[test]
    fn table_free_list_reuses_and_bounds() {
        let mut t = Handles::new(3).unwrap();
        let a = t.alloc(2).unwrap();
        let b = t.alloc(4).unwrap();
        let c = t.alloc(6).unwrap();
        assert_eq!(t.alloc(8), None, "pleine");
        unsafe {
            t.free(b);
            t.free(a);
        }
        assert_eq!(t.alloc(10), Some(a), "tête de liste");
        assert_eq!(t.alloc(12), Some(b));
        assert_eq!((t.get(a), t.get(b), t.get(c)), (10, 12, 6));
        assert_eq!(t.live(), 3);
    }
}
