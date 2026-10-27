//! Save progressive des ZSet (ANALYSE_ART_UNIFIE.md §13.7, SAVE_DESIGN.md) : le ZSet est
//! écrit dans l'enregistrement de sa clé, en flux, sur sa vue T0 ; rien n'est bloquant au
//! `begin`.
//!
//! - **En-têtes** (slab des ZSet) derrière une barrière : point de passage `zset_mut`.
//!   Image : copie profonde du bloc (petite forme), ou l'en-tête du B+tree (grande).
//! - **Feuilles du B+tree** derrière une barrière : points de passage `leaf_mut`,
//!   `free_leaf`. La save suit le chaînage T0 des feuilles (`next` de l'image si la
//!   feuille a changé) : les nœuds internes et l'index ne sont jamais lus (le rechargement
//!   les reconstruit), donc pas de barrière sur eux.
//! - **Listes différées** : une poignée libérée pendant la save (ZREM) reste valide pour
//!   les images ; un ZSet supprimé pendant la save garde son en-tête (et tout le reste)
//!   jusqu'à la fin. Vidées par [`drain_step`] après la save.
//!
//! Le GC peut déplacer membres et blocs pendant une save : les images gardent des poignées,
//! et la save lit l'en-tête vivant d'un ZSet intact.

use radixox_lib::handle;
use radixox_lib::save_barrier::{SaveBarrier, T0};
use radixox_lib::varint::{put_bytes, put_varint};

use super::btree::{self, BTree, Leaf};
use super::large::member;
use super::small::ZSmall;
use super::{ZSet, decode_score};
use crate::digest;

/// Image T0 d'un en-tête de ZSet.
pub(crate) enum ZImage {
    /// Copie profonde du bloc (rendue avec l'image).
    Small(ZSmall),
    /// En-tête du B+tree : première feuille et cardinal T0.
    Large(BTree),
}

impl ZImage {
    pub(crate) fn of(z: &ZSet) -> Self {
        match z {
            ZSet::Small(s) => Self::Small(s.deep_clone()),
            ZSet::Large(l) => Self::Large(l.tree),
        }
    }
}

impl Drop for ZImage {
    fn drop(&mut self) {
        if let Self::Small(s) = self {
            s.release();
        }
    }
}

#[derive(Default)]
struct ZSave {
    headers: SaveBarrier<ZImage>,
    leaves: SaveBarrier<Leaf>,
    /// Poignées libérées pendant la save.
    handles: Vec<u32>,
    /// En-têtes des ZSet supprimés pendant la save (idx de la slab des ZSet).
    zsets: Vec<u32>,
}

static mut ZSAVE: Option<ZSave> = None;

/// Lu à chaque écriture de feuille ou d'en-tête : toujours inliné, init à part.
#[inline(always)]
fn zsave() -> &'static mut ZSave {
    #[allow(static_mut_refs)]
    unsafe {
        match ZSAVE.as_mut() {
            Some(z) => z,
            None => init_zsave(),
        }
    }
}

#[cold]
#[inline(never)]
fn init_zsave() -> &'static mut ZSave {
    #[allow(static_mut_refs)]
    unsafe {
        ZSAVE.insert(ZSave::default())
    }
}

/// Une save est en cours (côté ZSet).
#[inline]
pub(crate) fn is_saving() -> bool {
    zsave().headers.is_active()
}

/// Début de save : vue T0 des en-têtes et des feuilles (deux copies de bitmap).
pub(crate) fn begin() {
    let z = zsave();
    z.headers.begin_with(|buf| {
        crate::value::zset_slab_occupancy(buf);
    });
    z.leaves.begin_with(|buf| {
        btree::slab_occupancy(buf);
    });
}

/// Fin de save (normale ou abandon) : images rendues ; les listes différées restent, pour
/// [`drain_step`].
pub(crate) fn end() {
    let z = zsave();
    z.headers.finish();
    z.leaves.finish();
}

/// Une tranche de ce que la save a différé : poignées, puis ZSet supprimés (rendus à la
/// libération par tranches, `super::lazy`). `budget` : 1 par poignée, 1 par ZSet. Rend le
/// budget restant ; vide quand [`deferred_empty`].
pub(crate) fn drain_step(mut budget: u32) -> u32 {
    debug_assert!(!is_saving());
    let z = zsave();
    while budget > 0
        && let Some(h) = z.handles.pop()
    {
        unsafe { handle::free(h) };
        budget -= 1;
    }
    while budget > 0
        && let Some(idx) = z.zsets.pop()
    {
        crate::value::free_zset(idx);
        budget -= 1;
    }
    if z.handles.is_empty() && z.zsets.is_empty() {
        z.handles = Vec::new();
        z.zsets = Vec::new();
    }
    budget
}

pub(crate) fn deferred_empty() -> bool {
    let z = zsave();
    z.handles.is_empty() && z.zsets.is_empty()
}

/// Avant d'écrire ou de libérer la feuille `i` : capture T0.
#[inline]
pub(crate) fn before_leaf_write(i: u32, capture: impl FnOnce() -> Leaf) {
    let z = zsave();
    if z.leaves.is_active() {
        z.leaves.before_write(i, capture);
    }
}

/// Avant de modifier l'en-tête `idx` (ou son bloc) : capture T0.
#[inline]
pub(crate) fn before_header_write(idx: u32, capture: impl FnOnce() -> ZImage) {
    let z = zsave();
    if z.headers.is_active() {
        z.headers.before_write(idx, capture);
    }
}

/// Libère une poignée, ou la diffère pendant une save.
#[inline]
pub(crate) fn free_handle(h: u32) {
    if is_saving() {
        zsave().handles.push(h);
    } else {
        unsafe { handle::free(h) };
    }
}

/// Pendant une save, un ZSet supprimé garde tout jusqu'à la fin : vrai si différé.
#[inline]
pub(crate) fn defer_zset(idx: u32) -> bool {
    if !is_saving() {
        return false;
    }
    zsave().zsets.push(idx);
    true
}

/// La feuille T0 `i` passée à `f` (l'image si elle a changé), puis considérée comme
/// sérialisée.
pub(crate) fn take_leaf<R>(i: u32, f: impl FnOnce(Option<&Leaf>) -> R) -> R {
    match zsave().leaves.take(i) {
        Some(T0::Slab) => f(None),
        Some(T0::Image(img)) => f(Some(img)),
        None => panic!("feuille {i} de ZSet absente de la vue T0"),
    }
}

/// Sérialiseur en cours d'un grand ZSet : feuille T0 suivante, entrées restant à écrire,
/// condensé (somme des éléments, comme `digest::zset_inner`).
pub(crate) struct ZSer {
    next: u32,
    left: u32,
    card: u32,
    sum: u64,
    /// Clé et échéance du terme du condensé.
    key: Vec<u8>,
    exp: Option<u64>,
}

/// Terme d'un élément (`digest::zset_inner`).
#[inline]
fn item_term(score: f64, m: &[u8]) -> u64 {
    let mut h = crc32fast::Hasher::new();
    digest::part(&mut h, m);
    digest::part(&mut h, &score.to_le_bytes());
    h.finalize() as u64
}

/// Écrit l'en-tête de valeur du ZSet `idx` (vue T0) : cardinal, puis toutes les entrées
/// d'une petite forme. Grande forme : rend le sérialiseur qui écrira les feuilles. Sans
/// sérialiseur, le terme du condensé est ajouté à `d`.
pub(crate) fn ser_begin(idx: u32, key: &[u8], exp: Option<u64>, out: &mut Vec<u8>, d: &mut digest::Digest) -> Option<ZSer> {
    let small = |z: &ZSmall, out: &mut Vec<u8>, d: &mut digest::Digest| {
        put_varint(out, z.len() as u64);
        let mut sum = 0u64;
        for (s, m) in z.iter() {
            let f = decode_score(s);
            out.extend_from_slice(&f.to_le_bytes());
            put_bytes(out, m);
            sum = sum.wrapping_add(item_term(f, m));
        }
        d.add(digest::seq_term(key, crate::save::format::T_ZSET, exp, (sum, z.len() as u64)));
    };
    let tree = match zsave().headers.take(idx) {
        Some(T0::Slab) => match crate::value::zset_ref(idx) {
            ZSet::Small(z) => {
                small(z, out, d);
                return None;
            }
            ZSet::Large(l) => l.tree,
        },
        Some(T0::Image(ZImage::Small(z))) => {
            small(z, out, d);
            return None;
        }
        Some(T0::Image(ZImage::Large(t))) => *t,
        None => panic!("ZSet {idx} absent de la vue T0"),
    };
    put_varint(out, tree.len() as u64);
    Some(ZSer {
        next: tree.first().map_or(u32::MAX, |c| c.leaf),
        left: tree.len(),
        card: tree.len(),
        sum: 0,
        key: key.to_vec(),
        exp,
    })
}

impl ZSer {
    /// Des feuilles entières jusqu'à épuiser `budget` (1 par feuille + 1 par entrée).
    /// Vrai à la fin (terme du condensé ajouté à `d`).
    pub(crate) fn step(&mut self, out: &mut Vec<u8>, budget: &mut u32, d: &mut digest::Digest) -> bool {
        while *budget > 0 && self.next != u32::MAX {
            let l = self.next;
            let (n, next) = btree::leaf_t0(l, |lf| {
                for (s, h) in lf.entries() {
                    let (f, m) = (decode_score(s), member(h));
                    out.extend_from_slice(&f.to_le_bytes());
                    put_bytes(out, m);
                    self.sum = self.sum.wrapping_add(item_term(f, m));
                }
                (lf.len() as u32, lf.next_leaf())
            });
            self.left = self.left.checked_sub(n).expect("plus d'entrées T0 que le cardinal");
            self.next = next;
            if next != u32::MAX {
                btree::prefetch(next);
            }
            *budget = budget.saturating_sub(1 + n);
        }
        if self.next != u32::MAX {
            return false;
        }
        assert_eq!(self.left, 0, "chaînage T0 incomplet");
        d.add(digest::seq_term(&self.key, crate::save::format::T_ZSET, self.exp, (self.sum, self.card as u64)));
        true
    }
}
