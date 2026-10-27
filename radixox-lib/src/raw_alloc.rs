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

/// Allocateur global du binaire (MEMORY_ARCH.md §18.12) : le tas de [`crate::large`]
/// (slabs par classe sous 16 Kio, granules de 4 Kio au-delà), compté (`INFO` :
/// `mem_heap_bytes`). La libc sert ce que le tas ne peut pas :
/// - un autre thread que le premier à allouer (le tas est mono-thread) ;
/// - un alignement au-delà de 4 Kio, une plage pleine ou refusée ;
/// - les allocations faites depuis le tas lui-même (ses tables, son initialisation).
///
/// Un bloc se rend là d'où il vient, reconnu à son adresse ([`crate::large::owns`]).
/// Un autre thread qui rendrait un bloc du tas arrête le processus : jamais le cas
/// (ARCHITECTURE.md §1, un seul thread métier).
pub mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

    use crate::large;

    // Thread du tas seulement : sans atomique.
    static mut BYTES: u64 = 0;
    static mut PEAK: u64 = 0;
    /// Repli sur la libc : tout thread peut y allouer.
    static LIBC: AtomicU64 = AtomicU64::new(0);
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
        let msg = b"radixox: bloc du tas rendu par un autre thread\n";
        unsafe {
            libc::write(2, msg.as_ptr() as *const _, msg.len());
            libc::abort()
        }
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
            let p = unsafe { System.alloc(l) };
            if !p.is_null() {
                LIBC.fetch_add(l.size() as u64, Relaxed);
            }
            p
        }

        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            if !large::owns(p as usize) {
                LIBC.fetch_sub(l.size() as u64, Relaxed);
                return unsafe { System.dealloc(p, l) };
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

    /// Octets demandés vivants dans le tas (sans l'arrondi de classe ; la part servie par la
    /// libc n'est pas comptée).
    pub fn bytes() -> u64 {
        unsafe { BYTES }
    }

    pub fn peak() -> u64 {
        unsafe { PEAK }
    }

    /// Octets vivants repliés sur la libc (réentrance du tas, autre thread, plage pleine).
    pub fn libc_bytes() -> u64 {
        LIBC.load(Relaxed)
    }
}
