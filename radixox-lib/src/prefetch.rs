use std::ptr::NonNull;

pub trait PreFetch {
    #[inline]
    fn prefetch(&self)
    where
        Self: Sized,
    {
        let mut ptr = self as *const Self as *const u8;
        let nb_step: usize = (size_of::<Self>() + 63) >> 6;
        for _ in 0..nb_step {
            prefetch_l1(ptr);
            ptr = unsafe { ptr.add(64) };
        }
    }
}
pub trait PreFetchPtr {
    fn ptr_prefetch(&self);
}
impl<T> PreFetchPtr for NonNull<T> {
    fn ptr_prefetch(&self) {
        prefetch_l1(self.as_ptr() as *const u8);
    }
}
impl<T> PreFetchPtr for *const T {
    fn ptr_prefetch(&self) {
        prefetch_l1(*self as *const T as *const u8);
    }
}
impl<T> PreFetchPtr for *mut T {
    fn ptr_prefetch(&self) {
        prefetch_l1(*self as *const T as *const u8);
    }
}

#[inline(always)]
pub fn prefetch_l1(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch(ptr as *const i8, _MM_HINT_T0);
    }

    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::{_PREFETCH_LOCALITY3, _PREFETCH_READ, _prefetch};
        _prefetch(ptr as *const i8, _PREFETCH_READ, _PREFETCH_LOCALITY3);
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = ptr;
    }
}
