//! Save des List (ANALYSE_ART_UNIFIE.md §14.4) : CoW des blocs derrière la barrière.

/// Avant d'écrire ou de libérer le bloc `i`.
#[inline(always)]
pub(crate) fn before_block_write(_i: u32) {}

/// Une save est en cours (côté List).
#[inline]
pub(crate) fn is_saving() -> bool {
    false
}
