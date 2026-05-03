use std::cell::RefCell;

use arrayvec::ArrayVec;
use hislab::HiSlab;

pub(crate) const CHILDS_SIZE: usize = 6;
const ASCII_MAX_CHAR: usize = 127;
const EXA_DIGIT_COUNT: usize = 16;
const LIGHT_OVERFLOW_SIZE: usize = EXA_DIGIT_COUNT - CHILDS_SIZE; // 10
const HUGE_OVERFLOW_CAPACITY: usize = ASCII_MAX_CHAR - CHILDS_SIZE; // 121

// ── Childs ───────────────────────────────────────────────────────────────────

#[repr(packed)]
pub(crate) struct Childs {
    idxs: [u32; CHILDS_SIZE],
    radixs: [u8; CHILDS_SIZE],
    len: u8,
}

pub(crate) trait ChildAble {
    fn find(&self, radix: u8) -> Option<u32>;
    fn push(&mut self, radix: u8, idx: u32);
    fn remove(&mut self, radix: u8) -> Option<u32>;
    fn is_empty(&self) -> bool;
    fn iter(&self) -> impl Iterator<Item = (u8, u32)>;
}

impl Default for Childs {
    fn default() -> Self {
        Self {
            idxs: [u32::MAX; CHILDS_SIZE],
            radixs: [0; CHILDS_SIZE],
            len: 0,
        }
    }
}

impl ChildAble for Childs {
    fn find(&self, radix: u8) -> Option<u32> {
        for (index, actual_radix) in (0..self.len()).map(|i| (i, self.radixs[i])) {
            if actual_radix == radix {
                return Some(self.idxs[index]);
            }
        }
        None
    }

    fn push(&mut self, radix: u8, idx: u32) {
        assert!(!self.is_full() && radix != 0);
        let len = self.len();
        self.idxs[len] = idx;
        self.radixs[len] = radix;
        self.len += 1;
    }

    fn remove(&mut self, radix: u8) -> Option<u32> {
        let pos = self.radixs[..self.len()].iter().position(|&c| c == radix)?;
        let last_idx = self.len() - 1;
        self.radixs[pos] = self.radixs[last_idx];
        let cpy = self.idxs[pos];
        self.idxs[pos] = self.idxs[last_idx];
        self.len -= 1;
        Some(cpy)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn iter(&self) -> impl Iterator<Item = (u8, u32)> {
        let len = self.len();
        (0..len).map(|i| (self.radixs[i], self.idxs[i]))
    }
}

impl Childs {
    pub(crate) fn is_full(&self) -> bool {
        self.len == CHILDS_SIZE as u8
    }

    pub(crate) fn old_get_single_child(&self) -> Option<(u8, u32)> {
        if self.len() == 1 {
            Some((self.radixs[0], self.idxs[0]))
        } else {
            None
        }
    }

    fn len(&self) -> usize {
        self.len as usize
    }
}

// ── HugeOverflow (thread-local HiSlab) ───────────────────────────────────────

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct HugeChildRegistry {
    radix: u8,
    idx: u32,
}

#[repr(align(64))]
#[derive(Default)]
pub(crate) struct HugeOverflow {
    entries: ArrayVec<HugeChildRegistry, HUGE_OVERFLOW_CAPACITY>,
}

impl HugeOverflow {
    fn new(radix: u8, idx: u32) -> Self {
        let mut entries = ArrayVec::new_const();
        entries.push(HugeChildRegistry { radix, idx });
        Self { entries }
    }
}

impl ChildAble for HugeOverflow {
    fn find(&self, radix: u8) -> Option<u32> {
        self.entries.iter().find(|e| e.radix == radix).map(|e| e.idx)
    }

    fn push(&mut self, radix: u8, idx: u32) {
        self.entries.push(HugeChildRegistry { radix, idx });
    }

    fn remove(&mut self, radix: u8) -> Option<u32> {
        let pos = self.entries.iter().position(|e| e.radix == radix)?;
        Some(self.entries.swap_remove(pos).idx)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (u8, u32)> {
        self.entries.iter().map(|e| (e.radix, e.idx))
    }
}

thread_local! {
    static HUGE_OVF: RefCell<HiSlab<HugeOverflow>> =
        RefCell::new(HiSlab::new(64, 65536).expect("can't alloc HugeOverflow slab"));
}

fn with_huge_ovf<R>(f: impl FnOnce(&mut HiSlab<HugeOverflow>) -> R) -> R {
    HUGE_OVF.with(|s| f(&mut s.borrow_mut()))
}

// ── Overflow ──────────────────────────────────────────────────────────────────

pub(crate) struct Overflow {
    idxs: [u32; LIGHT_OVERFLOW_SIZE],
    radixs: [u8; LIGHT_OVERFLOW_SIZE],
    len: u8,
    big_overflow_idx: Option<u32>,
}

impl Default for Overflow {
    fn default() -> Self {
        Self {
            idxs: [u32::MAX; LIGHT_OVERFLOW_SIZE],
            radixs: [0; LIGHT_OVERFLOW_SIZE],
            len: 0,
            big_overflow_idx: None,
        }
    }
}

impl ChildAble for Overflow {
    fn find(&self, radix: u8) -> Option<u32> {
        for i in 0..self.len as usize {
            if self.radixs[i] == radix {
                return Some(self.idxs[i]);
            }
        }
        if let Some(huge_idx) = self.big_overflow_idx {
            return with_huge_ovf(|s| s.get(huge_idx)?.find(radix));
        }
        None
    }

    fn push(&mut self, radix: u8, idx: u32) {
        if (self.len as usize) < LIGHT_OVERFLOW_SIZE {
            let len = self.len as usize;
            self.radixs[len] = radix;
            self.idxs[len] = idx;
            self.len += 1;
        } else if let Some(huge_idx) = self.big_overflow_idx {
            with_huge_ovf(|s| {
                s.get_mut(huge_idx)
                    .expect("big_overflow_idx must be valid")
                    .push(radix, idx)
            });
        } else {
            let new_huge_idx = with_huge_ovf(|s| s.insert(HugeOverflow::new(radix, idx)));
            self.big_overflow_idx = Some(new_huge_idx);
        }
    }

    fn remove(&mut self, radix: u8) -> Option<u32> {
        for i in 0..self.len as usize {
            if self.radixs[i] == radix {
                let last = self.len as usize - 1;
                let val = self.idxs[i];
                self.radixs[i] = self.radixs[last];
                self.idxs[i] = self.idxs[last];
                self.len -= 1;
                return Some(val);
            }
        }
        let huge_idx = self.big_overflow_idx?;
        let (val, is_empty) = with_huge_ovf(|s| {
            let huge = s.get_mut(huge_idx)?;
            let val = huge.remove(radix)?;
            Some((val, huge.is_empty()))
        })?;
        if is_empty {
            with_huge_ovf(|s| s.remove(huge_idx));
            self.big_overflow_idx = None;
        }
        Some(val)
    }

    fn is_empty(&self) -> bool {
        self.len == 0 && self.big_overflow_idx.is_none()
    }

    fn iter(&self) -> impl Iterator<Item = (u8, u32)> {
        let inline: Vec<(u8, u32)> = (0..self.len as usize)
            .map(|i| (self.radixs[i], self.idxs[i]))
            .collect();
        let huge: Vec<(u8, u32)> = self
            .big_overflow_idx
            .map(|huge_idx| {
                with_huge_ovf(|s| {
                    s.get(huge_idx)
                        .map(|h| h.iter().collect())
                        .unwrap_or_default()
                })
            })
            .unwrap_or_default();
        inline.into_iter().chain(huge)
    }
}

impl Overflow {
    fn drop_huge(&mut self) {
        if let Some(huge_idx) = self.big_overflow_idx.take() {
            with_huge_ovf(|s| s.remove(huge_idx));
        }
    }
}

// ── OverflowArena ─────────────────────────────────────────────────────────────

#[repr(align(64))]
enum OverflowSlot {
    Item(Overflow),
    NextFree(u32),
}

pub(crate) struct OverflowArena {
    slots: Vec<OverflowSlot>,
    free_head: u32, // u32::MAX = no free slots
    count: usize,
}

impl OverflowArena {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_head: u32::MAX,
            count: 0,
        }
    }

    pub(crate) fn alloc(&mut self) -> u32 {
        if self.free_head != u32::MAX {
            let idx = self.free_head;
            let OverflowSlot::NextFree(next) = self.slots[idx as usize] else {
                unreachable!("free_head points to occupied slot")
            };
            self.free_head = next;
            self.slots[idx as usize] = OverflowSlot::Item(Overflow::default());
            self.count += 1;
            idx
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(OverflowSlot::Item(Overflow::default()));
            self.count += 1;
            idx
        }
    }

    /// Frees the slot and its associated HugeOverflow entry if any.
    pub(crate) fn free(&mut self, idx: u32) {
        if let OverflowSlot::Item(ref mut overflow) = self.slots[idx as usize] {
            overflow.drop_huge();
        }
        self.slots[idx as usize] = OverflowSlot::NextFree(self.free_head);
        self.free_head = idx;
        self.count -= 1;
    }

    pub(crate) fn get(&self, idx: u32) -> Option<&Overflow> {
        match &self.slots[idx as usize] {
            OverflowSlot::Item(o) => Some(o),
            OverflowSlot::NextFree(_) => None,
        }
    }

    pub(crate) fn get_mut(&mut self, idx: u32) -> Option<&mut Overflow> {
        match &mut self.slots[idx as usize] {
            OverflowSlot::Item(o) => Some(o),
            OverflowSlot::NextFree(_) => None,
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.count
    }
}
