use hashbrown::HashMap;
use ordered_float::OrderedFloat;
use radixox_lib::shared_byte::SharedByte;
use std::collections::BTreeSet;

/// ZSet internal representation with double indexing for optimal performance.
///
/// - `sorted`: BTreeSet maintains (score, member) order for range queries (ZRANGE)
/// - `scores`: HashMap provides O(1) member -> score lookup (ZSCORE, ZREM)
///
/// Both structures are kept in sync on all mutations.
#[repr(align(64))]
#[derive(Clone, Debug, PartialEq)]
pub struct ZSetInner {
    /// Sorted index: (score, member) tuples ordered by score, then lexicographically.
    pub(crate) sorted: BTreeSet<(OrderedFloat<f64>, SharedByte)>,
    /// Score lookup: member -> score mapping for O(1) access.
    pub(crate) scores: HashMap<SharedByte, OrderedFloat<f64>>,
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
    pub fn insert(&mut self, score: f64, member: SharedByte) -> bool {
        let score = OrderedFloat(score);

        // Membre déjà là : on garde celui qui est stocké. Membre nouveau : externe,
        // le GC ne visite pas les ZSet (MEMORY_ARCH.md §16.9).
        let (member, is_new) = match self.scores.get(&member) {
            Some(&old_score) if old_score == score => return false,
            Some(&old_score) => {
                let (old, _) = self.scores.remove_entry(&member).unwrap();
                self.sorted.remove(&(old_score, old.clone()));
                (old, false)
            }
            None => (member.into_external(), true),
        };
        self.sorted.insert((score, member.clone()));
        self.scores.insert(member, score);
        is_new
    }

    /// Remove a member. Returns true if the member existed.
    pub fn remove(&mut self, member: SharedByte) -> bool {
        if let Some(score) = self.scores.remove(&member) {
            self.sorted.remove(&(score, member));
            true
        } else {
            false
        }
    }

    /// Get the score of a member. O(1) via HashMap.
    pub fn score(&self, member: &SharedByte) -> Option<f64> {
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
    pub fn iter(&self) -> impl Iterator<Item = &(OrderedFloat<f64>, SharedByte)> {
        self.sorted.iter()
    }
}

impl Default for ZSetInner {
    fn default() -> Self {
        Self::new()
    }
}
