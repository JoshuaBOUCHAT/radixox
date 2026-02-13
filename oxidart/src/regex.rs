use bytes::Bytes;
use regex_automata::dfa::{dense::DFA, Automaton};
use regex_automata::util::primitives::StateID;
use regex_automata::{Anchored, Input, MatchError};

use crate::OxidArt;
use crate::value::Value;

/// Error type for regex-based operations.
#[derive(Debug)]
pub enum RegexError {
    /// The regex pattern failed to compile into a DFA.
    Build(regex_automata::dfa::dense::BuildError),
    /// The DFA could not produce a start state.
    Start(MatchError),
}

impl From<regex_automata::dfa::dense::BuildError> for RegexError {
    fn from(e: regex_automata::dfa::dense::BuildError) -> Self {
        RegexError::Build(e)
    }
}

impl From<MatchError> for RegexError {
    fn from(e: MatchError) -> Self {
        RegexError::Start(e)
    }
}

impl OxidArt {
    /// Returns all key-value pairs whose key matches the given regex pattern.
    ///
    /// The pattern is compiled into a DFA and used to prune subtrees during
    /// traversal — branches where the DFA enters a dead state are skipped entirely.
    /// This makes `getn_regex("user:.*:admin:.*")` much faster than a full scan
    /// because the radix tree structure allows pruning at each edge.
    ///
    /// Takes `&self` — expired entries are silently skipped, no eviction is performed.
    ///
    /// # Arguments
    ///
    /// * `pattern` - A regular expression pattern (anchored to full key match).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use bytes::Bytes;
    ///
    /// let mut tree = OxidArt::new();
    /// tree.set(Bytes::from_static(b"user:1:admin:x"), Bytes::from_static(b"a"));
    /// tree.set(Bytes::from_static(b"user:2:viewer:y"), Bytes::from_static(b"b"));
    /// tree.set(Bytes::from_static(b"user:3:admin:z"), Bytes::from_static(b"c"));
    /// tree.set(Bytes::from_static(b"post:1"), Bytes::from_static(b"d"));
    ///
    /// let results = tree.getn_regex("user:.*:admin:.*").unwrap();
    /// // Only returns user:1:admin:x and user:3:admin:z
    /// // The "post:" subtree is pruned immediately (dead state on 'p')
    /// assert_eq!(results.len(), 2);
    /// ```
    pub fn getn_regex(&self, pattern: &str) -> Result<Vec<(Bytes, &Value)>, RegexError> {
        let dfa = DFA::new(pattern)?;
        let mut results = Vec::new();
        let start = dfa.start_state_forward(&Input::new(b"").anchored(Anchored::Yes))?;

        self.collect_regex(&dfa, self.root_idx, start, &mut results);
        Ok(results)
    }

    /// Iterative DFA-guided traversal of the radix tree.
    ///
    /// At each node we feed the compression bytes into the DFA.
    /// - Dead state → prune entire subtree
    /// - Match state + node has value → collect
    /// - Otherwise → push children onto stack
    fn collect_regex<'a>(
        &'a self,
        dfa: &DFA<Vec<u32>>,
        root_idx: u32,
        start_state: StateID,
        results: &mut Vec<(Bytes, &'a Value)>,
    ) {
        // Stack entries: (node_idx, key_path, dfa_state after radix byte)
        let mut stack: Vec<(u32, Vec<u8>, StateID)> = vec![(root_idx, Vec::new(), start_state)];

        while let Some((node_idx, mut key_path, mut state)) = stack.pop() {
            let Some(node) = self.try_get_node(node_idx) else {
                continue;
            };

            // Feed compression bytes into the DFA
            let mut dead = false;
            for &b in node.compression.iter() {
                state = dfa.next_state(state, b);
                if dfa.is_dead_state(state) {
                    dead = true;
                    break;
                }
            }
            if dead {
                continue; // Prune
            }
            key_path.extend_from_slice(&node.compression);

            // Check if this node's key is a full match via EOI transition
            let eoi_state = dfa.next_eoi_state(state);
            if dfa.is_match_state(eoi_state) {
                if let Some(val) = node.get_value(self.now) {
                    results.push((Bytes::from(key_path.clone()), val));
                }
            }

            // Push children onto stack, pruning dead branches at the radix byte
            self.iter_all_children(node_idx, |radix, child_idx| {
                let child_state = dfa.next_state(state, radix);
                if !dfa.is_dead_state(child_state) {
                    let mut child_key = key_path.clone();
                    child_key.push(radix);
                    stack.push((child_idx, child_key, child_state));
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree() -> OxidArt {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"user:1:admin:alice"), Value::String(Bytes::from_static(b"a")));
        tree.set(Bytes::from_static(b"user:2:viewer:bob"), Value::String(Bytes::from_static(b"b")));
        tree.set(Bytes::from_static(b"user:3:admin:charlie"), Value::String(Bytes::from_static(b"c")));
        tree.set(Bytes::from_static(b"user:4:editor:dave"), Value::String(Bytes::from_static(b"d")));
        tree.set(Bytes::from_static(b"post:1:title"), Value::String(Bytes::from_static(b"hello")));
        tree.set(Bytes::from_static(b"post:2:title"), Value::String(Bytes::from_static(b"world")));
        tree.set(Bytes::from_static(b"config:db:host"), Value::String(Bytes::from_static(b"localhost")));
        tree.set(Bytes::from_static(b"config:db:port"), Value::String(Bytes::from_static(b"5432")));
        tree
    }

    #[test]
    fn wildcard_middle_segment() {
        let tree = make_tree();
        let mut results = tree.getn_regex("user:.*:admin:.*").unwrap();
        results.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.as_ref(), b"user:1:admin:alice");
        assert_eq!(results[1].0.as_ref(), b"user:3:admin:charlie");
    }

    #[test]
    fn simple_prefix() {
        let tree = make_tree();
        let results = tree.getn_regex("post:.*").unwrap();
        assert_eq!(results.len(), 2);
        for (key, _) in &results {
            assert!(key.starts_with(b"post:"));
        }
    }

    #[test]
    fn exact_match() {
        let tree = make_tree();
        let results = tree.getn_regex("config:db:host").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.as_ref(), b"config:db:host");
        assert_eq!(results[0].1, &Value::String(Bytes::from_static(b"localhost")));
    }

    #[test]
    fn no_match() {
        let tree = make_tree();
        let results = tree.getn_regex("nonexistent:.*").unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn match_all() {
        let tree = make_tree();
        let results = tree.getn_regex(".*").unwrap();
        assert_eq!(results.len(), 8);
    }

    #[test]
    fn character_class() {
        let tree = make_tree();
        // Match user with single digit ID and any role
        let results = tree.getn_regex("user:[0-9]:.*").unwrap();
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn two_wildcards_specific_segments() {
        let tree = make_tree();
        // Match config keys ending with specific values
        let results = tree.getn_regex("config:.*:port").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, &Value::String(Bytes::from_static(b"5432")));
    }

    #[test]
    fn expired_entries_skipped() {
        let mut tree = OxidArt::new();
        tree.set_now(100);
        tree.set_ttl(
            Bytes::from_static(b"user:1:admin:x"),
            std::time::Duration::from_secs(10),
            Value::String(Bytes::from_static(b"val")),
        );
        tree.set(Bytes::from_static(b"user:2:admin:y"), Value::String(Bytes::from_static(b"val2")));

        // Before expiry
        let results = tree.getn_regex("user:.*:admin:.*").unwrap();
        assert_eq!(results.len(), 2);

        // After expiry
        tree.set_now(200);
        let results = tree.getn_regex("user:.*:admin:.*").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.as_ref(), b"user:2:admin:y");
    }
}
