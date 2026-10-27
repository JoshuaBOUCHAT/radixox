//! Compactage de la slab des nœuds (MEMORY_ARCH.md §17.2) : les nœuds des slots les plus
//! hauts descendent dans les trous les plus bas, puis la fin de la slab est rendue au
//! système par pages de 2 Mio. La slab est un tableau (`adresse = idx × 128`) : seule sa
//! fin peut se rendre.
//!
//! Machine à états par tranches : un déplacement corrige le parent (`set_child_idx`) et le
//! `parent_idx` de chaque enfant. Coût d'un déplacement en lignes de cache touchées au
//! hasard : 4 (copie) + 2 (parent) + 1 par enfant ; une tranche est bornée en unités.
//!
//! **Pipeline** (MEMORY_ARCH.md §17.2) : [`PIPELINE`] déplacements en vol, chacun une petite
//! machine à états ; à chaque tour, chacun fait le travail dont les données sont arrivées
//! et précharge celles de son étape suivante, si bien que les défauts de cache des K
//! déplacements se recouvrent. Une machine qui finit est remplacée par le nœud suivant ; un
//! nœud sans page ou dont le parent n'a pas de page saute une étape. La validation (copie,
//! parent, enfants) se fait d'un coup et relit les `idx` à ce moment-là : deux machines qui
//! déplacent un nœud et son parent restent justes dans tous les ordres, un préchargement
//! parti sur une ancienne adresse ne coûte qu'un défaut. Le pipeline est vidé en fin de
//! tranche : aucune réservation ne survit à une commande.
//!
//! Interdit pendant une save (son parcours et son occupation T0 sont indexés par `idx`) et
//! tant que quelqu'un tient des `idx` de nœuds entre deux tranches (jobs) : c'est au
//! pilote (le serveur) de ne pas l'avancer dans ces cas.

use radixox_lib::prefetch::prefetch_l1;

use crate::OxidArt;
use crate::node::{EntryRef, Node};

/// Slots par page de 2 Mio (nœuds de 128 o).
const PAGE_SLOTS: u32 = (2 << 20) / 128;
/// Libre gardé après rendu (2 Mio), pour que les insertions qui suivent ne refassent pas
/// tout de suite des fautes de page.
const KEEP_FREE_SLOTS: u32 = PAGE_SLOTS;
/// Coût fixe d'un déplacement (copie + parent), en lignes de cache.
const MOVE_COST: u32 = 6;
/// Déplacements en vol.
pub const PIPELINE: usize = 8;

/// Étape suivante d'un déplacement en vol.
#[derive(Clone, Copy)]
enum Stage {
    /// Source et destination préchargées : lire l'en-tête de la source.
    Header,
    /// Parent et page de la source préchargés : lire les enfants.
    Links,
    /// Tout est préchargé : valider.
    Commit,
}

#[derive(Clone, Copy)]
struct Mover {
    src: u32,
    dst: u32,
    stage: Stage,
}

#[inline(always)]
fn prefetch_node(art: &OxidArt, idx: u32) {
    let p = art.map.slot_ptr(idx) as *const u8;
    prefetch_l1(p);
    prefetch_l1(p.wrapping_add(64));
}

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

    /// Une tranche d'au plus `budget` unités, en pipeline. `Some` quand la passe est finie :
    /// la fin de la slab est rendue au-dessus de ce qui est occupé, en gardant le libre
    /// prévu.
    pub fn compact_step(&mut self, c: &mut Compaction, budget: u32) -> Option<CompactReport> {
        let mut movers: [Option<Mover>; PIPELINE] = [None; PIPELINE];
        let mut spent = 0;
        // prochaine source : sous la plus basse déjà prise, en partant de la fin
        let mut scan = self.map.last_occupied().map_or(0, |l| l + 1);
        let mut exhausted = false;
        loop {
            let mut in_flight = false;
            for slot in movers.iter_mut() {
                if slot.is_none() && !exhausted && spent < budget {
                    match self.claim(c, &mut scan) {
                        Some(m) => *slot = Some(m),
                        None => exhausted = true,
                    }
                } else if let Some(m) = slot {
                    if let Some(cost) = self.advance(m, &mut c.children) {
                        spent += cost;
                        c.moved += 1;
                        *slot = None;
                    }
                }
                in_flight |= slot.is_some();
            }
            if !in_flight && (exhausted || spent >= budget) {
                break;
            }
        }
        exhausted.then(|| self.finish_compaction(c))
    }

    /// Prend la prochaine source (la plus haute non prise) et réserve le plus bas trou ;
    /// précharge les deux. `None` : plus rien à descendre sous la cible.
    fn claim(&mut self, c: &Compaction, scan: &mut u32) -> Option<Mover> {
        let src = self.map.prev_occupied(*scan)?;
        if src < c.target {
            return None;
        }
        // SAFETY: rempli par `move_to` à la validation, dans cette tranche, ou rendu.
        let dst = unsafe { self.map.reserve_lowest() };
        if dst >= src || dst >= c.target {
            unsafe { self.map.unreserve(dst) };
            return None;
        }
        *scan = src;
        prefetch_node(self, src);
        prefetch_node(self, dst);
        Some(Mover { src, dst, stage: Stage::Header })
    }

    /// Une étape d'un déplacement. `Some(coût)` quand il est validé.
    fn advance(&mut self, m: &mut Mover, children: &mut Vec<(u8, u32)>) -> Option<u32> {
        // SAFETY: `m.src` est occupé jusqu'à la validation (seule cette machine le déplace).
        let node = unsafe { self.map.get_unchecked(m.src) };
        match m.stage {
            Stage::Header => {
                if node.parent_idx != u32::MAX {
                    prefetch_node(self, node.parent_idx);
                }
                if let Some(p) = node.page_start() {
                    prefetch_l1(p);
                }
                m.stage = Stage::Links;
                None
            }
            Stage::Links => {
                let mut fetched = false;
                for (_, e) in node.iter() {
                    if let EntryRef::Child(ch) = e {
                        let at = self.map.slot_ptr(ch) as *const u8;
                        prefetch_l1(at.wrapping_add(std::mem::offset_of!(Node, parent_idx)));
                        fetched = true;
                    }
                }
                if node.parent_idx != u32::MAX {
                    // l'entrée de l'enfant peut être dans la page du parent
                    let parent = unsafe { self.map.get_unchecked(node.parent_idx) };
                    if let Some(p) = parent.page_start() {
                        prefetch_l1(p);
                        fetched = true;
                    }
                }
                if fetched {
                    m.stage = Stage::Commit;
                    return None;
                }
                // rien de plus à attendre : un tour de moins
                Some(self.commit(m, children))
            }
            Stage::Commit => Some(self.commit(m, children)),
        }
    }

    fn commit(&mut self, m: &Mover, children: &mut Vec<(u8, u32)>) -> u32 {
        // SAFETY: `src` occupé, `dst` réservé par `claim` ; aucune référence vivante.
        unsafe { self.map.move_to(m.src, m.dst) };
        MOVE_COST + self.relink(m.src, m.dst, children)
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
        // racines valart tenues par ce nœud : leur indice suit
        self.hint_containers(dst);
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
