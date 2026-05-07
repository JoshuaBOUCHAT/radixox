use mimalloc::MiMalloc;
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::ops::Deref;

// Inline : inner[0] = (len << 1) | 1    len ∈ 0..=7
//          inner[1..8] = data
// Heap   : inner = ptr vers [u32 len | u8... data]   align 4 → bit 0 = 0 garanti
union Inner {
    heap: *mut u8, // align 8 → ptr read = 1 MOV aligné
    inline: [u8; 8],
}

pub struct CompactStr(Inner);

impl CompactStr {
    const INLINE_CAP: usize = 7;
    const HEAP_HEADER: usize = 4; // u32 len seulement — pas de capacity, immuable

    #[inline]
    pub fn new() -> Self {
        Self(Inner {
            inline: [1, 0, 0, 0, 0, 0, 0, 0],
        })
    }

    /// Recompression : remplace self par (self ++ radix ++ suffix) en une seule alloc.
    pub fn append_and_replace(&mut self, radix: u8, suffix: &[u8]) {
        let new = {
            let old: &[u8] = self;
            let new_len = old.len() + 1 + suffix.len();
            if new_len <= Self::INLINE_CAP {
                let mut inner = [0u8; 8];
                inner[0] = ((new_len as u8) << 1) | 1;
                inner[1..1 + old.len()].copy_from_slice(old);
                inner[1 + old.len()] = radix;
                inner[2 + old.len()..1 + new_len].copy_from_slice(suffix);
                Self(Inner { inline: inner })
            } else {
                unsafe {
                    let layout = Layout::from_size_align(Self::HEAP_HEADER + new_len, 4).unwrap();
                    let ptr = MiMalloc.alloc(layout);
                    assert!(!ptr.is_null());
                    (ptr as *mut u32).write(new_len as u32);
                    let data = ptr.add(Self::HEAP_HEADER);
                    std::ptr::copy_nonoverlapping(old.as_ptr(), data, old.len());
                    *data.add(old.len()) = radix;
                    std::ptr::copy_nonoverlapping(
                        suffix.as_ptr(),
                        data.add(old.len() + 1),
                        suffix.len(),
                    );
                    Self(Inner { heap: ptr })
                }
            }
        }; // emprunt `old` libéré ici → *self = new drop l'ancien en sécurité
        *self = new;
    }

    pub fn from_slice(data: &[u8]) -> Self {
        if data.len() <= Self::INLINE_CAP {
            let mut inner = [0u8; 8];
            inner[0] = ((data.len() as u8) << 1) | 1;
            inner[1..1 + data.len()].copy_from_slice(data);
            Self(Inner { inline: inner })
        } else {
            unsafe {
                let layout = Layout::from_size_align(Self::HEAP_HEADER + data.len(), 4).unwrap();
                let ptr = MiMalloc.alloc(layout);
                assert!(!ptr.is_null());
                (ptr as *mut u32).write(data.len() as u32);
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    ptr.add(Self::HEAP_HEADER),
                    data.len(),
                );
                Self(Inner { heap: ptr })
            }
        }
    }
}

impl Deref for CompactStr {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        unsafe {
            let tag = self.0.inline[0];
            if tag & 1 == 1 {
                let len = (tag >> 1) as usize;
                // Aide le compilo: len ne peut pas dépasser 7 par construction

                std::hint::assert_unchecked(len <= 7);
                self.0.inline.get_unchecked(1..1 + len)
            } else {
                let ptr = self.0.heap;
                let len = (ptr as *const u32).read() as usize;
                std::slice::from_raw_parts(ptr.add(Self::HEAP_HEADER), len)
            }
        }
    }
}

impl Default for CompactStr {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CompactStr {
    fn drop(&mut self) {
        unsafe {
            if self.0.inline[0] & 1 == 0 {
                let ptr = self.0.heap;
                let len = (ptr as *const u32).read() as usize;
                let layout = Layout::from_size_align(Self::HEAP_HEADER + len, 4).unwrap();
                MiMalloc.dealloc(ptr, layout);
            }
        }
    }
}
