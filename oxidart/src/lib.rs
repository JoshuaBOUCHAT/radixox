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
//! tree.set(Bytes::from_static(b"hello"), Bytes::from_static(b"world"));
//!
//! // Insert with TTL (requires `ttl` feature, enabled by default)
//! tree.set_now(1700000000); // Update internal clock
//! tree.set_ttl(Bytes::from_static(b"session"), Duration::from_secs(3600), Bytes::from_static(b"data"));
//!
//! // Retrieve a value
//! assert_eq!(tree.get(Bytes::from_static(b"hello")), Some(Bytes::from_static(b"world")));
//!
//! // Get all entries with a prefix
//! let entries = tree.getn(Bytes::from_static(b"hello"));
//!
//! // Delete a key
//! let deleted = tree.del(Bytes::from_static(b"hello"));
//!
//! // Delete all keys with a prefix
//! let count = tree.deln(Bytes::from_static(b"hello"));
//! ```
//!
//! ## Key Requirements
//!
//! Keys must be valid ASCII bytes. Non-ASCII keys will trigger a debug assertion.

mod compact_str;
pub mod error;
pub mod hcommand;
mod node_childs;
pub mod scommand;
pub mod value;
pub mod zcommand;
pub mod zset_inner;

// Prevent enabling both async runtimes at once
#[cfg(all(feature = "monoio", feature = "tokio"))]
compile_error!("Features 'monoio' and 'tokio' are mutually exclusive. Please enable only one.");

#[cfg(feature = "monoio")]
pub mod monoio;

#[cfg(feature = "tokio")]
pub mod tokio;

pub mod counter;

#[cfg(feature = "regex")]
pub mod regex;

#[cfg(test)]
mod test;

#[cfg(test)]
mod test_structures;

use bytes::Bytes;
use hislab::HiSlab;

use crate::compact_str::CompactStr;
use crate::node_childs::ChildAble;
use crate::node_childs::Childs;
use crate::node_childs::HugeChilds;
use crate::value::Value;

/// Internal sentinel value indicating no expiration (never expires)
#[cfg(feature = "ttl")]
const NO_EXPIRY: u64 = u64::MAX;

/// Result of a TTL lookup operation.
#[cfg(feature = "ttl")]
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
/// tree.set(Bytes::from_static(b"key"), Bytes::from_static(b"value"));
///
/// assert_eq!(tree.get(Bytes::from_static(b"key")), Some(Bytes::from_static(b"value")));
/// ```
///
pub struct OxidArt {
    pub(crate) map: HiSlab<Node>,
    pub(crate) child_list: HiSlab<HugeChilds>,
    /// Current timestamp (seconds since UNIX epoch).
    /// The server is responsible for updating this via `set_now()`.
    #[cfg(feature = "ttl")]
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
        let mut map = HiSlab::new();
        let root_idx = map.insert(Node::default());
        let child_list = HiSlab::new();

        Self {
            map,
            root_idx,
            child_list,
            #[cfg(feature = "ttl")]
            now: 0,
        }
    }

    /// Updates the current timestamp. Call this periodically from your async runtime.
    #[cfg(feature = "ttl")]
    #[inline]
    pub fn set_now(&mut self, now: u64) {
        self.now = now;
    }

    /// Evicts expired entries using Redis-style probabilistic sampling.
    ///
    /// Algorithm:
    /// 1. Sample up to 20 random entries with TTL
    /// 2. Delete those that are expired
    /// 3. If >= 25% (5+) were expired, repeat
    /// 4. Stop when < 25% expired or no more entries
    ///
    /// Returns the total number of evicted entries.
    #[cfg(feature = "ttl")]
    pub fn evict_expired(&mut self) -> usize {
        const SAMPLE_SIZE: usize = 20;
        const THRESHOLD: usize = 5; // 25% of 20

        let mut rng = rand::thread_rng();
        let mut total_evicted = 0;

        loop {
            let mut evicted_this_round = 0;
            let mut sampled = 0;

            // Sample up to SAMPLE_SIZE tagged nodes
            for _ in 0..SAMPLE_SIZE {
                let Some((idx, node)) = self.map.random_tagged(&mut rng) else {
                    // No more tagged entries
                    break;
                };
                sampled += 1;

                // Check if expired
                if node.is_expired(self.now) {
                    let parent_idx = node.parent_idx;
                    let parent_radix = node.parent_radix;

                    // Don't try to delete root
                    if parent_idx != u32::MAX {
                        self.delete_node_for_eviction(idx, parent_idx, parent_radix);
                        evicted_this_round += 1;
                    }
                }
            }

            total_evicted += evicted_this_round;

            // Stop if we sampled less than SAMPLE_SIZE (not enough entries)
            // or if less than 25% were expired
            if sampled < SAMPLE_SIZE || evicted_this_round < THRESHOLD {
                break;
            }
        }

        total_evicted
    }

    /// Delete a node during TTL eviction (similar to delete_node_inline but uses stored parent info)
    #[cfg(feature = "ttl")]
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
    #[cfg(feature = "ttl")]
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
        #[cfg(feature = "ttl")]
        return self.get_node(idx).get_value(self.now);
        #[cfg(not(feature = "ttl"))]
        return self.get_node(idx).get_value();
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
            #[cfg(feature = "ttl")]
            if self.get_node(self.root_idx).is_expired(self.now) {
                self.get_node_mut(self.root_idx).val = None;
                self.try_recompress(self.root_idx);
                return None;
            }
            #[cfg(feature = "ttl")]
            return Some(self.root_idx);
            #[cfg(not(feature = "ttl"))]
            return self.get_node(self.root_idx).get_value();
        }

        #[cfg(feature = "ttl")]
        let mut parent_idx = self.root_idx;
        #[cfg(feature = "ttl")]
        let mut parent_radix = key[0];
        let mut idx = self.find(self.root_idx, key[0])?;
        let mut cursor = 1;

        loop {
            let node = self.try_get_node(idx)?;
            match node.compare_compression_key(&key[cursor..]) {
                CompResult::Final => {
                    #[cfg(feature = "ttl")]
                    if node.is_expired(self.now) {
                        self.delete_node_inline(idx, parent_idx, parent_radix);
                        return None;
                    }
                    #[cfg(feature = "ttl")]
                    return Some(idx);
                    #[cfg(not(feature = "ttl"))]
                    return self.get_node(idx).get_value();
                }
                CompResult::Partial(_) => return None,
                CompResult::Path => {
                    cursor += node.compression.len();
                }
            }
            #[cfg(feature = "ttl")]
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
    #[cfg(feature = "ttl")]
    pub fn get_ttl(&self, key: Bytes) -> TtlResult {
        debug_assert!(key.is_ascii(), "key must be ASCII");

        let idx = match self.traverse_to_key(&key) {
            Some(idx) => idx,
            None => return TtlResult::KeyNotExist,
        };

        let node = self.get_node(idx);
        match &node.val {
            None => TtlResult::KeyNotExist,
            Some((_, expiry)) if *expiry == NO_EXPIRY => TtlResult::KeyWithoutTtl,
            Some((_, expiry)) if *expiry <= self.now => TtlResult::KeyNotExist,
            Some((_, expiry)) => TtlResult::KeyWithTtl(expiry - self.now),
        }
    }

    /// Sets a TTL on an existing key.
    ///
    /// Returns `true` if the key exists and the TTL was set, `false` otherwise.
    #[cfg(feature = "ttl")]
    pub fn expire(&mut self, key: Bytes, ttl: std::time::Duration) -> bool {
        debug_assert!(key.is_ascii(), "key must be ASCII");

        let Some(idx) = self.traverse_to_key(&key) else {
            return false;
        };

        let now = self.now;
        let new_expiry = now.saturating_add(ttl.as_secs());

        let node = self.get_node_mut(idx);
        match &mut node.val {
            None => false,
            Some((_, expiry)) if *expiry != NO_EXPIRY && *expiry <= now => false,
            Some((_, expiry)) => {
                let was_permanent = *expiry == NO_EXPIRY;
                *expiry = new_expiry;
                // Tag the node if it wasn't already (was permanent)
                if was_permanent {
                    self.map.tag(idx);
                }
                true
            }
        }
    }

    /// Removes the TTL from a key, making it permanent.
    ///
    /// Returns `true` if the key exists and had a TTL, `false` otherwise.
    #[cfg(feature = "ttl")]
    pub fn persist(&mut self, key: Bytes) -> bool {
        debug_assert!(key.is_ascii(), "key must be ASCII");

        let Some(idx) = self.traverse_to_key(&key) else {
            return false;
        };

        let now = self.now;

        let node = self.get_node_mut(idx);
        match &mut node.val {
            None => false,
            Some((_, expiry)) if *expiry == NO_EXPIRY => false, // Already permanent
            Some((_, expiry)) if *expiry <= now => false,       // Expired
            Some((_, expiry)) => {
                *expiry = NO_EXPIRY;
                // Untag the node since it no longer has TTL
                self.map.untag(idx);
                true
            }
        }
    }

    /// Traverses to a key and returns the node index if found.
    #[cfg(feature = "ttl")]
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
    #[cfg(feature = "ttl")]
    pub(crate) fn node_value_mut(&mut self, idx: u32) -> Option<&mut Value> {
        let now = self.now;
        let (val, ttl) = self.get_node_mut(idx).val.as_mut()?;
        if *ttl != NO_EXPIRY && *ttl < now {
            return None;
        }
        Some(val)
    }

    /// Deletes a node inline (used for TTL expiration cleanup)
    #[cfg(feature = "ttl")]
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
    /// tree.set(Bytes::from_static(b"user:1"), Bytes::from_static(b"alice"));
    /// tree.set(Bytes::from_static(b"user:2"), Bytes::from_static(b"bob"));
    /// tree.set(Bytes::from_static(b"post:1"), Bytes::from_static(b"hello"));
    ///
    /// let users = tree.getn(Bytes::from_static(b"user:"));
    /// assert_eq!(users.len(), 2);
    /// ```
    pub fn getn(&self, prefix: Bytes) -> Vec<(Bytes, &Value)> {
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
        results: &mut Vec<(Bytes, &'a Value)>,
    ) {
        let Some(node) = self.try_get_node(node_idx) else {
            return;
        };

        #[cfg(feature = "ttl")]
        if let Some(val) = node.get_value(self.now) {
            results.push((Bytes::from(key_path.clone()), val));
        }
        #[cfg(not(feature = "ttl"))]
        if let Some(val) = node.get_value() {
            results.push((Bytes::from(key_path.clone()), val));
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
        results: &mut Vec<(Bytes, &'a Value)>,
    ) {
        let Some(node) = self.try_get_node(node_idx) else {
            return;
        };

        key_prefix.extend_from_slice(&node.compression);

        #[cfg(feature = "ttl")]
        if let Some(val) = node.get_value(self.now) {
            results.push((Bytes::from(key_prefix.clone()), val));
        }
        #[cfg(not(feature = "ttl"))]
        if let Some(val) = node.get_value() {
            results.push((Bytes::from(key_prefix.clone()), val));
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
    /// tree.set(Bytes::from_static(b"key"), Bytes::from_static(b"value1"));
    ///
    /// // Update an existing key
    /// tree.set(Bytes::from_static(b"key"), Bytes::from_static(b"value2"));
    ///
    /// assert_eq!(tree.get(Bytes::from_static(b"key")), Some(Bytes::from_static(b"value2")));
    /// ```
    pub fn set(&mut self, key: Bytes, val: Value) {
        #[cfg(feature = "ttl")]
        self.set_internal(key, NO_EXPIRY, val);
        #[cfg(not(feature = "ttl"))]
        self.set_internal(key, val);
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
    /// tree.set_ttl(Bytes::from_static(b"session"), Duration::from_secs(60), Bytes::from_static(b"data"));
    ///
    /// // Key expires at timestamp 1060
    /// ```
    #[cfg(feature = "ttl")]
    pub fn set_ttl(&mut self, key: Bytes, ttl: std::time::Duration, val: Value) {
        let expires_at = self.now.saturating_add(ttl.as_secs());
        self.set_internal(key, expires_at, val);
    }

    #[cfg(feature = "ttl")]
    fn set_internal(&mut self, key: Bytes, ttl: u64, val: Value) {
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
        let (old_compression, old_val, old_childs, old_huge_idx) = {
            let node = self.get_node_mut(idx);
            let old_compression = std::mem::take(&mut node.compression);
            let old_val = node.val.take();
            let old_childs = std::mem::take(&mut node.childs);
            let old_huge_idx = std::mem::replace(&mut node.huge_childs_idx, u32::MAX);

            node.compression = CompactStr::from_slice(&old_compression[..common_len]);
            if val_on_intermediate && let Some(val) = val.take() {
                node.val = Some((val, ttl.unwrap_or(NO_EXPIRY)));
            }

            (old_compression, old_val, old_childs, old_huge_idx)
        };

        // Create a node for the old content
        let old_radix = old_compression[common_len];
        // Check if old value had a TTL (needs to stay tagged)
        let old_had_ttl = old_val
            .as_ref()
            .map(|(_, old_ttl)| *old_ttl != NO_EXPIRY)
            .unwrap_or(false);
        let old_child = Node {
            huge_childs_idx: old_huge_idx,
            compression: CompactStr::from_slice(&old_compression[common_len + 1..]),
            val: old_val,
            childs: old_childs,
            parent_idx: idx,
            parent_radix: old_radix,
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
                    ttl.unwrap_or(NO_EXPIRY),
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

    #[cfg(not(feature = "ttl"))]
    fn set_internal(&mut self, key: Bytes, val: Value) {
        debug_assert!(key.is_ascii(), "key must be ASCII");
        let key_len = key.len();
        if key_len == 0 {
            self.get_node_mut(self.root_idx).set_val(val);
            return;
        }
        let mut idx = self.root_idx;
        let mut cursor = 0;

        loop {
            let Some(child_idx) = self.find(idx, key[cursor]) else {
                self.create_node_with_val(idx, key[cursor], val, &key[(cursor + 1)..]);
                return;
            };
            idx = child_idx;
            cursor += 1;
            let node_comparaison = self.get_node(idx).compare_compression_key(&key[cursor..]);
            let common_len = match node_comparaison {
                CompResult::Final => {
                    self.get_node_mut(idx).set_val(val);
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
            let val_on_intermediate = common_len == key_rest.len();

            // Extract old state and configure intermediate in one pass
            let (old_compression, old_val, old_childs) = {
                let node = self.get_node_mut(idx);
                let old_compression = std::mem::take(&mut node.compression);
                let old_val = node.val.take();
                let old_childs = std::mem::take(&mut node.childs);

                node.compression = CompactStr::from_slice(&old_compression[..common_len]);
                if val_on_intermediate {
                    node.val = Some(val.clone());
                }

                (old_compression, old_val, old_childs)
            };

            // Create a node for the old content
            let old_radix = old_compression[common_len];
            let old_child = Node {
                compression: CompactStr::from_slice(&old_compression[common_len + 1..]),
                val: old_val,
                childs: old_childs,
            };
            let old_child_idx = self.insert(old_child);
            self.push_child_idx(idx, old_child_idx, old_radix);

            // If the value doesn't go on the intermediate node, create a new leaf
            if !val_on_intermediate {
                let new_radix = key_rest[common_len];
                let new_compression = &key_rest[common_len + 1..];
                self.create_node_with_val(idx, new_radix, val, new_compression);
            }

            return;
        }
    }

    #[cfg(feature = "ttl")]
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
        let inserted_idx = if ttl != NO_EXPIRY {
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

    #[cfg(not(feature = "ttl"))]
    fn create_node_with_val(&mut self, idx: u32, radix: u8, val: Value, compression: &[u8]) {
        let (is_full, huge_child_idx) = {
            let father_node = self.get_node(idx);
            (
                father_node.childs.is_full(),
                father_node.get_huge_childs_idx(),
            )
        };
        let new_leaf = Node::new_leaf(compression, val);
        let inserted_idx = self.insert(new_leaf);
        match (is_full, huge_child_idx) {
            (false, _) => self.push_child_idx(idx, inserted_idx, radix),
            (true, None) => {
                let new_child_idx = self.intiate_new_huge_child(radix, inserted_idx);
                self.get_node_mut(idx).childs.set_new_childs(new_child_idx);
            }
            (true, Some(huge_idx)) => {
                self.child_list
                    .get_mut(huge_idx)
                    .expect("if key exist childs should too")
                    .push(radix, inserted_idx);
            }
        }
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
    /// tree.set(Bytes::from_static(b"key"), Bytes::from_static(b"value"));
    ///
    /// let deleted = tree.del(Bytes::from_static(b"key"));
    /// assert_eq!(deleted, Some(Bytes::from_static(b"value")));
    ///
    /// // Key no longer exists
    /// assert_eq!(tree.get(Bytes::from_static(b"key")), None);
    /// ```
    pub fn del(&mut self, key: &[u8]) -> Option<Value> {
        debug_assert!(key.is_ascii(), "key must be ASCII");
        let key_len = key.len();
        if key_len == 0 {
            let old_val = self.get_node_mut(self.root_idx).val.take();
            self.try_recompress(self.root_idx);
            #[cfg(feature = "ttl")]
            return old_val.map(|(v, _)| v);
            #[cfg(not(feature = "ttl"))]
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
            #[cfg(feature = "ttl")]
            return Some(old_val.0);
            #[cfg(not(feature = "ttl"))]
            Some(old_val)
        } else {
            // Node without children (leaf): completely remove from the slab
            let node = self.map.remove(target_idx)?;
            let old_val = node.val?;
            self.remove_child(parent_idx, parent_radix);
            if parent_idx != self.root_idx {
                self.try_recompress(parent_idx);
            }
            #[cfg(feature = "ttl")]
            return Some(old_val.0);
            #[cfg(not(feature = "ttl"))]
            Some(old_val)
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
    /// tree.set(Bytes::from_static(b"user:1"), Bytes::from_static(b"alice"));
    /// tree.set(Bytes::from_static(b"user:2"), Bytes::from_static(b"bob"));
    /// tree.set(Bytes::from_static(b"post:1"), Bytes::from_static(b"hello"));
    ///
    /// // Delete all user entries
    /// let count = tree.deln(Bytes::from_static(b"user:"));
    /// assert_eq!(count, 2);
    ///
    /// // Only post entries remain
    /// assert_eq!(tree.getn(Bytes::from_static(b"")).len(), 1);
    /// ```
    pub fn deln(&mut self, prefix: Bytes) -> usize {
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
    #[cfg(feature = "ttl")]
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
        node.childs = child.childs;
    }

    /// If the node has exactly 1 child and no value, absorb the child
    #[cfg(not(feature = "ttl"))]
    fn try_recompress(&mut self, node_idx: u32) {
        let node = self.get_node(node_idx);
        if node.val.is_some() {
            return;
        }

        let Some((child_radix, child_idx)) = node.childs.get_single_child() else {
            return;
        };

        // Absorb the child: compression = current + radix + child.compression
        let Some(child) = self.map.remove(child_idx) else {
            return;
        };
        let node = self.get_node_mut(node_idx);

        node.compression.push(child_radix);
        node.compression.extend_from_slice(&child.compression);
        node.val = child.val;
        node.childs = child.childs;
    }

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
#[cfg(feature = "ttl")]
struct Node {
    childs: Childs,
    compression: CompactStr,
    val: Option<(Value, u64)>,
    huge_childs_idx: u32,
    /// Parent node index (for TTL eviction)
    parent_idx: u32,
    /// Radix used to reach this node from parent (for TTL eviction)
    parent_radix: u8,
}

#[cfg(feature = "ttl")]
impl Default for Node {
    fn default() -> Self {
        Self {
            huge_childs_idx: u32::MAX,
            childs: Childs::default(),
            compression: CompactStr::new(),
            val: None,
            parent_idx: u32::MAX, // Root has no parent
            parent_radix: 0,
        }
    }
}

#[cfg(not(feature = "ttl"))]
#[derive(Default)]
struct Node {
    compression: CompactStr,
    val: Option<Value>,
    childs: Childs,
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
    #[cfg(feature = "ttl")]
    fn set_val(&mut self, val: Value, ttl: u64) {
        self.val = Some((val, ttl));
    }

    #[cfg(not(feature = "ttl"))]
    fn set_val(&mut self, val: Value) {
        self.val = Some(val);
    }

    /// Returns the value if present and not expired
    #[cfg(feature = "ttl")]
    fn get_value(&self, now: u64) -> Option<&Value> {
        let (val, ttl) = self.val.as_ref()?;
        if *ttl != NO_EXPIRY && *ttl < now {
            return None;
        }
        Some(val)
    }
    fn get_value_mut(&mut self, now: u64) -> Option<&mut Value> {
        let (val, ttl) = self.val.as_mut()?;
        if *ttl != NO_EXPIRY && *ttl < now {
            return None;
        }
        Some(val)
    }

    #[cfg(not(feature = "ttl"))]
    fn get_value(&self) -> Option<&Value> {
        self.val.as_ref()
    }

    /// Check if value exists and is expired
    #[cfg(feature = "ttl")]
    fn is_expired(&self, now: u64) -> bool {
        if let Some((_, ttl)) = &self.val {
            *ttl != NO_EXPIRY && *ttl < now
        } else {
            false
        }
    }

    fn get_huge_childs_idx(&self) -> Option<u32> {
        if self.huge_childs_idx == u32::MAX {
            None
        } else {
            Some(self.huge_childs_idx)
        }
    }

    #[cfg(feature = "ttl")]
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
            val: Some((val, ttl)),
            childs: Childs::default(),
            parent_idx,
            parent_radix,
        }
    }
    fn new_empty_leaf(compression: &[u8], parent_idx: u32, parent_radix: u8) -> Self {
        Self {
            childs: Childs::default(),
            compression: CompactStr::from_slice(compression),
            val: None,
            huge_childs_idx: u32::MAX,
            parent_idx,
            parent_radix,
        }
    }

    #[cfg(not(feature = "ttl"))]
    fn new_leaf(compression: &[u8], val: Value) -> Self {
        Node {
            compression: CompactStr::from_slice(compression),
            val: Some(val),
            childs: Childs::default(),
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
}
