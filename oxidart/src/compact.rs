//! Compactage de la slab des nœuds (MEMORY_ARCH.md §17.2) : les nœuds des slots les plus
//! hauts descendent dans les trous les plus bas, puis la fin de la slab est rendue au
//! système par pages de 2 Mio. La slab est un tableau (`adresse = idx × 128`) : seule sa
//! fin peut se rendre.
//!
//! Machine à états par tranches : un déplacement corrige le parent (`set_child_idx`) et le
//! `parent_idx` de chaque enfant. Coût d'un déplacement en lignes de cache touchées au
//! hasard : 4 (copie) + 2 (parent) + 1 par enfant ; une tranche est bornée en unités.
//!
//! Interdit pendant une save (son parcours et son occupation T0 sont indexés par `idx`) et
//! tant que quelqu'un tient des `idx` de nœuds entre deux tranches (jobs) : c'est au
//! pilote (le serveur) de ne pas l'avancer dans ces cas.

use crate::OxidArt;
use crate::node::EntryRef;

/// Slots par page de 2 Mio (nœuds de 128 o).
const PAGE_SLOTS: u32 = (2 << 20) / 128;
/// Libre gardé après rendu (2 Mio), pour que les insertions qui suivent ne refassent pas
/// tout de suite des fautes de page.
const KEEP_FREE_SLOTS: u32 = PAGE_SLOTS;
/// Coût fixe d'un déplacement (copie + parent), en lignes de cache.
const MOVE_COST: u32 = 6;

/// Déclenchement : trous sous la fin au-delà de 3 Mio, 4 Mio pour une slab de plus de
/// 256 Mio (critères de l'utilisateur).
fn threshold_bytes(slab_bytes: u64) -> u64 {
    if slab_bytes > 256 << 20 { 4 << 20 } else { 3 << 20 }
}

/// Une passe de compactage en cours.
#[derive(Debug)]
pub struct Compaction {
    /// Les slots à partir de celui-ci sont vidés, puis rendus.
    target: u32,
    /// Slots libres gardés après rendu.
    keep: u32,
    pub moved: u64,
    /// Enfants du nœud déplacé (tampon réutilisé).
    children: Vec<(u8, u32)>,
}

/// Fin d'une passe.
#[derive(Debug, Clone, Copy)]
pub struct CompactReport {
    pub moved: u64,
    pub released_bytes: usize,
}

impl OxidArt {
    /// Octets de trous sous le dernier nœud occupé.
    pub fn slab_holes_bytes(&self) -> u64 {
        let span = self.map.last_occupied().map_or(0, |l| l + 1);
        (span - self.map.len()) as u64 * size_of::<crate::node::Node>() as u64
    }

    /// Une passe si les trous sous la fin dépassent le seuil ; `force` : toujours, sans
    /// garder de libre (tests).
    pub fn compact_want(&self, force: bool) -> Option<Compaction> {
        let node = size_of::<crate::node::Node>() as u64;
        let span = self.map.last_occupied()? + 1;
        let holes = self.slab_holes_bytes();
        if !force && holes <= threshold_bytes(span as u64 * node) {
            return None;
        }
        // forcé : tout ce qui dépasse l'occupation descend (tests, petits arbres compris)
        let keep = if force { 0 } else { KEEP_FREE_SLOTS };
        let target = if force { self.map.len() } else { (self.map.len() + keep).next_multiple_of(PAGE_SLOTS) };
        (target < span).then(|| Compaction { target, keep, moved: 0, children: Vec::new() })
    }

    /// Une tranche d'au plus `budget` unités. `Some` quand la passe est finie : la fin de
    /// la slab est rendue au-dessus de ce qui est occupé, en gardant le libre prévu.
    pub fn compact_step(&mut self, c: &mut Compaction, budget: u32) -> Option<CompactReport> {
        let mut spent = 0;
        while spent < budget {
            let Some(src) = self.map.last_occupied() else { break };
            if src < c.target {
                break;
            }
            // SAFETY: `src` occupé ; aucune référence dans la slab n'est vivante ici.
            let Some(dst) = (unsafe { self.map.move_down(src) }) else { break };
            spent += MOVE_COST + self.relink(src, dst, &mut c.children);
            c.moved += 1;
        }
        if spent < budget {
            return Some(self.finish_compaction(c));
        }
        None
    }

    /// Le nœud `src` est maintenant en `dst` : son parent et ses enfants pointent dessus.
    /// Rend le nombre d'enfants corrigés.
    fn relink(&mut self, src: u32, dst: u32, children: &mut Vec<(u8, u32)>) -> u32 {
        // SAFETY: `dst` vient d'être occupé par le nœud déplacé.
        let node = unsafe { self.map.get_unchecked(dst) };
        let (parent, radix) = (node.parent_idx, node.exp_and_radix.parent_radix());
        children.clear();
        children.extend(node.iter().filter_map(|(r, e)| match e {
            EntryRef::Child(c) => Some((r, c)),
            EntryRef::Leaf { .. } => None,
        }));
        if src == self.root_idx {
            self.root_idx = dst;
        } else {
            // Sans `node_mut` : pas de save en cours (interdit), et réécrire un idx vers le
            // même nœud n'est pas une mutation logique.
            let p = unsafe { self.map.get_unchecked_mut(parent) };
            let done = p.set_child_idx(radix, dst);
            debug_assert!(done, "parent {parent} sans l'enfant {src}");
        }
        for &(r, c) in children.iter() {
            self.set_parent(c, dst, r);
        }
        children.len() as u32
    }

    fn finish_compaction(&mut self, c: &Compaction) -> CompactReport {
        // Les insertions pendant la passe ont comblé des trous du bas : on rend au-dessus
        // du dernier occupé, en gardant `keep` slots libres à partir de l'occupation
        // actuelle (rien si la passe n'a pas pu libérer assez).
        let last = self.map.last_occupied().map_or(0, |l| l + 1);
        let from = last.max(self.map.len() + c.keep);
        // SAFETY: rien n'est occupé à partir de `last` ≤ `from`.
        let released_bytes = unsafe { self.map.release_above(from) };
        CompactReport { moved: c.moved, released_bytes }
    }
}
