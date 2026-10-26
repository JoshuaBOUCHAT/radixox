//! Grande forme du ZSet (ANALYSE_ART_UNIFIE.md §13.4) : index (membre → score) + B+tree
//! compté (ordre, rangs), membres = objets à poignée (une seule copie des octets).

use std::cmp::Ordering;

use radixox_lib::handle;

use super::btree::{BTree, Cursor};
use super::index::Index;
use crate::value::valart;

/// Hash d'un membre : celui de valart (graine du processus).
#[inline]
pub fn member_hash(m: &[u8]) -> u64 {
    valart().probe_hash(m)
}

/// Octets du membre `h`.
#[inline]
pub fn member(h: u32) -> &'static [u8] {
    // SAFETY: poignée tenue par le ZSet ; lue avant toute libération ou pas du GC.
    unsafe { handle::bytes(h) }
}

#[inline]
fn tie(a: u32, b: u32) -> Ordering {
    member(a).cmp(member(b))
}

#[inline]
fn rehash(h: u32) -> u64 {
    member_hash(member(h))
}

/// Résultat d'un ajout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    New,
    /// Ancien score (encodé).
    Updated(u64),
    Unchanged,
}

/// En-tête de la grande forme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZLarge {
    pub(crate) tree: BTree,
    pub(crate) index: Index,
}

impl ZLarge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Depuis des `(score, membre)` triés (passage petite → grande, rechargement). `None` :
    /// table des poignées pleine (rien n'est gardé).
    pub fn from_sorted<'a>(entries: impl ExactSizeIterator<Item = (u64, &'a [u8])>) -> Option<Self> {
        let mut hs = Vec::with_capacity(entries.len());
        for (s, m) in entries {
            match handle::new_obj(m) {
                Some(h) => hs.push((s, h)),
                None => {
                    for (_, h) in hs {
                        unsafe { handle::free(h) };
                    }
                    return None;
                }
            }
        }
        let mut z = Self { tree: BTree::from_sorted(hs.iter().copied()), index: Index::new() };
        for &(s, h) in &hs {
            z.index.insert(rehash(h), h, s, rehash);
        }
        Some(z)
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.tree.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// `(poignée, score)` de `m`.
    #[inline]
    pub fn get(&self, m: &[u8]) -> Option<(u32, u64)> {
        self.index.find(member_hash(m), |h| member(h) == m).map(Index::entry)
    }

    /// Ajoute `m` ou change son score. `None` : table des poignées pleine.
    pub fn add(&mut self, s: u64, m: &[u8]) -> Option<Added> {
        let hm = member_hash(m);
        if let Some(loc) = self.index.find(hm, |h| member(h) == m) {
            let (h, old) = Index::entry(loc);
            if old == s {
                return Some(Added::Unchanged);
            }
            self.tree.update(old, h, s, &tie);
            Index::set_score(loc, s);
            return Some(Added::Updated(old));
        }
        let h = handle::new_obj(m)?;
        self.tree.insert(s, h, &tie);
        self.index.insert(hm, h, s, rehash);
        Some(Added::New)
    }

    /// Retire `m` ; rend son score.
    pub fn remove(&mut self, m: &[u8]) -> Option<u64> {
        let (h, s) = self.index.remove(member_hash(m), |h| member(h) == m)?;
        let ok = self.tree.remove(s, h, &tie);
        debug_assert!(ok);
        unsafe { handle::free(h) };
        Some(s)
    }

    /// Rang de `m` (croissant).
    pub fn rank(&self, m: &[u8]) -> Option<u32> {
        let (h, s) = self.get(m)?;
        Some(self.tree.rank(s, h, &tie))
    }

    /// Position du rang `r`.
    #[inline]
    pub fn select(&self, r: u32) -> Option<Cursor> {
        self.tree.select(r)
    }

    /// `(score, membre)` à partir du rang `r`, croissants.
    pub fn iter_from(&self, r: u32) -> impl Iterator<Item = (u64, &'static [u8])> {
        BTree::iter_from(self.tree.select(r)).map(|(s, h)| (s, member(h)))
    }

    /// `(score, membre)` à partir du rang `r`, décroissants.
    pub fn iter_back_from(&self, r: u32) -> impl Iterator<Item = (u64, &'static [u8])> {
        BTree::iter_back_from(self.tree.select(r)).map(|(s, h)| (s, member(h)))
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, &'static [u8])> {
        self.iter_from(0)
    }

    /// Premier rang dont l'entrée n'est pas avant la sonde (`probe(score, membre)`).
    pub fn lower_bound(&self, mut probe: impl FnMut(u64, &[u8]) -> Ordering) -> u32 {
        self.tree.lower_bound(|s, h| probe(s, member(h))).0
    }

    /// Rend tout : membres, nœuds de l'arbre et de l'index ; reste vide.
    pub fn release(&mut self) {
        for (_, h) in self.tree.iter() {
            unsafe { handle::free(h) };
        }
        self.tree.free_all();
        self.index.free_all();
    }

    /// Vérificateur : arbre et index cohérents entre eux (mêmes couples), chacun valide.
    pub fn verify(&self) -> Result<(), String> {
        self.tree.verify(&tie)?;
        self.index.verify(rehash)?;
        if self.tree.len() != self.index.len() {
            return Err(format!("arbre {} entrées, index {}", self.tree.len(), self.index.len()));
        }
        for (s, h) in self.tree.iter() {
            match self.index.find(rehash(h), |x| x == h).map(Index::entry) {
                Some((_, is)) if is == s => {}
                other => return Err(format!("poignée {h} : arbre {s}, index {other:?}")),
            }
        }
        Ok(())
    }
}
