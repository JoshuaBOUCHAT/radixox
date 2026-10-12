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
