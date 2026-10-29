//! Chaînes AMAC du ZSet (ANALYSE_ART_UNIFIE.md §13.6), phase 1 bis de
//! `OxidArt::amac_fields` : préchargement seul, rien n'est gardé (l'exécution refait ses
//! descentes, cache chaud ; une mutation plus tôt dans la fenêtre est vue par la
//! redescente). Une étape ne lit que ce que l'étape précédente a préchargé.
//!
//! ```text
//! en-tête (slab des ZSet) ─┬─ petite forme : lignes du bloc
//!                          └─ grande : index (répertoires → seau) → case de la poignée
//!                             → octets du membre → [ancien score] → B+tree jusqu'à la feuille
//! ```
//! Membre absent : la descente du B+tree part de `(score, membre)` pour un ZADD, rien sinon.

#[allow(unused_imports)]
use crate::prelude::*;
use radixox_lib::handle;
use radixox_lib::prefetch::prefetch_l1;

use super::btree::{self, BTree};
use super::index::{self, Index};
use super::large::{member, member_hash};
use super::{ZSet, encode_score};
use crate::prefetch_batch::{FieldStep, ZProbe};
use crate::value::zset_ref;

/// Un pas de la chaîne de la voie : `idx` = nœud courant (en-tête, index ou B+tree),
/// `fh` = hash du membre, `zidx` = idx de l'en-tête, `zscore` = score de la descente du
/// B+tree.
#[inline]
pub(crate) fn step(
    s: FieldStep,
    field: &[u8],
    zp: ZProbe,
    idx: &mut u32,
    fh: &mut u64,
    zidx: &mut u32,
    zscore: &mut u64,
) -> FieldStep {
    match s {
        FieldStep::ZHead => match zset_ref(*idx) {
            ZSet::Small(z) => {
                let (p, size) = (z.0 as *const u8, z.block_bytes());
                for off in (0..size.min(256)).step_by(64) {
                    prefetch_l1(unsafe { p.add(off) });
                }
                FieldStep::Done
            }
            ZSet::Large(z) => {
                *zidx = *idx;
                *fh = member_hash(field);
                let root = z.index.root();
                if z.index.is_empty() {
                    return absent(field, zp, *zidx, idx, zscore);
                }
                index::prefetch(root);
                *idx = root;
                FieldStep::ZIdx { level: 0 }
            }
        },
        FieldStep::ZIdx { level } => {
            if let Some(next) = Index::probe_step(*idx, *fh, level as usize) {
                index::prefetch(next);
                *idx = next;
                return FieldStep::ZIdx { level: level + 1 };
            }
            match Index::candidate(*idx, Index::fingerprint(*fh), 0) {
                Some((i, h, _)) => {
                    prefetch_l1(handle::slot_ptr(h) as *const u8);
                    FieldStep::ZSlot { i: i as u8 }
                }
                None => absent(field, zp, *zidx, idx, zscore),
            }
        }
        FieldStep::ZSlot { i } => {
            let (_, h, _) = Index::candidate(*idx, Index::fingerprint(*fh), i as usize).expect("candidat");
            prefetch_l1(handle::addr(h) as *const u8);
            FieldStep::ZObj { i }
        }
        FieldStep::ZObj { i } => {
            let f = Index::fingerprint(*fh);
            let (_, h, old) = Index::candidate(*idx, f, i as usize).expect("candidat");
            if member(h) == field {
                return match zp {
                    ZProbe::Index => FieldStep::Done,
                    ZProbe::Tree | ZProbe::Add(_) => tree(field, old, *zidx, idx, zscore),
                };
            }
            match Index::candidate(*idx, f, i as usize + 1) {
                Some((j, h, _)) => {
                    prefetch_l1(handle::slot_ptr(h) as *const u8);
                    FieldStep::ZSlot { i: j as u8 }
                }
                None => absent(field, zp, *zidx, idx, zscore),
            }
        }
        FieldStep::ZTree { left } => {
            *idx = BTree::child_toward(*idx, *zscore, |h| field.cmp(member(h)));
            if left == 1 { FieldStep::Done } else { FieldStep::ZTree { left: left - 1 } }
        }
        _ => FieldStep::Done,
    }
}

/// Membre absent de l'index : un ZADD descend vers sa future place.
#[inline]
fn absent(field: &[u8], zp: ZProbe, zidx: u32, idx: &mut u32, zscore: &mut u64) -> FieldStep {
    match zp {
        ZProbe::Add(f) if !f.is_nan() => tree(field, encode_score(f), zidx, idx, zscore),
        _ => FieldStep::Done,
    }
}

/// Début de la descente du B+tree vers `(score, membre)`.
#[inline]
fn tree(_field: &[u8], score: u64, zidx: u32, idx: &mut u32, zscore: &mut u64) -> FieldStep {
    let ZSet::Large(z) = zset_ref(zidx) else { return FieldStep::Done };
    if z.tree.is_empty() {
        return FieldStep::Done;
    }
    *zscore = score;
    *idx = z.tree.root();
    btree::prefetch(*idx);
    match z.tree.height() {
        0 => FieldStep::Done,
        h => FieldStep::ZTree { left: h },
    }
}
