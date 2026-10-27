//! Petits conteneurs : Hash et Set d'au plus [`SMALL_MAX`] entrées, rangés
//! dans un seul bloc du pool au lieu d'un arbre valart.
//!
//! ```text
//! bloc : [n u8 | cap u8 | 6 o de pad | entrée × cap]
//! entrée : [SharedByte; W]   W = 2 (champ, valeur) pour un Hash, 1 pour un Set
//! ```
//!
//! Les champs, valeurs et membres restent des `SharedByte` : HGET rend un
//! clone, HSET déplace la valeur reçue, comme avec valart. Capacité par
//! paliers (1, 2, 4, 8) ; au-delà de [`SMALL_MAX`] entrées, le conteneur
//! passe en valart (et n'en revient pas). Ordre des entrées : insertion.
//!
//! Le bloc appartient à la valeur du nœud oxidart : le modifier, c'est
//! modifier le nœud (`value_mut` → barrière de save), et l'image de save en
//! garde une copie profonde ([`deep_clone`]).

use std::mem::ManuallyDrop;
use std::ptr;

use radixox_lib::pool;
use radixox_lib::shared_byte::SharedByte;

/// Au-delà, le conteneur passe en valart.
pub(crate) const SMALL_MAX: usize = 8;
const HEADER: usize = 8;

/// Entrée d'un petit Hash : (champ, valeur).
pub(crate) type Pair = [SharedByte; 2];
/// Entrée d'un petit Set.
pub(crate) type Member = [SharedByte; 1];

#[inline]
fn bytes_for<const W: usize>(cap: usize) -> usize {
    HEADER + cap * W * size_of::<SharedByte>()
}

#[inline]
unsafe fn slots<const W: usize>(p: *mut u8) -> *mut [SharedByte; W] {
    unsafe { p.add(HEADER) as *mut [SharedByte; W] }
}

/// Nombre d'entrées.
#[inline]
pub(crate) unsafe fn len(p: *const u8) -> usize {
    unsafe { *p as usize }
}

#[inline]
unsafe fn cap(p: *const u8) -> usize {
    unsafe { *p.add(1) as usize }
}

/// Taille du bloc vivant `p` (multiple de 8).
///
/// # Safety
/// `p` est un bloc vivant.
#[inline]
pub(crate) unsafe fn block_bytes<const W: usize>(p: *const u8) -> usize {
    bytes_for::<W>(unsafe { cap(p) })
}

/// # Safety
/// `p` est un bloc vivant, possédé, dont les entrées sont déjà sorties ou libérées.
#[inline]
unsafe fn free_block<const W: usize>(p: *mut u8) {
    unsafe { pool::struct_free(p, block_bytes::<W>(p)) }
}

/// Bloc vide de capacité `cap` (≤ [`SMALL_MAX`]).
pub(crate) fn alloc<const W: usize>(cap: usize) -> *mut u8 {
    debug_assert!((1..=SMALL_MAX).contains(&cap));
    unsafe {
        // Pool des structures, aligné sur 8 (MEMORY_ARCH.md §17.4) ; taille multiple de 8.
        let p = pool::struct_alloc(bytes_for::<W>(cap));
        assert!(!p.is_null(), "petit conteneur : allocation");
        *p = 0;
        *p.add(1) = cap as u8;
        p
    }
}

/// Bloc rempli avec `entries` (au plus [`SMALL_MAX`]).
pub(crate) fn from_entries<const W: usize>(entries: impl ExactSizeIterator<Item = [SharedByte; W]>) -> *mut u8 {
    let n = entries.len();
    debug_assert!(n <= SMALL_MAX);
    let p = alloc::<W>(n.max(1).next_power_of_two());
    unsafe {
        for (i, e) in entries.enumerate() {
            slots::<W>(p).add(i).write(e);
        }
        *p = n as u8;
    }
    p
}

/// Entrées, dans l'ordre d'insertion.
///
/// # Safety
/// `p` est un bloc vivant ; la tranche est invalidée par toute modification.
#[inline]
pub(crate) unsafe fn entries<'a, const W: usize>(p: *const u8) -> &'a [[SharedByte; W]] {
    unsafe { std::slice::from_raw_parts(slots::<W>(p as *mut u8), len(p)) }
}

/// Entrées en écriture.
///
/// # Safety
/// `p` est un bloc vivant, possédé ; aucune autre référence vers lui.
#[inline]
pub(crate) unsafe fn entries_mut<'a, const W: usize>(p: *mut u8) -> &'a mut [[SharedByte; W]] {
    unsafe { std::slice::from_raw_parts_mut(slots::<W>(p), len(p)) }
}

/// Position de l'entrée dont le premier élément vaut `key`.
#[inline]
pub(crate) unsafe fn find<const W: usize>(p: *const u8, key: &[u8]) -> Option<usize> {
    unsafe { entries::<W>(p).iter().position(|e| e[0].as_slice() == key) }
}

/// Ajoute une entrée. `false` (entrée rendue) si le bloc a déjà
/// [`SMALL_MAX`] entrées : à l'appelant de passer en valart.
///
/// # Safety
/// `*p` est un bloc vivant, possédé ; il peut être réalloué.
pub(crate) unsafe fn push<const W: usize>(p: &mut *mut u8, e: [SharedByte; W]) -> Result<(), [SharedByte; W]> {
    unsafe {
        let n = len(*p);
        if n == SMALL_MAX {
            return Err(e);
        }
        if n == cap(*p) {
            let q = alloc::<W>(n * 2);
            ptr::copy_nonoverlapping(slots::<W>(*p), slots::<W>(q), n);
            *q = n as u8;
            free_block::<W>(*p);
            *p = q;
        }
        slots::<W>(*p).add(n).write(e);
        **p = (n + 1) as u8;
        Ok(())
    }
}

/// Retire l'entrée `i` (l'ordre des suivantes est gardé).
///
/// # Safety
/// `p` est un bloc vivant, possédé ; `i < len(p)`.
pub(crate) unsafe fn remove_at<const W: usize>(p: *mut u8, i: usize) -> [SharedByte; W] {
    unsafe {
        let n = len(p);
        debug_assert!(i < n);
        let s = slots::<W>(p);
        let e = s.add(i).read();
        ptr::copy(s.add(i + 1), s.add(i), n - i - 1);
        *p = (n - 1) as u8;
        e
    }
}

/// Vide le bloc sans libérer ses entrées (propriété transférée à l'appelant)
/// puis le libère.
///
/// # Safety
/// `p` est un bloc vivant, possédé ; il ne doit plus être utilisé.
pub(crate) unsafe fn into_entries<const W: usize>(p: *mut u8) -> Vec<[SharedByte; W]> {
    unsafe {
        let n = len(p);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(slots::<W>(p).add(i).read());
        }
        free_block::<W>(p);
        out
    }
}

/// Libère le bloc et ses entrées.
///
/// # Safety
/// `p` est un bloc vivant, possédé ; il ne doit plus être utilisé.
pub(crate) unsafe fn free<const W: usize>(p: *mut u8) {
    unsafe { drop(into_entries::<W>(p)) }
}

/// Copie profonde (entrées à refcount +1), pour une image de save.
///
/// # Safety
/// `p` est un bloc vivant.
pub(crate) unsafe fn deep_clone<const W: usize>(p: *const u8) -> *mut u8 {
    unsafe { from_entries::<W>(entries::<W>(p).iter().cloned()) }
}

/// `SharedByte` lue sans refcount dans une entrée (copie bit à bit, à ne pas
/// dropper).
#[inline]
pub(crate) unsafe fn peek(sb: &SharedByte) -> ManuallyDrop<SharedByte> {
    unsafe { ManuallyDrop::new(ptr::read(sb)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb(s: &str) -> SharedByte {
        SharedByte::from_str(s)
    }

    #[test]
    fn grow_remove_and_drop_keep_refcounts() {
        let pool: Vec<SharedByte> = (0..10).map(|i| sb(&format!("v{i}"))).collect();
        let mut p = alloc::<2>(1);
        for (i, v) in pool.iter().enumerate().take(SMALL_MAX) {
            assert!(unsafe { push::<2>(&mut p, [sb(&format!("f{i}")), v.clone()]) }.is_ok());
        }
        assert!(unsafe { push::<2>(&mut p, [sb("trop"), pool[9].clone()]) }.is_err());
        assert_eq!(unsafe { len(p) }, SMALL_MAX);
        assert_eq!(unsafe { find::<2>(p, b"f3") }, Some(3));
        let [f, v] = unsafe { remove_at::<2>(p, 3) };
        assert_eq!((f.as_slice(), v.as_slice()), (&b"f3"[..], &b"v3"[..]));
        assert_eq!(unsafe { find::<2>(p, b"f4") }, Some(3));
        let copy = unsafe { deep_clone::<2>(p) };
        assert_eq!(pool[0].rc(), 3);
        unsafe { free::<2>(p) };
        unsafe { free::<2>(copy) };
        drop((f, v));
        assert!(pool.iter().all(|b| b.rc() == 1));
    }
}
