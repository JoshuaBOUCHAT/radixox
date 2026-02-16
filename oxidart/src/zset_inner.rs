use bytes::Bytes;
use ordered_float::OrderedFloat;
use std::collections::{BTreeSet, HashMap};

/// ZSet internal representation with double indexing for optimal performance.
///
/// - `sorted`: BTreeSet maintains (score, member) order for range queries (ZRANGE)
/// - `scores`: HashMap provides O(1) member -> score lookup (ZSCORE, ZREM)
///
/// Both structures are kept in sync on all mutations.
#[derive(Clone, Debug, PartialEq)]
pub struct ZSetInner {
    /// Sorted index: (score, member) tuples ordered by score, then lexicographically.
    pub(crate) sorted: BTreeSet<(OrderedFloat<f64>, Bytes)>,
    /// Score lookup: member -> score mapping for O(1) access.
    pub(crate) scores: HashMap<Bytes, OrderedFloat<f64>>,
}

impl ZSetInner {
    /// Create a new empty ZSet.
    pub fn new() -> Self {
        Self {
            sorted: BTreeSet::new(),
            scores: HashMap::new(),
        }
    }

    /// Insert or update a member with a score.
    /// Returns true if this is a new member (not an update).
    pub fn insert(&mut self, score: f64, member: Bytes) -> bool {
        let score = OrderedFloat(score);

        // Remove old entry if exists (score might have changed)
        if let Some(&old_score) = self.scores.get(&member) {
            if old_score != score {
                // Score changed - remove old sorted entry
                self.sorted.remove(&(old_score, member.clone()));
            } else {
                // Same score - no change needed
                return false;
            }
        }

        // Insert new entry
        self.sorted.insert((score, member.clone()));
        let is_new = self.scores.insert(member, score).is_none();
        is_new
    }

    /// Remove a member. Returns true if the member existed.
    pub fn remove(&mut self, member: &Bytes) -> bool {
        if let Some(score) = self.scores.remove(member) {
            self.sorted.remove(&(score, member.clone()));
            true
        } else {
            false
        }
    }

    /// Get the score of a member. O(1) via HashMap.
    pub fn score(&self, member: &[u8]) -> Option<f64> {
        self.scores.get(member).map(|s| s.into_inner())
    }

    /// Get the number of members.
    pub fn len(&self) -> usize {
        self.scores.len()
    }

    /// Check if the zset is empty.
    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    /// Iterate over members in score order (ascending).
    pub fn iter(&self) -> impl Iterator<Item = &(OrderedFloat<f64>, Bytes)> {
        self.sorted.iter()
    }
}

impl Default for ZSetInner {
    fn default() -> Self {
        Self::new()
    }
}
