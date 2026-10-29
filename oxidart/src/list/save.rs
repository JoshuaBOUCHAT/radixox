//! Save des List en CoW (ANALYSE_ART_UNIFIE.md §14.4, SAVE_DESIGN.md §D) : la List est
//! écrite dans l'enregistrement de sa clé, en flux, sur sa vue T0 ; au `begin`, seulement
//! la copie des bitmaps d'occupation.
//!
//! - **En-têtes** (slab des List) derrière une barrière : point de passage `list_mut`,
//!   `free_list`. Image : l'en-tête (`first`, `len`).
//! - **Blocs** derrière une barrière : points de passage `block_mut`, `free_block`. Image :
//!   le bloc suivant et les valeurs vivantes, **clonées** : le GC les voit partagées et ne
//!   les déplace pas, et une valeur retirée pendant la save reste lisible par l'image.
//! - Une List supprimée pendant la save garde ses blocs jusqu'à la fin (liste différée,
//!   rendue par tranches ensuite) ; la libération par tranches (`lazy`) est gelée.

#[allow(unused_imports)]
use crate::prelude::*;
use radixox_lib::save_barrier::{SaveBarrier, T0};
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::varint::{put_bytes, put_varint};

use super::{List, block, is_end};
use crate::digest;

/// Image T0 d'un bloc.
pub(crate) struct BlockImage {
    next: u32,
    vals: Vec<SharedByte>,
}

#[derive(Default)]
struct LSave {
    headers: SaveBarrier<List>,
    blocks: SaveBarrier<BlockImage>,
    /// List supprimées pendant la save (blocs intacts pour la vue T0).
    deferred: Vec<List>,
}

static mut LSAVE: Option<LSave> = None;

#[inline(always)]
fn lsave() -> &'static mut LSave {
    #[allow(static_mut_refs)]
    unsafe {
        match LSAVE.as_mut() {
            Some(s) => s,
            None => init_lsave(),
        }
    }
}

#[cold]
#[inline(never)]
fn init_lsave() -> &'static mut LSave {
    #[allow(static_mut_refs)]
    unsafe {
        LSAVE.insert(LSave::default())
    }
}

/// Une save est en cours (côté List).
#[inline]
pub(crate) fn is_saving() -> bool {
    lsave().headers.is_active()
}

/// Début de save : vue T0 des en-têtes et des blocs (deux copies de bitmap).
pub(crate) fn begin() {
    let s = lsave();
    s.headers.begin_with(crate::value::list_slab_occupancy);
    s.blocks.begin_with(super::slab_occupancy);
}

/// Fin de save (normale ou abandon) : images rendues ; les List différées restent, pour
/// [`drain_step`].
pub(crate) fn end() {
    let s = lsave();
    s.headers.finish();
    s.blocks.finish();
}

/// Avant d'écrire ou de libérer le bloc `i` : capture T0.
#[inline(always)]
pub(crate) fn before_block_write(i: u32) {
    let s = lsave();
    if s.blocks.is_active() {
        s.blocks.before_write(i, || {
            let b = block(i);
            BlockImage { next: b.next_block(), vals: b.values().cloned().collect() }
        });
    }
}

/// Avant de modifier ou de libérer l'en-tête `idx` : capture T0.
#[inline]
pub(crate) fn before_header_write(idx: u32, capture: impl FnOnce() -> List) {
    let s = lsave();
    if s.headers.is_active() {
        s.headers.before_write(idx, capture);
    }
}

/// Pendant une save, une List supprimée garde ses blocs jusqu'à la fin : `None` si
/// différée.
#[inline]
pub(crate) fn defer_list(l: List) -> Option<List> {
    if !is_saving() {
        return Some(l);
    }
    lsave().deferred.push(l);
    None
}

/// Après la save : les List différées partent à la libération (par tranches au-delà de
/// `lazy::LAZY_MIN`). `budget` : 1 par List. Rend le budget restant.
pub(crate) fn drain_step(mut budget: u32) -> u32 {
    debug_assert!(!is_saving());
    let s = lsave();
    while budget > 0
        && let Some(l) = s.deferred.pop()
    {
        super::lazy::release(l);
        budget -= 1;
    }
    if s.deferred.is_empty() {
        s.deferred = Vec::new();
    }
    budget
}

pub(crate) fn deferred_empty() -> bool {
    lsave().deferred.is_empty()
}

/// Sérialiseur en cours d'une List : bloc T0 suivant, éléments restant à écrire, condensé
/// (comme `digest::list_inner` : le rang compte).
pub(crate) struct LSer {
    next: u32,
    left: u32,
    card: u32,
    sum: u64,
    rank: u64,
    key: Vec<u8>,
    exp: Option<u64>,
}

/// Écrit le cardinal T0 de la List `idx`, rend son sérialiseur.
pub(crate) fn ser_begin(idx: u32, key: &[u8], exp: Option<u64>, out: &mut Vec<u8>) -> LSer {
    let h = match lsave().headers.take(idx) {
        Some(T0::Slab) => *crate::value::list_ref(idx),
        Some(T0::Image(l)) => *l,
        None => panic!("List {idx} absente de la vue T0"),
    };
    put_varint(out, h.len() as u64);
    LSer { next: h.first_block(), left: h.len(), card: h.len(), sum: 0, rank: 0, key: key.to_vec(), exp }
}

impl LSer {
    /// Des blocs entiers jusqu'à épuiser `budget` (1 par bloc + 1 par élément). Vrai à la
    /// fin (terme du condensé ajouté à `d`).
    pub(crate) fn step(&mut self, out: &mut Vec<u8>, budget: &mut u32, d: &mut digest::Digest) -> bool {
        while *budget > 0 && !is_end(self.next) {
            let b = self.next;
            let mut write = |v: &[u8]| {
                put_bytes(out, v);
                self.sum = self.sum.wrapping_add(digest::list_item(self.rank, v));
                self.rank += 1;
            };
            let (n, next) = match lsave().blocks.take(b) {
                Some(T0::Slab) => {
                    let x = block(b);
                    let mut n = 0;
                    for v in x.values() {
                        write(v);
                        n += 1;
                    }
                    (n, x.next_block())
                }
                Some(T0::Image(img)) => {
                    for v in &img.vals {
                        write(v);
                    }
                    (img.vals.len() as u32, img.next)
                }
                None => panic!("bloc {b} de List absent de la vue T0"),
            };
            self.left = self.left.checked_sub(n).expect("plus d'éléments T0 que le cardinal");
            self.next = next;
            if !is_end(next) {
                super::prefetch(next);
            }
            *budget = budget.saturating_sub(1 + n);
        }
        if !is_end(self.next) {
            return false;
        }
        assert_eq!(self.left, 0, "chaînage T0 incomplet");
        d.add(digest::seq_term(&self.key, crate::save::format::T_LIST, self.exp, (self.sum, self.card as u64)));
        true
    }
}
