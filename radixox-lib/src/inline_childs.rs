use std::mem::MaybeUninit;

use crate::shared_byte::SharedByte;

/// Struct-of-arrays over `(prefix: u8, childs_idx: u32, inline: SharedByte)`,
/// capacity `N` fixed at compile time, one shared `len: u8` for all three.
///
/// Replaces three separately-declared `[T; N]` fields (as in
/// `prefix: [u8; N]`, `childs_idx: [u32; N]`, `inline_childs: [SharedByte; N]`)
/// with a single type that keeps them in lockstep and drops `inline_childs`
/// correctly (plain `[SharedByte; N]` can't be `MaybeUninit`-partial on its
/// own without this kind of wrapper).
///
/// Field order is what makes this compact: with `#[repr(C)]`, laying fields
/// out by *descending* alignment — `inline_childs` (align 8), then
/// `childs_idx` (align 4), then `prefix`/`len` (align 1) — means every field
/// already starts at an offset that satisfies the next field's alignment, so
/// no inter-field padding is ever inserted. The only padding left is
/// trailing padding rounding the whole struct up to `align_of::<SharedByte>()
/// == 8`, which is unavoidable in any layout containing a `SharedByte`.
#[repr(C, packed(8))]
pub struct InlineChilds<const N: usize> {
    inline_childs: [MaybeUninit<SharedByte>; N],
    childs_idx: [MaybeUninit<u32>; N],
    prefix: [MaybeUninit<u8>; N],
    len: u8,
}

impl<const N: usize> InlineChilds<N> {
    pub fn new() -> Self {
        const { assert!(N <= u8::MAX as usize, "InlineChilds: N must fit in u8") };
        Self {
            inline_childs: [const { MaybeUninit::uninit() }; N],
            childs_idx: [const { MaybeUninit::uninit() }; N],
            prefix: [const { MaybeUninit::uninit() }; N],
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

    pub fn push(&mut self, prefix: u8, childs_idx: u32, inline: SharedByte) {
        assert!(!self.is_full());
        let i = self.len();
        self.prefix[i].write(prefix);
        self.childs_idx[i].write(childs_idx);
        self.inline_childs[i].write(inline);
        self.len += 1;
    }

    /// Swap-remove: drops the removed `SharedByte`, moves the last slot into `i`.
    pub fn remove_swap(&mut self, i: usize) -> (u8, u32, SharedByte) {
        assert!(i < self.len());
        let last = self.len() - 1;
        self.len -= 1;
        unsafe {
            let prefix = self.prefix[i].assume_init_read();
            let childs_idx = self.childs_idx[i].assume_init_read();
            let inline = self.inline_childs[i].assume_init_read();
            if i != last {
                self.prefix[i] = MaybeUninit::new(self.prefix[last].assume_init_read());
                self.childs_idx[i] = MaybeUninit::new(self.childs_idx[last].assume_init_read());
                self.inline_childs[i] =
                    MaybeUninit::new(self.inline_childs[last].assume_init_read());
            }
            (prefix, childs_idx, inline)
        }
    }

    #[inline]
    pub fn get_prefix(&self, i: usize) -> u8 {
        assert!(i < self.len());
        unsafe { self.prefix[i].assume_init_read() }
    }

    #[inline]
    pub fn get_childs_idx(&self, i: usize) -> u32 {
        assert!(i < self.len());
        unsafe { self.childs_idx[i].assume_init_read() }
    }

    #[inline]
    pub fn set_childs_idx(&mut self, i: usize, idx: u32) {
        assert!(i < self.len());
        self.childs_idx[i].write(idx);
    }

    #[inline]
    pub fn get_inline(&self, i: usize) -> &SharedByte {
        assert!(i < self.len());
        unsafe { self.inline_childs[i].assume_init_ref() }
    }

    #[inline]
    pub fn get_inline_mut(&mut self, i: usize) -> &mut SharedByte {
        assert!(i < self.len());
        unsafe { self.inline_childs[i].assume_init_mut() }
    }

    /// Linear scan for `radix`, unchecked pointer walk (no bounds checks, no
    /// iterator machinery) — same rationale as `ArrayVec::find`.
    pub fn find(&self, radix: u8) -> Option<u32> {
        let len = self.len();
        for i in 0..len {
            // SAFETY: i < len, so prefix[i]/childs_idx[i] are initialized.
            let p = unsafe { self.prefix[i].assume_init_read() };
            if p == radix {
                return Some(unsafe { self.childs_idx[i].assume_init_read() });
            }
        }
        None
    }
}

impl<const N: usize> Default for InlineChilds<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Drop for InlineChilds<N> {
    fn drop(&mut self) {
        unsafe {
            for i in 0..self.len() {
                self.inline_childs[i].assume_init_drop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_has_only_trailing_padding() {
        // inline_childs: 8*N, childs_idx: 4*N, prefix: N, len: 1 -> 13N+1,
        // rounded up to a multiple of align_of::<SharedByte>() == 8.
        const N: usize = 8;
        let raw = 13 * N + 1;
        let expected = raw.next_multiple_of(8);
        assert_eq!(size_of::<InlineChilds<N>>(), expected);
    }

    #[test]
    fn push_get_find() {
        let mut c: InlineChilds<4> = InlineChilds::new();
        c.push(b'a', 10, SharedByte::from_str("alpha"));
        c.push(b'b', 20, SharedByte::from_str("beta"));
        assert_eq!(c.len(), 2);
        assert_eq!(c.get_prefix(0), b'a');
        assert_eq!(c.get_childs_idx(1), 20);
        assert_eq!(c.get_inline(1).as_slice(), b"beta");
        assert_eq!(c.find(b'b'), Some(20));
        assert_eq!(c.find(b'z'), None);
    }

    #[test]
    fn remove_swap_drops_and_reorders() {
        let mut c: InlineChilds<4> = InlineChilds::new();
        c.push(1, 100, SharedByte::from_str("one"));
        c.push(2, 200, SharedByte::from_str("two"));
        c.push(3, 300, SharedByte::from_str("three"));
        let (p, idx, val) = c.remove_swap(0);
        assert_eq!((p, idx), (1, 100));
        assert_eq!(val.as_slice(), b"one");
        assert_eq!(c.len(), 2);
        // last element (3/300/"three") swapped into slot 0
        assert_eq!(c.get_prefix(0), 3);
        assert_eq!(c.get_childs_idx(0), 300);
    }

    #[test]
    fn drop_releases_all_inline_shared_bytes() {
        let mut c: InlineChilds<4> = InlineChilds::new();
        let s = SharedByte::from_str("shared");
        for i in 0..3u8 {
            c.push(i, i as u32, s.clone());
        }
        assert_eq!(s.rc(), 4);
        drop(c);
        assert_eq!(s.rc(), 1);
    }
}
