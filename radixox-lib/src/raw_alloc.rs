//! Allocation brute des blocs `SharedByte`, `CompactPath` et overflows valart quand le
//! pool est absent (`--cfg radixox_nopool`) ou sous `--cfg radixox_sysalloc` : malloc/free
//! de la libc (AddressSanitizer voit ces blocs) :
//!
//! ```text
//! RUSTFLAGS="-Zsanitizer=address --cfg radixox_sysalloc" cargo +nightly test ...
//! ```

mod imp {
    unsafe extern "C" {
        #[link_name = "malloc"]
        fn c_malloc(size: usize) -> *mut u8;
        #[link_name = "calloc"]
        fn c_calloc(n: usize, size: usize) -> *mut u8;
        #[link_name = "free"]
        fn c_free(ptr: *mut u8);
    }
    #[inline]
    pub unsafe fn malloc(size: usize) -> *mut u8 {
        unsafe { c_malloc(size) }
    }
    #[inline]
    pub unsafe fn zalloc(size: usize) -> *mut u8 {
        unsafe { c_calloc(1, size) }
    }
    #[inline]
    pub unsafe fn free(ptr: *mut u8) {
        unsafe { c_free(ptr) }
    }
}

/// Bloc de `size` octets, aligné sur 8 au moins.
///
/// # Safety
/// Libérer avec [`free`] uniquement.
#[inline]
pub unsafe fn malloc(size: usize) -> *mut u8 {
    unsafe { imp::malloc(size) }
}

/// Comme [`malloc`], mis à zéro.
///
/// # Safety
/// Libérer avec [`free`] uniquement.
#[inline]
pub unsafe fn zalloc(size: usize) -> *mut u8 {
    unsafe { imp::zalloc(size) }
}

/// # Safety
/// `ptr` vient de [`malloc`] ou [`zalloc`] et n'a pas déjà été libéré.
#[inline]
pub unsafe fn free(ptr: *mut u8) {
    unsafe { imp::free(ptr) }
}

/// Objets externes (MEMORY_ARCH.md §18) : valeurs de plus de 16 Kio, et objets qui vivent
/// hors de l'arbre (§16.9, épinglés : le GC ne les voit pas). Allocateur de RadixOx
/// (`crate::large` : tas de granules de 4 Kio, slabs par classe), rendu au système par
/// `release_step` sans thread de fond. Sous `--cfg radixox_sysalloc`, malloc/free de la
/// libc comme le reste.
pub mod external {
    // Mono-thread (ARCHITECTURE.md §1) : comptes sans atomique.
    static mut BYTES: u64 = 0;
    static mut OBJECTS: u64 = 0;

    /// # Safety
    /// Libérer avec [`free`] et la même taille.
    #[inline]
    pub unsafe fn malloc(size: usize) -> *mut u8 {
        unsafe {
            BYTES += size as u64;
            OBJECTS += 1;
            imp::malloc(size)
        }
    }

    /// # Safety
    /// `ptr` vient de [`malloc`] avec cette `size` et n'a pas déjà été libéré.
    #[inline]
    pub unsafe fn free(ptr: *mut u8, size: usize) {
        unsafe {
            BYTES -= size as u64;
            OBJECTS -= 1;
            imp::free(ptr, size)
        }
    }

    /// Octets demandés vivants (en-têtes compris).
    pub fn bytes() -> u64 {
        unsafe { BYTES }
    }

    pub fn objects() -> u64 {
        unsafe { OBJECTS }
    }

    #[cfg(not(radixox_sysalloc))]
    mod imp {
        #[inline]
        pub unsafe fn malloc(size: usize) -> *mut u8 {
            unsafe { crate::large::malloc(size) }
        }
        #[inline]
        pub unsafe fn free(ptr: *mut u8, size: usize) {
            unsafe { crate::large::free(ptr, size) }
        }
    }

    #[cfg(radixox_sysalloc)]
    mod imp {
        #[inline]
        pub unsafe fn malloc(size: usize) -> *mut u8 {
            unsafe { super::super::imp::malloc(size) }
        }
        #[inline]
        pub unsafe fn free(ptr: *mut u8, _size: usize) {
            unsafe { super::super::imp::free(ptr) }
        }
    }
}

/// Mappings directs (MEMORY_ARCH.md §18.13) : ce que le tas ne sert pas — un autre thread
/// que le sien, un alignement au-delà de 4 Kio, une plage pleine ou refusée. Un `mmap` par
/// bloc, en-tête dans la même page, de tout thread. Ne passe jamais par la libc : le
/// binaire exporte `malloc` ([`heap::c`]), la libc retomberait sur nous.
pub mod direct {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    const PAGE: usize = 4096;
    const MAGIC: usize = 0x5258_4449_5245_4354; // "RXDIRECT"
    /// En-tête juste avant les données : `[MAGIC, base, len]`.
    const HEADER: usize = 3 * size_of::<usize>();

    static BYTES: AtomicU64 = AtomicU64::new(0);

    /// Message sur stderr sans allouer.
    pub fn message(msg: &[u8]) {
        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()) };
    }

    #[cold]
    fn bad_free() -> ! {
        message(b"radixox: free d'un pointeur inconnu\n");
        unsafe { libc::abort() }
    }

    /// Bloc de `size` octets aligné sur `align` (puissance de deux) ; nul si `mmap` refuse.
    pub fn alloc(size: usize, align: usize) -> *mut u8 {
        let align = align.max(16);
        let Some(len) = size.checked_add(align + HEADER).map(|n| n.next_multiple_of(PAGE)) else {
            return std::ptr::null_mut();
        };
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return std::ptr::null_mut();
        }
        let base = base as usize;
        let data = (base + HEADER).next_multiple_of(align);
        unsafe { ((data - HEADER) as *mut [usize; 3]).write([MAGIC, base, len]) };
        BYTES.fetch_add(len as u64, Relaxed);
        data as *mut u8
    }

    /// `(base, len)` du mapping de `p`.
    unsafe fn header(p: *mut u8) -> (usize, usize) {
        let [magic, base, len] = unsafe { ((p as usize - HEADER) as *const [usize; 3]).read() };
        if magic != MAGIC || base > p as usize || p as usize - base >= len {
            bad_free();
        }
        (base, len)
    }

    /// # Safety
    /// `p` vient de [`alloc`], pas encore rendu.
    pub unsafe fn free(p: *mut u8) {
        let (base, len) = unsafe { header(p) };
        unsafe { libc::munmap(base as *mut _, len) };
        BYTES.fetch_sub(len as u64, Relaxed);
    }

    /// # Safety
    /// `p` vient de [`alloc`], pas encore rendu.
    pub unsafe fn usable(p: *mut u8) -> usize {
        let (base, len) = unsafe { header(p) };
        base + len - p as usize
    }

    /// Octets mappés (pages entières, en-têtes compris).
    pub fn bytes() -> u64 {
        BYTES.load(Relaxed)
    }
}

/// Allocateur global du binaire (MEMORY_ARCH.md §18.12, §18.13) : le tas de
/// [`crate::large`] (slabs par classe sous 16 Kio, granules de 4 Kio au-delà), compté
/// (`INFO` : `mem_heap_bytes`). Ce que le tas ne peut pas servir part en [`direct`] :
/// - un autre thread que le premier à allouer (le tas est mono-thread) ;
/// - un alignement au-delà de 4 Kio, une plage pleine ou refusée.
///
/// Plus rien ne va à la libc : [`c`] sert `malloc` & co., exportés par le binaire, donc
/// aussi la libc elle-même et le C lié (zstd). Un bloc se rend là d'où il vient, reconnu à
/// son adresse ([`crate::large::owns`]). Un autre thread qui rendrait un bloc du tas arrête
/// le processus : jamais le cas (ARCHITECTURE.md §1, un seul thread métier).
pub mod heap {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    use super::direct;
    use crate::large;

    // Thread du tas seulement : sans atomique.
    static mut BYTES: u64 = 0;
    static mut PEAK: u64 = 0;
    /// Thread du tas : adresse de son [`MARK`] (0 : personne n'a encore alloué).
    static OWNER: AtomicUsize = AtomicUsize::new(0);
    thread_local! { static MARK: u8 = const { 0 }; }

    pub struct RxAlloc;

    /// Vrai sur le thread du tas (le premier à allouer).
    #[inline]
    fn on_owner() -> bool {
        let me = MARK.with(|m| m as *const u8 as usize);
        let o = OWNER.load(Relaxed);
        o == me || (o == 0 && OWNER.compare_exchange(0, me, Relaxed, Relaxed).is_ok())
    }

    #[cold]
    fn foreign_free() -> ! {
        direct::message(b"radixox: bloc du tas rendu par un autre thread\n");
        unsafe { libc::abort() }
    }

    #[inline]
    fn count(add: usize, sub: usize) {
        unsafe {
            BYTES = BYTES + add as u64 - sub as u64;
            PEAK = PEAK.max(BYTES);
        }
        if add >= 4096 {
            trace(add);
        }
    }

    /// `RADIXOX_HEAP_TRACE=octets` : pile d'appel de chaque allocation d'au moins cette
    /// taille sur stderr (binaire avec symboles : `CARGO_PROFILE_RELEASE_STRIP=none`,
    /// `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`). Lu une fois, sous la garde.
    #[cold]
    fn trace(size: usize) {
        use std::cell::Cell;
        thread_local! { static IN: Cell<bool> = const { Cell::new(false) }; }
        // usize::MAX : pas encore lu ; 0 : éteint
        static MIN: AtomicUsize = AtomicUsize::new(usize::MAX);
        if IN.with(Cell::get) {
            return;
        }
        IN.with(|g| g.set(true));
        let mut min = MIN.load(Relaxed);
        if min == usize::MAX {
            min = std::env::var("RADIXOX_HEAP_TRACE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            MIN.store(min, Relaxed);
        }
        if min != 0 && size >= min {
            eprintln!("[heap] {size} o\n{}", std::backtrace::Backtrace::force_capture());
        }
        IN.with(|g| g.set(false));
    }

    unsafe impl GlobalAlloc for RxAlloc {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if on_owner() {
                let p = large::global_alloc(l.size(), l.align());
                if !p.is_null() {
                    count(l.size(), 0);
                    return p;
                }
            }
            direct::alloc(l.size(), l.align())
        }

        /// Pages neuves déjà à zéro : pas d'écriture (voir [`large::global_alloc_zeroed`]) ;
        /// un mapping direct est neuf.
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            if on_owner() {
                let p = large::global_alloc_zeroed(l.size(), l.align());
                if !p.is_null() {
                    count(l.size(), 0);
                    return p;
                }
            }
            direct::alloc(l.size(), l.align())
        }

        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            if !large::owns(p as usize) {
                return unsafe { direct::free(p) };
            }
            if !on_owner() {
                foreign_free();
            }
            unsafe { large::global_free(p, l.size(), l.align()) };
            count(0, l.size());
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            if large::owns(p as usize) && large::global_fits(l.size(), new, l.align()) {
                if !on_owner() {
                    foreign_free();
                }
                count(new, l.size());
                return p;
            }
            // SAFETY: contrat de `realloc` (taille non nulle, arrondie sans débordement).
            let nl = unsafe { Layout::from_size_align_unchecked(new, l.align()) };
            let q = unsafe { self.alloc(nl) };
            if !q.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                    self.dealloc(p, l);
                }
            }
            q
        }
    }

    /// `malloc` & co. du C, sur le même tas (MEMORY_ARCH.md §18.13). Le binaire les exporte
    /// sous leurs noms (`src/bin/uringloop.rs`) : la libc, zstd et std passent par ici. Sans
    /// taille à la libération : la route se retrouve depuis l'adresse. Comptés en octets
    /// utilisables (taille de classe), des deux côtés.
    pub mod c {
        use super::*;

        /// Alignement de `max_align_t`.
        const ALIGN: usize = 16;

        fn enomem() -> *mut u8 {
            unsafe { *libc::__errno_location() = libc::ENOMEM };
            std::ptr::null_mut()
        }

        fn alloc(size: usize, align: usize) -> *mut u8 {
            alloc_z(size, align, false)
        }

        /// `zero` : à zéro, sans écrire les pages neuves (suite propre, mapping direct).
        fn alloc_z(size: usize, align: usize, zero: bool) -> *mut u8 {
            let size = size.max(1);
            if on_owner() {
                let p = if zero { large::global_alloc_zeroed(size, align) } else { large::global_alloc(size, align) };
                if !p.is_null() {
                    count(large::usable_size(p), 0);
                    return p;
                }
            }
            let p = direct::alloc(size, align);
            if p.is_null() { enomem() } else { p }
        }

        pub fn usable_size(p: *mut u8) -> usize {
            if p.is_null() {
                0
            } else if large::owns(p as usize) {
                large::usable_size(p)
            } else {
                unsafe { direct::usable(p) }
            }
        }

        pub fn malloc(size: usize) -> *mut u8 {
            alloc(size, ALIGN)
        }

        /// # Safety
        /// `p` nul ou servi par ce module, pas encore rendu.
        pub unsafe fn free(p: *mut u8) {
            if p.is_null() {
                return;
            }
            if !large::owns(p as usize) {
                return unsafe { direct::free(p) };
            }
            if !on_owner() {
                foreign_free();
            }
            let u = large::usable_size(p);
            unsafe { large::free_ptr(p) };
            count(0, u);
        }

        pub fn calloc(n: usize, size: usize) -> *mut u8 {
            let Some(total) = n.checked_mul(size) else { return enomem() };
            alloc_z(total, ALIGN, true)
        }

        /// # Safety
        /// `p` nul ou servi par ce module, pas encore rendu.
        pub unsafe fn realloc(p: *mut u8, new: usize) -> *mut u8 {
            if p.is_null() {
                return malloc(new);
            }
            if new == 0 {
                unsafe { free(p) };
                return std::ptr::null_mut();
            }
            if large::owns(p as usize) && on_owner() && large::ptr_fits(p, new, ALIGN) {
                return p;
            }
            let old = usable_size(p);
            let q = malloc(new);
            if !q.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(p, q, old.min(new));
                    free(p);
                }
            }
            q
        }

        /// # Safety
        /// Comme [`realloc`].
        pub unsafe fn reallocarray(p: *mut u8, n: usize, size: usize) -> *mut u8 {
            match n.checked_mul(size) {
                Some(total) => unsafe { realloc(p, total) },
                None => enomem(),
            }
        }

        /// Nul (`EINVAL`) si `align` n'est pas une puissance de deux.
        pub fn aligned_alloc(align: usize, size: usize) -> *mut u8 {
            if !align.is_power_of_two() {
                unsafe { *libc::__errno_location() = libc::EINVAL };
                return std::ptr::null_mut();
            }
            alloc(size, align.max(ALIGN))
        }

        /// # Safety
        /// `out` valide en écriture.
        pub unsafe fn posix_memalign(out: *mut *mut u8, align: usize, size: usize) -> i32 {
            if !align.is_power_of_two() || align % size_of::<usize>() != 0 {
                return libc::EINVAL;
            }
            let p = alloc(size, align.max(ALIGN));
            if p.is_null() {
                return libc::ENOMEM;
            }
            unsafe { *out = p };
            0
        }

        pub fn valloc(size: usize) -> *mut u8 {
            alloc(size, 4096)
        }

        pub fn pvalloc(size: usize) -> *mut u8 {
            alloc(size.next_multiple_of(4096), 4096)
        }
    }

    /// Octets demandés vivants dans le tas (sans l'arrondi de classe pour les blocs de
    /// Rust ; taille de classe pour ceux du C).
    pub fn bytes() -> u64 {
        unsafe { BYTES }
    }

    pub fn peak() -> u64 {
        unsafe { PEAK }
    }

    /// Octets en mappings directs ([`direct`]) : autre thread, alignement, plage pleine.
    pub fn direct_bytes() -> u64 {
        direct::bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::heap::{self, c};

    /// L'API C de bout en bout (un seul test : le tas appartient au premier thread qui y
    /// alloue, et chaque test a le sien).
    #[test]
    fn c_api_roundtrip() {
        let (b0, d0) = (heap::bytes(), heap::direct_bytes());
        // Tailles des slabs, du tas de granules, et au-delà.
        let sizes = [1usize, 7, 16, 24, 100, 4095, 16 * 1024, 100_000, 3 << 20];
        let ps: Vec<*mut u8> = sizes.iter().map(|&n| c::malloc(n)).collect();
        for (&p, &n) in ps.iter().zip(&sizes) {
            assert!(!p.is_null() && p as usize % 16 == 0, "{n} o : {p:?}");
            assert!(c::usable_size(p) >= n);
            unsafe { std::ptr::write_bytes(p, 0xAB, n) };
        }
        assert!(heap::bytes() > b0);
        // Mémoire sale réutilisée : calloc la remet à zéro quand même.
        for &p in &ps {
            unsafe { c::free(p) };
        }
        for &n in &sizes {
            let p = c::calloc(1, n);
            assert!(unsafe { std::slice::from_raw_parts(p, n) }.iter().all(|&b| b == 0), "calloc {n} o");
            unsafe { c::free(p) };
        }
        assert!(c::calloc(usize::MAX, 2).is_null(), "débordement");
        // realloc : contenu gardé en grandissant et en rétrécissant, sur place si même route.
        let mut p = c::malloc(10);
        unsafe { std::ptr::copy_nonoverlapping(b"0123456789".as_ptr(), p, 10) };
        for n in [12, 200, 50_000, 2 << 20, 30, 5] {
            p = unsafe { c::realloc(p, n) };
            let keep = n.min(10);
            assert_eq!(unsafe { std::slice::from_raw_parts(p, keep) }, &b"0123456789"[..keep]);
        }
        let q = unsafe { c::realloc(p, 6) };
        assert_eq!(q, p, "même classe : sur place");
        assert!(unsafe { c::realloc(q, 0) }.is_null());
        // Alignements : jusqu'à 4 Kio dans le tas, au-delà en mapping direct.
        for align in [8usize, 16, 64, 4096, 8192, 1 << 20] {
            let mut out = std::ptr::null_mut();
            assert_eq!(unsafe { c::posix_memalign(&mut out, align, 3000) }, 0);
            assert_eq!(out as usize % align, 0, "align {align}");
            assert_eq!(crate::large::owns(out as usize), align <= 4096, "align {align}");
            assert!(c::usable_size(out) >= 3000);
            unsafe { c::free(out) };
        }
        let mut out = std::ptr::null_mut();
        assert_eq!(unsafe { c::posix_memalign(&mut out, 24, 8) }, libc::EINVAL);
        assert!(c::aligned_alloc(3, 8).is_null());
        let p = c::pvalloc(10);
        assert!(p as usize % 4096 == 0 && c::usable_size(p) >= 4096);
        unsafe { c::free(p) };
        // Un autre thread : mappings directs, rendus de n'importe où.
        let far = std::thread::spawn(|| c::malloc(1000) as usize).join().unwrap() as *mut u8;
        assert!(!crate::large::owns(far as usize));
        assert!(heap::direct_bytes() > d0);
        unsafe { c::free(far) };
        assert_eq!((heap::bytes(), heap::direct_bytes()), (b0, d0), "tout rendu");
    }
}
