use std::{
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};

/// Purely-static vec: capacity `N` is known at compile time, no heap at all.
///
/// `#[repr(C)]` with `data` declared first and `len: u8` last: `data`'s
/// natural alignment (`align_of::<T>()`) drives the layout, and `len` — a
/// single byte, `align(1)` — slots right after it with no padding needed
/// *before* it. The only padding that can ever appear is trailing padding
/// after `len`, to round the whole struct up to `align_of::<T>()`, which is
/// unavoidable in any layout that holds a `[T; N]` at all. `len: u8` caps
/// `N` at 255, enforced at construction.
#[repr(C)]
pub struct ArrayVec<T, const N: usize> {
    data: [MaybeUninit<T>; N],
    len: u8,
}

impl<T, const N: usize> ArrayVec<T, N> {
    #[inline]
    fn data_ptr(&self) -> *const T {
        self.data.as_ptr().cast::<T>()
    }

    #[inline]
    fn data_ptr_mut(&mut self) -> *mut T {
        self.data.as_mut_ptr().cast::<T>()
    }

    pub fn new() -> Self {
        const { assert!(N <= u8::MAX as usize, "ArrayVec: N must fit in u8") };
        Self {
            data: [const { MaybeUninit::uninit() }; N],
            len: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        N
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len as usize == N
    }

    /// Returns `item` back if the array is already full.
    pub fn push(&mut self, item: T) -> Result<(), T> {
        if self.is_full() {
            return Err(item);
        }
        let len = self.len();
        unsafe { self.data_ptr_mut().add(len).write(item) };
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(unsafe { self.data_ptr_mut().add(self.len as usize).read() })
    }

    pub fn find<P>(&self, mut predicate: P) -> Option<&T>
    where
        P: FnMut(&T) -> bool,
    {
        let len = self.len();
        let ptr = self.data_ptr();
        for i in 0..len {
            // SAFETY: i < len, so ptr.add(i) is a valid, initialized T within `data`.
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
        let ptr = self.data_ptr_mut();
        for i in 0..len {
            // SAFETY: i < len, so ptr.add(i) is a valid, initialized T within `data`.
            let item = unsafe { &mut *ptr.add(i) };
            if predicate(item) {
                return Some(item);
            }
        }
        None
    }
}

impl<T, const N: usize> Default for ArrayVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for ArrayVec<T, N> {
    fn drop(&mut self) {
        if std::mem::needs_drop::<T>() {
            unsafe {
                std::ptr::drop_in_place(std::slice::from_raw_parts_mut(
                    self.data_ptr_mut(),
                    self.len(),
                ));
            }
        }
    }
}

impl<T, const N: usize> Deref for ArrayVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.data_ptr(), self.len()) }
    }
}

impl<T, const N: usize> DerefMut for ArrayVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        let len = self.len();
        unsafe { std::slice::from_raw_parts_mut(self.data_ptr_mut(), len) }
    }
}

impl<T: std::fmt::Debug, const N: usize> std::fmt::Debug for ArrayVec<T, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: PartialEq, const N: usize> PartialEq for ArrayVec<T, N> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Clone, const N: usize> Clone for ArrayVec<T, N> {
    fn clone(&self) -> Self {
        let mut new_vec = Self::new();
        for item in self.iter() {
            // SAFETY: len <= N by construction, cloning from self can never overflow.
            let _ = new_vec.push(item.clone());
        }
        new_vec
    }
}

pub struct ArrayVecIntoIter<T, const N: usize> {
    vec: ArrayVec<T, N>,
    idx: usize,
}

impl<T, const N: usize> IntoIterator for ArrayVec<T, N> {
    type Item = T;
    type IntoIter = ArrayVecIntoIter<T, N>;
    fn into_iter(self) -> Self::IntoIter {
        ArrayVecIntoIter { vec: self, idx: 0 }
    }
}

impl<T, const N: usize> Iterator for ArrayVecIntoIter<T, N> {
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

impl<T, const N: usize> Drop for ArrayVecIntoIter<T, N> {
    fn drop(&mut self) {
        let len = self.vec.len();
        if std::mem::needs_drop::<T>() && self.idx < len {
            unsafe {
                let remaining = std::slice::from_raw_parts_mut(
                    self.vec.data_ptr_mut().add(self.idx),
                    len - self.idx,
                );
                std::ptr::drop_in_place(remaining);
            }
        }
        // Prevent ArrayVec::drop from re-dropping the (already-consumed) elements.
        self.vec.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_compact() {
        // data: N * size_of::<T>(), len: u8 right after, only trailing
        // padding up to align_of::<T>() if any.
        assert_eq!(size_of::<ArrayVec<u8, 4>>(), 5);
        assert!(size_of::<ArrayVec<u32, 4>>() <= 4 * 4 + 4); // +4 = padding up to align 4
    }

    #[test]
    fn push_pop_basic() {
        let mut v: ArrayVec<u32, 4> = ArrayVec::new();
        assert!(v.push(1).is_ok());
        assert!(v.push(2).is_ok());
        assert!(v.push(3).is_ok());
        assert!(v.push(4).is_ok());
        assert_eq!(v.push(5), Err(5));
        assert_eq!(&*v, &[1, 2, 3, 4]);
        assert_eq!(v.pop(), Some(4));
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn drop_runs_on_elements() {
        use std::rc::Rc;
        let counter = Rc::new(());
        let mut v: ArrayVec<Rc<()>, 4> = ArrayVec::new();
        for _ in 0..4 {
            let _ = v.push(counter.clone());
        }
        assert_eq!(Rc::strong_count(&counter), 5);
        drop(v);
        assert_eq!(Rc::strong_count(&counter), 1);
    }

    #[test]
    fn find_and_find_mut() {
        let mut v: ArrayVec<u32, 4> = ArrayVec::new();
        let _ = v.push(1);
        let _ = v.push(2);
        let _ = v.push(3);
        assert_eq!(v.find(|&x| x == 2), Some(&2));
        if let Some(x) = v.find_mut(|&x| x == 2) {
            *x = 42;
        }
        assert_eq!(&*v, &[1, 42, 3]);
    }
}
