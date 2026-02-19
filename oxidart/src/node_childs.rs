use arrayvec::ArrayVec;

pub(crate) const CHILDS_SIZE: usize = 9;
const ASCII_MAX_CHAR: usize = 127;
pub(crate) const HUGE_CHILDS_SIZE: usize = ASCII_MAX_CHAR - CHILDS_SIZE;

#[repr(C)]
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
        let pos = self.radixs.iter().position(|&c| c == radix)?;
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

    /// Retourne (radix, idx) si exactement 1 enfant
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

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct HugeChildRegistry {
    radix: u8,
    idx: u32,
}

#[repr(align(64))]
#[derive(Default)]
pub(crate) struct HugeChilds {
    entries: ArrayVec<HugeChildRegistry, HUGE_CHILDS_SIZE>,
}

impl HugeChilds {
    pub(crate) fn new(radix: u8, idx: u32) -> Self {
        let mut entries = ArrayVec::new_const();
        entries.push(HugeChildRegistry { radix, idx });
        Self { entries }
    }
}

impl ChildAble for HugeChilds {
    fn find(&self, radix: u8) -> Option<u32> {
        self.entries
            .iter()
            .find(|e| e.radix == radix)
            .map(|e| e.idx)
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
