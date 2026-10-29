#[allow(unused_imports)]
use crate::prelude::*;
use core::fmt::Write;

use regex_automata::dfa::{Automaton, dense::DFA};
use regex_automata::nfa::thompson;
use regex_automata::util::primitives::StateID;
use regex_automata::util::syntax;
use regex_automata::{Anchored, Input, MatchError};

use crate::node::EntryRef;
use crate::value::{Value, value_from_raw_ref};
use crate::{OxidArt, expired};
use radixox_lib::shared_byte::SharedByte;

/// Error type for regex-based operations.
#[derive(Debug)]
pub enum RegexError {
    /// The regex pattern failed to compile into a DFA.
    Build(Box<regex_automata::dfa::dense::BuildError>),
    /// The DFA could not produce a start state.
    Start(MatchError),
}

impl From<regex_automata::dfa::dense::BuildError> for RegexError {
    fn from(e: regex_automata::dfa::dense::BuildError) -> Self {
        RegexError::Build(Box::new(e))
    }
}

impl From<MatchError> for RegexError {
    fn from(e: MatchError) -> Self {
        RegexError::Start(e)
    }
}

/// Taille maximale d'une DFA (et de sa construction), en octets.
const DFA_SIZE_LIMIT: usize = 4 << 20;

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
    /// tree.set(SharedByte::from_str("user:1:admin:x"), SharedByte::from_str("a"));
    /// tree.set(SharedByte::from_str("user:2:viewer:y"), SharedByte::from_str("b"));
    /// tree.set(SharedByte::from_str("user:3:admin:z"), SharedByte::from_str("c"));
    /// tree.set(SharedByte::from_str("post:1"), SharedByte::from_str("d"));
    ///
    /// let results = tree.getn_regex("user:.*:admin:.*").unwrap();
    /// // Only returns user:1:admin:x and user:3:admin:z
    /// // The "post:" subtree is pruned immediately (dead state on 'p')
    /// assert_eq!(results.len(), 2);
    /// ```
    ///
    /// The regex works on **bytes**, not on Unicode text: keys are binary.
    /// `.` matches any single byte (including `\n`), and `\xNN` matches the
    /// raw byte `NN` (e.g. `\xFF`, which is not valid UTF-8 on its own).
    pub fn getn_regex(&self, pattern: &str) -> Result<Vec<(SharedByte, Value)>, RegexError> {
        let dfa = DFA::builder()
            // Bornes : un motif comme `*a` + 40 `?` (`.*a.{40}`) a ~2^40 états
            // déterministes ; sans limite, un seul KEYS fige le serveur. Au-delà,
            // erreur (« motif trop complexe ») en quelques ms.
            .configure(
                DFA::config()
                    .dfa_size_limit(Some(DFA_SIZE_LIMIT))
                    .determinize_size_limit(Some(DFA_SIZE_LIMIT)),
            )
            .syntax(
                syntax::Config::new()
                    .unicode(false)
                    .utf8(false)
                    .dot_matches_new_line(true),
            )
            .thompson(thompson::Config::new().utf8(false))
            .build(pattern)?;
        let mut results = Vec::new();
        let start = dfa.start_state_forward(&Input::new(b"").anchored(Anchored::Yes))?;

        self.collect_regex(&dfa, self.root_idx, start, &mut results);
        Ok(results)
    }

    /// Keys matching a Redis glob pattern (`*`, `?`, `[...]`, `\x`), byte-wise.
    /// See [`glob_to_regex`].
    pub fn getn_glob(&self, glob: &[u8]) -> Result<Vec<(SharedByte, Value)>, RegexError> {
        self.getn_regex(&glob_to_regex(glob))
    }

    /// Iterative DFA-guided traversal of the radix tree.
    ///
    /// Each node feeds its prefix into the DFA, then each entry its radix
    /// (and a leaf its suffix). A dead state prunes the subtree or the leaf;
    /// a match on end of input collects the value.
    fn collect_regex(
        &self,
        dfa: &DFA<Vec<u32>>,
        root_idx: u32,
        start_state: StateID,
        results: &mut Vec<(SharedByte, Value)>,
    ) {
        let now = self.now;
        let mut path: Vec<u8> = Vec::new();
        // (nœud, longueur de la clé du parent, radix vers le nœud, état après
        // le radix ; la racine n'a pas de radix)
        let mut stack: Vec<(u32, u32, Option<u8>, StateID)> = vec![(root_idx, 0, None, start_state)];

        'node: while let Some((idx, depth, radix, mut state)) = stack.pop() {
            path.truncate(depth as usize);
            if let Some(r) = radix {
                path.push(r);
            }
            let node = self.node(idx);
            for &b in node.prefix.iter() {
                state = dfa.next_state(state, b);
                if dfa.is_dead_state(state) {
                    continue 'node;
                }
            }
            path.extend_from_slice(&node.prefix);
            if node.has_val()
                && !expired(node.exp_and_radix.exp(), now)
                && dfa.is_match_state(dfa.next_eoi_state(state))
            {
                // SAFETY: valeur réelle, tag cohérent.
                results.push((SharedByte::from_slice(&path), unsafe { value_from_raw_ref(node.tag, &node.val) }));
            }
            let base = path.len();
            'entry: for (radix, e) in node.iter() {
                let mut s = dfa.next_state(state, radix);
                if dfa.is_dead_state(s) {
                    continue;
                }
                if let EntryRef::Child(c) = e {
                    stack.push((c, base as u32, Some(radix), s));
                    continue;
                }
                path.push(radix);
                match e {
                    EntryRef::Child(_) => unreachable!(),
                    EntryRef::Leaf { tag, val, ttl, sfx } => {
                        if let Some(sfx) = sfx {
                            for &b in sfx.iter() {
                                s = dfa.next_state(s, b);
                                if dfa.is_dead_state(s) {
                                    path.truncate(base);
                                    continue 'entry;
                                }
                            }
                            path.extend_from_slice(sfx);
                        }
                        if !expired(ttl, now) && dfa.is_match_state(dfa.next_eoi_state(s)) {
                            // SAFETY: valeur réelle, tag cohérent.
                            results.push((SharedByte::from_slice(&path), unsafe { value_from_raw_ref(tag, val) }));
                        }
                    }
                }
                path.truncate(base);
            }
        }
    }
}

/// How a Redis glob pattern can be answered.
#[derive(Debug, PartialEq, Eq)]
pub enum GlobKind<'a> {
    /// No special character: matches only this exact key.
    Exact(&'a [u8]),
    /// Literal prefix followed by a single trailing `*` (`""` = every key).
    Prefix(&'a [u8]),
    /// Anything else: needs the DFA ([`glob_to_regex`]).
    Pattern,
}

/// Classifies a glob so that `KEYS user:1` is an exact lookup and `KEYS user:*`
/// a plain prefix scan (no DFA). `\` counts as special: `user\*` is the
/// literal key `user*`, not a prefix.
pub fn glob_kind(glob: &[u8]) -> GlobKind<'_> {
    let special = |b: &u8| matches!(b, b'*' | b'?' | b'[' | b'\\');
    match glob.split_last() {
        Some((b'*', head)) if !head.iter().any(special) => GlobKind::Prefix(head),
        _ if !glob.iter().any(special) => GlobKind::Exact(glob),
        _ => GlobKind::Pattern,
    }
}

/// Literal bytes before the first special character (`*`, `?`, `[`, `\`):
/// every key matching `glob` starts with them.
pub fn glob_literal_prefix(glob: &[u8]) -> &[u8] {
    let end = glob
        .iter()
        .position(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
        .unwrap_or(glob.len());
    &glob[..end]
}

/// Does `key` match the Redis glob `pat`? Same semantics as
/// [`glob_to_regex`], without building a DFA (SCAN MATCH tests one key at a
/// time). Every token except `*` matches exactly one byte, so backtracking
/// to the last `*` is enough.
pub fn glob_match(pat: &[u8], key: &[u8]) -> bool {
    let (mut p, mut i) = (0, 0);
    // (pattern position after the last `*`, key position it was tried at)
    let mut star: Option<(usize, usize)> = None;
    loop {
        if pat.get(p) == Some(&b'*') {
            while pat.get(p) == Some(&b'*') {
                p += 1;
            }
            if p == pat.len() {
                return true;
            }
            star = Some((p, i));
            continue;
        }
        if i == key.len() {
            return p == pat.len();
        }
        if p < pat.len()
            && let Some(next) = glob_token(pat, p, key[i])
        {
            p = next;
            i += 1;
            continue;
        }
        match star {
            Some((sp, si)) => {
                star = Some((sp, si + 1));
                p = sp;
                i = si + 1;
            }
            None => return false,
        }
    }
}

/// Matches the token at `pat[p]` (not `*`) against `c`; returns the position
/// of the next token.
fn glob_token(pat: &[u8], p: usize, c: u8) -> Option<usize> {
    match pat[p] {
        b'?' => Some(p + 1),
        b'\\' if p + 1 < pat.len() => (pat[p + 1] == c).then_some(p + 2),
        b'[' => {
            let mut i = p + 1;
            let negate = pat.get(i) == Some(&b'^');
            if negate {
                i += 1;
            }
            let mut matched = false;
            while i < pat.len() && pat[i] != b']' {
                let mut lo = pat[i];
                if lo == b'\\' && i + 1 < pat.len() {
                    i += 1;
                    lo = pat[i];
                }
                if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
                    let mut j = i + 2;
                    if pat[j] == b'\\' && j + 1 < pat.len() {
                        j += 1;
                    }
                    let hi = pat[j];
                    let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                    matched |= (a..=b).contains(&c);
                    i = j + 1;
                    continue;
                }
                matched |= lo == c;
                i += 1;
            }
            (matched != negate).then_some((i + 1).min(pat.len()))
        }
        lit => (lit == c).then_some(p + 1),
    }
}

/// Converts a Redis glob into an anchored **byte-wise** regex for
/// [`OxidArt::getn_regex`].
///
/// Redis semantics (`stringmatchlen`):
///   `*`      any sequence of bytes (including `\n` and none)
///   `?`      exactly one byte
///   `[abc]`  one byte of the set ; `[^abc]` negation ; `[a-z]` range (bounds
///            swapped if reversed) ; `\` escapes inside a class too ;
///            unterminated class = closed at the end of the pattern
///   `\x`     the literal byte `x`
/// Every byte except ASCII alphanumerics is emitted as `\xNN`, so binary keys
/// (and regex metacharacters) are matched literally.
pub fn glob_to_regex(glob: &[u8]) -> String {
    let mut re = String::with_capacity(glob.len() * 4 + 2);
    re.push('^');
    let mut i = 0;
    while i < glob.len() {
        match glob[i] {
            b'*' => re.push_str(".*"),
            b'?' => re.push('.'),
            b'\\' if i + 1 < glob.len() => {
                i += 1;
                push_byte(&mut re, glob[i]);
            }
            b'[' => {
                i += 1;
                let negate = glob.get(i) == Some(&b'^');
                if negate {
                    i += 1;
                }
                let mut class = String::new();
                while i < glob.len() && glob[i] != b']' {
                    let mut lo = glob[i];
                    if lo == b'\\' && i + 1 < glob.len() {
                        i += 1;
                        lo = glob[i];
                    }
                    // range `lo-hi` (a `-` right before `]` is a literal)
                    if i + 2 < glob.len() && glob[i + 1] == b'-' && glob[i + 2] != b']' {
                        let mut j = i + 2;
                        if glob[j] == b'\\' && j + 1 < glob.len() {
                            j += 1;
                        }
                        let hi = glob[j];
                        let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                        push_byte(&mut class, a);
                        class.push('-');
                        push_byte(&mut class, b);
                        i = j + 1;
                        continue;
                    }
                    push_byte(&mut class, lo);
                    i += 1;
                }
                // `i` est sur le `]` (ou en fin de motif) : sauté par le `i += 1` final.
                match (class.is_empty(), negate) {
                    (true, false) => re.push_str("[^\\x00-\\xFF]"), // `[]` : ne matche rien
                    (true, true) => re.push_str("[\\x00-\\xFF]"),   // `[^]` : n'importe quel octet
                    (false, neg) => {
                        re.push('[');
                        if neg {
                            re.push('^');
                        }
                        re.push_str(&class);
                        re.push(']');
                    }
                }
            }
            b => push_byte(&mut re, b),
        }
        i += 1;
    }
    re.push('$');
    re
}

#[inline]
fn push_byte(re: &mut String, b: u8) {
    if b.is_ascii_alphanumeric() {
        re.push(b as char);
    } else {
        let _ = write!(re, "\\x{b:02X}");
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    #[allow(unused_imports)]
    use std::{dbg, eprintln, print, println, thread_local};
    use radixox_lib::shared_byte::SharedByte;

    use super::*;

    fn make_tree() -> OxidArt {
        let mut tree = OxidArt::new();
        tree.set(
            SharedByte::from_str("user:1:admin:alice"),
            Value::String(SharedByte::from_str("a")),
        );
        tree.set(
            SharedByte::from_str("user:2:viewer:bob"),
            Value::String(SharedByte::from_str("b")),
        );
        tree.set(
            SharedByte::from_str("user:3:admin:charlie"),
            Value::String(SharedByte::from_str("c")),
        );
        tree.set(
            SharedByte::from_str("user:4:editor:dave"),
            Value::String(SharedByte::from_str("d")),
        );
        tree.set(
            SharedByte::from_str("post:1:title"),
            Value::String(SharedByte::from_str("hello")),
        );
        tree.set(
            SharedByte::from_str("post:2:title"),
            Value::String(SharedByte::from_str("world")),
        );
        tree.set(
            SharedByte::from_str("config:db:host"),
            Value::String(SharedByte::from_str("localhost")),
        );
        tree.set(
            SharedByte::from_str("config:db:port"),
            Value::String(SharedByte::from_str("5432")),
        );
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
        assert_eq!(
            results[0].1,
            Value::String(SharedByte::from_str("localhost"))
        );
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
        assert_eq!(results[0].1, Value::String(SharedByte::from_str("5432")));
    }

    #[test]
    fn expired_entries_skipped() {
        let mut tree = OxidArt::new();
        tree.set_now(100);
        tree.set_ttl(
            SharedByte::from_str("user:1:admin:x"),
            core::time::Duration::from_secs(10),
            Value::String(SharedByte::from_str("val")),
        );
        tree.set(
            SharedByte::from_str("user:2:admin:y"),
            Value::String(SharedByte::from_str("val2")),
        );

        // Before expiry
        let results = tree.getn_regex("user:.*:admin:.*").unwrap();
        assert_eq!(results.len(), 2);

        // After expiry
        tree.set_now(200);
        let results = tree.getn_regex("user:.*:admin:.*").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.as_ref(), b"user:2:admin:y");
    }

    /// `glob_match` contre le chemin DFA (`getn_glob`), sur des clés et des
    /// motifs aléatoires d'un petit alphabet (classes, plages, échappements,
    /// classes non fermées compris).
    #[test]
    fn glob_match_agrees_with_dfa() {
        use rand::{Rng, SeedableRng, rngs::StdRng};
        let mut rng = StdRng::seed_from_u64(7);
        const KEY_ALPHA: &[u8] = b"ab-]^\\*";
        const PAT_ALPHA: &[u8] = b"ab-]^\\*?[";
        let mut tree = OxidArt::new();
        let mut keys = alloc::collections::BTreeSet::new();
        for _ in 0..400 {
            let len = rng.gen_range(1..7);
            let k: Vec<u8> = (0..len).map(|_| KEY_ALPHA[rng.gen_range(0..KEY_ALPHA.len())]).collect();
            tree.set(SharedByte::from_slice(&k), Value::String(SharedByte::from_str("v")));
            keys.insert(k);
        }
        for _ in 0..3000 {
            let len = rng.gen_range(0..7);
            let pat: Vec<u8> = (0..len).map(|_| PAT_ALPHA[rng.gen_range(0..PAT_ALPHA.len())]).collect();
            let mut dfa: Vec<Vec<u8>> =
                tree.getn_glob(&pat).unwrap().into_iter().map(|(k, _)| k.to_vec()).collect();
            dfa.sort();
            let direct: Vec<Vec<u8>> = keys.iter().filter(|k| glob_match(&pat, k)).cloned().collect();
            assert_eq!(direct, dfa, "motif {:?}", String::from_utf8_lossy(&pat));
            let lit = glob_literal_prefix(&pat);
            assert!(direct.iter().all(|k| k.starts_with(lit)));
        }
    }
}
