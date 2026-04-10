//! # OxidArt
//!
//! A high-performance, compressed Adaptive Radix Tree (ART) implementation in Rust
//! for fast key-value storage operations.
//!
//! ## Features
//!
//! - **O(k) operations**: All operations (get, set, del) run in O(k) time where k is the key length
//! - **Path compression**: Minimizes memory usage by compressing single-child paths
//! - **Prefix operations**: Supports `getn` and `deln` for prefix-based queries and deletions
//! - **Zero-copy values**: Uses `bytes::Bytes` for efficient value handling
//!
//! ## Example
//!
//! ```rust,ignore
//! use oxidart::OxidArt;
//! use bytes::Bytes;
//! use std::time::Duration;
//!
//! let mut tree = OxidArt::new();
//!
//! // Insert key-value pairs
//! tree.set(SharedByte::from_str("hello"), SharedByte::from_str("world"));
//!
//! // Insert with TTL (requires `ttl` feature, enabled by default)
//! tree.set_now(1700000000); // Update internal clock
//! tree.set_ttl(SharedByte::from_str("session"), Duration::from_secs(3600), SharedByte::from_str("data"));
//!
//! // Retrieve a value
//! assert_eq!(tree.get(SharedByte::from_str("hello")), Some(SharedByte::from_str("world")));
//!
//! // Get all entries with a prefix
//! let entries = tree.getn(SharedByte::from_str("hello"));
//!
//! // Delete a key
//! let deleted = tree.del(SharedByte::from_str("hello"));
//!
//! // Delete all keys with a prefix
//! let count = tree.deln(SharedByte::from_str("hello"));
//! ```
//!
//! ## Key Requirements
//!
//! Keys must be valid ASCII bytes. Non-ASCII keys will trigger a debug assertion.

pub mod async_command;
mod compact_str;
pub mod error;
pub mod hcommand;
mod node_childs;
pub mod scommand;
pub mod value;
pub mod zcommand;
pub mod zset_inner;

pub mod monoio;

pub mod counter;

#[cfg(feature = "regex")]
pub mod regex;

#[cfg(test)]
mod test;

#[cfg(test)]
mod test_structures;

use hislab::HiSlab;
use hislab::TaggedHiSlab;
use radixox_lib::shared_byte::SharedByte;
use rand::rngs::ThreadRng;

use crate::compact_str::CompactStr;

use crate::node_childs::ChildAble;
use crate::node_childs::Childs;
use crate::node_childs::HugeChilds;
use crate::value::Value;

/// Internal sentinel value indicating no expiration (never expires)

/// Result of a TTL lookup operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlResult {
    /// The key does not exist.
    KeyNotExist,
    /// The key exists and has a TTL (remaining seconds until expiration).
    KeyWithTtl(u64),
    /// The key exists but has no TTL (permanent).
    KeyWithoutTtl,
}

/// A compressed Adaptive Radix Tree for fast key-value storage.
///
/// `OxidArt` provides O(k) time complexity for all operations where k is the key length.
/// It uses path compression to minimize memory footprint while maintaining high performance.
///
/// # Example
///
/// ```rust,ignore
/// use oxidart::OxidArt;
/// use bytes::Bytes;
///
/// let mut tree = OxidArt::new();
/// tree.set(SharedByte::from_str("key"), SharedByte::from_str("value"));
///
/// assert_eq!(tree.get(SharedByte::from_str("key")), Some(SharedByte::from_str("value")));
/// ```
///
pub struct OxidArt {
    pub(crate) map: TaggedHiSlab<Node>,
    pub(crate) child_list: HiSlab<HugeChilds>,
    /// Current timestamp (seconds since UNIX epoch).
    /// The server is responsible for updating this via `set_now()`.
    pub now: u64,
    root_idx: u32,
}
impl Default for OxidArt {
    fn default() -> Self {
        Self::new()
    }
}

impl OxidArt {
    /// Creates a new empty `OxidArt` tree.
    ///
    /// The tree is pre-allocated with capacity for 1024 nodes.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    ///
    /// let tree = OxidArt::new();
    /// ```
    pub fn new() -> Self {
        let map = TaggedHiSlab::new(20000, 25000000).expect("Can't allocate oxidart");
        let root_idx = map.insert(Node::default());
        let child_list = HiSlab::new(1000, 25000000).expect("Can't allocate oxidart");

        Self {
            map,
            root_idx,
            child_list,
            now: 0,
        }
    }

    /// Updates the current timestamp. Call this periodically from your async runtime.
    #[inline]
    pub fn set_now(&mut self, now: u64) {
        self.now = now;
    }

    /// Returns the number of HugeChilds blocks currently allocated.
    pub fn huge_childs_count(&self) -> usize {
        self.child_list.count_occupied()
    }

    /// Returns the number of nodes currently allocated.
    pub fn node_count(&self) -> usize {
        {
            let mut n = 0usize;
            self.map.for_each_occupied(|_, _| n += 1);
            n
        }
    }

    const MAX_SAMPLE: usize = 20;
    const SAMPLE_SIZE: usize = 20;
    const THRESHOLD: usize = Self::SAMPLE_SIZE / 4; // 25% of 20

    /// Evicts expired entries using Redis-style probabilistic sampling.
    ///
    /// Algorithm:
    /// 1. Sample up to 20 random entries with TTL
    /// 2. Delete those that are expired
    /// 3. If >= 25% (5+) were expired, repeat
    /// 4. Stop when < 25% expired or no more entries
    ///
    /// Returns the total number of evicted entries.
    pub fn evict_expired(&mut self) -> usize {
        let mut rng = rand::thread_rng();
        let mut total_evicted = 0;

        for _ in 0..Self::MAX_SAMPLE {
            let (evicted_this_round, sampled) = self.evict_cycle(&mut rng);

            total_evicted += evicted_this_round;

            // Stop if we sampled less than SAMPLE_SIZE (not enough entries)
            // or if less than 25% were expired
            if sampled < Self::SAMPLE_SIZE || evicted_this_round < Self::THRESHOLD {
                break;
            }
        }

        total_evicted
    }
    fn evict_cycle(&mut self, rng: &mut ThreadRng) -> (usize, usize) {
        let mut evicted_this_round = 0;
        let mut sampled = 0;
        for _ in 0..Self::SAMPLE_SIZE {
            let Some((idx, node)) = self.map.random_tagged(rng) else {
                // No more tagged entries
                break;
            };
            sampled += 1;

            // Check if expired
            if node.is_expired(self.now) {
                let parent_idx = node.parent_idx;
                let parent_radix = node.parent_radix();

                // Don't try to delete root
                if parent_idx != u32::MAX {
                    self.delete_node_for_eviction(idx, parent_idx, parent_radix);
                    evicted_this_round += 1;
                }
            }
        }
        (evicted_this_round, sampled)
    }

    /// Delete a node during TTL eviction (similar to delete_node_inline but uses stored parent info)
    fn delete_node_for_eviction(&mut self, target_idx: u32, parent_idx: u32, parent_radix: u8) {
        let has_children = {
            let Some(node) = self.try_get_node(target_idx) else {
                return;
            };
            !node.childs.is_empty() || node.get_huge_childs_idx().is_some()
        };

        if has_children {
            // Node has children: just clear the value, keep the node
            self.get_node_mut(target_idx).val = None;
            // Untag since it no longer has a TTL value
            self.map.untag(target_idx);
            self.try_recompress(target_idx);
        } else {
            // Leaf node: remove completely
            self.map.remove(target_idx);
            self.remove_child(parent_idx, parent_radix);
            if parent_idx != self.root_idx {
                self.try_recompress(parent_idx);
            }
        }
    }

    /// Insert a node without TTL tag
    #[inline]
    fn insert(&mut self, node: Node) -> u32 {
        self.map.insert(node)
    }

    /// Insert a node with TTL tag (for random sampling during expiration)
    #[inline]
    fn insert_tagged(&mut self, node: Node) -> u32 {
        self.map.insert_tagged(node)
    }
    fn get_node(&self, idx: u32) -> &Node {
        self.try_get_node(idx)
            .expect("Call to unfailable get_node failed")
    }
    fn get_node_mut(&mut self, idx: u32) -> &mut Node {
        self.try_get_node_mut(idx)
            .expect("Call to unfailable get_node failed")
    }

    fn try_get_node(&self, idx: u32) -> Option<&Node> {
        self.map.get(idx)
    }
    fn try_get_node_mut(&mut self, idx: u32) -> Option<&mut Node> {
        self.map.get_mut(idx)
    }
    fn find(&self, idx: u32, radix: u8) -> Option<u32> {
        let node = self.try_get_node(idx)?;

        if let Some(index) = node.childs.find(radix) {
            return Some(index);
        }
        self.child_list
            .get(node.get_huge_childs_idx()?)?
            .find(radix)
    }
    fn intiate_new_huge_child(&mut self, radix: u8, idx: u32) -> u32 {
        self.child_list.insert(HugeChilds::new(radix, idx))
    }
}
impl OxidArt {
    /// Retrieves the value associated with the given key.
    ///
    /// Returns `Some(value)` if the key exists (and is not expired with `ttl` feature), or `None` otherwise.
    /// With the `ttl` feature, expired entries are automatically deleted and recompressed.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to look up. Must be valid ASCII.
    pub fn get(&mut self, key: &[u8]) -> Option<&Value> {
        let idx = self.get_idx(key)?;
        debug_assert!(key.is_ascii(), "key must be ASCII");
        return self.get_node(idx).get_value(self.now);
    }
    pub(crate) fn get_mut(&mut self, key: &[u8]) -> Option<&mut Value> {
        let idx = self.get_idx(key)?;
        debug_assert!(key.is_ascii(), "key must be ASCII");
        let now = self.now;
        self.get_node_mut(idx).get_value_mut(now)
    }
    fn get_idx(&mut self, key: &[u8]) -> Option<u32> {
        debug_assert!(key.is_ascii(), "key must be ASCII");
        let key_len = key.len();
        if key_len == 0 {
            if self.get_node(self.root_idx).is_expired(self.now) {
                self.get_node_mut(self.root_idx).val = None;
                self.try_recompress(self.root_idx);
                return None;
            }
            return Some(self.root_idx);
        }

        let mut parent_idx = self.root_idx;
        let mut parent_radix = key[0];
        let mut idx = self.find(self.root_idx, key[0])?;
        let mut cursor = 1;

        loop {
            let node = self.try_get_node(idx)?;
            match node.compare_compression_key(&key[cursor..]) {
                CompResult::Final => {
                    if node.is_expired(self.now) {
                        self.delete_node_inline(idx, parent_idx, parent_radix);
                        return None;
                    }
                    return Some(idx);
                }
                CompResult::Partial(_) => return None,
                CompResult::Path => {
                    cursor += node.compression.len();
                }
            }
            {
                parent_idx = idx;
                parent_radix = key[cursor];
            }
            idx = self.find(idx, key[cursor])?;
            cursor += 1;
        }
    }

    /// Returns the TTL status of a key.
    ///
    /// # Returns
    ///
    /// - `TtlResult::KeyNotExist` - The key does not exist or is expired
    /// - `TtlResult::KeyWithTtl(remaining)` - The key exists with remaining seconds until expiration
    /// - `TtlResult::KeyWithoutTtl` - The key exists but has no TTL (permanent)
    pub fn get_ttl(&self, key: SharedByte) -> TtlResult {
        debug_assert!(key.is_ascii(), "key must be ASCII");
        eprintln!("si test s'affiche pas la clé existe just pas");
        let idx = match self.traverse_to_key(&key) {
            Some(idx) => idx,
            None => return TtlResult::KeyNotExist,
        };

        let node = self.get_node(idx);
        eprintln!("test truc truc");
        if node.is_expired(self.now) {
            eprintln!("key not existe at all ");
            return TtlResult::KeyNotExist;
        }
        match node.exp_and_radix.exp() {
            Some(exp) => TtlResult::KeyWithTtl(exp - self.now),
            None => TtlResult::KeyWithoutTtl,
        }
    }

    /// Sets a TTL on an existing key.
    ///
    /// Returns `true` if the key exists and the TTL was set, `false` otherwise.
    pub fn expire(&mut self, key: SharedByte, ttl: std::time::Duration) -> bool {
        debug_assert!(key.is_ascii(), "key must be ASCII");
        let now = self.now;
        let Some(idx) = self.traverse_to_key(&key) else {
            return false;
        };

        let node = self.get_node_mut(idx);
        if node.is_expired(now) {
            return false;
        }

        let new_expiry = now.saturating_add(ttl.as_secs());
        let was_permanent = !node.does_expire();
        node.exp_and_radix.set_exp(new_expiry);

        if was_permanent {
            self.map.tag(idx);
        }

        was_permanent
    }

    /// Removes the TTL from a key, making it permanent.
    ///
    /// Returns `true` if the key exists and had a TTL, `false` otherwise.
    pub fn persist(&mut self, key: SharedByte) -> bool {
        debug_assert!(key.is_ascii(), "key must be ASCII");

        let Some(idx) = self.traverse_to_key(&key) else {
            return false;
        };

        let node = self.get_node_mut(idx);
        if !node.exp_and_radix.does_expire() {
            return false;
        }
        node.exp_and_radix.set_no_expiracy();
        if node.val.is_none() {
            return false;
        }
        self.map.untag(idx);
        true
    }

    /// Traverses to a key and returns the node index if found.
    pub(crate) fn traverse_to_key(&self, key: &[u8]) -> Option<u32> {
        let key_len = key.len();
        if key_len == 0 {
            return Some(self.root_idx);
        }

        let mut idx = self.find(self.root_idx, key[0])?;
        let mut cursor = 1;

        loop {
            let node = self.try_get_node(idx)?;
            match node.compare_compression_key(&key[cursor..]) {
                CompResult::Final => return Some(idx),
                CompResult::Partial(_) => return None,
                CompResult::Path => {
                    cursor += node.compression.len();
                }
            }
            idx = self.find(idx, key[cursor])?;
            cursor += 1;
        }
    }

    pub(crate) fn ensure_key(&mut self, key: &[u8]) -> u32 {
        let key_len = key.len();
        if key_len == 0 {
            return self.root_idx;
        }

        let Some(mut idx) = self.find(self.root_idx, key[0]) else {
            return self.ensure(key, self.root_idx);
        };
        let mut cursor = 1;

        loop {
            let node = self
                .try_get_node_mut(idx)
                .expect("idx exist so node should exist");
            match node.compare_compression_key(&key[cursor..]) {
                CompResult::Final => return idx,
                CompResult::Partial(common_len) => {
                    let key_rest = &key[cursor..];
                    return self.split_node(common_len, key_rest, idx, None, None);
                }
                CompResult::Path => {
                    cursor += node.compression.len();
                }
            }
            if let Some(new_idx) = self.find(idx, key[cursor]) {
                idx = new_idx;
                cursor += 1;
            } else {
                return self.ensure(&key[cursor..], idx);
            }
        }
    }
    fn ensure(&mut self, key_rest: &[u8], parent_idx: u32) -> u32 {
        let new_node = Node::new_empty_leaf(&key_rest[1..], parent_idx, key_rest[0]);
        let idx = self.insert(new_node);
        self.push_child_idx(parent_idx, idx, key_rest[0]);
        idx
    }

    /// Returns a mutable reference to the value at a node index.
    /// Returns `None` if the node has no value or the value is expired.
    /// TTL is preserved — only the value bytes can be modified.
    pub(crate) fn node_value_mut(&mut self, idx: u32) -> Option<&mut Value> {
        let now = self.now;
        let node = self.get_node_mut(idx);
        if node.is_expired(now) {
            return None;
        }
        node.val.as_mut()
    }

    /// Deletes a node inline (used for TTL expiration cleanup)
    fn delete_node_inline(&mut self, target_idx: u32, parent_idx: u32, parent_radix: u8) {
        let has_children = {
            let node = self.get_node(target_idx);
            !node.childs.is_empty() || node.get_huge_childs_idx().is_some()
        };

        if has_children {
            self.get_node_mut(target_idx).val = None;
            self.try_recompress(target_idx);
        } else {
            self.map.remove(target_idx);
            self.remove_child(parent_idx, parent_radix);
            if parent_idx != self.root_idx {
                self.try_recompress(parent_idx);
            }
        }
    }

    /// Returns all key-value pairs where the key starts with the given prefix.
    ///
    /// If the prefix is empty, returns all entries in the tree.
    ///
    /// # Arguments
    ///
    /// * `prefix` - The prefix to match. Must be valid ASCII.
    ///
    /// # Returns
    ///
    /// A vector of `(key, value)` tuples for all matching entries.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use bytes::Bytes;
    ///
    /// let mut tree = OxidArt::new();
    /// tree.set(SharedByte::from_str("user:1"), SharedByte::from_str("alice"));
    /// tree.set(SharedByte::from_str("user:2"), SharedByte::from_str("bob"));
    /// tree.set(SharedByte::from_str("post:1"), SharedByte::from_str("hello"));
    ///
    /// let users = tree.getn(SharedByte::from_str("user:"));
    /// assert_eq!(users.len(), 2);
    /// ```
    pub fn getn(&self, prefix: SharedByte) -> Vec<(SharedByte, &Value)> {
        debug_assert!(prefix.is_ascii(), "prefix must be ASCII");
        let mut results = Vec::new();
        let prefix_len = prefix.len();

        if prefix_len == 0 {
            self.collect_all(self.root_idx, Vec::new(), &mut results);
            return results;
        }

        // Traverse like get, tracking the actual path
        let mut idx = self.root_idx;
        let mut cursor = 0;
        let mut key_path: Vec<u8> = Vec::new();

        loop {
            let radix = prefix[cursor];
            let Some(child_idx) = self.find(idx, radix) else {
                return results;
            };
            idx = child_idx;
            key_path.push(radix);

            let Some(node) = self.try_get_node(idx) else {
                return results;
            };
            cursor += 1;

            match node.compare_compression_key(&prefix[cursor..]) {
                CompResult::Final => {
                    // Exact prefix found
                    key_path.extend_from_slice(&node.compression);
                    self.collect_all_from(idx, key_path, &mut results);
                    return results;
                }
                CompResult::Partial(common_len) => {
                    let prefix_rest_len = prefix_len - cursor;
                    if common_len == prefix_rest_len {
                        // Prefix ends within the compression
                        key_path.extend_from_slice(&node.compression);
                        self.collect_all_from(idx, key_path, &mut results);
                    }
                    return results;
                }
                CompResult::Path => {
                    key_path.extend_from_slice(&node.compression);
                    cursor += node.compression.len();
                }
            }
        }
    }

    /// Collects from a node whose key is already complete in key_path
    fn collect_all_from<'a>(
        &'a self,
        node_idx: u32,
        key_path: Vec<u8>,
        results: &mut Vec<(SharedByte, &'a Value)>,
    ) {
        let Some(node) = self.try_get_node(node_idx) else {
            return;
        };

        if let Some(val) = node.get_value(self.now) {
            results.push((SharedByte::from_slice(&key_path), val));
        }

        self.iter_all_children(node_idx, |radix, child_idx| {
            let mut child_key = key_path.clone();
            child_key.push(radix);
            self.collect_all(child_idx, child_key, results);
        });
    }

    /// Recursively collects, adding the node's compression
    fn collect_all<'a>(
        &'a self,
        node_idx: u32,
        mut key_prefix: Vec<u8>,
        results: &mut Vec<(SharedByte, &'a Value)>,
    ) {
        let Some(node) = self.try_get_node(node_idx) else {
            return;
        };

        key_prefix.extend_from_slice(&node.compression);

        if let Some(val) = node.get_value(self.now) {
            results.push((SharedByte::from_slice(&key_prefix), val));
        }

        self.iter_all_children(node_idx, |radix, child_idx| {
            let mut child_key = key_prefix.clone();
            child_key.push(radix);
            self.collect_all(child_idx, child_key, results);
        });
    }

    /// Iterates over all children of a node (childs + huge_childs)
    fn iter_all_children<F>(&self, node_idx: u32, mut f: F)
    where
        F: FnMut(u8, u32),
    {
        let Some(node) = self.try_get_node(node_idx) else {
            return;
        };

        for (radix, child_idx) in node.childs.iter() {
            f(radix, child_idx);
        }

        if let Some(huge_idx) = node.get_huge_childs_idx()
            && let Some(huge_childs) = self.child_list.get(huge_idx)
        {
            for (radix, child_idx) in huge_childs.iter() {
                f(radix, child_idx);
            }
        }
    }

    /// Inserts or updates a key-value pair in the tree (no expiration).
    ///
    /// If the key already exists, the value is replaced.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert. Must be valid ASCII.
    /// * `val` - The value to associate with the key.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use bytes::Bytes;
    ///
    /// let mut tree = OxidArt::new();
    ///
    /// // Insert a new key
    /// tree.set(SharedByte::from_str("key"), SharedByte::from_str("value1"));
    ///
    /// // Update an existing key
    /// tree.set(SharedByte::from_str("key"), SharedByte::from_str("value2"));
    ///
    /// assert_eq!(tree.get(SharedByte::from_str("key")), Some(SharedByte::from_str("value2")));
    /// ```
    pub fn set(&mut self, key: SharedByte, val: Value) {
        self.set_internal(key, ExpAndRadix::NO_EXPIRACY, val);
    }

    /// Inserts or updates a key-value pair with a time-to-live duration.
    ///
    /// The key will expire after `ttl` duration from the current timestamp (`self.now`).
    /// Make sure to call `tick()` or `set_now()` to keep the internal clock updated.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert. Must be valid ASCII.
    /// * `ttl` - Duration after which the key expires.
    /// * `val` - The value to associate with the key.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use bytes::Bytes;
    /// use std::time::Duration;
    ///
    /// let mut tree = OxidArt::new();
    /// tree.set_now(1000); // Set current time
    ///
    /// // Insert with 60 second TTL
    /// tree.set_ttl(SharedByte::from_str("session"), Duration::from_secs(60), SharedByte::from_str("data"));
    ///
    /// // Key expires at timestamp 1060
    /// ```
    pub fn set_ttl(&mut self, key: SharedByte, ttl: std::time::Duration, val: Value) {
        let expires_at = self.now.saturating_add(ttl.as_secs());
        self.set_internal(key, expires_at, val);
    }

    fn set_internal(&mut self, key: SharedByte, ttl: u64, val: Value) {
        debug_assert!(key.is_ascii(), "key must be ASCII");
        let key_len = key.len();
        if key_len == 0 {
            self.get_node_mut(self.root_idx).set_val(val, ttl);
            return;
        }
        let mut idx = self.root_idx;
        let mut cursor = 0;

        loop {
            let Some(child_idx) = self.find(idx, key[cursor]) else {
                self.create_node_with_val(idx, key[cursor], val, &key[(cursor + 1)..], ttl);
                return;
            };
            idx = child_idx;
            cursor += 1;
            let node_comparaison = self.get_node(idx).compare_compression_key(&key[cursor..]);
            let common_len = match node_comparaison {
                CompResult::Final => {
                    self.get_node_mut(idx).set_val(val, ttl);
                    return;
                }
                CompResult::Path => {
                    cursor += self.get_node(idx).compression.len();
                    continue;
                }
                CompResult::Partial(common_len) => common_len,
            };

            // Split: node compression only partially matches the key
            let key_rest = &key[cursor..];
            self.split_node(common_len, key_rest, idx, Some(ttl), Some(val));

            return;
        }
    }
    fn split_node(
        &mut self,
        common_len: usize,
        key_rest: &[u8],
        idx: u32,
        ttl: Option<u64>,
        mut val: Option<Value>,
    ) -> u32 {
        let val_on_intermediate = common_len == key_rest.len();
        let (old_compression, old_val, old_childs, old_huge_idx, old_exp) = {
            let node = self.get_node_mut(idx);
            let old_compression = std::mem::take(&mut node.compression);
            let old_val = node.val.take();
            let old_exp = node.exp_and_radix;
            node.exp_and_radix.set_no_expiracy();
            let old_childs = std::mem::take(&mut node.childs);
            let old_huge_idx = std::mem::replace(&mut node.huge_childs_idx, u32::MAX);

            node.compression = CompactStr::from_slice(&old_compression[..common_len]);
            if val_on_intermediate && let Some(val) = val.take() {
                node.val = Some(val);
                if let Some(ttl) = ttl {
                    node.exp_and_radix.set_exp(ttl);
                }
            }

            (old_compression, old_val, old_childs, old_huge_idx, old_exp)
        };

        // Create a node for the old content
        let old_radix = old_compression[common_len];
        // Check if old value had a TTL (needs to stay tagged)
        let old_had_ttl = old_exp.does_expire();
        let old_child = Node {
            huge_childs_idx: old_huge_idx,
            compression: CompactStr::from_slice(&old_compression[common_len + 1..]),
            val: old_val,
            childs: old_childs,
            parent_idx: idx,
            exp_and_radix: old_exp,
        };
        let old_child_idx = if old_had_ttl {
            self.insert_tagged(old_child)
        } else {
            self.insert(old_child)
        };

        self.push_child_idx(idx, old_child_idx, old_radix);

        // If the value doesn't go on the intermediate node, create a new leaf
        if !val_on_intermediate {
            let new_radix = key_rest[common_len];
            let new_compression = &key_rest[common_len + 1..];
            return if let Some(val) = val {
                self.create_node_with_val(
                    idx,
                    new_radix,
                    val,
                    new_compression,
                    ttl.unwrap_or(ExpAndRadix::NO_EXPIRACY),
                )
            } else {
                let new_node = Node::new_empty_leaf(new_compression, idx, new_radix);
                let new_node_idx = self.insert(new_node);
                self.push_child_idx(idx, new_node_idx, new_radix);
                new_node_idx
            };
        }

        idx
    }

    fn create_node_with_val(
        &mut self,
        parent_idx: u32,
        radix: u8,
        val: Value,
        compression: &[u8],
        ttl: u64,
    ) -> u32 {
        let (is_full, huge_child_idx) = {
            let father_node = self.get_node(parent_idx);
            (
                father_node.childs.is_full(),
                father_node.get_huge_childs_idx(),
            )
        };
        let new_leaf = Node::new_leaf(compression, val, ttl, parent_idx, radix);
        // Tag the node if it has a real TTL (not NO_EXPIRY)
        let inserted_idx = if ttl != ExpAndRadix::NO_EXPIRACY {
            self.insert_tagged(new_leaf)
        } else {
            self.insert(new_leaf)
        };
        match (is_full, huge_child_idx) {
            (false, _) => self
                .get_node_mut(parent_idx)
                .childs
                .push(radix, inserted_idx),
            (true, None) => {
                let new_child_idx = self.intiate_new_huge_child(radix, inserted_idx);
                self.get_node_mut(parent_idx).set_new_childs(new_child_idx);
            }
            (true, Some(huge_idx)) => {
                self.child_list
                    .get_mut(huge_idx)
                    .expect("if key exist childs should too")
                    .push(radix, inserted_idx);
            }
        }
        inserted_idx
    }

    /// Deletes a key from the tree and returns its value.
    ///
    /// Returns `Some(value)` if the key existed, or `None` if it didn't.
    /// The tree automatically recompresses paths after deletion.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to delete. Must be valid ASCII.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use bytes::Bytes;
    ///
    /// let mut tree = OxidArt::new();
    /// tree.set(SharedByte::from_str("key"), SharedByte::from_str("value"));
    ///
    /// let deleted = tree.del(SharedByte::from_str("key"));
    /// assert_eq!(deleted, Some(SharedByte::from_str("value")));
    ///
    /// // Key no longer exists
    /// assert_eq!(tree.get(SharedByte::from_str("key")), None);
    /// ```
    pub fn del(&mut self, key: &[u8]) -> Option<Value> {
        debug_assert!(key.is_ascii(), "key must be ASCII");
        let key_len = key.len();
        if key_len == 0 {
            let old_val = self.get_node_mut(self.root_idx).val.take();
            self.try_recompress(self.root_idx);
            return old_val;
        }

        // Traverse like get, keeping track of the immediate parent
        let mut parent_idx = self.root_idx;
        let mut parent_radix = key[0];
        let mut idx = self.find(parent_idx, parent_radix)?;
        let mut cursor = 1;

        let target_idx = loop {
            let node = self.try_get_node(idx)?;
            match node.compare_compression_key(&key[cursor..]) {
                CompResult::Final => break idx,
                CompResult::Partial(_) => return None,
                CompResult::Path => {
                    cursor += node.compression.len();
                }
            }

            // Continue traversal
            parent_idx = idx;
            parent_radix = key[cursor];
            idx = self.find(idx, parent_radix)?;
            cursor += 1;
        };

        // Check if the node has children
        let has_children = {
            let node = self.get_node(target_idx);
            !node.childs.is_empty() || node.get_huge_childs_idx().is_some()
        };

        if has_children {
            // Node with children: keep the node, just remove the value
            let old_val = self.get_node_mut(target_idx).val.take()?;
            self.try_recompress(target_idx);
            return Some(old_val);
        } else {
            // Node without children (leaf): completely remove from the slab
            let node = self.map.remove(target_idx)?;
            let old_val = node.val?;
            self.remove_child(parent_idx, parent_radix);
            if parent_idx != self.root_idx {
                self.try_recompress(parent_idx);
            }
            return Some(old_val);
        }
    }

    /// Deletes all keys that start with the given prefix.
    ///
    /// Returns the number of key-value pairs that were deleted.
    /// If the prefix is empty, all entries are deleted.
    ///
    /// # Arguments
    ///
    /// * `prefix` - The prefix to match. Must be valid ASCII.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use bytes::Bytes;
    ///
    /// let mut tree = OxidArt::new();
    /// tree.set(SharedByte::from_str("user:1"), SharedByte::from_str("alice"));
    /// tree.set(SharedByte::from_str("user:2"), SharedByte::from_str("bob"));
    /// tree.set(SharedByte::from_str("post:1"), SharedByte::from_str("hello"));
    ///
    /// // Delete all user entries
    /// let count = tree.deln(SharedByte::from_str("user:"));
    /// assert_eq!(count, 2);
    ///
    /// // Only post entries remain
    /// assert_eq!(tree.getn(SharedByte::from_str("")).len(), 1);
    /// ```
    pub fn deln(&mut self, prefix: &[u8]) -> usize {
        debug_assert!(prefix.is_ascii(), "prefix must be ASCII");
        let prefix_len = prefix.len();

        if prefix_len == 0 {
            // Delete everything from root (keep root node, clear its content)
            let root = self.get_node_mut(self.root_idx);
            let had_val = root.val.take().is_some();
            let childs_to_free: Vec<u32> = self.collect_child_indices(self.root_idx);

            // Clear children of root (note: root's huge_childs not freed, negligible)
            self.get_node_mut(self.root_idx).childs = Childs::default();

            let freed = self.free_subtree_iterative(childs_to_free);
            return freed + if had_val { 1 } else { 0 };
        }

        // Traverse like del
        let mut parent_idx = self.root_idx;
        let mut parent_radix = prefix[0];
        let Some(mut idx) = self.find(parent_idx, parent_radix) else {
            return 0;
        };
        let mut cursor = 1;

        let target_idx = loop {
            let Some(node) = self.try_get_node(idx) else {
                return 0;
            };

            match node.compare_compression_key(&prefix[cursor..]) {
                CompResult::Final => break idx,
                CompResult::Partial(common_len) => {
                    // Does the prefix end within the compression?
                    let prefix_rest_len = prefix_len - cursor;
                    if common_len == prefix_rest_len {
                        break idx;
                    }
                    // Divergence, nothing to delete
                    return 0;
                }
                CompResult::Path => {
                    cursor += node.compression.len();
                }
            }

            // Continue traversal
            parent_idx = idx;
            parent_radix = prefix[cursor];
            let Some(child_idx) = self.find(idx, parent_radix) else {
                return 0;
            };
            idx = child_idx;
            cursor += 1;
        };

        // Cut the link from parent
        self.remove_child(parent_idx, parent_radix);

        // Free the entire subtree (iterative DFS)
        let count = self.free_subtree_iterative(vec![target_idx]);

        // Recompression of parent (except root since get doesn't handle root with compression)
        if parent_idx != self.root_idx {
            self.try_recompress(parent_idx);
        }

        count
    }

    /// Collects all child indices of a node
    fn collect_child_indices(&self, node_idx: u32) -> Vec<u32> {
        let mut indices = Vec::new();
        let Some(node) = self.try_get_node(node_idx) else {
            return indices;
        };

        for (_, child_idx) in node.childs.iter() {
            indices.push(child_idx);
        }

        if let Some(huge_idx) = node.get_huge_childs_idx()
            && let Some(huge_childs) = self.child_list.get(huge_idx)
        {
            for (_, child_idx) in huge_childs.iter() {
                indices.push(child_idx);
            }
        }

        indices
    }

    /// Frees a subtree iteratively (DFS), returns the number of deleted values
    fn free_subtree_iterative(&mut self, initial_nodes: Vec<u32>) -> usize {
        let mut stack = initial_nodes;
        let mut count = 0;

        while let Some(node_idx) = stack.pop() {
            // Collect children before removing the node
            let (children, has_val, huge_child_idx) = {
                let Some(node) = self.try_get_node(node_idx) else {
                    continue;
                };

                let mut children: Vec<u32> = node.childs.iter().map(|(_, idx)| idx).collect();

                let huge_idx = node.get_huge_childs_idx();
                if let Some(hi) = huge_idx
                    && let Some(huge_childs) = self.child_list.get(hi)
                {
                    children.extend(huge_childs.iter().map(|(_, idx)| idx));
                }

                (children, node.val.is_some(), huge_idx)
            };

            // Add children to the stack
            stack.extend(children);

            // Count if it had a value
            if has_val {
                count += 1;
            }

            // Remove huge_childs if present
            if let Some(huge_idx) = huge_child_idx {
                self.child_list.remove(huge_idx);
            }

            // Remove the node from the slab
            self.map.remove(node_idx);
        }

        count
    }

    /// If the node has exactly 1 child and no value, absorb the child
    fn try_recompress(&mut self, node_idx: u32) {
        let Some(node) = self.try_get_node(node_idx) else {
            return;
        };
        if node.val.is_some() {
            return;
        }

        let Some((child_radix, child_idx)) = node.get_single_child() else {
            return;
        };

        // Absorb the child: compression = current + radix + child.compression
        let Some(child) = self.map.remove(child_idx) else {
            return;
        };

        // Update parent_idx for all grandchildren (they now point to node_idx)
        for (_, grandchild_idx) in child.childs.iter() {
            if let Some(grandchild) = self.map.get_mut(grandchild_idx) {
                grandchild.parent_idx = node_idx;
            }
        }
        if let Some(huge_idx) = child.get_huge_childs_idx()
            && let Some(huge_childs) = self.child_list.get(huge_idx)
        {
            let indices: Vec<u32> = huge_childs.iter().map(|(_, idx)| idx).collect();
            for grandchild_idx in indices {
                if let Some(grandchild) = self.map.get_mut(grandchild_idx) {
                    grandchild.parent_idx = node_idx;
                }
            }
        }

        let node = self.get_node_mut(node_idx);
        node.compression.push(child_radix);
        node.compression.extend_from_slice(&child.compression);
        node.val = child.val;
        node.exp_and_radix = child.exp_and_radix;
        node.childs = child.childs;
    }

    /// If the node has exactly 1 child and no value, absorb the child

    fn remove_child(&mut self, parent_idx: u32, radix: u8) {
        let Some(parent) = self.try_get_node_mut(parent_idx) else {
            // Parent was absorbed/removed during recompression, nothing to do
            return;
        };
        if parent.childs.remove(radix).is_some() {
            return;
        }
        // Otherwise it's in huge_childs

        if let Some(huge_idx) = parent.get_huge_childs_idx() {
            self.child_list
                .get_mut(huge_idx)
                .expect("huge_childs should exist")
                .remove(radix);
        }
    }
    fn push_child_idx(&mut self, parent_idx: u32, idx: u32, radix: u8) {
        let huge_idx = {
            let node = self.get_node_mut(parent_idx);
            if !node.childs.is_full() {
                node.childs.push(radix, idx);
                return;
            }
            node.get_huge_childs_idx()
        };

        let Some(huge_idx) = huge_idx else {
            let new_huge_idx = self.intiate_new_huge_child(radix, idx);
            self.get_node_mut(parent_idx).huge_childs_idx = new_huge_idx;
            return;
        };

        let huge = self
            .child_list
            .get_mut(huge_idx)
            .expect("expect id exist so huge childs should");
        huge.push(radix, idx);
    }
}

#[repr(C, align(128))]
struct Node {
    compression: CompactStr,
    childs: Childs,
    val: Option<Value>,
    exp_and_radix: ExpAndRadix,
    huge_childs_idx: u32,
    /// Parent node index (for TTL eviction)
    parent_idx: u32,
}
#[derive(Clone, Copy)]
#[repr(transparent)]
struct ExpAndRadix {
    inner: u64,
}
impl ExpAndRadix {
    const NO_EXPIRACY: u64 = 0x00FFFFFFFFFFFFFF;
    const RADIX_MASK: u64 = !Self::NO_EXPIRACY;
    const EXP_LENGTH: u64 = 56;
    const fn no_expiracy(parent_radix: u8) -> Self {
        Self {
            inner: ((parent_radix as u64) << 56) | Self::NO_EXPIRACY,
        }
    }
    fn exp(self) -> Option<u64> {
        let exp = self.inner & Self::NO_EXPIRACY;
        if exp == Self::NO_EXPIRACY {
            None
        } else {
            Some(exp)
        }
    }
    fn parent_radix(self) -> u8 {
        ((self.inner & Self::RADIX_MASK) >> Self::EXP_LENGTH) as u8
    }
    fn does_expire(self) -> bool {
        self.inner & Self::NO_EXPIRACY != Self::NO_EXPIRACY
    }
    ///this function panic if the 8 upper bit of the ttl provide is not at 0 because the niche is needed to store radix
    fn set_exp(&mut self, exp: u64) {
        assert!(exp & Self::RADIX_MASK == 0);
        self.inner = self.inner & Self::RADIX_MASK | exp
    }
    fn set_no_expiracy(&mut self) {
        self.inner |= Self::NO_EXPIRACY;
    }
    ///this function panic if the 8 upper bit of the ttl provide is not at 0 because the niche is needed to store radix
    fn new(exp: u64, parent_radix: u8) -> Self {
        assert!(exp & Self::RADIX_MASK == 0);
        Self {
            inner: ((parent_radix as u64) << Self::EXP_LENGTH) | exp,
        }
    }
}

impl Default for Node {
    fn default() -> Self {
        Self {
            huge_childs_idx: u32::MAX,
            childs: Childs::default(),
            compression: CompactStr::new(),
            val: None,
            parent_idx: u32::MAX, // Root has no parent
            exp_and_radix: ExpAndRadix::no_expiracy(0),
        }
    }
}

enum CompResult {
    ///The compresion completely part of the key need travel for more
    Path,
    Final,
    Partial(usize),
}

impl Node {
    fn compare_compression_key(&self, key_rest: &[u8]) -> CompResult {
        use std::cmp::Ordering::*;
        let common_len = self.get_common_len(key_rest);
        match self.compression.len().cmp(&key_rest.len()) {
            Equal => {
                if common_len == key_rest.len() {
                    CompResult::Final
                } else {
                    CompResult::Partial(common_len)
                }
            }
            Greater => CompResult::Partial(common_len),
            Less => {
                if common_len == self.compression.len() {
                    CompResult::Path
                } else {
                    CompResult::Partial(common_len)
                }
            }
        }
    }
    fn get_common_len(&self, key_rest: &[u8]) -> usize {
        self.compression
            .iter()
            .zip(key_rest)
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| self.compression.len().min(key_rest.len()))
    }
    fn set_val(&mut self, val: Value, exp: u64) {
        self.val = Some(val);
        self.exp_and_radix.set_exp(exp);
    }

    /// Returns the value if present and not expired
    fn get_value(&self, now: u64) -> Option<&Value> {
        if self.is_expired(now) {
            return None;
        }
        self.val.as_ref()
    }
    fn get_value_mut(&mut self, now: u64) -> Option<&mut Value> {
        if self.is_expired(now) {
            return None;
        }
        self.val.as_mut()
    }
    /// Check if value expired
    fn is_expired(&self, now: u64) -> bool {
        self.exp_and_radix.exp().is_some_and(|exp| exp < now)
    }

    fn get_huge_childs_idx(&self) -> Option<u32> {
        if self.huge_childs_idx == u32::MAX {
            None
        } else {
            Some(self.huge_childs_idx)
        }
    }

    fn new_leaf(
        compression: &[u8],
        val: Value,
        ttl: u64,
        parent_idx: u32,
        parent_radix: u8,
    ) -> Self {
        Node {
            huge_childs_idx: u32::MAX,
            compression: CompactStr::from_slice(compression),
            val: Some(val),
            childs: Childs::default(),
            parent_idx,
            exp_and_radix: ExpAndRadix::new(ttl, parent_radix),
        }
    }
    fn new_empty_leaf(compression: &[u8], parent_idx: u32, parent_radix: u8) -> Self {
        Self {
            childs: Childs::default(),
            compression: CompactStr::from_slice(compression),
            val: None,
            huge_childs_idx: u32::MAX,
            parent_idx,
            exp_and_radix: ExpAndRadix::no_expiracy(parent_radix),
        }
    }

    pub(crate) fn set_new_childs(&mut self, idx: u32) {
        assert!(self.huge_childs_idx == u32::MAX);
        self.huge_childs_idx = idx
    }
    pub(crate) fn get_single_child(&self) -> Option<(u8, u32)> {
        if let Some(ret) = self.childs.old_get_single_child()
            && self.get_huge_childs_idx().is_none()
        {
            return Some(ret);
        }
        None
    }
    #[inline]
    fn does_expire(&self) -> bool {
        self.exp_and_radix.does_expire()
    }
    #[inline]
    fn parent_radix(&self) -> u8 {
        self.exp_and_radix.parent_radix()
    }
}
