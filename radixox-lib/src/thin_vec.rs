use std::{
    alloc::{Layout, alloc, dealloc, handle_alloc_error, realloc},
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::prefetch::{PreFetch, PreFetchPtr};

/// `len` and `cap` live at the very start of the heap allocation, immediately
/// followed by the `T` data. `ThinVec` itself only stores a pointer to the
/// *data*, not to the allocation start, so `size_of::<ThinVec<T>>() == 8`
/// regardless of `T`.
/// `align(8)`: `size_of::<Header>() == 8` too, so for any `T` with
/// `align_of::<T>() <= 8`, `data_offset()` never exceeds 8 — i.e. never
/// exceeds `size_of::<Header>()`. That means `base + data_offset` lands at
/// most one-past-the-end of `EMPTY_HEADER`, which `pointer::add` allows
/// (as long as it's never dereferenced, which it isn't: `cap == 0` there
/// always). No padding bytes needed.
#[repr(C, align(8))]
struct Header {
    cap: u32,
    len: u32,
}

/// Shared placeholder allocation used by empty vecs so `new()` never
/// allocates. `cap == 0` there always, so it is never written to and never
/// indexed into as `T` data — only its `Header` is ever read.
static EMPTY_HEADER: Header = Header { cap: 0, len: 0 };

/// `ptr` points at the first `T`, not at the `Header`. The `Header` sits
/// `Self::data_offset()` bytes before it in the same allocation.
/// `PhantomData<T>` is required because `ptr`'s pointee type (`()`) doesn't
/// actually describe what's pointed to (a `Header` followed by `[T]`), so it
/// can't stand in for ownership/variance of `T` on its own.
#[repr(transparent)]
pub struct ThinVec<T> {
    ptr: NonNull<()>,
    phantom: PhantomData<T>,
}

impl<T> ThinVec<T> {
    const fn data_offset() -> usize {
        const { assert!(align_of::<T>() <= 8, "ThinVec: T alignment too large") };
        let align = align_of::<T>();
        let header_size = size_of::<Header>();
        (header_size + align - 1) & !(align - 1)
    }

    fn layout_for(cap: usize) -> Layout {
        let align = align_of::<Header>().max(align_of::<T>());
        let size = Self::data_offset() + cap * size_of::<T>();
        Layout::from_size_align(size, align).unwrap()
    }

    fn header_ptr(&self) -> *mut Header {
        unsafe {
            self.ptr
                .as_ptr()
                .cast::<u8>()
                .sub(Self::data_offset())
                .cast::<Header>()
        }
    }

    fn header(&self) -> &Header {
        unsafe { &*self.header_ptr() }
    }

    fn data_ptr(&self) -> *mut T {
        self.ptr.as_ptr().cast::<T>()
    }

    pub fn new() -> Self {
        let ptr = unsafe { (&EMPTY_HEADER as *const Header as *mut u8).add(Self::data_offset()) };
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) }.cast(),
            phantom: PhantomData,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        if cap == 0 {
            return Self::new();
        }
        let layout = Self::layout_for(cap);
        unsafe {
            let raw = alloc(layout);
            if raw.is_null() {
                handle_alloc_error(layout);
            }
            (raw as *mut Header).write(Header {
                cap: cap as u32,
                len: 0,
            });
            Self {
                ptr: NonNull::new_unchecked(raw.add(Self::data_offset())).cast(),
                phantom: PhantomData,
            }
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.header().len as usize
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.header().cap as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Classic doubling growth: 0 -> 4, then cap -> cap*2.
    fn grow(&mut self) {
        let old_cap = self.capacity();
        let new_cap = if old_cap == 0 { 4 } else { old_cap * 2 };
        let new_layout = Self::layout_for(new_cap);
        let raw = if old_cap == 0 {
            unsafe { alloc(new_layout) }
        } else {
            let old_layout = Self::layout_for(old_cap);
            let old_base = unsafe { self.ptr.as_ptr().cast::<u8>().sub(Self::data_offset()) };
            unsafe { realloc(old_base, old_layout, new_layout.size()) }
        };
        if raw.is_null() {
            handle_alloc_error(new_layout);
        }
        unsafe {
            if old_cap == 0 {
                (raw as *mut Header).write(Header {
                    cap: new_cap as u32,
                    len: 0,
                });
            } else {
                (raw as *mut u32).write(new_cap as u32); // Header::cap is the first field
            }
            self.ptr = NonNull::new_unchecked(raw.add(Self::data_offset())).cast();
        }
    }

    pub fn push(&mut self, item: T) {
        if self.len() == self.capacity() {
            self.grow();
        }
        let len = self.len();
        unsafe {
            self.data_ptr().add(len).write(item);
            (*self.header_ptr()).len = (len + 1) as u32;
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let new_len = len - 1;
        unsafe {
            (*self.header_ptr()).len = new_len as u32;
            Some(self.data_ptr().add(new_len).read())
        }
    }

    pub fn swap_remove(&mut self, index: usize) -> T {
        let len = self.len();
        assert!(index < len, "swap_remove index out of bounds");
        let new_len = len - 1;
        unsafe {
            let ptr = self.data_ptr();
            let item = ptr.add(index).read();
            if index != new_len {
                let last = ptr.add(new_len).read();
                ptr.add(index).write(last);
            }
            (*self.header_ptr()).len = new_len as u32;
            item
        }
    }

    pub fn find<P>(&self, mut predicate: P) -> Option<&T>
    where
        P: FnMut(&T) -> bool,
    {
        let len = self.len();
        let ptr = self.data_ptr();
        for i in 0..len {
            // SAFETY: i < len, so ptr.add(i) is a valid, initialized T within this allocation.
            let item = unsafe { &*ptr.add(i) };
            if predicate(item) {
                return Some(item);
            }
        }
        None
    }

    pub fn find_mut<P>(&mut self, mut predicate: P) -> Option<&mut T>
    where
        P: FnMut(&T) -> bool,
    {
        let len = self.len();
        let ptr = self.data_ptr();
        for i in 0..len {
            // SAFETY: i < len, so ptr.add(i) is a valid, initialized T within this allocation.
            let item = unsafe { &mut *ptr.add(i) };
            if predicate(item) {
                return Some(item);
            }
        }
        None
    }
}

impl<T> Default for ThinVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for ThinVec<T> {
    fn drop(&mut self) {
        let cap = self.capacity();
        if cap == 0 {
            return;
        }
        unsafe {
            std::ptr::drop_in_place(std::slice::from_raw_parts_mut(self.data_ptr(), self.len()));
            let base = self.ptr.as_ptr().cast::<u8>().sub(Self::data_offset());
            dealloc(base, Self::layout_for(cap));
        }
    }
}

impl<T> Deref for ThinVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.data_ptr(), self.len()) }
    }
}

impl<T> DerefMut for ThinVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        let len = self.len();
        unsafe { std::slice::from_raw_parts_mut(self.data_ptr(), len) }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ThinVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: PartialEq> PartialEq for ThinVec<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Clone> Clone for ThinVec<T> {
    fn clone(&self) -> Self {
        let mut new_vec = Self::with_capacity(self.len());
        for item in self.iter() {
            new_vec.push(item.clone());
        }
        new_vec
    }
}

pub struct ThinVecIntoIter<T> {
    vec: ThinVec<T>,
    idx: usize,
}

impl<T> IntoIterator for ThinVec<T> {
    type Item = T;
    type IntoIter = ThinVecIntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        ThinVecIntoIter { vec: self, idx: 0 }
    }
}

impl<T> Iterator for ThinVecIntoIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        if self.idx >= self.vec.len() {
            return None;
        }
        let item = unsafe { self.vec.data_ptr().add(self.idx).read() };
        self.idx += 1;
        Some(item)
    }
}

impl<T> Drop for ThinVecIntoIter<T> {
    fn drop(&mut self) {
        let len = self.vec.len();
        if self.idx < len {
            unsafe {
                let remaining = std::slice::from_raw_parts_mut(
                    self.vec.data_ptr().add(self.idx),
                    len - self.idx,
                );
                std::ptr::drop_in_place(remaining);
            }
        }
        // Prevent ThinVec::drop from re-dropping the (already-consumed) elements.
        unsafe { (*self.vec.header_ptr()).len = 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_one_pointer() {
        assert_eq!(size_of::<ThinVec<u64>>(), size_of::<usize>());
    }

    #[test]
    fn push_pop_basic() {
        let mut v: ThinVec<u32> = ThinVec::new();
        assert_eq!(v.len(), 0);
        assert_eq!(v.capacity(), 0);
        for i in 0..100 {
            v.push(i);
        }
        assert_eq!(v.len(), 100);
        assert_eq!(&v[..5], &[0, 1, 2, 3, 4]);
        for i in (0..100).rev() {
            assert_eq!(v.pop(), Some(i));
        }
        assert_eq!(v.pop(), None);
    }

    #[test]
    fn drop_runs_on_elements() {
        use std::rc::Rc;
        let counter = Rc::new(());
        let mut v: ThinVec<Rc<()>> = ThinVec::new();
        for _ in 0..10 {
            v.push(counter.clone());
        }
        assert_eq!(Rc::strong_count(&counter), 11);
        drop(v);
        assert_eq!(Rc::strong_count(&counter), 1);
    }

    #[test]
    fn into_iter_order_and_drop() {
        let mut v: ThinVec<u32> = ThinVec::new();
        for i in 0..5 {
            v.push(i);
        }
        let collected: Vec<u32> = v.into_iter().collect();
        assert_eq!(collected, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn swap_remove_basic() {
        let mut v: ThinVec<u32> = ThinVec::new();
        for i in 0..5 {
            v.push(i);
        }
        assert_eq!(v.swap_remove(1), 1);
        assert_eq!(&*v, &[0, 4, 2, 3]);
        assert_eq!(v.swap_remove(3), 3);
        assert_eq!(&*v, &[0, 4, 2]);
        assert_eq!(v.swap_remove(2), 2);
        assert_eq!(&*v, &[0, 4]);
    }

    #[test]
    fn clone_is_independent() {
        let mut v: ThinVec<u32> = ThinVec::new();
        v.push(1);
        v.push(2);
        let mut c = v.clone();
        c.push(3);
        assert_eq!(&*v, &[1, 2]);
        assert_eq!(&*c, &[1, 2, 3]);
    }
}
impl<T> PreFetch for ThinVec<T> {
    fn prefetch(&self) {
        self.ptr.ptr_prefetch();
    }
}
