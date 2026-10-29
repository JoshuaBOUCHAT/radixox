//! État global du thread de la boucle, sans `thread_local!` (absent sous `no_std`).

#[allow(unused_imports)]
use crate::prelude::*;

use core::ops::Deref;

/// Valeur d'un `static` touchée par un seul thread à la fois : le thread de la boucle
/// (seul à exécuter des commandes, comme pour les slabs de l'arbre), ou celui d'un test
/// (`RUST_TEST_THREADS=1`). Enveloppe un `Cell` / `RefCell`, qui garde ses vérifications.
pub(crate) struct Mono<T>(T);

// SAFETY: jamais deux threads à la fois (voir le type) ; c'est l'invariant de tout le
// processus, déjà exigé par les slabs statiques.
unsafe impl<T> Sync for Mono<T> {}

impl<T> Mono<T> {
    pub(crate) const fn new(v: T) -> Self {
        Self(v)
    }
}

impl<T> Deref for Mono<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}
