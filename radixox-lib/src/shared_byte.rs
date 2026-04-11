use std::alloc::{Layout, alloc, dealloc};
use std::borrow::Borrow;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;

#[repr(transparent)]
pub struct SharedByte {
    ptr: NonNull<u8>,
    _not_send: PhantomData<*mut u8>,
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
            let total = Self::HEADER_SIZE + data.len();
            let layout = Layout::from_size_align(total, 4).unwrap();
            let ptr = alloc(layout);
            assert!(!ptr.is_null());

            // écrire len
            (ptr as *mut u32).write(data.len() as u32);
            // écrire rc
            (ptr.add(4) as *mut u16).write(1);

            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.add(Self::HEADER_SIZE), data.len());

            Self {
                ptr: NonNull::new_unchecked(ptr),
                _not_send: PhantomData {},
            }
        }
    }
    #[inline]
    pub fn from_slice(data: impl AsRef<[u8]>) -> Self {
        Self::from_byte(data.as_ref())
    }

    pub fn from_str(s: &str) -> Self {
        Self::from_slice(s.as_bytes())
    }

    pub fn len(&self) -> usize {
        unsafe { self.len_ptr().read() as usize }
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
            let layout = Layout::from_size_align(Self::HEADER_SIZE + len, 4).unwrap();
            unsafe {
                let new_ptr = alloc(layout);
                assert!(!new_ptr.is_null());
                (new_ptr as *mut u32).write(len as u32);
                (new_ptr.add(4) as *mut u16).write(1);
                let src = self.ptr.as_ptr().add(Self::HEADER_SIZE);
                let dst = new_ptr.add(Self::HEADER_SIZE);
                for i in 0..len {
                    *dst.add(i) = (*src.add(i)).to_ascii_uppercase();
                }
                // detach from old allocation
                let rc = self.rc_ptr();
                *rc -= 1;
                if *rc == 0 {
                    dealloc(self.ptr.as_ptr(), layout);
                }
                self.ptr = NonNull::new_unchecked(new_ptr);
            }
        }
    }
}

impl Clone for SharedByte {
    #[inline]
    fn clone(&self) -> Self {
        unsafe {
            let rc = self.rc_ptr();
            debug_assert!(*rc < u16::MAX, "rc overflow");
            *rc += 1;
        }

        Self {
            ptr: self.ptr,
            _not_send: PhantomData {},
        }
    }
}

impl Drop for SharedByte {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let rc = self.rc_ptr();
            *rc -= 1;
            if *rc == 0 {
                let total = Self::HEADER_SIZE + self.len();
                let layout = Layout::from_size_align(total, 4).unwrap();
                dealloc(self.ptr.as_ptr(), layout);
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
        write!(f, "SharedString({:?}, rc={})", self.as_str(), self.rc())
    }
}
impl SharedByte {}

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
        Some(self.as_slice().cmp(&other.as_slice()))
    }
}
impl Ord for SharedByte {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_slice().cmp(&other.as_slice())
    }
}
impl Borrow<[u8]> for SharedByte {
    fn borrow(&self) -> &[u8] {
        &self // si SharedByte: Deref<Target=[u8]>
        // ou self.as_slice() selon ton API
    }
}
