use std::ops::Deref;

use bytes::Bytes;
use tinypointers::TinyBox;

const INLINE_LEN: usize = 14;

#[derive(Default)]
pub struct TinyString {
    len: u8,
    data: [u8; INLINE_LEN],
}

pub enum CompactStr {
    Inline(TinyString),
    Heap(TinyBox<Bytes>),
}

impl Deref for TinyString {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.data[..(self.len as usize)]
    }
}

impl TinyString {
    fn from_slice(data: &[u8]) -> Self {
        debug_assert!(data.len() <= INLINE_LEN);
        let mut inline_data = [0u8; INLINE_LEN];
        inline_data[..data.len()].copy_from_slice(data);
        Self {
            len: data.len() as u8,
            data: inline_data,
        }
    }
}

impl Deref for CompactStr {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        match self {
            CompactStr::Heap(v) => v,
            CompactStr::Inline(arr) => arr,
        }
    }
}

impl CompactStr {
    pub fn new() -> Self {
        Self::Inline(TinyString::default())
    }

    pub fn from_slice(data: &[u8]) -> Self {
        if data.len() > INLINE_LEN {
            return Self::Heap(TinyBox::new(Bytes::copy_from_slice(data)));
        }
        Self::Inline(TinyString::from_slice(data))
    }

    pub fn from_bytes(bytes: Bytes) -> Self {
        if bytes.len() > INLINE_LEN {
            return Self::Heap(TinyBox::new(bytes));
        }
        Self::Inline(TinyString::from_slice(&bytes))
    }

    pub fn push(&mut self, byte: u8) {
        if let CompactStr::Inline(ts) = self {
            if (ts.len as usize) < INLINE_LEN {
                ts.data[ts.len as usize] = byte;
                ts.len += 1;
                return;
            }
        }
        // Spill to heap or grow heap
        let mut new_data = Vec::with_capacity(self.len() + 1);
        new_data.extend_from_slice(self);
        new_data.push(byte);
        *self = CompactStr::Heap(TinyBox::new(Bytes::from(new_data)));
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let CompactStr::Inline(ts) = self {
            let new_len = ts.len as usize + data.len();
            if new_len <= INLINE_LEN {
                ts.data[ts.len as usize..new_len].copy_from_slice(data);
                ts.len = new_len as u8;
                return;
            }
        }
        let mut new_data = Vec::with_capacity(self.len() + data.len());
        new_data.extend_from_slice(self);
        new_data.extend_from_slice(data);
        *self = CompactStr::Heap(TinyBox::new(Bytes::from(new_data)));
    }
}

impl Default for CompactStr {
    fn default() -> Self {
        Self::new()
    }
}
