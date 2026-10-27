//! Libération par tranches des grandes List (DEL, écrasement, expiration, FLUSHDB), comme
//! les grands ZSet (`zset::lazy`) : la clé disparaît tout de suite, les blocs attendent
//! dans une file. Pendant une save, rien n'avance.

use std::collections::VecDeque;

use super::{CAP, List};

/// En dessous, la libération est faite tout de suite (quelques blocs).
pub const LAZY_MIN: u32 = 16 * CAP as u32;

static mut QUEUE: VecDeque<List> = VecDeque::new();

#[inline]
fn queue() -> &'static mut VecDeque<List> {
    #[allow(static_mut_refs)]
    unsafe {
        &mut QUEUE
    }
}

/// Rend la List `l`, sortie de sa slab : tout de suite si elle est petite, sinon par la file.
pub(crate) fn release(mut l: List) {
    if l.len() > LAZY_MIN {
        queue().push_back(l);
    } else {
        l.clear();
    }
}

/// Une tranche : jusqu'à `budget` blocs. Vrai s'il reste du travail (y compris en attente
/// de la fin d'une save).
pub fn step(budget: u32) -> bool {
    if super::save::is_saving() {
        return !queue().is_empty();
    }
    let mut budget = budget;
    while budget > 0
        && let Some(l) = queue().front_mut()
    {
        let n = budget.min(l.len().div_ceil(CAP as u32).max(1));
        if l.clear_step(n) {
            queue().pop_front();
        }
        budget -= n;
    }
    if queue().is_empty() && queue().capacity() > 0 {
        *queue() = VecDeque::new();
    }
    !queue().is_empty()
}

/// List en attente de libération.
pub fn pending() -> usize {
    queue().len()
}
