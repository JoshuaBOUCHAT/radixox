//! Allocation brute des blocs `SharedByte`, `CompactPath` et overflows valart.
//!
//! mimalloc en direct (`mi_malloc` et pas `MiMalloc.alloc`, qui passe par
//! `mi_malloc_aligned` et sur-alloue). Avec `--cfg radixox_sysalloc`
//! (RUSTFLAGS), malloc/free de la libc à la place, pour qu'AddressSanitizer
//! voie ces blocs :
//!
//! ```text
//! RUSTFLAGS="-Zsanitizer=address --cfg radixox_sysalloc" cargo +nightly test ...
//! ```

#[cfg(not(radixox_sysalloc))]
mod imp {
    #[inline]
    pub unsafe fn malloc(size: usize) -> *mut u8 {
        unsafe { libmimalloc_sys::mi_malloc(size) as *mut u8 }
    }
    #[inline]
    pub unsafe fn zalloc(size: usize) -> *mut u8 {
        unsafe { libmimalloc_sys::mi_zalloc(size) as *mut u8 }
    }
    #[inline]
    pub unsafe fn free(ptr: *mut u8) {
        unsafe { libmimalloc_sys::mi_free(ptr as *mut _) }
    }
}

#[cfg(radixox_sysalloc)]
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

/// Objets externes des `SharedByte` (MEMORY_ARCH.md §16.7) : valeurs de plus de
/// 16 Kio, et celles qui vivent hors de l'arbre (§16.9). jemalloc **préfixé** : il ne
/// remplace pas le `malloc` du processus et ne sert qu'à ces objets. Son thread de fond
/// rend la mémoire au système sans toucher à aucune donnée de RadixOx. Sous
/// `--cfg radixox_sysalloc`, malloc/free de la libc comme le reste.
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

    /// Le thread de fond de jemalloc tourne (relu par `mallctl`, pas supposé).
    pub fn background_thread() -> bool {
        imp::background_thread()
    }

    #[cfg(not(radixox_sysalloc))]
    mod imp {
        use std::ffi::{c_char, c_void};
        use tikv_jemalloc_sys as je;

        #[repr(transparent)]
        struct Conf(*const c_char);
        unsafe impl Sync for Conf {}

        /// Lu par jemalloc à son initialisation (avant la première allocation externe).
        /// Nom préfixé : `_rjem_malloc_conf` dans tikv-jemalloc-sys 0.6.1.
        #[used]
        #[unsafe(export_name = "_rjem_malloc_conf")]
        static MALLOC_CONF: Conf =
            Conf(c"background_thread:true,dirty_decay_ms:10000,muzzy_decay_ms:0".as_ptr());

        #[inline]
        pub unsafe fn malloc(size: usize) -> *mut u8 {
            unsafe { je::malloc(size) as *mut u8 }
        }
        #[inline]
        pub unsafe fn free(ptr: *mut u8, size: usize) {
            unsafe { je::sdallocx(ptr as *mut c_void, size, 0) }
        }
        pub fn background_thread() -> bool {
            let mut on = false;
            let mut len = size_of::<bool>();
            let r = unsafe {
                je::mallctl(
                    c"background_thread".as_ptr(),
                    &mut on as *mut bool as *mut c_void,
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            r == 0 && on
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
        pub fn background_thread() -> bool {
            false
        }
    }
}

/// Allocateur par défaut du processus (glibc) compté, sous `--cfg radixox_heapcount` :
/// mesure de ce qui échappe encore au pool, aux slabs et à `external` (MEMORY_ARCH.md
/// §17.6). Le binaire le déclare `#[global_allocator]` ; sans le cfg, rien n'est compté.
/// Atomiques : tous les threads du processus passent par là.
pub mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static BYTES: AtomicU64 = AtomicU64::new(0);
    static PEAK: AtomicU64 = AtomicU64::new(0);

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        #[inline]
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(l) };
            if !p.is_null() {
                let now = BYTES.fetch_add(l.size() as u64, Relaxed) + l.size() as u64;
                PEAK.fetch_max(now, Relaxed);
            }
            p
        }
        #[inline]
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            BYTES.fetch_sub(l.size() as u64, Relaxed);
            unsafe { System.dealloc(p, l) }
        }
        #[inline]
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let q = unsafe { System.realloc(p, l, new) };
            if !q.is_null() {
                BYTES.fetch_sub(l.size() as u64, Relaxed);
                let now = BYTES.fetch_add(new as u64, Relaxed) + new as u64;
                PEAK.fetch_max(now, Relaxed);
            }
            q
        }
    }

    /// Octets demandés vivants (sans l'en-tête ni l'arrondi de glibc).
    pub fn bytes() -> u64 {
        BYTES.load(Relaxed)
    }

    pub fn peak() -> u64 {
        PEAK.load(Relaxed)
    }
}
