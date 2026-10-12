use crate::raw_alloc::{free as mi_free, malloc as mi_malloc};
use std::borrow::Borrow;

use std::ops::Deref;
use std::ptr::NonNull;

use crate::prefetch::{PreFetch, PreFetchPtr};

#[repr(transparent)]
pub struct SharedByte {
    ptr: NonNull<u8>,
}

impl SharedByte {
    // layout heap : [len:u32 | rc:u16 | pad:2 | data:u8...]
    const HEADER_SIZE: usize = 6;

    #[inline]
    fn len_ptr(&self) -> *mut u32 {
        self.ptr.as_ptr() as *mut u32
    }

    #[inline]
    fn rc_ptr(&self) -> *mut u16 {
        unsafe { (self.ptr.as_ptr() as *mut u16).add(2) }
    }

    #[inline]
    fn data_ptr(&self) -> *const u8 {
        unsafe { self.ptr.as_ptr().add(Self::HEADER_SIZE) }
    }

    pub fn from_byte(data: impl Deref<Target = [u8]>) -> Self {
        assert!(data.len() <= u32::MAX as usize);

        unsafe {
            // mi_malloc plutôt que MiMalloc.alloc : ce dernier passe par
            // mi_malloc_aligned qui, sur page vide, sur-alloue size+align-1
            // (106 B → bloc 128 au lieu de 112). mi_malloc garantit déjà
            // un alignement ≥ 8.
            let total = Self::HEADER_SIZE + data.len();
            let ptr = mi_malloc(total) as *mut u8;
            assert!(!ptr.is_null());

            // écrire len
            (ptr as *mut u32).write(data.len() as u32);
            // écrire rc
            (ptr.add(4) as *mut u16).write(1);

            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.add(Self::HEADER_SIZE), data.len());

            Self {
                ptr: NonNull::new_unchecked(ptr),
            }
        }
    }
    #[inline]
    pub fn from_slice(data: impl AsRef<[u8]>) -> Self {
        Self::from_byte(data.as_ref())
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        Self::from_slice(s.as_bytes())
    }

    pub fn len(&self) -> usize {
        unsafe { self.len_ptr().read() as usize }
    }

    /// Prefetch de l'en-tête et du début des données.
    #[inline]
    pub fn prefetch(&self) {
        crate::prefetch::prefetch_l1(self.ptr.as_ptr());
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
    /// buffer, copies uppercase in one pass, and decrements rc on the old allocation.
    ///
    /// After this call, `as_slice()` is guaranteed to be ASCII-uppercase.
    pub fn to_uppercase(&mut self) {
        let len = self.len();
        if self.rc() == 1 {
            unsafe {
                let data = self.ptr.as_ptr().add(Self::HEADER_SIZE);
                for i in 0..len {
                    *data.add(i) = (*data.add(i)).to_ascii_uppercase();
                }
            }
        } else {
            unsafe {
                let new_ptr = mi_malloc(Self::HEADER_SIZE + len) as *mut u8;
                assert!(!new_ptr.is_null());
                (new_ptr as *mut u32).write(len as u32);
                (new_ptr.add(4) as *mut u16).write(1);
                let src = self.ptr.as_ptr().add(Self::HEADER_SIZE);
                let dst = new_ptr.add(Self::HEADER_SIZE);
                for i in 0..len {
                    *dst.add(i) = (*src.add(i)).to_ascii_uppercase();
                }

                // Pas `self.ptr = new_ptr` : réécrire le champ (NonNull est Copy)
                // ne drop rien, l'ancien buffer garderait son rc et fuirait.
                // Assigner `*self` drop l'ancienne valeur → décrémente son rc.
                *self = Self {
                    ptr: NonNull::new_unchecked(new_ptr),
                };
            }
        }
    }
}

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
                mi_free(self.ptr.as_ptr() as *mut u8);
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
        self.ptr.ptr_prefetch();
    }
}

///Represente l'invariant rc == 1 donc vue que un owner alors Send

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
unsafe impl Send for OwnedByte {}
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
