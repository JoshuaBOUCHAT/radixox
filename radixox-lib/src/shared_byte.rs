use crate::raw_alloc::{external, free as mi_free, malloc as mi_malloc};
use std::borrow::Borrow;

use std::ops::Deref;
use std::ptr::NonNull;

use crate::prefetch::PreFetch;

/// `ptr` pointe sur les **données** ; l'en-tête est lu à reculons, en `u16` alignés
/// (l'adresse des données est toujours paire). MEMORY_ARCH.md §16.2 :
///
/// ```text
/// forme courte : [len_lo u16][rc u16][données…]              len ≤ POOL_MAX_LEN
/// forme longue : [len_hi u16][len_lo u16][rc u16][données…]  bit 15 de len_lo = EXTERNE
/// ```
///
/// La forme longue est allouée par jemalloc ([`external`]) ; la courte par le pool
/// ([`crate::pool`]) sous `--cfg radixox_pool`, sinon par mimalloc.
#[repr(transparent)]
pub struct SharedByte {
    ptr: NonNull<u8>,
}

impl SharedByte {
    const SHORT_HEADER: usize = 4;
    const LONG_HEADER: usize = 6;
    const EXTERNAL: u16 = 0x8000;
    /// G : au-delà, toute valeur est externe.
    pub const POOL_MAX_LEN: usize = 16 * 1024;
    /// 2³¹ − 1 : `len_hi` porte 16 bits, `len_lo` 15.
    pub const MAX_LEN: usize = (1 << 31) - 1;

    #[inline]
    fn rc_ptr(&self) -> *mut u16 {
        unsafe { (self.ptr.as_ptr() as *mut u16).sub(1) }
    }

    #[inline]
    fn len_lo(&self) -> u16 {
        unsafe { (self.ptr.as_ptr() as *const u16).sub(2).read() }
    }

    #[inline]
    fn data_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Alloue un objet de `len` octets, en-tête écrit et `rc == 1`, données non
    /// initialisées. `external` force la forme longue.
    ///
    /// # Safety
    /// L'appelant écrit les `len` octets de données avant toute lecture.
    unsafe fn alloc(len: usize, external: bool) -> Self {
        assert!(len <= Self::MAX_LEN);
        unsafe {
            let short = if external || len > Self::POOL_MAX_LEN {
                std::ptr::null_mut()
            } else {
                short_alloc(Self::slot(len))
            };
            let data = if !short.is_null() {
                (short as *mut u16).write(len as u16);
                short.add(Self::SHORT_HEADER)
            } else {
                // Forme longue : demandée, trop grande, ou pool plein (repli).
                let base = external::malloc(Self::LONG_HEADER + len);
                assert!(!base.is_null());
                (base as *mut u16).write((len >> 15) as u16);
                (base as *mut u16).add(1).write(Self::EXTERNAL | (len as u16 & 0x7FFF));
                base.add(Self::LONG_HEADER)
            };
            (data as *mut u16).sub(1).write(1);
            Self { ptr: NonNull::new_unchecked(data) }
        }
    }

    /// Emplacement d'un objet en forme courte : en-tête + données, arrondi au granule (2 o).
    #[inline]
    fn slot(len: usize) -> usize {
        (Self::SHORT_HEADER + len + 1) & !1
    }

    fn copy_of(data: &[u8], external: bool) -> Self {
        unsafe {
            let sb = Self::alloc(data.len(), external);
            std::ptr::copy_nonoverlapping(data.as_ptr(), sb.ptr.as_ptr(), data.len());
            sb
        }
    }

    pub fn from_byte(data: impl Deref<Target = [u8]>) -> Self {
        Self::copy_of(&data, false)
    }
    #[inline]
    pub fn from_slice(data: impl AsRef<[u8]>) -> Self {
        Self::copy_of(data.as_ref(), false)
    }

    /// Toujours la forme longue, quelle que soit la taille : pour les `SharedByte` qui
    /// vivent longtemps hors de l'arbre, que le GC ne voit pas (MEMORY_ARCH.md §16.9).
    pub fn from_slice_external(data: impl AsRef<[u8]>) -> Self {
        Self::copy_of(data.as_ref(), true)
    }

    /// `self` en forme longue : copie si l'objet est en forme courte, sinon rien.
    pub fn into_external(self) -> Self {
        if self.is_external() { self } else { Self::copy_of(&self, true) }
    }

    /// Écrase les données en place (MEMORY_ARCH.md §16.5) : seulement si l'objet n'est
    /// partagé avec personne (`rc == 1`), en forme courte, et si `src` tient dans son
    /// emplacement. Faux : rien n'a changé, à l'appelant de remplacer l'objet.
    ///
    /// `rc == 1` ne protège que des copies qui clonent : une structure dont la capture
    /// de save copie les `SharedByte` bit à bit (valart) doit refuser elle-même pendant
    /// une save.
    pub fn overwrite_from(&mut self, src: &[u8]) -> bool {
        if self.rc() != 1 || self.is_external() || src.len() > Self::POOL_MAX_LEN {
            return false;
        }
        let (old, new) = (Self::slot(self.len()), Self::slot(src.len()));
        if new > old {
            return false;
        }
        unsafe {
            std::ptr::copy(src.as_ptr(), self.ptr.as_ptr(), src.len());
            (self.ptr.as_ptr() as *mut u16).sub(2).write(src.len() as u16);
            if new < old {
                short_shrink(self.ptr.as_ptr().sub(Self::SHORT_HEADER), old, new);
            }
        }
        true
    }

    /// GC (MEMORY_ARCH.md §16.6) : un objet d'un bloc marqué est recopié au curseur des
    /// survivants et l'ancien emplacement rendu. Tenu ailleurs (`rc > 1`) : épinglé,
    /// laissé en place. À n'appeler que sur un emplacement que l'arbre est seul à voir
    /// (énumérateurs du GC) : `self` est réécrit vers les mêmes octets.
    pub fn gc_relocate(&mut self) {
        #[cfg(radixox_pool)]
        {
            if self.is_external() {
                return;
            }
            let base = self.header_ptr() as *mut u8;
            if !crate::pool::is_marked(base as usize) {
                return;
            }
            if self.rc() != 1 {
                crate::pool::note_pinned();
                return;
            }
            let size = Self::slot(self.len());
            let new = crate::pool::alloc_survivor(size);
            if new.is_null() {
                return; // plage pleine : l'objet reste
            }
            unsafe {
                std::ptr::copy_nonoverlapping(base, new, size);
                let moved = Self { ptr: NonNull::new_unchecked(new.add(Self::SHORT_HEADER)) };
                // rc 1 → 0 : l'ancien emplacement est rendu par le chemin normal.
                drop(std::mem::replace(self, moved));
            }
            crate::pool::note_evacuated(size);
        }
    }

    /// Emplacement dans le pool (adresse de l'en-tête, taille), `None` hors pool
    /// (forme longue, ou forme courte dans mimalloc). Vérificateur du GC.
    pub fn pool_slot(&self) -> Option<(usize, usize)> {
        (cfg!(radixox_pool) && !self.is_external())
            .then(|| (self.header_ptr() as usize, Self::slot(self.len())))
    }

    #[inline]
    pub fn is_external(&self) -> bool {
        self.len_lo() & Self::EXTERNAL != 0
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        Self::from_slice(s.as_bytes())
    }

    #[inline]
    pub fn len(&self) -> usize {
        let lo = self.len_lo();
        if lo & Self::EXTERNAL == 0 {
            lo as usize
        } else {
            let hi = unsafe { (self.ptr.as_ptr() as *const u16).sub(3).read() };
            ((hi as usize) << 15) | (lo & 0x7FFF) as usize
        }
    }

    /// Prefetch de l'en-tête et du début des données.
    #[inline]
    pub fn prefetch(&self) {
        crate::prefetch::prefetch_l1(self.header_ptr());
    }

    /// Début de l'en-tête (des deux formes, la ligne de cache de `len_lo` et `rc`).
    #[inline]
    fn header_ptr(&self) -> *const u8 {
        unsafe { self.ptr.as_ptr().sub(Self::SHORT_HEADER) }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data_ptr(), self.len()) }
    }

    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(self.as_slice())
    }

    pub fn rc(&self) -> u16 {
        unsafe { self.rc_ptr().read() }
    }

    /// Uppercases the bytes in-place if `rc == 1`, otherwise allocates a new
    /// buffer (same form), copies uppercase in one pass, and decrements rc on the old allocation.
    ///
    /// After this call, `as_slice()` is guaranteed to be ASCII-uppercase.
    pub fn to_uppercase(&mut self) {
        let len = self.len();
        if self.rc() == 1 {
            unsafe {
                let data = self.ptr.as_ptr();
                for i in 0..len {
                    *data.add(i) = (*data.add(i)).to_ascii_uppercase();
                }
            }
        } else {
            unsafe {
                let new = Self::alloc(len, self.is_external());
                let src = self.data_ptr();
                let dst = new.ptr.as_ptr();
                for i in 0..len {
                    *dst.add(i) = (*src.add(i)).to_ascii_uppercase();
                }
                // Assigner `*self` drop l'ancienne valeur → décrémente son rc.
                *self = new;
            }
        }
    }

    /// Dernier `drop` : rend l'objet à son allocateur, aiguillé par le bit EXTERNE.
    #[cold]
    unsafe fn dealloc(&self) {
        unsafe {
            if self.is_external() {
                let size = Self::LONG_HEADER + self.len();
                external::free(self.ptr.as_ptr().sub(Self::LONG_HEADER), size);
            } else {
                short_free(self.ptr.as_ptr().sub(Self::SHORT_HEADER), Self::slot(self.len()));
            }
        }
    }
}

/// Forme courte : le pool (`--cfg radixox_pool`) ou mimalloc. Nul → forme longue.
#[cfg(radixox_pool)]
#[inline]
unsafe fn short_alloc(size: usize) -> *mut u8 {
    unsafe { crate::pool::alloc(size) }
}
#[cfg(radixox_pool)]
#[inline]
unsafe fn short_free(base: *mut u8, size: usize) {
    unsafe { crate::pool::free(base, size) }
}
#[cfg(radixox_pool)]
#[inline]
unsafe fn short_shrink(base: *mut u8, old: usize, new: usize) {
    unsafe { crate::pool::shrink(base, old, new) }
}
// mi_malloc plutôt que MiMalloc.alloc : ce dernier passe par mi_malloc_aligned qui, sur
// page vide, sur-alloue size+align-1 (106 B → bloc 128 au lieu de 112). mi_malloc
// garantit déjà un alignement ≥ 8.
#[cfg(not(radixox_pool))]
#[inline]
unsafe fn short_alloc(size: usize) -> *mut u8 {
    unsafe { mi_malloc(size) }
}
#[cfg(not(radixox_pool))]
#[inline]
unsafe fn short_free(base: *mut u8, _size: usize) {
    unsafe { mi_free(base) }
}
/// mimalloc : le bloc garde sa taille, rien à compter.
#[cfg(not(radixox_pool))]
#[inline]
unsafe fn short_shrink(_base: *mut u8, _old: usize, _new: usize) {}

impl Clone for SharedByte {
    #[inline]
    fn clone(&self) -> Self {
        // Saturation (comme les refcount du noyau) : à u16::MAX le compteur
        // ne bouge plus et le bloc n'est jamais libéré. Une fuite plutôt
        // qu'un use-after-free si une valeur est clonée plus de 65 535 fois.
        unsafe {
            let rc = self.rc_ptr();
            if *rc != u16::MAX {
                *rc += 1;
            }
        }

        Self { ptr: self.ptr }
    }
}

impl Drop for SharedByte {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let rc = self.rc_ptr();
            if *rc == u16::MAX {
                return; // saturé : immortel
            }
            *rc -= 1;
            if *rc == 0 {
                self.dealloc();
            }
        }
    }
}

impl std::ops::Deref for SharedByte {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::fmt::Debug for SharedByte {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // octets échappés : une clé binaire (non UTF-8) reste lisible
        write!(f, "SharedByte(\"{}\", rc={})", self.as_slice().escape_ascii(), self.rc())
    }
}

impl PartialEq for SharedByte {
    fn eq(&self, other: &Self) -> bool {
        // ptr identity fast path
        if self.ptr == other.ptr {
            return true;
        }
        self.as_slice() == other.as_slice()
    }
}
impl Eq for SharedByte {}

impl std::hash::Hash for SharedByte {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}
impl PartialOrd for SharedByte {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SharedByte {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}
impl Borrow<[u8]> for SharedByte {
    fn borrow(&self) -> &[u8] {
        self
    }
}
impl PreFetch for SharedByte {
    fn prefetch(&self)
    where
        Self: Sized,
    {
        crate::prefetch::prefetch_l1(self.header_ptr());
    }
}

/// Un `SharedByte` à un seul propriétaire (rc == 1). Pas `Send` : l'allocateur des
/// `SharedByte` est mono-thread (MEMORY_ARCH.md §15, worker de persistance).

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct OwnedByte(SharedByte);
impl Borrow<[u8]> for OwnedByte {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}
impl OwnedByte {
    pub fn from_slice(data: impl AsRef<[u8]>) -> Self {
        Self(SharedByte::from_slice(data.as_ref()))
    }
    #[inline]
    pub fn into_shared(self) -> SharedByte {
        self.0
    }
}
impl std::ops::Deref for OwnedByte {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}
impl Clone for OwnedByte {
    fn clone(&self) -> Self {
        Self(SharedByte::from_slice(self.0.as_slice()))
    }
}
impl Into<SharedByte> for OwnedByte {
    fn into(self) -> SharedByte {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_uppercase_shared_releases_old_rc() {
        let a = SharedByte::from_slice(b"hello");
        let mut b = a.clone();
        assert_eq!(a.rc(), 2);
        b.to_uppercase();
        assert_eq!(b.as_slice(), b"HELLO");
        assert_eq!(b.rc(), 1);
        assert_eq!(a.as_slice(), b"hello");
        assert_eq!(a.rc(), 1);
    }

    #[test]
    fn to_uppercase_unique_in_place() {
        let mut a = SharedByte::from_slice(b"MiXeD");
        a.to_uppercase();
        assert_eq!(a.as_slice(), b"MIXED");
        assert_eq!(a.rc(), 1);
    }
}
