use std::{
    alloc::{Layout, alloc, dealloc, realloc},
    mem::{ManuallyDrop, MaybeUninit},
    ops::Deref,
    ptr::NonNull,
};

use crate::shared_byte::{OwnedByte, SharedByte};

pub struct SmallVec<const S: usize, T> {
    size: u32, //This never shrink back so if size > S we are in HeapMode
    len: u32,
    data: SmallVecData<S, T>,
}

impl<const S: usize, T> SmallVec<S, T> {
    #[inline]
    fn is_heap(&self) -> bool {
        self.size > S as u32
    }
    #[inline]
    fn is_inline(&self) -> bool {
        (self.size as usize) <= S
    }
    pub fn new() -> Self {
        Self {
            size: S as u32,
            len: 0,
            data: SmallVecData::new(),
        }
    }
    pub fn push(&mut self, item: T) {
        if self.is_inline() {
            if self.size > self.len {
                // ptr::write avoids dropping the uninitialized slot
                unsafe { self.get_ptr().add(self.len as usize).write(item) }
                self.len += 1;
                return;
            }
            //here we simply promote to heap version so we later use the heap part of push
            self.promote();
        }
        if self.len == self.size {
            self.growth();
        }
        unsafe { self.data.ptr.add(self.len as usize).write(item) }
        self.len += 1;
    }
    fn promote(&mut self) {
        let heap_size = 4.max(self.size * 2);
        let layout = Layout::array::<T>(heap_size as usize).unwrap();
        let ptr = unsafe {
            let ptr = alloc(layout);

            if ptr.is_null() {
                panic!("alloc failed");
            }
            NonNull::new_unchecked(ptr as *mut T)
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.data.inline.as_ptr(),
                ptr.as_ptr(),
                self.len as usize,
            );
        }
        self.data.ptr = ptr;
        self.size = heap_size;
    }
    //Simply double the size on the heap and double the capacity
    fn growth(&mut self) {
        assert!(self.is_heap());
        let new_size = self.size * 2;
        let ptr = unsafe {
            let ptr = realloc(
                self.data.ptr.as_ptr() as *mut u8,
                Layout::array::<T>(self.size as usize).unwrap(),
                Layout::array::<T>(new_size as usize).unwrap().size(),
            );
            if ptr.is_null() {
                panic!("alloc failed");
            }
            NonNull::new_unchecked(ptr as *mut T)
        };
        self.data.ptr = ptr;
        self.size = new_size;
    }
    unsafe fn get_ptr(&mut self) -> NonNull<T> {
        unsafe {
            if self.is_heap() {
                self.data.ptr
            } else {
                NonNull::new_unchecked((*self.data.inline).as_mut_ptr())
            }
        }
    }
}
impl<const S: usize, T> Drop for SmallVec<S, T> {
    fn drop(&mut self) {
        if self.is_heap() {
            unsafe { self.data.free_heap(self.len as usize, self.size as usize) };
        } else {
            unsafe { self.data.free_inline(self.len as usize) };
        }
    }
}
impl<const S: usize, T> Deref for SmallVec<S, T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        unsafe {
            if self.is_heap() {
                std::slice::from_raw_parts(self.data.ptr.as_ptr(), self.len as usize)
            } else {
                std::slice::from_raw_parts(self.data.inline.as_ptr(), self.len as usize)
            }
        }
    }
}
impl<const S: usize, T> IntoIterator for SmallVec<S, T> {
    type Item = T;
    type IntoIter = SmallVecIterator<S, T>;
    fn into_iter(mut self) -> Self::IntoIter {
        let ptr = if self.len == 0 {
            unsafe { self.get_ptr() }
        } else {
            unsafe { self.get_ptr().add(self.len as usize - 1) }
        };
        SmallVecIterator { ptr, vec: self }
    }
}
pub struct SmallVecIterator<const S: usize, T> {
    ptr: NonNull<T>,
    vec: SmallVec<S, T>,
}
impl<const S: usize, T> Iterator for SmallVecIterator<S, T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        if self.vec.len == 0 {
            return None;
        }
        self.vec.len -= 1;
        let item = unsafe { self.ptr.read() };
        unsafe { self.ptr = self.ptr.sub(1) };
        Some(item)
    }
}

#[repr(align(8))]
union SmallVecData<const S: usize, T> {
    inline: ManuallyDrop<[T; S]>,
    ptr: NonNull<T>,
}

impl<const S: usize, T> SmallVecData<S, T> {
    fn new() -> Self {
        let inline = unsafe { std::mem::MaybeUninit::assume_init(MaybeUninit::uninit()) };
        Self { inline }
    }
    ///Safety the unrderlying data should represente an inline array and count the number on initialise and valide item
    unsafe fn free_inline(&mut self, count: usize) {
        if std::mem::needs_drop::<T>() {
            unsafe {
                let base_ptr = self.inline.as_mut_ptr() as *mut T;

                let slice = std::slice::from_raw_parts_mut(base_ptr, count);

                std::ptr::drop_in_place(slice);
            }
        }
    }
    ///Safety the unrderlying data should represente a ptr that point to len valid item and size*sizeof(item) memory space
    unsafe fn free_heap(&mut self, len: usize, size: usize) {
        if std::mem::needs_drop::<T>() {
            unsafe {
                let slice = std::slice::from_raw_parts_mut(self.ptr.as_ptr(), len);
                std::ptr::drop_in_place(slice);
            }
        }
        unsafe {
            dealloc(
                self.ptr.as_ptr() as *mut u8,
                Layout::array::<T>(size).unwrap(),
            )
        }
    }
}
macro_rules! impl_into_shareds {
    ($src:ty, $dst:ty) => {
        impl<const S: usize> SmallVec<S, $src> {
            pub fn into_shareds(self) -> SmallVec<S, $dst> {
                const { assert!(size_of::<$src>() == size_of::<$dst>()) };
                unsafe {
                    let result = (&self as *const SmallVec<S, $src>)
                        .cast::<SmallVec<S, $dst>>()
                        .read();
                    std::mem::forget(self);
                    result
                }
            }
        }
    };
}

impl_into_shareds!(OwnedByte, SharedByte);
impl_into_shareds!((OwnedByte, OwnedByte), (SharedByte, SharedByte));
impl_into_shareds!((f64, OwnedByte), (f64, SharedByte));
