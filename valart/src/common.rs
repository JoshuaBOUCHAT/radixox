//! Pièces communes au Set et au Hash : hash des membres, dispatch, slots,
//! buckets de collision.

#[allow(unused_imports)]
use crate::prelude::*;
use core::hash::{BuildHasher, Hasher};

use crate::ValTrees;

/// Nombre de niveaux de dispatch (octets `h[0..7]`).
pub(crate) const DISPATCH_LEVELS: usize = 7;

/// Hash 64 bits d'un membre (champ de Hash ou membre de Set) : octets
/// `h[0..7]` = dispatch des niveaux 0..=6, octet `h[7]` = octet de contrôle.
#[derive(Clone, Copy)]
pub(crate) struct MemberHash(pub(crate) u64);

impl MemberHash {
    #[inline(always)]
    pub(crate) fn radix(self, depth: usize) -> u8 {
        debug_assert!(depth < DISPATCH_LEVELS);
        (self.0 >> (8 * depth)) as u8
    }

    #[inline(always)]
    pub(crate) fn ctrl(self) -> u8 {
        (self.0 >> 56) as u8
    }
}

/// Position d'une entrée dans un nœud : inline ou dans son overflow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slot {
    Inline(usize),
    Overflow(usize),
}

/// Buckets de collision : atteints quand les 7 octets de dispatch sont
/// épuisés. Hors de l'union `ValNode` (cas quasi inexistant, aucune
/// optimisation de layout).
pub(crate) struct CollisionBuckets<T> {
    buckets: Vec<Vec<T>>,
    pub(crate) free: Vec<u32>,
    /// Copie profonde au T0 d'une save (SAVE_DESIGN.md §2.E) : les buckets
    /// sont modifiés et droppés sans barrière.
    t0: Vec<Vec<T>>,
}

impl<T> Default for CollisionBuckets<T> {
    fn default() -> Self {
        Self {
            buckets: Vec::new(),
            free: Vec::new(),
            t0: Vec::new(),
        }
    }
}

impl<T: Clone> CollisionBuckets<T> {
    /// Copie tous les buckets (clones, rc+1 pour les `SharedByte`).
    pub(crate) fn snapshot_t0(&mut self) {
        self.t0.clone_from(&self.buckets);
    }
}

impl<T> CollisionBuckets<T> {
    /// Toutes les entrées des buckets vivants (pas la vue T0), en place (GC).
    pub(crate) fn for_each_mut(&mut self, mut f: impl FnMut(&mut T)) {
        self.buckets.iter_mut().flatten().for_each(&mut f);
    }

    pub(crate) fn alloc(&mut self, first: T) -> u32 {
        match self.free.pop() {
            Some(idx) => {
                self.buckets[idx as usize].push(first);
                idx
            }
            None => {
                self.buckets.push(vec![first]);
                (self.buckets.len() - 1) as u32
            }
        }
    }

    pub(crate) fn get(&self, idx: u32) -> &Vec<T> {
        &self.buckets[idx as usize]
    }

    pub(crate) fn get_mut(&mut self, idx: u32) -> &mut Vec<T> {
        &mut self.buckets[idx as usize]
    }

    /// Bucket tel qu'au T0 de la save en cours.
    pub(crate) fn get_t0(&self, idx: u32) -> &Vec<T> {
        &self.t0[idx as usize]
    }

    pub(crate) fn clear_t0(&mut self) {
        self.t0.clear();
    }

    pub(crate) fn release(&mut self, idx: u32) {
        self.buckets[idx as usize] = Vec::new();
        self.free.push(idx);
    }
}

impl ValTrees {
    #[inline(always)]
    pub(crate) fn member_hash(&self, member: &[u8]) -> MemberHash {
        let mut hasher = self.hasher.build_hasher();
        hasher.write(member);
        let h = hasher.finish();
        #[cfg(test)]
        let h = h & self.hash_mask;
        MemberHash(h)
    }
}

/// Mélangeur 64 bits (splitmix64) pour les descentes aléatoires.
#[inline(always)]
pub(crate) fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}
