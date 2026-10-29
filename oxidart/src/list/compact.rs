//! Compactage de la slab des blocs de List (comme celle des nœuds, MEMORY_ARCH.md §17.2) :
//! les blocs les plus hauts descendent dans les trous les plus bas, puis la fin de la slab
//! est rendue au système. Un bloc n'est référencé que par ses deux voisins, ou, en bout de
//! chaîne, par l'en-tête de sa List (`HEAD | idx`) : un déplacement corrige au plus deux
//! références, sans parcours. Les blocs de bout d'une List hors de la slab (détachée, en
//! cours de libération : `NONE`) restent en place.
//!
//! Interdit pendant une save (son parcours et son occupation T0 sont indexés par `idx`) :
//! c'est au pilote de ne pas l'avancer. Rien ne garde d'idx de bloc entre deux commandes.

#[allow(unused_imports)]
use crate::prelude::*;
use super::{Block, HEAD, NONE, block, block_mut, is_end, slab};

/// Blocs par page de 2 Mio.
const PAGE_BLOCKS: u32 = ((2 << 20) / size_of::<Block>()) as u32;
/// Coût d'un déplacement (copie + deux références), en lignes de cache.
const MOVE_COST: u32 = 8;

/// Une passe en cours.
#[derive(Debug)]
pub struct ListCompaction {
    /// Les blocs à partir de celui-ci sont vidés, puis rendus.
    target: u32,
    /// Blocs libres gardés après rendu.
    keep: u32,
    /// Prochaine source : sous ce bloc.
    scan: u32,
    pub moved: u64,
}

/// Octets de trous sous le dernier bloc occupé.
pub fn holes_bytes() -> u64 {
    let s = slab();
    let span = s.last_occupied().map_or(0, |l| l + 1);
    (span - s.len()) as u64 * size_of::<Block>() as u64
}

/// Une passe si les trous sous la fin dépassent 3 Mio (4 Mio au-delà de 256 Mio de slab,
/// comme la slab des nœuds) ; `force` : toujours, sans garder de libre (tests).
pub fn want(force: bool) -> Option<ListCompaction> {
    let s = slab();
    let span = s.last_occupied()? + 1;
    let slab_bytes = span as u64 * size_of::<Block>() as u64;
    let threshold = if slab_bytes > 256 << 20 { 4 << 20 } else { 3 << 20 };
    if !force && holes_bytes() <= threshold {
        return None;
    }
    let keep = if force { 0 } else { PAGE_BLOCKS };
    let target = if force { s.len() } else { (s.len() + keep).next_multiple_of(PAGE_BLOCKS) };
    (target < span).then_some(ListCompaction { target, keep, scan: span, moved: 0 })
}

/// Une tranche d'au plus `budget` unités. `Some((déplacés, octets rendus))` quand la passe
/// est finie.
pub fn step(c: &mut ListCompaction, budget: u32) -> Option<(u64, usize)> {
    let mut spent = 0;
    while spent < budget {
        let s = slab();
        let Some(src) = s.prev_occupied(c.scan).filter(|&src| src >= c.target) else { return Some(finish(c)) };
        c.scan = src;
        let b = block(src);
        // bout d'une List hors de la slab : son en-tête n'est pas joignable
        if b.prev == NONE || b.next == NONE {
            spent += 1;
            continue;
        }
        // SAFETY: rempli par `move_to` tout de suite, ou rendu.
        let dst = unsafe { s.reserve_lowest() };
        if dst >= src || dst >= c.target {
            unsafe { s.unreserve(dst) };
            return Some(finish(c));
        }
        // SAFETY: `src` occupé, `dst` réservé, aucune référence vivante dans la slab.
        unsafe { s.move_to(src, dst) };
        relink(dst);
        c.moved += 1;
        spent += MOVE_COST;
    }
    None
}

/// Le bloc est maintenant en `dst` : ses voisins (ou l'en-tête de sa List) pointent dessus.
fn relink(dst: u32) {
    let (prev, next) = (block(dst).prev, block(dst).next);
    if is_end(prev) {
        crate::value::list_header_raw(prev & !HEAD).first = dst;
    } else {
        block_mut(prev).next = dst;
    }
    if is_end(next) {
        crate::value::list_header_raw(next & !HEAD).last = dst;
    } else {
        block_mut(next).prev = dst;
    }
}

fn finish(c: &ListCompaction) -> (u64, usize) {
    let s = slab();
    let last = s.last_occupied().map_or(0, |l| l + 1);
    let from = last.max(s.len() + c.keep);
    // SAFETY: rien n'est occupé à partir de `last` ≤ `from`.
    (c.moved, unsafe { s.release_above(from) })
}
