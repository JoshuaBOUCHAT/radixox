use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use radixox_lib::shared_byte::SharedByte;

use crate::OxidArt;
use crate::node_childs::ChildAble;

// ─── Yield primitive ─────────────────────────────────────────────────────────

/// Rend la main en se plaçant **derrière** les tâches prêtes.
///
/// Sous monoio, un `wake_by_ref` pendant son propre poll remet la tâche en tête
/// de file (`push_front`, monoio scheduler.rs) : elle repasse aussitôt et
/// affame les connexions jusqu'à la fin du travail découpé (KEYS, UNLINK,
/// BGSAVE). Le réveil passe donc par une micro-tâche : venu d'une autre tâche,
/// il est rangé en queue (`push_back`).
pub struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        let waker = cx.waker().clone();
        ::monoio::spawn(async move { waker.wake() });
        Poll::Pending
    }
}

pub fn yield_now() -> YieldNow {
    YieldNow(false)
}

/// Prête quand aucune save n'est en cours. Les libérations d'UNLINK
/// attendent la fin de la save (SAVE_DESIGN.md §2.F) : le sous-arbre coupé
/// reste intact, sans capture en masse. `finish_save` réveille les tâches.
struct SaveDone<'a>(&'a Rc<RefCell<OxidArt>>);

impl Future for SaveDone<'_> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut art = self.0.borrow_mut();
        if !art.is_saving() {
            return Poll::Ready(());
        }
        art.save.waiters.push(cx.waker().clone());
        Poll::Pending
    }
}

// ─── Number of nodes visited per borrow before yielding ──────────────────────

const YIELD_BUDGET: usize = 256;

// ─── Internal helpers on OxidArt ─────────────────────────────────────────────
//
// All the traversal and free logic lives here so lib.rs stays sync-only.

impl OxidArt {
    /// Traverses to the node that exactly covers `prefix` and returns
    /// `(node_idx, full_key_path)` where `key_path` already includes the
    /// matched node's own compression bytes.
    fn find_prefix_node(&self, prefix: &[u8]) -> Option<(u32, Vec<u8>)> {
        use crate::CompResult;

        let prefix_len = prefix.len();

        if prefix_len == 0 {
            return Some((self.root_idx, Vec::new()));
        }

        let mut idx = self.root_idx;
        let mut cursor = 0;
        let mut key_path: Vec<u8> = Vec::new();

        loop {
            let radix = prefix[cursor];
            let child_idx = self.find(idx, radix)?;
            idx = child_idx;
            key_path.push(radix);

            let node = self.try_get_node(idx)?;
            cursor += 1;

            match node.compare_compression_key(&prefix[cursor..]) {
                CompResult::Final => {
                    key_path.extend_from_slice(&node.compression);
                    return Some((idx, key_path));
                }
                CompResult::Partial(common_len) => {
                    let prefix_rest_len = prefix_len - cursor;
                    if common_len == prefix_rest_len {
                        key_path.extend_from_slice(&node.compression);
                        return Some((idx, key_path));
                    }
                    return None;
                }
                CompResult::Path => {
                    key_path.extend_from_slice(&node.compression);
                    cursor += node.compression.len();
                }
            }
        }
    }

    /// Handles the start node of a collect: pushes its key into `keys` if it
    /// has a live value, then seeds `stack` with its children.
    ///
    /// `key_path` must already include the start node's own compression bytes.
    fn init_keys_stack(
        &self,
        start_idx: u32,
        key_path: Vec<u8>,
        keys: &mut Vec<SharedByte>,
        stack: &mut Vec<(u32, Vec<u8>)>,
    ) {
        let Some(node) = self.try_get_node(start_idx) else {
            return;
        };
        if node.get_value(self.now).is_some() {
            keys.push(SharedByte::from_slice(&key_path));
        }
        self.iter_all_children(start_idx, |radix, child_idx| {
            let mut child_key = key_path.clone();
            child_key.push(radix);
            stack.push((child_idx, child_key));
        });
    }

    /// Pops up to `budget` entries from `stack`, appends live keys to `keys`,
    /// and pushes each visited node's children back onto `stack`.
    ///
    /// Stack entry format: `(node_idx, key_prefix_before_this_nodes_compression)`.
    fn collect_keys_chunk(
        &self,
        stack: &mut Vec<(u32, Vec<u8>)>,
        keys: &mut Vec<SharedByte>,
        budget: usize,
    ) {
        let mut processed = 0;
        while processed < budget {
            let Some((node_idx, key_prefix)) = stack.pop() else {
                break;
            };
            let Some(node) = self.try_get_node(node_idx) else {
                continue;
            };
            let mut full_key = key_prefix;
            full_key.extend_from_slice(&node.compression);
            if node.get_value(self.now).is_some() {
                keys.push(SharedByte::from_slice(&full_key));
            }
            self.iter_all_children(node_idx, |radix, child_idx| {
                let mut child_key = full_key.clone();
                child_key.push(radix);
                stack.push((child_idx, child_key));
            });
            processed += 1;
        }
    }

    /// Traverses to the subtree covered by `prefix`, severs it from the tree,
    /// and returns `(free_stack, parent_idx, initial_count)`.
    ///
    /// - `free_stack`     : node indices to free iteratively via `free_chunk`
    /// - `parent_idx`     : node to recompress after freeing (`root_idx` → skip)
    /// - `initial_count`  : deletions already counted (root val for empty prefix)
    fn find_and_cut_prefix(&mut self, prefix: &[u8]) -> (Vec<u32>, u32, usize) {
        let prefix_len = prefix.len();

        if prefix_len == 0 {
            let now = self.now;
            let childs = self.collect_child_indices(self.root_idx);
            let root = self.node_mut(self.root_idx);
            let had_val = root.has_val() && !root.is_expired(now);
            root.clear_val();
            root.childs = Default::default();
            return (childs, self.root_idx, usize::from(had_val));
        }

        let mut parent_idx = self.root_idx;
        let mut parent_radix = prefix[0];
        let Some(mut idx) = self.find(parent_idx, parent_radix) else {
            return (vec![], self.root_idx, 0);
        };
        let mut cursor = 1;

        let target_idx = loop {
            use crate::CompResult;
            let Some(node) = self.try_get_node(idx) else {
                return (vec![], self.root_idx, 0);
            };
            match node.compare_compression_key(&prefix[cursor..]) {
                CompResult::Final => break idx,
                CompResult::Partial(common_len) => {
                    let prefix_rest_len = prefix_len - cursor;
                    if common_len == prefix_rest_len {
                        break idx;
                    }
                    return (vec![], self.root_idx, 0);
                }
                CompResult::Path => {
                    cursor += node.compression.len();
                }
            }
            parent_idx = idx;
            parent_radix = prefix[cursor];
            let Some(child_idx) = self.find(idx, parent_radix) else {
                return (vec![], self.root_idx, 0);
            };
            idx = child_idx;
            cursor += 1;
        };

        self.remove_child(parent_idx, parent_radix);
        (vec![target_idx], parent_idx, 0)
    }

    /// Pops up to `budget` node indices from `stack`, frees each one (and its
    /// Overflow entry if present), returns the number of values deleted.
    fn free_chunk(&mut self, stack: &mut Vec<u32>, budget: usize) -> usize {
        let mut count = 0;
        let mut processed = 0;
        while processed < budget {
            let Some(node_idx) = stack.pop() else {
                break;
            };
            let (children, has_val, overflow_idx) = {
                let Some(node) = self.try_get_node(node_idx) else {
                    continue;
                };
                let mut children: Vec<u32> = node.childs.iter().map(|(_, idx)| idx).collect();
                let overflow_idx = node.get_overflow_idx();
                if let Some(oi) = overflow_idx
                    && let Some(overflow) = self.overflow_arena.get(oi)
                {
                    children.extend(overflow.iter().map(|(_, idx)| idx));
                }
                (children, node.has_val(), overflow_idx)
            };
            stack.extend(children);
            if has_val {
                count += 1;
            }
            // Nœud d'abord, overflow ensuite (la capture copie l'overflow).
            self.remove_node(node_idx);
            if let Some(oi) = overflow_idx {
                self.overflow_arena.free(oi);
            }
            processed += 1;
        }
        count
    }
}

// ─── Public async trait ───────────────────────────────────────────────────────

#[allow(async_fn_in_trait)]
pub trait OxidArtAsync {
    /// Returns all keys whose key starts with `prefix`.
    ///
    /// Yields back to the event loop every `YIELD_BUDGET` nodes so that other
    /// connections are not starved during large prefix scans. The `RefCell`
    /// borrow is dropped before each yield point.
    async fn getn_async(&self, prefix: SharedByte) -> Vec<SharedByte>;

    /// Deletes all keys whose key starts with `prefix` and returns the count.
    ///
    /// Same cooperative-yield guarantee as `getn_async`. The mutable borrow is
    /// dropped before each yield so other tasks may still read the tree between
    /// chunks.
    async fn deln_async(&self, prefix: SharedByte) -> usize;
}

impl OxidArtAsync for Rc<RefCell<OxidArt>> {
    async fn getn_async(&self, prefix: SharedByte) -> Vec<SharedByte> {
        let mut keys: Vec<SharedByte> = Vec::new();

        // Phase 1 — find the prefix node, seed the DFS stack (single borrow).
        let mut stack: Vec<(u32, Vec<u8>)> = {
            let art = self.borrow();
            let Some((start_idx, key_path)) = art.find_prefix_node(prefix.as_ref()) else {
                return keys;
            };
            let mut stack = Vec::new();
            art.init_keys_stack(start_idx, key_path, &mut keys, &mut stack);
            stack
        }; // borrow dropped

        // Phase 2 — chunked DFS: borrow → YIELD_BUDGET nodes → drop → yield.
        while !stack.is_empty() {
            self.borrow()
                .collect_keys_chunk(&mut stack, &mut keys, YIELD_BUDGET);
            // borrow dropped at end of statement
            if !stack.is_empty() {
                yield_now().await;
            }
        }

        keys
    }

    async fn deln_async(&self, prefix: SharedByte) -> usize {
        // Phase 1 — traverse + sever subtree (single borrow_mut).
        let (mut free_stack, parent_idx, initial_count) = {
            let mut art = self.borrow_mut();
            art.find_and_cut_prefix(prefix.as_ref())
        }; // borrow_mut dropped

        if free_stack.is_empty() {
            return initial_count;
        }

        // Phase 2 — chunked free: borrow_mut → YIELD_BUDGET nodes → drop → yield.
        // Pendant une save, chaque tranche attend la fin de la save.
        let mut total = initial_count;
        while !free_stack.is_empty() {
            SaveDone(self).await;
            total += self.borrow_mut().free_chunk(&mut free_stack, YIELD_BUDGET);
            // borrow_mut dropped at end of statement
            if !free_stack.is_empty() {
                yield_now().await;
            }
        }

        // Phase 3 — recompression (single borrow_mut).
        {
            let mut art = self.borrow_mut();
            let root_idx = art.root_idx;
            if parent_idx != root_idx {
                art.try_recompress(parent_idx);
            }
        }

        total
    }
}

#[cfg(test)]
mod tests {
    use radixox_lib::shared_byte::SharedByte;

    use super::*;
    use crate::value::Value;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn make_art() -> Rc<RefCell<OxidArt>> {
        let art = Rc::new(RefCell::new(OxidArt::new()));
        let mut a = art.borrow_mut();
        a.set(
            SharedByte::from_str("user:1:name"),
            Value::String(SharedByte::from_str("alice")),
        );
        a.set(
            SharedByte::from_str("user:1:role"),
            Value::String(SharedByte::from_str("admin")),
        );
        a.set(
            SharedByte::from_str("user:2:name"),
            Value::String(SharedByte::from_str("bob")),
        );
        a.set(
            SharedByte::from_str("user:2:role"),
            Value::String(SharedByte::from_str("viewer")),
        );
        a.set(
            SharedByte::from_str("user:3:name"),
            Value::String(SharedByte::from_str("carol")),
        );
        a.set(
            SharedByte::from_str("session:abc"),
            Value::String(SharedByte::from_str("tok1")),
        );
        a.set(
            SharedByte::from_str("session:def"),
            Value::String(SharedByte::from_str("tok2")),
        );
        a.set(
            SharedByte::from_str("config:timeout"),
            Value::String(SharedByte::from_str("30")),
        );
        drop(a);
        art
    }

    #[monoio::test]
    async fn getn_async_prefix() {
        let art = make_art();
        let mut keys = art.getn_async(SharedByte::from_str("user:1:")).await;
        keys.sort();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].as_ref(), b"user:1:name");
        assert_eq!(keys[1].as_ref(), b"user:1:role");
    }

    #[monoio::test]
    async fn getn_async_empty_prefix_returns_all() {
        let art = make_art();
        assert_eq!(art.getn_async(SharedByte::from_str("")).await.len(), 8);
    }

    #[monoio::test]
    async fn getn_async_no_match() {
        let art = make_art();
        assert!(
            art.getn_async(SharedByte::from_str("notfound:"))
                .await
                .is_empty()
        );
    }

    #[monoio::test]
    async fn deln_async_prefix() {
        let art = make_art();
        assert_eq!(art.deln_async(SharedByte::from_str("user:")).await, 5);
        assert!(
            art.getn_async(SharedByte::from_str("user:"))
                .await
                .is_empty()
        );
        assert_eq!(
            art.getn_async(SharedByte::from_str("session:")).await.len(),
            2
        );
        assert_eq!(
            art.getn_async(SharedByte::from_str("config:")).await.len(),
            1
        );
    }

    #[monoio::test]
    async fn deln_async_empty_prefix_flushdb() {
        let art = make_art();
        assert_eq!(art.deln_async(SharedByte::from_str("")).await, 8);
        assert!(art.getn_async(SharedByte::from_str("")).await.is_empty());
    }

    /// UNLINK pendant une save : coupe immédiate (la clé disparaît de la vue
    /// vivante), libération seulement après `finish_save`, vue T0 complète.
    #[monoio::test]
    async fn deln_async_waits_for_save() {
        let art = make_art();
        let expect = crate::save::reference::expected(&art.borrow());
        let nodes = art.borrow().node_count();
        let mut out = Vec::new();
        let mut cur = art.borrow_mut().save_start(&mut out);
        let mut unlink = Box::pin(art.deln_async(SharedByte::from_str("user:")));
        // Premier poll : coupe, puis attente de la fin de la save.
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(unlink.as_mut().poll(&mut cx).is_pending());
        assert!(unlink.as_mut().poll(&mut cx).is_pending());
        assert!(art.borrow().getn(SharedByte::from_str("user:")).is_empty());
        assert_eq!(art.borrow().node_count(), nodes, "rien n'est libéré pendant la save");
        while !art.borrow_mut().save_step(&mut cur, &mut out, 1) {
            assert!(unlink.as_mut().poll(&mut cx).is_pending());
        }
        art.borrow_mut().finish_save();
        assert_eq!(crate::save::reference::decoded(&out), expect);
        assert_eq!(unlink.await, 5);
        assert!(art.borrow().node_count() < nodes);
    }

    #[monoio::test]
    async fn deln_async_empty_prefix_clears_root_value() {
        let art = make_art();
        art.borrow_mut()
            .set(SharedByte::from_str(""), Value::from_str("root"));
        assert_eq!(art.deln_async(SharedByte::from_str("")).await, 9);
        assert_eq!(art.borrow_mut().get(b""), None);
    }

    #[monoio::test]
    async fn deln_async_no_match_returns_zero() {
        let art = make_art();
        assert_eq!(art.deln_async(SharedByte::from_str("ghost:")).await, 0);
        assert_eq!(art.getn_async(SharedByte::from_str("")).await.len(), 8);
    }

    #[monoio::test]
    async fn deln_async_partial_prefix() {
        let art = make_art();
        assert_eq!(art.deln_async(SharedByte::from_str("user:1:")).await, 2);
        assert_eq!(art.getn_async(SharedByte::from_str("user:")).await.len(), 3);
    }

    #[monoio::test]
    async fn getn_then_deln_async_consistency() {
        let art = make_art();
        assert_eq!(
            art.getn_async(SharedByte::from_str("session:")).await.len(),
            2
        );
        assert_eq!(art.deln_async(SharedByte::from_str("session:")).await, 2);
        assert!(
            art.getn_async(SharedByte::from_str("session:"))
                .await
                .is_empty()
        );
    }

    #[monoio::test]
    async fn multiple_async_ops_leave_tree_consistent() {
        let art = make_art();
        art.deln_async(SharedByte::from_str("user:1:")).await;
        art.deln_async(SharedByte::from_str("session:")).await;

        let remaining = art.getn_async(SharedByte::from_str("")).await;
        assert_eq!(remaining.len(), 4); // user:2:name, user:2:role, user:3:name, config:timeout
        for key in &remaining {
            assert!(
                key.starts_with(b"user:") || key.starts_with(b"config:"),
                "unexpected key: {:?}",
                key
            );
        }
    }
}
