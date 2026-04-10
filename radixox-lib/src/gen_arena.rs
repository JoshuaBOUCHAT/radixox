const SENTINEL: u32 = u32::MAX;

/// Generational key. Stale after the slot is removed.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub idx: u32,
    pub generation: u32,
}

struct Slot<T> {
    data: Option<T>,
    generation: u32,
    /// Next free slot index. Only meaningful when `data.is_none()`.
    next_free: u32,
}

/// Generational arena — O(1) insert / remove / lookup, zero deps.
///
/// The generational key automatically detects dead connections:
/// once a slot is removed its Key returns `None` on any future lookup,
/// no explicit signalling needed.
///
/// Usage: `Rc<RefCell<GenArena<Conn>>>` — borrows are always synchronous
/// and one-shot (never held across `.await`).
pub struct GenArena<T> {
    slots: Vec<Slot<T>>,
    free_head: u32,
    len: u32,
}

impl<T> Default for GenArena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> GenArena<T> {
    pub fn new() -> Self {
        Self { slots: Vec::new(), free_head: SENTINEL, len: 0 }
    }

    pub fn with_capacity(cap: usize) -> Self {
        let mut a = Self::new();
        a.grow_to(cap);
        a
    }

    /// Insert a value and return its key. O(1).
    pub fn insert(&mut self, value: T) -> Key {
        if self.free_head == SENTINEL {
            self.grow_to(self.slots.len() + 1);
        }
        let idx = self.free_head;
        let slot = &mut self.slots[idx as usize];
        self.free_head = slot.next_free;
        let generation = slot.generation;
        slot.data = Some(value);
        self.len += 1;
        Key { idx, generation }
    }

    /// Remove a value by key. Returns `None` if the key is stale. O(1).
    pub fn remove(&mut self, key: Key) -> Option<T> {
        let idx = key.idx as usize;
        let slot = self.slots.get_mut(idx)?;
        if slot.generation != key.generation || slot.data.is_none() {
            return None;
        }
        let value = slot.data.take();
        slot.generation = slot.generation.wrapping_add(1);
        slot.next_free = self.free_head;
        self.free_head = key.idx;
        self.len -= 1;
        value
    }

    /// Immutable lookup. O(1).
    pub fn get(&self, key: Key) -> Option<&T> {
        self.slot(key)?.data.as_ref()
    }

    /// Mutable lookup. O(1).
    pub fn get_mut(&mut self, key: Key) -> Option<&mut T> {
        self.slot_mut(key)?.data.as_mut()
    }

    pub fn contains(&self, key: Key) -> bool {
        self.slot(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    fn slot(&self, key: Key) -> Option<&Slot<T>> {
        let s = self.slots.get(key.idx as usize)?;
        if s.generation == key.generation && s.data.is_some() { Some(s) } else { None }
    }

    fn slot_mut(&mut self, key: Key) -> Option<&mut Slot<T>> {
        let s = self.slots.get_mut(key.idx as usize)?;
        if s.generation == key.generation && s.data.is_some() { Some(s) } else { None }
    }

    /// Grow to hold at least `cap` slots, adding free slots in bulk.
    fn grow_to(&mut self, cap: usize) {
        let start = self.slots.len();
        if cap <= start { return; }
        let additional = cap - start;
        self.slots.reserve(additional);
        for i in 0..additional as u32 {
            let next_free = if i + 1 < additional as u32 {
                start as u32 + i + 1
            } else {
                self.free_head
            };
            self.slots.push(Slot { data: None, generation: 0, next_free });
        }
        self.free_head = start as u32;
    }
}

// ── Index ─────────────────────────────────────────────────────────────────────

impl<T> std::ops::Index<Key> for GenArena<T> {
    type Output = T;
    fn index(&self, key: Key) -> &T {
        self.get(key).expect("GenArena: stale or invalid key")
    }
}

impl<T> std::ops::IndexMut<Key> for GenArena<T> {
    fn index_mut(&mut self, key: Key) -> &mut T {
        self.get_mut(key).expect("GenArena: stale or invalid key")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_remove() {
        let mut a: GenArena<u32> = GenArena::new();
        let k1 = a.insert(10);
        let k2 = a.insert(20);
        assert_eq!(a.get(k1), Some(&10));
        assert_eq!(a.get(k2), Some(&20));
        assert_eq!(a.remove(k1), Some(10));
        assert_eq!(a.get(k1), None); // stale
    }

    #[test]
    fn generation_invalidates_reused_slot() {
        let mut a: GenArena<u32> = GenArena::new();
        let k1 = a.insert(1);
        a.remove(k1);
        let k2 = a.insert(2); // reuses slot, generation bumped
        assert_eq!(a.get(k1), None);
        assert_eq!(a.get(k2), Some(&2));
    }

    #[test]
    fn len_and_empty() {
        let mut a: GenArena<&str> = GenArena::new();
        assert!(a.is_empty());
        let k = a.insert("hello");
        assert_eq!(a.len(), 1);
        a.remove(k);
        assert!(a.is_empty());
    }

    #[test]
    fn index_operator() {
        let mut a: GenArena<String> = GenArena::new();
        let k = a.insert("world".to_string());
        a[k].push_str("!");
        assert_eq!(&a[k], "world!");
    }

    #[test]
    fn many_inserts_and_removes() {
        let mut a: GenArena<usize> = GenArena::new();
        let keys: Vec<_> = (0..70).map(|i| a.insert(i)).collect();
        for (i, &k) in keys.iter().enumerate() {
            assert_eq!(a.get(k), Some(&i));
        }
        for k in &keys {
            a.remove(*k);
        }
        assert!(a.is_empty());
    }
}
