use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use radixox_lib::shared_byte::SharedByte;

use crate::node::EntryRef;
use crate::{OxidArt, PrefixHit, expired};

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

/// Étage du parcours de KEYS : nœud, clé jusqu'à son radix inclus (son
/// préfixe pas encore ajouté, sauf pour le nœud de départ).
type KeysFrame = (u32, Vec<u8>, bool);

impl OxidArt {
    /// Visite au plus `budget` nœuds de `stack`, ajoute les clés vivantes à
    /// `keys`. Les idx sont relus par `try_get_node` : entre deux tranches,
    /// l'arbre a pu changer (lecture au mieux, comme le KEYS de Redis).
    fn collect_keys_chunk(&self, stack: &mut Vec<KeysFrame>, keys: &mut Vec<SharedByte>, budget: usize) {
        let now = self.now;
        for _ in 0..budget {
            let Some((idx, mut key, is_start)) = stack.pop() else {
                break;
            };
            let Some(node) = self.try_get_node(idx) else {
                continue;
            };
            if !is_start {
                key.extend_from_slice(&node.prefix);
            }
            if node.has_val() && !expired(node.exp_and_radix.exp(), now) {
                keys.push(SharedByte::from_slice(&key));
            }
            let base = key.len();
            for (r, e) in node.iter() {
                key.push(r);
                match e {
                    EntryRef::Child(c) => stack.push((c, key.clone(), false)),
                    EntryRef::Leaf { ttl, sfx, .. } => {
                        if !expired(ttl, now) {
                            if let Some(sfx) = sfx {
                                key.extend_from_slice(sfx);
                            }
                            keys.push(SharedByte::from_slice(&key));
                        }
                    }
                }
                key.truncate(base);
            }
        }
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

        // Phase 1 — descente vers le préfixe (un seul emprunt).
        let mut stack: Vec<KeysFrame> = {
            let art = self.borrow();
            let mut path = Vec::new();
            match art.find_prefix(prefix.as_ref(), &mut path) {
                PrefixHit::None => return keys,
                PrefixHit::Leaf { node, radix } => {
                    if let Some(EntryRef::Leaf { ttl, .. }) = art.node(node).get(radix)
                        && !expired(ttl, art.now)
                    {
                        keys.push(SharedByte::from_slice(&path));
                    }
                    return keys;
                }
                PrefixHit::Node(idx) => vec![(idx, path, true)],
            }
        }; // borrow dropped

        // Phase 2 — DFS par tranches : emprunt → YIELD_BUDGET nœuds → yield.
        while !stack.is_empty() {
            self.borrow().collect_keys_chunk(&mut stack, &mut keys, YIELD_BUDGET);
            if !stack.is_empty() {
                yield_now().await;
            }
        }
        keys
    }

    async fn deln_async(&self, prefix: SharedByte) -> usize {
        // Phase 1 — coupe du sous-arbre et normalisation (un seul emprunt) :
        // ce qui reste à libérer est déjà inaccessible.
        let (mut free_stack, mut total) = self.borrow_mut().cut_prefix(prefix.as_ref());

        // Phase 2 — libération par tranches. Pendant une save, chaque tranche
        // attend la fin de la save (le sous-arbre coupé reste dans la vue T0).
        while !free_stack.is_empty() {
            SaveDone(self).await;
            total += self.borrow_mut().free_subtrees(&mut free_stack, YIELD_BUDGET);
            if !free_stack.is_empty() {
                yield_now().await;
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
