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
    pub fn with_capacity(cap: usize) -> Self {
        if cap <= S {
            return Self::new();
        }
        let layout = Layout::array::<T>(cap).unwrap();
        let ptr = unsafe {
            let ptr = alloc(layout);
            if ptr.is_null() {
                panic!("alloc failed");
            }
            NonNull::new_unchecked(ptr as *mut T)
        };
        Self {
            size: cap as u32,
            len: 0,
            data: SmallVecData { ptr },
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
                self.data.inline_ptr(),
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
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(unsafe { self.get_ptr().add(self.len as usize).read() })
    }
    pub fn remove_swap(&mut self, index: usize) -> T {
        assert!(index < self.len as usize);
        self.len -= 1;
        let ptr = unsafe { self.get_ptr() };
        unsafe {
            let removed = ptr.add(index).read();
            if index < self.len as usize {
                ptr.add(index).write(ptr.add(self.len as usize).read());
            }
            removed
        }
    }
    /// Insère à `index` en décalant la suite (`index <= len`).
    pub fn insert(&mut self, index: usize, item: T) {
        assert!(index <= self.len as usize);
        self.push(item);
        self[index..].rotate_right(1);
    }
    /// Retire l'élément `index` en gardant l'ordre de la suite.
    pub fn remove(&mut self, index: usize) -> T {
        assert!(index < self.len as usize);
        self[index..].rotate_left(1);
        self.pop().unwrap()
    }
    /// Capacité pour au moins `additional` éléments de plus, sans la doubler (une seule
    /// allocation exacte).
    pub fn reserve_exact(&mut self, additional: usize) {
        let need = self.len as usize + additional;
        if need <= self.size as usize {
            return;
        }
        if self.is_inline() {
            self.promote();
        }
        if need > self.size as usize {
            let ptr = unsafe {
                let p = realloc(
                    self.data.ptr.as_ptr() as *mut u8,
                    Layout::array::<T>(self.size as usize).unwrap(),
                    Layout::array::<T>(need).unwrap().size(),
                );
                if p.is_null() {
                    panic!("alloc failed");
                }
                NonNull::new_unchecked(p as *mut T)
            };
            self.data.ptr = ptr;
            self.size = need as u32;
        }
    }
    pub fn clear(&mut self) {
        self.truncate(0);
    }
    /// Garde les `n` premiers (rien si `n >= len`).
    pub fn truncate(&mut self, n: usize) {
        while self.len as usize > n {
            drop(self.pop());
        }
    }
    pub fn capacity(&self) -> usize {
        self.size as usize
    }
    unsafe fn get_ptr(&mut self) -> NonNull<T> {
        unsafe {
            if self.is_heap() {
                self.data.ptr
            } else {
                NonNull::new_unchecked(self.data.inline_mut_ptr())
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
                std::slice::from_raw_parts(self.data.inline_ptr(), self.len as usize)
            }
        }
    }
}
impl<const S: usize, T> std::ops::DerefMut for SmallVec<S, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            if self.is_heap() {
                std::slice::from_raw_parts_mut(self.data.ptr.as_ptr(), self.len as usize)
            } else {
                std::slice::from_raw_parts_mut(self.data.inline_mut_ptr(), self.len as usize)
            }
        }
    }
}
impl<const S: usize, T> IntoIterator for SmallVec<S, T> {
    type Item = T;
    type IntoIter = SmallVecIterator<S, T>;
    fn into_iter(self) -> Self::IntoIter {
        SmallVecIterator { vec: ManuallyDrop::new(self), pos: 0 }
    }
}
impl<'a, const S: usize, T> IntoIterator for &'a SmallVec<S, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, const S: usize, T> IntoIterator for &'a mut SmallVec<S, T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}
/// Itère dans l'ordre. Ne garde pas de pointeur : en mode inline, les données vivent dans
/// `vec` lui-même, qui est déplacé avec l'itérateur. Les éléments non consommés sont
/// droppés avec l'itérateur.
pub struct SmallVecIterator<const S: usize, T> {
    vec: ManuallyDrop<SmallVec<S, T>>,
    pos: u32,
}
impl<const S: usize, T> Iterator for SmallVecIterator<S, T> {
    type Item = T;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos == self.vec.len {
            return None;
        }
        let i = self.pos as usize;
        self.pos += 1;
        // SAFETY: `i < len`, chaque case n'est lue qu'une fois (pos avance, len recule).
        Some(unsafe { self.vec.get_ptr().add(i).read() })
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.vec.len - self.pos) as usize;
        (n, Some(n))
    }
}
impl<const S: usize, T> DoubleEndedIterator for SmallVecIterator<S, T> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.pos == self.vec.len {
            return None;
        }
        self.vec.len -= 1;
        Some(unsafe { self.vec.get_ptr().add(self.vec.len as usize).read() })
    }
}
impl<const S: usize, T> ExactSizeIterator for SmallVecIterator<S, T> {}
impl<const S: usize, T> Drop for SmallVecIterator<S, T> {
    fn drop(&mut self) {
        // les `pos` premiers sont déjà sortis : on décale le reste au début, puis le Drop
        // du vecteur libère comme d'habitude
        let (pos, len) = (self.pos as usize, self.vec.len as usize);
        unsafe {
            let p = self.vec.get_ptr().as_ptr();
            std::ptr::copy(p.add(pos), p, len - pos);
            self.vec.len = (len - pos) as u32;
            ManuallyDrop::drop(&mut self.vec);
        }
    }
}

#[repr(align(8))]
union SmallVecData<const S: usize, T> {
    /// Seuls les `len` premiers éléments sont initialisés.
    inline: ManuallyDrop<MaybeUninit<[T; S]>>,
    ptr: NonNull<T>,
}

impl<const S: usize, T> SmallVecData<S, T> {
    fn new() -> Self {
        Self {
            inline: ManuallyDrop::new(MaybeUninit::uninit()),
        }
    }
    /// # Safety
    /// Mode inline.
    #[inline]
    unsafe fn inline_ptr(&self) -> *const T {
        unsafe { self.inline.as_ptr() as *const T }
    }
    /// # Safety
    /// Mode inline.
    #[inline]
    unsafe fn inline_mut_ptr(&mut self) -> *mut T {
        unsafe { (*self.inline).as_mut_ptr() as *mut T }
    }
    ///Safety the unrderlying data should represente an inline array and count the number on initialise and valide item
    unsafe fn free_inline(&mut self, count: usize) {
        if std::mem::needs_drop::<T>() {
            unsafe {
                let base_ptr = self.inline_mut_ptr();

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
impl<const S: usize, T> Default for SmallVec<S, T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<const S: usize, T> Extend<T> for SmallVec<S, T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for x in iter {
            self.push(x);
        }
    }
}
impl<const S: usize, T> FromIterator<T> for SmallVec<S, T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let mut v = Self::with_capacity(iter.size_hint().0);
        v.extend(iter);
        v
    }
}
// SAFETY: le vecteur possède ses éléments (pointeur de tas ou cases en ligne).
unsafe impl<const S: usize, T: Send> Send for SmallVec<S, T> {}
unsafe impl<const S: usize, T: Sync> Sync for SmallVec<S, T> {}

impl<const S: usize, T: std::fmt::Debug> std::fmt::Debug for SmallVec<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}
impl<const S: usize, T: PartialEq> PartialEq for SmallVec<S, T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<const S: usize, T: Clone> Clone for SmallVec<S, T> {
    fn clone(&self) -> Self {
        let mut new_vec = Self::with_capacity(self.len as usize);
        for item in self.iter() {
            new_vec.push(item.clone());
        }
        new_vec
    }
}
impl_into_shareds!(OwnedByte, SharedByte);
impl_into_shareds!((OwnedByte, OwnedByte), (SharedByte, SharedByte));
impl_into_shareds!((f64, OwnedByte), (f64, SharedByte));

#[cfg(test)]
mod tests {
    use super::SmallVec;
    use std::rc::Rc;

    #[test]
    fn into_iter_in_order_inline_and_heap() {
        for n in [0usize, 1, 3, 4, 5, 40] {
            let v: SmallVec<4, usize> = (0..n).collect();
            assert_eq!(&*v, &(0..n).collect::<Vec<_>>()[..]);
            assert_eq!(v.into_iter().collect::<Vec<_>>(), (0..n).collect::<Vec<_>>());
            let v: SmallVec<4, usize> = (0..n).collect();
            assert_eq!(v.into_iter().rev().collect::<Vec<_>>(), (0..n).rev().collect::<Vec<_>>());
        }
    }

    #[test]
    fn partial_iteration_drops_rest_once() {
        for n in [3usize, 4, 9] {
            for take in 0..=n {
                let rc = Rc::new(());
                let v: SmallVec<4, Rc<()>> = (0..n).map(|_| rc.clone()).collect();
                let mut it = v.into_iter();
                let got: Vec<_> = it.by_ref().take(take).collect();
                if take < n {
                    it.next_back();
                }
                drop(it);
                assert_eq!(Rc::strong_count(&rc), 1 + got.len());
                drop(got);
                assert_eq!(Rc::strong_count(&rc), 1);
            }
        }
    }

    #[test]
    fn insert_remove_reserve_exact() {
        let mut v: SmallVec<2, String> = SmallVec::new();
        let mut model = Vec::new();
        for i in 0..12usize {
            let at = (i * 7) % (model.len() + 1);
            v.insert(at, i.to_string());
            model.insert(at, i.to_string());
            if i % 3 == 2 {
                let at = (i * 5) % model.len();
                assert_eq!(v.remove(at), model.remove(at));
            }
            assert_eq!(&*v, &model[..]);
        }
        let mut w: SmallVec<2, u8> = SmallVec::new();
        w.push(1);
        w.push(2);
        w.reserve_exact(1);
        assert!(w.capacity() >= 3);
        w.push(3);
        assert_eq!(&*w, &[1, 2, 3]);
    }

    #[test]
    fn clear_truncate_extend() {
        let rc = Rc::new(());
        let mut v: SmallVec<2, Rc<()>> = SmallVec::default();
        v.extend((0..5).map(|_| rc.clone()));
        assert_eq!(v.len(), 5);
        v.truncate(2);
        assert_eq!(Rc::strong_count(&rc), 3);
        v.clear();
        assert!(v.is_empty());
        assert_eq!(Rc::strong_count(&rc), 1);
        v.push(rc.clone());
        assert_eq!(v.len(), 1);
    }
}
