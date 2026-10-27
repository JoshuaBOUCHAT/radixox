//! Libération par tranches des grands ZSet (DEL, écrasement, expiration, FLUSHDB) :
//! supprimer un ZSet, c'est rendre chacun de ses membres, ses feuilles, ses nœuds et son
//! index ; au-delà de [`LAZY_MIN`] membres, c'est une machine à état avancée d'une tranche
//! à la fois (ARCHITECTURE.md §1.2 : jamais bloquant).
//!
//! La clé disparaît tout de suite (le ZSet est détaché) ; le contenu attend dans une file.
//! Pendant une save, rien n'avance : chaque feuille rendue serait capturée par la barrière
//! (SAVE_DESIGN.md §2.F, même règle qu'UNLINK).

use std::collections::VecDeque;

use super::ZSet;
use super::btree::BTree;
use super::index::Index;

/// En dessous, la libération est faite tout de suite (quelques feuilles).
pub const LAZY_MIN: u32 = 512;

/// Un ZSet à rendre : d'abord l'arbre (feuilles avec leurs membres, nœuds internes), puis
/// l'index.
struct Job {
    tree: Vec<(u32, u8)>,
    index: Vec<u32>,
}

static mut QUEUE: VecDeque<Job> = VecDeque::new();

#[inline]
fn queue() -> &'static mut VecDeque<Job> {
    #[allow(static_mut_refs)]
    unsafe {
        &mut QUEUE
    }
}

/// Rend le ZSet `z`, sorti de sa slab : tout de suite s'il est petit, sinon par la file.
pub(crate) fn release(z: &mut ZSet) {
    if let ZSet::Large(l) = z
        && l.len() > LAZY_MIN
        && let Some(mut l) = z.take_large()
    {
        queue().push_back(Job { tree: l.tree.take_for_free(), index: l.index.take_for_free() });
    }
    // le reste (petit, ou vidé) part avec le `Drop`
}

/// Une tranche : jusqu'à `budget` unités (1 par nœud, 1 par membre). Vrai s'il reste du
/// travail (y compris en attente de la fin d'une save).
pub fn step(mut budget: u32) -> bool {
    if super::save::is_saving() {
        return !queue().is_empty();
    }
    while budget > 0
        && let Some(job) = queue().front_mut()
    {
        if !job.tree.is_empty() {
            BTree::free_step(&mut job.tree, &mut budget, super::save::free_handle);
        } else if !job.index.is_empty() {
            Index::free_step(&mut job.index, &mut budget);
        } else {
            queue().pop_front();
        }
    }
    if queue().is_empty() && queue().capacity() > 0 {
        *queue() = VecDeque::new();
    }
    !queue().is_empty()
}

/// ZSet en attente de libération.
pub fn pending() -> usize {
    queue().len()
}
