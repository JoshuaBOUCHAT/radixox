//! Registre des pages (MEMORY_ARCH.md §10.4) : la mémoire du processus est comptée **avant**
//! d'être écrite, pas mesurée après. Chaque région déclare ce qu'elle peut rendre résident
//! (par tranche de 2 Mio sous THP, par page de 4 Kio sinon) au moment où elle grandit, et le
//! rend quand elle rend ses pages au système. Le compte surestime (une tranche entière pour
//! un octet écrit), jamais il ne sous-estime : c'est la borne que `maxmemory` peut tenir.
//!
//! [`charge`] ne refuse jamais : une région qui grandit est au milieu d'une écriture. Refuser
//! se fait avant la commande, contre [`charged`] (réservation de `maxmemory`).
//!
//! Coût : rien sur les chemins chauds. Une région teste déjà où elle en est (sommet d'un
//! HiSlab, page neuve du pool) ; le registre ne bouge qu'au franchissement d'une tranche.
//! `mincore` (`crate::resident`) reste la vérification : `tests/memory.rs` exige que le
//! registre couvre le résident mesuré.

use core::cell::Cell;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Huge page : granularité des régions sous THP.
pub const TRANCHE: usize = 2 << 20;
/// Page : granularité des régions en `MADV_NOHUGEPAGE`.
pub const PAGE: usize = 4096;

// Atomiques : les mappings directs se créent de tout thread. Relaxed : un compte, pas une
// synchronisation ; chemins froids seulement.
static CHARGED: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

/// `bytes` de plus peuvent devenir résidents.
#[inline]
pub fn charge(bytes: usize) {
    let n = CHARGED.fetch_add(bytes as u64, Relaxed) + bytes as u64;
    PEAK.fetch_max(n, Relaxed);
}

/// `bytes` rendus au système.
#[inline]
pub fn credit(bytes: usize) {
    CHARGED.fetch_sub(bytes as u64, Relaxed);
}

/// Octets comptés, toutes régions.
pub fn charged() -> u64 {
    CHARGED.load(Relaxed)
}

pub fn peak() -> u64 {
    PEAK.load(Relaxed)
}

/// Part comptée d'une région écrite par le début (`[base, base + n)`). Le compte part de la
/// frontière de granularité sous `base` : une huge page prise à la première écriture d'une
/// région non alignée commence avant elle, et doit être à quelqu'un (§18.14). Deux régions
/// voisines peuvent compter la même tranche : surestimation, jamais l'inverse.
pub struct Tranches {
    base: usize,
    gran: usize,
    /// Octets comptés, depuis la frontière sous `base`.
    charged: Cell<usize>,
}

impl Tranches {
    pub const fn new(base: usize, gran: usize) -> Self {
        Self { base, gran, charged: Cell::new(0) }
    }

    /// Fin (exclue, depuis `base`) déjà couverte : écrire en dessous ne demande rien.
    #[inline]
    pub fn covered(&self) -> usize {
        let c = self.charged.get();
        if c == 0 { 0 } else { self.base / self.gran * self.gran + c - self.base }
    }

    /// La région va écrire jusqu'à `end` (exclu, depuis `base`).
    #[inline]
    pub fn ensure(&self, end: usize) {
        if end > self.covered() {
            self.grow(end);
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&self, end: usize) {
        let need = self.need(end);
        let c = self.charged.get();
        if need > c {
            charge(need - c);
            self.charged.set(need);
        }
    }

    /// La région a rendu ses pages au-delà de `end` (depuis `base`).
    pub fn shrink(&self, end: usize) {
        let need = if end == 0 { 0 } else { self.need(end) };
        let c = self.charged.get();
        if need < c {
            credit(c - need);
            self.charged.set(need);
        }
    }

    fn need(&self, end: usize) -> usize {
        let lo = self.base / self.gran * self.gran;
        (self.base + end).next_multiple_of(self.gran) - lo
    }

    /// Octets comptés par cette région.
    pub fn bytes(&self) -> usize {
        self.charged.get()
    }
}

impl Drop for Tranches {
    fn drop(&mut self) {
        credit(self.charged.get());
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::*;

    #[test]
    fn tranches_cover_unaligned_start_and_give_back() {
        let c0 = charged();
        // Région qui commence 4 Kio avant une frontière de 2 Mio.
        let base = 10 * TRANCHE - PAGE;
        let t = Tranches::new(base, TRANCHE);
        t.ensure(1);
        assert_eq!(t.bytes(), TRANCHE, "la tranche sous base");
        assert_eq!(t.covered(), PAGE);
        t.ensure(PAGE + 1);
        assert_eq!(t.bytes(), 2 * TRANCHE);
        t.ensure(PAGE + 10);
        assert_eq!(charged() - c0, 2 * TRANCHE as u64, "rien de plus dans la même tranche");
        t.shrink(PAGE);
        assert_eq!(t.bytes(), TRANCHE);
        drop(t);
        assert_eq!(charged(), c0);
    }
}
