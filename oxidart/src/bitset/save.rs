//! Save des bitsets (BITSET_DESIGN.md §10.3) : barrière par bitset entier sur le slab des
//! en-têtes ; l'image T0 est une copie (offsets ou mots) prise à la première écriture
//! pendant la save. La sérialisation avance par tranches avec `peek` (lit l'image si elle
//! est apparue entre deux tranches) et consomme la version T0 à la fin.
//!
//! Un bitset supprimé pendant la save garde sa case jusqu'à la fin (liste différée).

#[allow(unused_imports)]
use crate::prelude::*;
use radixox_lib::save_barrier::{SaveBarrier, T0};
use radixox_lib::varint::put_varint;

use super::{END, Image};
use crate::digest;

#[derive(Default)]
struct BSave {
    barrier: SaveBarrier<Image>,
    deferred: Vec<u32>,
}

static mut BSAVE: Option<BSave> = None;

#[inline(always)]
fn bsave() -> &'static mut BSave {
    #[allow(static_mut_refs)]
    unsafe {
        BSAVE.get_or_insert_with(BSave::default)
    }
}

/// Une save est en cours (côté bitsets).
#[inline]
pub(crate) fn is_saving() -> bool {
    bsave().barrier.is_active()
}

pub(crate) fn begin() {
    bsave().barrier.begin_with(super::slab_occupancy);
}

pub(crate) fn end() {
    bsave().barrier.finish();
}

/// Avant de modifier ou de libérer le bitset `idx` : capture T0.
#[inline]
pub(crate) fn before_write(idx: u32) {
    let s = bsave();
    if s.barrier.is_active() {
        s.barrier.before_write(idx, || super::get(idx).image());
    }
}

/// Pendant une save, un bitset supprimé garde sa case : vrai si différé.
pub(crate) fn defer(idx: u32) -> bool {
    if !is_saving() {
        return false;
    }
    bsave().deferred.push(idx);
    true
}

/// Après la save : libère les bitsets différés (`budget` : 1 par bitset). Rend le reste.
pub(crate) fn drain_step(mut budget: u32) -> u32 {
    debug_assert!(!is_saving());
    let s = bsave();
    while budget > 0
        && let Some(idx) = s.deferred.pop()
    {
        super::free(idx);
        budget -= 1;
    }
    if s.deferred.is_empty() {
        s.deferred = Vec::new();
    }
    budget
}

pub(crate) fn deferred_empty() -> bool {
    bsave().deferred.is_empty()
}

/// Terme d'un offset dans le condensé.
#[inline]
pub(crate) fn digest_item(off: u32) -> u64 {
    let mut h = crc32fast::Hasher::new();
    digest::part(&mut h, &off.to_le_bytes());
    h.finalize() as u64
}

/// Sérialiseur en cours d'un bitset : prochain offset à écrire, offset précédent (deltas).
pub(crate) struct BSer {
    idx: u32,
    next: u64,
    prev: Option<u32>,
    left: u64,
    card: u64,
    sum: u64,
    key: Vec<u8>,
    exp: Option<u64>,
}

/// Écrit le nombre de bits à 1 T0 du bitset `idx`, rend son sérialiseur.
pub(crate) fn ser_begin(idx: u32, key: &[u8], exp: Option<u64>, out: &mut Vec<u8>) -> BSer {
    let card = match bsave().barrier.peek(idx) {
        Some(T0::Slab) => super::get(idx).count(),
        Some(T0::Image(img)) => img.count(),
        None => panic!("bitset {idx} absent de la vue T0"),
    };
    put_varint(out, card);
    BSer { idx, next: 0, prev: None, left: card, card, sum: 0, key: key.to_vec(), exp }
}

impl BSer {
    /// Des offsets jusqu'à épuiser `budget` (1 par offset). Vrai à la fin (version T0
    /// consommée, terme du condensé ajouté à `d`).
    pub(crate) fn step(&mut self, out: &mut Vec<u8>, budget: &mut u32, d: &mut digest::Digest) -> bool {
        if self.left > 0 && *budget > 0 {
            let before = self.left;
            let from = self.next;
            let mut n = (*budget as u64).min(self.left);
            let mut write = |k: u32| {
                if n == 0 {
                    return false;
                }
                put_varint(out, match self.prev {
                    None => k as u64,
                    Some(p) => (k - p) as u64,
                });
                self.prev = Some(k);
                self.sum = self.sum.wrapping_add(digest_item(k));
                self.next = k as u64 + 1;
                self.left -= 1;
                n -= 1;
                true
            };
            match bsave().barrier.peek(self.idx) {
                Some(T0::Slab) => {
                    super::get(self.idx).scan(from, END, &mut write);
                }
                Some(T0::Image(img)) => img.scan(from, &mut write),
                None => panic!("bitset {} absent de la vue T0", self.idx),
            }
            *budget = budget.saturating_sub((before - self.left) as u32 + 1);
        }
        if self.left > 0 {
            return false;
        }
        bsave().barrier.take(self.idx);
        d.add(digest::seq_term(&self.key, crate::save::format::T_BITSET, self.exp, (self.sum, self.card)));
        true
    }
}
