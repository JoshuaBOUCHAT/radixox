//! HiSlab : slab à indices stables sur une réservation virtuelle, places libres suivies
//! par un arbre de bitmaps (O(1) à l'insertion et au retrait). Ancien crate `hislab`
//! (0.2.6, publié), intégré ici le 2026-10-28 et réduit à ce que RadixOx utilise
//! (MEMORY_ARCH.md §18.14).
//!
//! La réservation est **alignée et dimensionnée sur 2 Mio** : sous THP `always`, une
//! huge page prise à la première écriture ne déborde jamais sur une autre région, et
//! [`HiSlab::resident_bytes`] la compte entière.

use std::cell::{Cell, UnsafeCell};
use std::ops::{Index, IndexMut};

use self::bit_block::BitBlock;
use self::bitmap_tree::BitmapTree;

mod bit_block;
mod bitmap_tree;

/// Huge page : alignement et granularité de la réservation.
const HUGE: usize = 2 << 20;

/// Réservation anonyme alignée sur 2 Mio, de taille multiple de 2 Mio (remplace `memmap2`).
/// Sans `MAP_NORESERVE`, comme avant : les pages écrites comptent dans le commit.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn new(bytes: usize) -> Result<Self, std::io::Error> {
        let len = bytes.max(1).next_multiple_of(HUGE);
        let raw = len + HUGE;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), raw, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // Rognage des deux bouts : il ne reste que `[début aligné, + len)`.
        let base = p as usize;
        let start = base.next_multiple_of(HUGE);
        unsafe {
            if start > base {
                libc::munmap(p, start - base);
            }
            let end = start + len;
            if base + raw > end {
                libc::munmap(end as *mut _, base + raw - end);
            }
        }
        Ok(Self { ptr: start as *mut u8, len })
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
    }
}
#[cfg(test)]
mod test;

// ============================================================================
// HiSlab
// ============================================================================

/// A slab allocator with O(1) insert and remove using hierarchical bitmaps.
///
/// `HiSlab` stores elements in one contiguous region reserved up front with an
/// anonymous `mmap` (indices are stable, nothing is ever moved), and tracks free
/// slots with a 4-level bitmap hierarchy, so a free slot is found in constant time
/// regardless of fragmentation.
///
/// `!Send + !Sync` by construction (raw pointer): wrap it in a `Mutex` if needed.
/// In exchange, `insert` and `remove` take `&self` (interior mutability through
/// `UnsafeCell`, sound because `!Sync` forbids any concurrent access).
///
/// For tagging support, see [`TaggedHiSlab`].
pub struct HiSlab<T: 'static> {
    data: *mut T,
    virtual_capacity: u32,
    /// Hierarchical bitmap, mutable through `&self` via `UnsafeCell`.
    /// Sound because `!Sync` forbids any concurrent access.
    pub(crate) tree: UnsafeCell<BitmapTree>,
    mmap: Mapping,
    /// Slots occupés.
    len: Cell<u32>,
    /// Au-delà de ce slot, rien n'a jamais été écrit (ou tout a été rendu) : borne de
    /// [`HiSlab::release_above`].
    /// Fin (exclue) des slots couverts par le registre des pages : multiple de la tranche
    /// de 2 Mio (la réservation est alignée). Écrire en dessous ne demande rien.
    touched_end: Cell<u32>,
    /// Part de la réservation comptée dans le registre ([`crate::pages`]).
    pages: crate::pages::Tranches,
}

impl<T> HiSlab<T> {
    /// Creates a new empty `HiSlab`.
    ///
    /// `virtual_capacity * size_of::<T>()` bytes are reserved right away in virtual
    /// memory. The pages covering `initial_capacity` slots are pre-faulted with
    /// `madvise(MADV_POPULATE_WRITE)`; the following pages are committed on demand.
    ///
    /// # Panics
    ///
    /// - If `initial_capacity > virtual_capacity`
    /// - If `virtual_capacity * size_of::<T>()` overflows `usize`
    ///
    /// # Errors
    ///
    /// Returns an error if `mmap` fails (system out of memory).
    pub fn new(initial_capacity: u32, virtual_capacity: u32) -> Result<Self, std::io::Error> {
        assert!(
            initial_capacity <= virtual_capacity,
            "initial_capacity must be <= virtual_capacity"
        );

        let virtual_bytes = (virtual_capacity as usize)
            .checked_mul(std::mem::size_of::<T>())
            .expect("virtual_capacity * size_of::<T>() dépasse usize::MAX");

        let mmap = Mapping::new(virtual_bytes)?;
        let ptr = mmap.ptr as *mut T;

        if initial_capacity > 0 {
            let initial_bytes = initial_capacity as usize * std::mem::size_of::<T>();
            unsafe {
                libc::madvise(
                    ptr as *mut libc::c_void,
                    initial_bytes,
                    libc::MADV_POPULATE_WRITE | libc::MADV_HUGEPAGE,
                );
            }
        }

        let slab = Self {
            data: ptr,
            virtual_capacity,
            tree: UnsafeCell::new(BitmapTree::new()),
            pages: crate::pages::Tranches::new(ptr as usize, HUGE),
            mmap,
            len: Cell::new(0),
            touched_end: Cell::new(0),
        };
        if initial_capacity > 0 {
            slab.grow(initial_capacity - 1);
        }
        Ok(slab)
    }
}

impl<T> HiSlab<T> {
    /// Inserts a value and returns its index.
    ///
    /// The returned index is stable and can be used to access the value
    /// until it is removed.
    #[inline(always)]
    pub fn insert(&self, val: T) -> u32 {
        // SAFETY: !Sync guarantees single-threaded access; UnsafeCell provides interior mutability.
        let idx = unsafe { &mut *self.tree.get() }.reserve_free_idx();
        assert!(
            idx < self.virtual_capacity,
            "HiSlab: virtual_capacity épuisée"
        );
        unsafe { std::ptr::write(self.data.add(idx as usize), val) };
        self.len.set(self.len.get() + 1);
        if idx >= self.touched_end.get() {
            self.grow(idx);
        }
        idx
    }

    /// Le slot `idx` est au-delà de la partie comptée : une tranche de plus au registre
    /// (une fois par 2 Mio de slots).
    #[cold]
    #[inline(never)]
    fn grow(&self, idx: u32) {
        let size = std::mem::size_of::<T>().max(1);
        self.pages.ensure((idx as usize + 1) * size);
        let end = (self.pages.covered() / size).min(self.virtual_capacity as usize);
        self.touched_end.set(end as u32);
    }

    /// Removes the element at the given index and returns it, or `None` if the slot is empty.
    ///
    /// The slot becomes available for future insertions.
    pub fn remove(&self, idx: u32) -> Option<T> {
        if !self.is_occupied(idx) {
            return None;
        }
        // SAFETY: !Sync guarantees single-threaded access.
        unsafe { &mut *self.tree.get() }.clear_bit(idx);
        self.len.set(self.len.get() - 1);
        Some(unsafe { std::ptr::read(self.data.add(idx as usize)) })
    }

    /// Number of occupied slots.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len.get()
    }

    /// `true` if no slot is occupied.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len.get() == 0
    }

    /// Highest occupied slot, or `None` if the slab is empty. Scans the level-1 bitmap
    /// downwards one 64-bit word at a time.
    pub fn last_occupied(&self) -> Option<u32> {
        let lvl1 = &unsafe { &*self.tree.get() }.lvl1;
        // SAFETY: see `copy_occupancy`, a `Vec<BitBlock>` is a contiguous run of u64.
        let words =
            unsafe { std::slice::from_raw_parts(lvl1.as_ptr() as *const u64, lvl1.len() * 8) };
        let w = words.iter().rposition(|&w| w != 0)?;
        Some((w * 64) as u32 + 63 - words[w].leading_zeros())
    }

    /// Compaction: moves the element of the occupied slot `src` into the lowest free slot,
    /// if that slot is below `src`, and returns it (`None`: no free slot below, nothing
    /// moved). The value is moved bitwise: the caller fixes whatever refers to `src`.
    ///
    /// # Safety
    /// `src` is occupied, and no reference into the slab is alive.
    pub unsafe fn move_down(&self, src: u32) -> Option<u32> {
        debug_assert!(self.is_occupied(src));
        let tree = unsafe { &mut *self.tree.get() };
        let dst = tree.find_free_idx();
        if dst >= src {
            return None;
        }
        tree.set_bit(dst);
        unsafe { std::ptr::copy_nonoverlapping(self.data.add(src as usize), self.data.add(dst as usize), 1) };
        tree.clear_bit(src);
        Some(dst)
    }

    /// Highest occupied slot strictly below `below`, or `None`.
    pub fn prev_occupied(&self, below: u32) -> Option<u32> {
        if below == 0 {
            return None;
        }
        let lvl1 = &unsafe { &*self.tree.get() }.lvl1;
        // SAFETY: see `copy_occupancy`.
        let words =
            unsafe { std::slice::from_raw_parts(lvl1.as_ptr() as *const u64, lvl1.len() * 8) };
        let last = (below - 1) as usize;
        let mut w = (last / 64).min(words.len().checked_sub(1)?);
        let mut word = if w == last / 64 { words[w] & (u64::MAX >> (63 - last % 64)) } else { words[w] };
        loop {
            if word != 0 {
                return Some((w * 64) as u32 + 63 - word.leading_zeros());
            }
            w = w.checked_sub(1)?;
            word = words[w];
        }
    }

    /// Compaction in a pipeline: reserves the lowest free slot without writing it (its
    /// bit is set, its content undefined until [`HiSlab::move_to`]).
    ///
    /// # Safety
    /// The slot is filled by `move_to` or given back by [`HiSlab::unreserve`] before any
    /// other use of the slab.
    pub unsafe fn reserve_lowest(&self) -> u32 {
        unsafe { &mut *self.tree.get() }.reserve_free_idx()
    }

    /// Gives back a slot reserved by [`HiSlab::reserve_lowest`] and never filled.
    ///
    /// # Safety
    /// `idx` was reserved and not filled.
    pub unsafe fn unreserve(&self, idx: u32) {
        unsafe { &mut *self.tree.get() }.clear_bit(idx);
    }

    /// Moves the element of the occupied slot `src` into the reserved slot `dst` (bitwise).
    ///
    /// # Safety
    /// `src` is occupied, `dst` reserved by [`HiSlab::reserve_lowest`], no reference into the
    /// slab is alive.
    pub unsafe fn move_to(&self, src: u32, dst: u32) {
        unsafe {
            std::ptr::copy_nonoverlapping(self.data.add(src as usize), self.data.add(dst as usize), 1);
            (*self.tree.get()).clear_bit(src);
        }
        if dst >= self.touched_end.get() {
            self.grow(dst);
        }
    }

    /// Address of slot `idx`, for prefetching (no check, never dereferenced here).
    #[inline(always)]
    pub fn slot_ptr(&self, idx: u32) -> *const T {
        self.data.wrapping_add(idx as usize)
    }

    /// Returns to the system the memory of every whole 2 MiB page lying at or above slot
    /// `from` (`madvise(MADV_DONTNEED)`; the pages fault back in, zeroed, if slots there
    /// are used again). Returns the bytes released.
    ///
    /// # Safety
    /// Every slot at or above `from` is free.
    pub unsafe fn release_above(&self, from: u32) -> usize {
        const PAGE: usize = 2 << 20;
        debug_assert!(self.last_occupied().is_none_or(|l| l < from));
        let size = std::mem::size_of::<T>();
        let base = self.data as usize;
        let start = (base + from as usize * size).next_multiple_of(PAGE);
        let end = (base + self.touched_end.get() as usize * size).next_multiple_of(PAGE);
        if end <= start {
            return 0;
        }
        unsafe { libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED) };
        self.pages.shrink(start - base);
        self.touched_end.set(((start - base) / size) as u32);
        end - start
    }

    /// Returns `true` if the slot at the given index is occupied.
    #[inline(always)]
    pub fn is_occupied(&self, idx: u32) -> bool {
        unsafe { &*self.tree.get() }.is_set(idx)
    }

    /// Returns a reference to the element at the given index, or `None` if empty.
    pub fn get(&self, idx: u32) -> Option<&T> {
        if self.is_occupied(idx) {
            unsafe { Some(self.data.add(idx as usize).as_ref()?) }
        } else {
            None
        }
    }

    /// Returns a mutable reference to the element at the given index, or `None` if empty.
    pub fn get_mut(&mut self, idx: u32) -> Option<&mut T> {
        if self.is_occupied(idx) {
            unsafe { Some(self.data.add(idx as usize).as_mut()?) }
        } else {
            None
        }
    }
}

impl<T> Index<u32> for HiSlab<T> {
    type Output = T;
    fn index(&self, idx: u32) -> &Self::Output {
        self.get(idx)
            .expect("Index out of bounds or element removed")
    }
}

impl<T> IndexMut<u32> for HiSlab<T> {
    fn index_mut(&mut self, idx: u32) -> &mut Self::Output {
        self.get_mut(idx)
            .expect("Index out of bounds or element removed")
    }
}

impl<T> HiSlab<T> {
    /// Returns a reference without checking if the slot is occupied.
    ///
    /// # Safety
    /// The caller must ensure the index is valid and occupied.
    #[inline(always)]
    pub unsafe fn get_unchecked(&self, idx: u32) -> &T {
        unsafe { self.data.add(idx as usize).as_ref().unwrap() }
    }

    /// Returns a mutable reference without checking if the slot is occupied.
    ///
    /// # Safety
    /// The caller must ensure the index is valid and occupied.
    #[inline(always)]
    pub unsafe fn get_unchecked_mut(&mut self, idx: u32) -> &mut T {
        unsafe { self.data.add(idx as usize).as_mut().unwrap() }
    }
}

impl<T> HiSlab<T> {
    /// Copies the level-1 occupancy bitmap into `out` (reused across calls):
    /// slot `i` is occupied ⇔ bit `i % 64` of word `i / 64`. A single memcpy.
    /// Useful as a point-in-time presence snapshot, e.g. at the start of an
    /// incremental save. Returns the number of slots covered (`out.len() * 64`).
    pub fn copy_occupancy(&self, out: &mut Vec<u64>) -> u32 {
        let lvl1 = &unsafe { &*self.tree.get() }.lvl1;
        // SAFETY: BitBlock = [u64; 8] en repr(align(64)), taille 64 :
        // un Vec<BitBlock> est une suite contiguë de u64.
        let words =
            unsafe { std::slice::from_raw_parts(lvl1.as_ptr() as *const u64, lvl1.len() * 8) };
        out.clear();
        out.extend_from_slice(words);
        (words.len() * 64) as u32
    }

    /// Number of slots covered by the occupancy bitmap: every occupied slot is
    /// below it. Grows with the slab, never shrinks.
    #[inline]
    pub fn slot_span(&self) -> u32 {
        (unsafe { &*self.tree.get() }.lvl1.len() << 9) as u32
    }

    /// Octets couverts par le registre des pages (tranches de 2 Mio jusqu'au plus haut slot
    /// écrit, moins ce que [`HiSlab::release_above`] a rendu) : ce que les données du slab
    /// peuvent tenir résident. Les bitmaps sont sur le tas, comptés ailleurs.
    #[inline]
    pub fn touched_bytes(&self) -> usize {
        self.touched_end.get() as usize * std::mem::size_of::<T>()
    }

    /// Octets des données du slab réellement résidents (`mincore` sur la partie touchée
    /// jusqu'à la frontière de 2 Mio suivante, [`crate::resident::region`]). Pour `INFO`.
    pub fn resident_bytes(&self) -> usize {
        crate::resident::region(self.data as usize, self.touched_bytes(), self.mmap.len()) as usize
    }

    /// First occupied slot at or after `from`, or `None`. Scans the level-1 bitmap
    /// one 64-bit word at a time: resumable traversal by slices, e.g. a collector
    /// visiting a bounded number of slots per call.
    pub fn next_occupied(&self, from: u32) -> Option<u32> {
        let lvl1 = &unsafe { &*self.tree.get() }.lvl1;
        // SAFETY: see `copy_occupancy`, a `Vec<BitBlock>` is a contiguous run of u64.
        let words =
            unsafe { std::slice::from_raw_parts(lvl1.as_ptr() as *const u64, lvl1.len() * 8) };
        let mut w = from as usize / 64;
        let mut word = *words.get(w)? & (!0u64 << (from % 64));
        while word == 0 {
            w += 1;
            word = *words.get(w)?;
        }
        Some((w * 64) as u32 + word.trailing_zeros())
    }

    /// Iterates over all occupied slots with maximum performance.
    ///
    /// This is faster than using the iterator for simple operations
    /// as it avoids iterator overhead.
    pub fn for_each_occupied<F>(&self, mut f: F)
    where
        F: FnMut(u32, &T),
    {
        for (b_idx, block) in unsafe { &*self.tree.get() }.lvl1.iter().enumerate() {
            for (w_idx, &word) in block.data.iter().enumerate() {
                if word == 0 {
                    continue;
                }

                let mut temp_word = word;
                let base_idx = (b_idx << 9) | (w_idx << 6);

                while temp_word != 0 {
                    let bit = temp_word.trailing_zeros();
                    let final_idx = base_idx | (bit as usize);

                    unsafe {
                        f(final_idx as u32, self.data.add(final_idx).as_ref().unwrap());
                    }

                    temp_word &= temp_word - 1;
                }
            }
        }
    }
}

pub struct SlabIter<'a, T> {
    slab: *const T,
    lvl1: &'a [BitBlock],
    block_idx: usize,
    word_idx: usize,
    current_word: u64,
}

impl<'a, T> SlabIter<'a, T> {
    fn new(slab: &'a HiSlab<T>) -> Self {
        // SAFETY: HiSlab is !Sync; single-threaded access guaranteed.
        // The BitmapTree lives at least 'a since slab: &'a HiSlab<T>.
        let tree: &'a BitmapTree = unsafe { &*slab.tree.get() };
        let first_word = tree.lvl1.first().map(|b| b.data[0]).unwrap_or(0);
        Self {
            slab: slab.data,
            lvl1: &tree.lvl1,
            block_idx: 0,
            word_idx: 0,
            current_word: first_word,
        }
    }
}

impl<'a, T: 'static> Iterator for SlabIter<'a, T> {
    type Item = (u32, &'a T);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while self.current_word == 0 {
            self.word_idx += 1;

            if self.word_idx >= 8 {
                self.word_idx = 0;
                self.block_idx += 1;
            }

            self.current_word = self.lvl1.get(self.block_idx)?.data[self.word_idx];
        }

        let bit = self.current_word.trailing_zeros();
        let final_idx = (self.block_idx << 9) | (self.word_idx << 6) | (bit as usize);

        self.current_word &= self.current_word - 1;

        unsafe { Some((final_idx as u32, self.slab.add(final_idx).as_ref().unwrap())) }
    }
}

impl<'a, T> IntoIterator for &'a HiSlab<T> {
    type Item = (u32, &'a T);
    type IntoIter = SlabIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        SlabIter::new(self)
    }
}

pub struct SlabIterMut<'a, T> {
    data_ptr: *mut T,
    lvl1: &'a [BitBlock],
    block_idx: usize,
    word_idx: usize,
    current_word: u64,
    _marker: std::marker::PhantomData<&'a mut T>,
}

impl<'a, T> SlabIterMut<'a, T> {
    fn new(slab: &'a mut HiSlab<T>) -> Self {
        // SAFETY: we hold &'a mut HiSlab<T>, exclusive access guaranteed.
        let tree: &'a BitmapTree = unsafe { &*slab.tree.get() };
        let first_word = tree.lvl1.first().map(|b| b.data[0]).unwrap_or(0);
        Self {
            data_ptr: slab.data,
            lvl1: &tree.lvl1,
            block_idx: 0,
            word_idx: 0,
            current_word: first_word,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'a, T> Iterator for SlabIterMut<'a, T> {
    type Item = (u32, &'a mut T);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while self.current_word == 0 {
            self.word_idx += 1;

            if self.word_idx >= 8 {
                self.word_idx = 0;
                self.block_idx += 1;
            }

            self.current_word = self.lvl1.get(self.block_idx)?.data[self.word_idx];
        }

        let bit = self.current_word.trailing_zeros();
        let final_idx = (self.block_idx << 9) | (self.word_idx << 6) | (bit as usize);
        self.current_word &= self.current_word - 1;

        unsafe { Some((final_idx as u32, &mut *self.data_ptr.add(final_idx))) }
    }
}

impl<'a, T> IntoIterator for &'a mut HiSlab<T> {
    type Item = (u32, &'a mut T);
    type IntoIter = SlabIterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        SlabIterMut::new(self)
    }
}

pub struct SlabIntoIter<T> {
    data: *mut T,
    lvl1: Vec<BitBlock>,
    block_idx: usize,
    word_idx: usize,
    current_word: u64,
    _mmap: Mapping,
}

impl<T> Iterator for SlabIntoIter<T> {
    type Item = (u32, T);
    fn next(&mut self) -> Option<Self::Item> {
        while self.current_word == 0 {
            self.word_idx += 1;

            if self.word_idx >= 8 {
                self.word_idx = 0;
                self.block_idx += 1;
            }

            self.current_word = self.lvl1.get(self.block_idx)?.data[self.word_idx];
        }

        let bit = self.current_word.trailing_zeros();
        let final_idx = (self.block_idx << 9) | (self.word_idx << 6) | (bit as usize);

        self.current_word &= self.current_word - 1;

        let value = unsafe { std::ptr::read(self.data.add(final_idx)) };
        Some((final_idx as u32, value))
    }
}

impl<T> IntoIterator for HiSlab<T> {
    type Item = (u32, T);
    type IntoIter = SlabIntoIter<T>;

    fn into_iter(mut self) -> Self::IntoIter {
        let data = self.data;
        let tree = self.tree.get_mut();
        let lvl1 = std::mem::take(&mut tree.lvl1);
        let _lvl2 = std::mem::take(&mut tree.lvl2);
        let _lvl3 = std::mem::take(&mut tree.lvl3);
        let first_word = lvl1.first().map(|b| b.data[0]).unwrap_or(0);
        // Safety: ptr::read + forget is the standard pattern for moving out of a
        // Drop type field-by-field.
        let mmap = unsafe { std::ptr::read(&self.mmap) };
        std::mem::forget(self);

        SlabIntoIter {
            data,
            lvl1,
            block_idx: 0,
            word_idx: 0,
            current_word: first_word,
            _mmap: mmap,
        }
    }
}

impl<T> Drop for SlabIntoIter<T> {
    fn drop(&mut self) {
        for _ in self.by_ref() {}
    }
}

impl<T> Drop for HiSlab<T> {
    fn drop(&mut self) {
        for (b_idx, block) in self.tree.get_mut().lvl1.iter().enumerate() {
            for (w_idx, &word) in block.data.iter().enumerate() {
                if word == 0 {
                    continue;
                }
                let mut temp_word = word;
                let base_idx = (b_idx << 9) | (w_idx << 6);
                while temp_word != 0 {
                    let bit = temp_word.trailing_zeros();
                    let final_idx = base_idx | (bit as usize);
                    unsafe {
                        std::ptr::drop_in_place(self.data.add(final_idx));
                    }
                    temp_word &= temp_word - 1;
                }
            }
        }
    }
}

impl<T> HiSlab<T> {
    /// Nombre d'éléments occupés, compté sur le bitmap de niveau 1.
    pub fn count_occupied(&self) -> usize {
        unsafe { &*self.tree.get() }.lvl1.iter().map(|b| b.popcnt() as usize).sum()
    }
}

// ============================================================================
// TaggedHiSlab
// ============================================================================

/// A slab allocator with tagging support, built on top of [`HiSlab`].
///
/// `TaggedHiSlab` wraps a [`HiSlab`] and maintains a second bitmap tree to track
/// "tagged" elements, enabling O(1) operations and efficient iteration over
/// tagged elements only.
///
/// All base slab operations are delegated to the inner [`HiSlab`].
pub struct TaggedHiSlab<T: 'static> {
    inner: HiSlab<T>,
    tagged_tree: UnsafeCell<BitmapTree>,
}

impl<T> TaggedHiSlab<T> {
    /// Creates a new empty `TaggedHiSlab`.
    ///
    /// See [`HiSlab::new`] for the meaning of `initial_capacity` and `virtual_capacity`.
    pub fn new(initial_capacity: u32, virtual_capacity: u32) -> Result<Self, std::io::Error> {
        Ok(Self {
            inner: HiSlab::new(initial_capacity, virtual_capacity)?,
            tagged_tree: UnsafeCell::new(BitmapTree::new()),
        })
    }
}

impl<T: 'static> TaggedHiSlab<T> {
    /// Inserts a value and returns its index. The element is not tagged.
    #[inline(always)]
    pub fn insert(&self, val: T) -> u32 {
        let idx = self.inner.insert(val);
        // Sécurité : effacer le tag si le slot réutilisé était taggé
        unsafe { &mut *self.tagged_tree.get() }.clear_bit(idx);
        idx
    }

    /// Removes the element at the given index and returns it, or `None` if the slot is empty.
    ///
    /// Also clears the tagged flag if set.
    pub fn remove(&self, idx: u32) -> Option<T> {
        let result = self.inner.remove(idx);
        if result.is_some() {
            unsafe { &mut *self.tagged_tree.get() }.clear_bit(idx);
        }
        result
    }

    /// Returns `true` if the slot at the given index is occupied.
    #[inline(always)]
    pub fn is_occupied(&self, idx: u32) -> bool {
        self.inner.is_occupied(idx)
    }

    /// Returns a reference to the element at the given index, or `None` if empty.
    pub fn get(&self, idx: u32) -> Option<&T> {
        self.inner.get(idx)
    }

    /// Returns a mutable reference to the element at the given index, or `None` if empty.
    pub fn get_mut(&mut self, idx: u32) -> Option<&mut T> {
        self.inner.get_mut(idx)
    }

    /// Returns a reference without checking if the slot is occupied.
    ///
    /// # Safety
    /// The caller must ensure the index is valid and occupied.
    #[inline(always)]
    pub unsafe fn get_unchecked(&self, idx: u32) -> &T {
        unsafe { self.inner.get_unchecked(idx) }
    }

    /// Returns a mutable reference without checking if the slot is occupied.
    ///
    /// # Safety
    /// The caller must ensure the index is valid and occupied.
    #[inline(always)]
    pub unsafe fn get_unchecked_mut(&mut self, idx: u32) -> &mut T {
        unsafe { self.inner.get_unchecked_mut(idx) }
    }

    /// See [`HiSlab::copy_occupancy`].
    pub fn copy_occupancy(&self, out: &mut Vec<u64>) -> u32 {
        self.inner.copy_occupancy(out)
    }

    /// See [`HiSlab::len`].
    #[inline]
    pub fn len(&self) -> u32 {
        self.inner.len()
    }

    /// See [`HiSlab::is_empty`].
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// See [`HiSlab::last_occupied`].
    pub fn last_occupied(&self) -> Option<u32> {
        self.inner.last_occupied()
    }

    /// See [`HiSlab::move_down`]. The tag follows the element.
    ///
    /// # Safety
    /// Same as [`HiSlab::move_down`].
    pub unsafe fn move_down(&self, src: u32) -> Option<u32> {
        let dst = unsafe { self.inner.move_down(src) }?;
        let tags = unsafe { &mut *self.tagged_tree.get() };
        if tags.is_set(src) {
            tags.clear_bit(src);
            tags.set_bit(dst);
        }
        Some(dst)
    }

    /// See [`HiSlab::prev_occupied`].
    pub fn prev_occupied(&self, below: u32) -> Option<u32> {
        self.inner.prev_occupied(below)
    }

    /// See [`HiSlab::reserve_lowest`].
    ///
    /// # Safety
    /// Same as [`HiSlab::reserve_lowest`].
    pub unsafe fn reserve_lowest(&self) -> u32 {
        unsafe { self.inner.reserve_lowest() }
    }

    /// See [`HiSlab::unreserve`].
    ///
    /// # Safety
    /// Same as [`HiSlab::unreserve`].
    pub unsafe fn unreserve(&self, idx: u32) {
        unsafe { self.inner.unreserve(idx) }
    }

    /// See [`HiSlab::move_to`]. The tag follows the element.
    ///
    /// # Safety
    /// Same as [`HiSlab::move_to`].
    pub unsafe fn move_to(&self, src: u32, dst: u32) {
        unsafe { self.inner.move_to(src, dst) };
        let tags = unsafe { &mut *self.tagged_tree.get() };
        if tags.is_set(src) {
            tags.clear_bit(src);
            tags.set_bit(dst);
        }
    }

    /// See [`HiSlab::slot_ptr`].
    #[inline(always)]
    pub fn slot_ptr(&self, idx: u32) -> *const T {
        self.inner.slot_ptr(idx)
    }

    /// See [`HiSlab::release_above`].
    ///
    /// # Safety
    /// Same as [`HiSlab::release_above`].
    pub unsafe fn release_above(&self, from: u32) -> usize {
        unsafe { self.inner.release_above(from) }
    }

    /// See [`HiSlab::slot_span`].
    #[inline]
    pub fn slot_span(&self) -> u32 {
        self.inner.slot_span()
    }

    /// See [`HiSlab::touched_bytes`].
    #[inline]
    pub fn touched_bytes(&self) -> usize {
        self.inner.touched_bytes()
    }

    /// See [`HiSlab::resident_bytes`].
    pub fn resident_bytes(&self) -> usize {
        self.inner.resident_bytes()
    }

    /// See [`HiSlab::next_occupied`].
    pub fn next_occupied(&self, from: u32) -> Option<u32> {
        self.inner.next_occupied(from)
    }

    /// Premier slot tagué à `from` ou après, ou `None` : lit le bitmap des tags de niveau 1
    /// mot par mot (balayage de l'évicteur TTL, sans tirage).
    pub fn next_tagged(&self, from: u32) -> Option<u32> {
        let lvl1 = &unsafe { &*self.tagged_tree.get() }.lvl1;
        // SAFETY: comme `HiSlab::next_occupied`, un `Vec<BitBlock>` est une suite de u64.
        let words = unsafe { std::slice::from_raw_parts(lvl1.as_ptr() as *const u64, lvl1.len() * 8) };
        let mut w = from as usize / 64;
        let mut word = *words.get(w)? & (!0u64 << (from % 64));
        while word == 0 {
            w += 1;
            word = *words.get(w)?;
        }
        Some((w * 64) as u32 + word.trailing_zeros())
    }

    /// Iterates over all occupied slots with maximum performance.
    pub fn for_each_occupied<F>(&self, f: F)
    where
        F: FnMut(u32, &T),
    {
        self.inner.for_each_occupied(f)
    }
}

impl<T: 'static> Index<u32> for TaggedHiSlab<T> {
    type Output = T;
    fn index(&self, idx: u32) -> &Self::Output {
        self.get(idx)
            .expect("Index out of bounds or element removed")
    }
}

impl<T: 'static> IndexMut<u32> for TaggedHiSlab<T> {
    fn index_mut(&mut self, idx: u32) -> &mut Self::Output {
        self.get_mut(idx)
            .expect("Index out of bounds or element removed")
    }
}

impl<'a, T: 'static> IntoIterator for &'a TaggedHiSlab<T> {
    type Item = (u32, &'a T);
    type IntoIter = SlabIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        SlabIter::new(&self.inner)
    }
}

impl<'a, T: 'static> IntoIterator for &'a mut TaggedHiSlab<T> {
    type Item = (u32, &'a mut T);
    type IntoIter = SlabIterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        SlabIterMut::new(&mut self.inner)
    }
}

impl<T: 'static> IntoIterator for TaggedHiSlab<T> {
    type Item = (u32, T);
    type IntoIter = SlabIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        // tagged_tree contient uniquement des bitmaps (pas de données), il sera
        // droppé normalement. inner.into_iter() prend ownership des données.
        let TaggedHiSlab {
            inner,
            tagged_tree: _,
        } = self;
        inner.into_iter()
    }
}

// ============================================================================
// TaggedHiSlab — méthodes de tagging
// ============================================================================

impl<T: 'static> TaggedHiSlab<T> {
    /// Inserts a value with the tagged flag set and returns its index.
    #[inline(always)]
    pub fn insert_tagged(&self, val: T) -> u32 {
        let idx = self.inner.insert(val);
        unsafe { &mut *self.tagged_tree.get() }.set_bit(idx);
        idx
    }

    /// Tags an existing element at the given index.
    ///
    /// Returns `true` if the element was successfully tagged (it exists and wasn't already tagged).
    /// Returns `false` if the slot is empty or already tagged.
    #[inline]
    pub fn tag(&self, idx: u32) -> bool {
        if !self.inner.is_occupied(idx) {
            return false;
        }
        let tagged = unsafe { &mut *self.tagged_tree.get() };
        if tagged.is_set(idx) {
            return false;
        }
        tagged.set_bit(idx);
        true
    }

    /// Untags an element at the given index.
    ///
    /// Returns `true` if the element was successfully untagged.
    /// Returns `false` if the slot is empty or wasn't tagged.
    #[inline]
    pub fn untag(&self, idx: u32) -> bool {
        if !self.inner.is_occupied(idx) {
            return false;
        }
        let tagged = unsafe { &mut *self.tagged_tree.get() };
        if !tagged.is_set(idx) {
            return false;
        }
        tagged.clear_bit(idx);
        true
    }

    /// Returns `true` if the slot at the given index is tagged.
    #[inline(always)]
    pub fn is_tagged(&self, idx: u32) -> bool {
        unsafe { &*self.tagged_tree.get() }.is_set(idx)
    }
}

impl<T> TaggedHiSlab<T> {
    /// Nombre d'éléments occupés.
    #[inline]
    pub fn count_occupied(&self) -> usize {
        self.inner.count_occupied()
    }

    /// Un élément tagué au hasard ; `below(n)` rend un entier uniforme dans `[0, n)`.
    /// `None` si aucun n'est tagué.
    pub fn random_tagged(&self, below: impl FnOnce(u32) -> u32) -> Option<(u32, &T)> {
        let idx = unsafe { &*self.tagged_tree.get() }.random_set(below)?;
        unsafe { Some((idx, self.inner.get_unchecked(idx))) }
    }
}
