//! Libération par tranches des grands ZSet (DEL, écrasement, expiration, FLUSHDB) :
//! supprimer un ZSet, c'est rendre chacun de ses membres, ses feuilles, ses nœuds et son
//! index ; au-delà de [`LAZY_MIN`] membres, c'est une machine à état avancée d'une tranche
//! à la fois (ARCHITECTURE.md §1.2 : jamais bloquant).
//!
//! La clé disparaît tout de suite (le ZSet est détaché) ; le contenu attend dans une file.
//! Pendant une save, rien n'avance : chaque feuille rendue serait capturée par la barrière
//! (SAVE_DESIGN.md §2.F, même règle qu'UNLINK).

#[allow(unused_imports)]
use crate::prelude::*;
use alloc::collections::VecDeque;

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

/// Cimetières : sous-arbres décrochés par un retrait de plage (`ZLarge::remove_range`),
/// par ZSet (idx de son en-tête). Leurs membres ont encore une entrée dans l'index du ZSet
/// (périmée, `ZLarge::stale`) : chaque tranche la retire, puis rend la poignée et les nœuds.
static mut GRAVES: VecDeque<(u32, Vec<(u32, u8)>)> = VecDeque::new();

#[inline]
fn graves() -> &'static mut VecDeque<(u32, Vec<(u32, u8)>)> {
    #[allow(static_mut_refs)]
    unsafe {
        &mut GRAVES
    }
}

/// Sous-arbres décrochés du ZSet `zset`, à nettoyer.
pub(crate) fn bury(zset: u32, stack: Vec<(u32, u8)>) {
    match graves().iter_mut().find(|g| g.0 == zset) {
        Some(g) => g.1.extend(stack),
        None => graves().push_back((zset, stack)),
    }
}

/// ZSet `zset` supprimé : son cimetière est rendu comme un arbre ordinaire (l'index part
/// entier avec le ZSet).
pub(crate) fn exhume(zset: u32) {
    if let Some(i) = graves().iter().position(|g| g.0 == zset) {
        let (_, tree) = graves().remove(i).expect("cimetière");
        queue().push_back(Job { tree, index: Vec::new() });
    }
}

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
        return !queue().is_empty() || !graves().is_empty();
    }
    while budget > 0
        && let Some((zset, stack)) = graves().front_mut()
    {
        let ZSet::Large(z) = crate::value::zset_mut(*zset) else { unreachable!("cimetière d'un petit ZSet") };
        BTree::free_step(stack, &mut budget, |h| {
            z.index.remove_by(super::large::rehash(h), |x, _| x == h).expect("entrée périmée");
            z.stale -= 1;
            super::save::free_handle(h);
        });
        if stack.is_empty() {
            graves().pop_front();
        }
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
    !queue().is_empty() || !graves().is_empty()
}

/// ZSet en attente de libération.
pub fn pending() -> usize {
    queue().len() + graves().len()
}
