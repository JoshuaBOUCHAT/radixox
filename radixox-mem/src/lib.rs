//! `memcpy`, `memmove`, `memset`, `memcmp` et `bcmp` pour un binaire sans libc : le
//! compilateur les appelle quel que soit le code (copies de structures, tranches), et en
//! Rust stable rien d'autre ne les fournit sur une cible `*-linux-gnu`.
//!
//! Comme la glibc : jusqu'à 64 octets, chargements et écritures qui se chevauchent
//! (8, 16 ou 32 octets d'un coup, aucune boucle) ; jusqu'à [`REP_MIN`], blocs de 32 octets
//! et une fin chevauchante ; au-delà, `rep movsb` / `rep stosb` (rapides avec ERMS / FSRM).
//! Comparaisons par mots de 8 octets lus en gros-boutiste, puis par 16 octets (SSE2).
//!
//! `#![no_builtins]` : sans lui, LLVM reconnaîtrait les boucles et les remplacerait par un
//! appel à `memcpy`, c'est-à-dire à elles-mêmes. Pour la même raison, le code n'utilise que
//! des lectures et écritures de taille fixe, jamais `ptr::copy` sur une longueur variable.
//!
//! Noms C seulement avec la feature `export` ; sinon les fonctions servent aux tests et aux
//! mesures contre la libc.

#![no_std]
#![no_builtins]
// `*mut u8` au lieu de `*mut c_void` dans les signatures C : même ABI (des pointeurs).
#![allow(suspicious_runtime_symbol_definitions)]

use core::ptr::{read_unaligned as ld, write_unaligned as st};

/// À partir de cette taille, `rep movsb` / `rep stosb` (x86-64).
pub const REP_MIN: usize = 2048;

/// Bloc de 32 octets : un registre AVX2, ou deux SSE2.
#[derive(Clone, Copy)]
#[repr(C)]
struct B32([u64; 4]);

#[inline(always)]
unsafe fn ld32(p: *const u8) -> B32 {
    unsafe { ld(p as *const B32) }
}

#[inline(always)]
unsafe fn st32(p: *mut u8, v: B32) {
    unsafe { st(p as *mut B32, v) }
}

/// Copie de `n ≤ 64` octets, tous les chargements avant les écritures : sûre même si les
/// zones se chevauchent (sert aussi à `memmove`).
#[inline(always)]
unsafe fn copy_le64(d: *mut u8, s: *const u8, n: usize) {
    unsafe {
        if n >= 32 {
            let (a, b) = (ld32(s), ld32(s.add(n - 32)));
            st32(d, a);
            st32(d.add(n - 32), b);
        } else if n >= 16 {
            let (a, b) = (ld::<u128>(s as _), ld::<u128>(s.add(n - 16) as _));
            st(d as *mut u128, a);
            st(d.add(n - 16) as *mut u128, b);
        } else if n >= 8 {
            let (a, b) = (ld::<u64>(s as _), ld::<u64>(s.add(n - 8) as _));
            st(d as *mut u64, a);
            st(d.add(n - 8) as *mut u64, b);
        } else if n >= 4 {
            let (a, b) = (ld::<u32>(s as _), ld::<u32>(s.add(n - 4) as _));
            st(d as *mut u32, a);
            st(d.add(n - 4) as *mut u32, b);
        } else if n >= 2 {
            let (a, b) = (ld::<u16>(s as _), ld::<u16>(s.add(n - 2) as _));
            st(d as *mut u16, a);
            st(d.add(n - 2) as *mut u16, b);
        } else if n == 1 {
            *d = *s;
        }
    }
}

/// Copie vers l'avant de `n > 64` octets ; correcte aussi si `d < s` (chevauchement) : la
/// fin est lue avant la boucle, qui n'écrit que sous ce qu'elle a déjà lu.
#[inline(always)]
unsafe fn copy_fwd(d: *mut u8, s: *const u8, n: usize) {
    unsafe {
        let tail = ld32(s.add(n - 32));
        let mut i = 0;
        while i + 32 < n {
            st32(d.add(i), ld32(s.add(i)));
            i += 32;
        }
        st32(d.add(n - 32), tail);
    }
}

/// Copie vers l'arrière de `n > 64` octets, pour `d > s` avec chevauchement.
#[inline(always)]
unsafe fn copy_bwd(d: *mut u8, s: *const u8, n: usize) {
    unsafe {
        let head = ld32(s);
        let mut i = n;
        while i > 32 {
            i -= 32;
            st32(d.add(i), ld32(s.add(i)));
        }
        st32(d, head);
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn rep_movsb(d: *mut u8, s: *const u8, n: usize) {
    unsafe {
        core::arch::asm!("rep movsb", inout("rcx") n => _, inout("rdi") d => _, inout("rsi") s => _, options(nostack, preserves_flags));
    }
}

/// `memcpy(3)`.
///
/// # Safety
/// `d` et `s` valides sur `n` octets, sans chevauchement.
#[cfg_attr(feature = "export", unsafe(no_mangle))]
pub unsafe extern "C" fn memcpy(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    unsafe {
        if n <= 64 {
            copy_le64(d, s, n);
        } else {
            #[cfg(target_arch = "x86_64")]
            if n >= REP_MIN {
                rep_movsb(d, s, n);
                return d;
            }
            copy_fwd(d, s, n);
        }
    }
    d
}

/// `memmove(3)`.
///
/// # Safety
/// `d` et `s` valides sur `n` octets (chevauchement permis).
#[cfg_attr(feature = "export", unsafe(no_mangle))]
pub unsafe extern "C" fn memmove(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    unsafe {
        if n <= 64 {
            copy_le64(d, s, n);
        } else if (d as usize).wrapping_sub(s as usize) >= n {
            // `d` avant `s`, ou pas de chevauchement : vers l'avant (`rep movsb` copie octet
            // par octet vers l'avant, correct dans ce cas aussi)
            #[cfg(target_arch = "x86_64")]
            if n >= REP_MIN {
                rep_movsb(d, s, n);
                return d;
            }
            copy_fwd(d, s, n);
        } else {
            copy_bwd(d, s, n);
        }
    }
    d
}

/// `memset(3)`.
///
/// # Safety
/// `d` valide en écriture sur `n` octets.
#[cfg_attr(feature = "export", unsafe(no_mangle))]
pub unsafe extern "C" fn memset(d: *mut u8, c: i32, n: usize) -> *mut u8 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    if n >= 32 {
        use core::arch::x86_64::*;
        unsafe {
            if n >= REP_MIN {
                core::arch::asm!("rep stosb", inout("rcx") n => _, inout("rdi") d => _, in("al") c as u8, options(nostack, preserves_flags));
                return d;
            }
            let v = _mm256_set1_epi8(c as u8 as i8);
            if n <= 64 {
                _mm256_storeu_si256(d as _, v);
                _mm256_storeu_si256(d.add(n - 32) as _, v);
                return d;
            }
            let mut i = 0;
            while i + 64 < n {
                _mm256_storeu_si256(d.add(i) as _, v);
                _mm256_storeu_si256(d.add(i + 32) as _, v);
                i += 64;
            }
            _mm256_storeu_si256(d.add(n - 64) as _, v);
            _mm256_storeu_si256(d.add(n - 32) as _, v);
        }
        return d;
    }
    let b = c as u8;
    let w = u64::from_ne_bytes([b; 8]);
    unsafe {
        if n <= 64 {
            if n >= 32 {
                st32(d, B32([w; 4]));
                st32(d.add(n - 32), B32([w; 4]));
            } else if n >= 16 {
                st(d as *mut u128, (w as u128) << 64 | w as u128);
                st(d.add(n - 16) as *mut u128, (w as u128) << 64 | w as u128);
            } else if n >= 8 {
                st(d as *mut u64, w);
                st(d.add(n - 8) as *mut u64, w);
            } else if n >= 4 {
                st(d as *mut u32, w as u32);
                st(d.add(n - 4) as *mut u32, w as u32);
            } else if n >= 2 {
                st(d as *mut u16, w as u16);
                st(d.add(n - 2) as *mut u16, w as u16);
            } else if n == 1 {
                *d = b;
            }
            return d;
        }
        #[cfg(target_arch = "x86_64")]
        if n >= REP_MIN {
            core::arch::asm!("rep stosb", inout("rcx") n => _, inout("rdi") d => _, in("al") b, options(nostack, preserves_flags));
            return d;
        }
        let mut i = 0;
        while i + 32 < n {
            st32(d.add(i), B32([w; 4]));
            i += 32;
        }
        st32(d.add(n - 32), B32([w; 4]));
    }
    d
}

/// Premier octet différent (en mémoire) de deux mots de 8 octets lus tels quels, `a ≠ b` :
/// `a - b` sur cet octet.
#[inline(always)]
fn word_diff(a: u64, b: u64) -> i32 {
    let x = a ^ b;
    // octet de plus faible adresse : le moins significatif en petit-boutiste
    #[cfg(target_endian = "little")]
    let k = x.trailing_zeros() / 8 * 8;
    #[cfg(target_endian = "big")]
    let k = 56 - x.leading_zeros() / 8 * 8;
    ((a >> k) as u8 as i32) - ((b >> k) as u8 as i32)
}

/// `memcmp(3)` : signe du premier octet différent (non signé), 0 si égaux.
///
/// # Safety
/// `a` et `b` valides en lecture sur `n` octets.
#[cfg_attr(feature = "export", unsafe(no_mangle))]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    unsafe {
        if n < 8 {
            let mut i = 0;
            while i < n {
                let (x, y) = (*a.add(i), *b.add(i));
                if x != y {
                    return x as i32 - y as i32;
                }
                i += 1;
            }
            return 0;
        }
        let mut i = 0;
        #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
        if n >= 16 {
            use core::arch::x86_64::*;
            // premier octet différent à partir de `k`, sachant que le masque d'égalité `m`
            // (bit à 1 = octet égal) n'est pas plein
            let at = |k: usize, m: u32| {
                let k = k + (!m).trailing_zeros() as usize;
                *a.add(k) as i32 - *b.add(k) as i32
            };
            if n <= 32 {
                // deux blocs de 16 qui se chevauchent : le second ne sert que si le premier
                // est égal, ses octets communs avec lui le sont donc aussi
                let eq16 = |k: usize| _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_loadu_si128(a.add(k) as _), _mm_loadu_si128(b.add(k) as _))) as u32 | 0xFFFF_0000;
                let m = eq16(0);
                if m != u32::MAX {
                    return at(0, m);
                }
                let m = eq16(n - 16);
                return if m != u32::MAX { at(n - 16, m) } else { 0 };
            }
            let eq = |k: usize| _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(a.add(k) as _), _mm256_loadu_si256(b.add(k) as _))) as u32;
            // 64 octets par tour, un seul branchement ; on ne localise qu'en cas d'écart
            while i + 64 <= n {
                let (m0, m1) = (eq(i), eq(i + 32));
                if m0 & m1 != u32::MAX {
                    return if m0 != u32::MAX { at(i, m0) } else { at(i + 32, m1) };
                }
                i += 64;
            }
            if i + 32 <= n {
                let m = eq(i);
                if m != u32::MAX {
                    return at(i, m);
                }
                i += 32;
            }
            if i < n {
                // fin chevauchante : les octets déjà vus sont égaux
                let m = eq(n - 32);
                if m != u32::MAX {
                    return at(n - 32, m);
                }
            }
            return 0;
        }
        #[cfg(target_arch = "x86_64")]
        {
            use core::arch::x86_64::*;
            while i + 16 <= n {
                let (x, y) = (_mm_loadu_si128(a.add(i) as _), _mm_loadu_si128(b.add(i) as _));
                let m = _mm_movemask_epi8(_mm_cmpeq_epi8(x, y)) as u32;
                if m != 0xFFFF {
                    let k = i + (!m).trailing_zeros() as usize;
                    return *a.add(k) as i32 - *b.add(k) as i32;
                }
                i += 16;
            }
        }
        while i + 8 <= n {
            let (x, y) = (ld::<u64>(a.add(i) as _), ld::<u64>(b.add(i) as _));
            if x != y {
                return word_diff(x, y);
            }
            i += 8;
        }
        if i < n {
            // fin chevauchante : les octets déjà vus sont égaux
            let (x, y) = (ld::<u64>(a.add(n - 8) as _), ld::<u64>(b.add(n - 8) as _));
            if x != y {
                return word_diff(x, y);
            }
        }
    }
    0
}

/// `strlen(3)` : `core` l'appelle (`CStr::from_ptr`) ; rare (noms du vDSO au démarrage),
/// octet par octet.
///
/// # Safety
/// `s` pointe sur une chaîne terminée par un octet nul.
#[cfg_attr(feature = "export", unsafe(no_mangle))]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut n = 0;
    // SAFETY: lecture jusqu'au nul, qui existe (voir la fonction).
    while unsafe { s.add(n).read_volatile() } != 0 {
        n += 1;
    }
    n
}

/// `bcmp(3)` : 0 si égaux, autre chose sinon (Rust l'appelle pour `==` sur des tranches).
///
/// # Safety
/// `a` et `b` valides en lecture sur `n` octets.
#[cfg_attr(feature = "export", unsafe(no_mangle))]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    unsafe {
        if n <= 16 {
            if n >= 8 {
                let x = ld::<u64>(a as _) ^ ld::<u64>(b as _);
                let y = ld::<u64>(a.add(n - 8) as _) ^ ld::<u64>(b.add(n - 8) as _);
                return ((x | y) != 0) as i32;
            }
            if n >= 4 {
                let x = ld::<u32>(a as _) ^ ld::<u32>(b as _);
                let y = ld::<u32>(a.add(n - 4) as _) ^ ld::<u32>(b.add(n - 4) as _);
                return ((x | y) != 0) as i32;
            }
            if n >= 2 {
                let x = ld::<u16>(a as _) ^ ld::<u16>(b as _);
                let y = ld::<u16>(a.add(n - 2) as _) ^ ld::<u16>(b.add(n - 2) as _);
                return ((x | y) != 0) as i32;
            }
            return (n == 1 && *a != *b) as i32;
        }
        #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
        {
            use core::arch::x86_64::*;
            let x = |k: usize| _mm256_xor_si256(_mm256_loadu_si256(a.add(k) as _), _mm256_loadu_si256(b.add(k) as _));
            if n <= 32 {
                let d = (ld::<u128>(a as _) ^ ld::<u128>(b as _)) | (ld::<u128>(a.add(n - 16) as _) ^ ld::<u128>(b.add(n - 16) as _));
                return (d != 0) as i32;
            }
            if n <= 64 {
                let v = _mm256_or_si256(x(0), x(n - 32));
                return (_mm256_testz_si256(v, v) == 0) as i32;
            }
            let mut i = 0;
            while i + 128 < n {
                let v = _mm256_or_si256(_mm256_or_si256(x(i), x(i + 32)), _mm256_or_si256(x(i + 64), x(i + 96)));
                if _mm256_testz_si256(v, v) == 0 {
                    return 1;
                }
                i += 128;
            }
            while i + 64 < n {
                let v = _mm256_or_si256(x(i), x(i + 32));
                if _mm256_testz_si256(v, v) == 0 {
                    return 1;
                }
                i += 64;
            }
            let v = _mm256_or_si256(x(n - 64), x(n - 32));
            return (_mm256_testz_si256(v, v) == 0) as i32;
        }
        #[allow(unreachable_code)]
        let mut i = 0;
        let mut acc = 0u128;
        while i + 16 <= n {
            acc |= ld::<u128>(a.add(i) as _) ^ ld::<u128>(b.add(i) as _);
            if acc != 0 {
                return 1;
            }
            i += 16;
        }
        acc |= ld::<u128>(a.add(n - 16) as _) ^ ld::<u128>(b.add(n - 16) as _);
        (acc != 0) as i32
    }
}
