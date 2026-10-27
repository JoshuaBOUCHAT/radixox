//! Compactage de la slab des nœuds valart (MEMORY_ARCH.md §17.6, comme celle d'oxidart,
//! §17.2) : les nœuds des slots les plus hauts descendent dans les trous les plus bas, puis
//! la fin de la slab est rendue au système. Un nœud non racine connaît son parent et le
//! radix de son slot (pointeur retour) : un déplacement corrige le parent et le pointeur
//! retour de chaque enfant. Une racine est désignée par la valeur de sa clé dans oxidart :
//! elle porte un **indice** (le nœud oxidart qui tient cette valeur) ; l'appelant vérifie
//! que ce nœud la désigne bien et réécrit l'idx, sinon elle reste en place.
//!
//! Interdit pendant une save, pendant la destruction différée d'un conteneur (elle garde
//! des idx d'une tranche à l'autre) et en même temps qu'une passe du GC (il parcourt la
//! slab par idx) : c'est au pilote de ne pas l'avancer dans ces cas.

use crate::{NodeType, ValNode, ValTrees};

/// Slots par page de 2 Mio.
const PAGE_SLOTS: u32 = ((2 << 20) / size_of::<ValNode>()) as u32;
/// Coût fixe d'un déplacement (copie + parent), en lignes de cache.
const MOVE_COST: u32 = 6;

/// Une passe en cours.
#[derive(Debug)]
pub struct ValCompaction {
    target: u32,
    keep: u32,
    scan: u32,
    pub moved: u64,
}

impl ValTrees {
    /// Octets de trous sous le dernier nœud occupé.
    pub fn compact_holes_bytes(&self) -> u64 {
        let span = self.nodes.last_occupied().map_or(0, |l| l + 1);
        (span - self.nodes.len()) as u64 * size_of::<ValNode>() as u64
    }

    /// Une passe si les trous sous la fin dépassent 3 Mio (4 Mio au-delà de 256 Mio) ;
    /// `force` : toujours, sans garder de libre (tests). Rien pendant une save ou une
    /// destruction différée.
    pub fn compact_want(&self, force: bool) -> Option<ValCompaction> {
        if self.save.is_active() || self.draining.is_some() {
            return None;
        }
        let span = self.nodes.last_occupied()? + 1;
        let slab_bytes = span as u64 * size_of::<ValNode>() as u64;
        let threshold = if slab_bytes > 256 << 20 { 4 << 20 } else { 3 << 20 };
        if !force && self.compact_holes_bytes() <= threshold {
            return None;
        }
        let keep = if force { 0 } else { PAGE_SLOTS };
        let len = self.nodes.len();
        let target = if force { len } else { (len + keep).next_multiple_of(PAGE_SLOTS) };
        (target < span).then_some(ValCompaction { target, keep, scan: span, moved: 0 })
    }

    /// Une tranche d'au plus `budget` unités. `Some((déplacés, octets rendus))` quand la
    /// passe est finie (ou arrêtée : save, destruction différée). `owner(indice, src, dst)`
    /// : pour une racine, vrai si la valeur désignée par l'indice tient bien `src` — et
    /// l'appelant y a alors écrit `dst` ; faux : la racine ne bouge pas.
    pub fn compact_step(
        &mut self,
        c: &mut ValCompaction,
        budget: u32,
        mut owner: impl FnMut(u32, u32, u32) -> bool,
    ) -> Option<(u64, usize)> {
        if self.save.is_active() || self.draining.is_some() {
            return Some((c.moved, 0));
        }
        let mut spent = 0;
        while spent < budget {
            let Some(src) = self.nodes.prev_occupied(c.scan).filter(|&s| s >= c.target) else {
                return Some(self.compact_finish(c));
            };
            c.scan = src;
            let (tag, root) = {
                let n = unsafe { self.nodes.get_unchecked(src) };
                (n.tag(), n.is_root())
            };
            // SAFETY: rempli par `move_to` tout de suite, ou rendu.
            let dst = unsafe { self.nodes.reserve_lowest() };
            if dst >= src || dst >= c.target {
                unsafe { self.nodes.unreserve(dst) };
                return Some(self.compact_finish(c));
            }
            if root {
                let hint = match tag {
                    NodeType::Hash => self.hash_root_hint(src),
                    NodeType::Set => self.set_root_hint(src),
                };
                if !owner(hint, src, dst) {
                    unsafe { self.nodes.unreserve(dst) };
                    spent += 1;
                    continue;
                }
            }
            // SAFETY: `src` occupé, `dst` réservé, aucune référence vivante dans la slab.
            unsafe { self.nodes.move_to(src, dst) };
            let fixed = match tag {
                NodeType::Hash => self.hash_relink(dst),
                _ => self.set_relink(dst),
            };
            c.moved += 1;
            spent += MOVE_COST + fixed;
        }
        None
    }

    fn compact_finish(&mut self, c: &ValCompaction) -> (u64, usize) {
        let last = self.nodes.last_occupied().map_or(0, |l| l + 1);
        let from = last.max(self.nodes.len() + c.keep);
        // SAFETY: rien n'est occupé à partir de `last` ≤ `from`.
        (c.moved, unsafe { self.nodes.release_above(from) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radixox_lib::shared_byte::SharedByte;
    use std::collections::{BTreeMap, BTreeSet};

    fn sb(s: String) -> SharedByte {
        SharedByte::from_slice(s.as_bytes())
    }

    /// Beaucoup de Hash et de Set (plusieurs niveaux), un sur deux détruit, des champs
    /// retirés : passe forcée ; contenus et invariants (pointeurs retour compris) gardés,
    /// fin de slab rendue.
    #[test]
    fn compaction_keeps_containers() {
        let mut t = ValTrees::new();
        let mut hashes: Vec<(u32, BTreeMap<String, String>)> = Vec::new();
        let mut sets: Vec<(u32, BTreeSet<String>)> = Vec::new();
        for i in 0..300 {
            let (root, mut model) = (t.hash_create(), BTreeMap::new());
            for j in 0..(i % 7) * 40 {
                let (f, v) = (format!("f{i}-{j}"), format!("v{j}"));
                t.hash_set(root, sb(f.clone()), sb(v.clone()));
                model.insert(f, v);
            }
            hashes.push((root, model));
            let (root, mut model) = (t.set_create(), BTreeSet::new());
            for j in 0..(i % 5) * 50 {
                let m = format!("m{i}-{j}");
                t.set_add(root, sb(m.clone()));
                model.insert(m);
            }
            sets.push((root, model));
        }
        let mut kh = Vec::new();
        for (k, (root, mut model)) in hashes.into_iter().enumerate() {
            if k % 2 == 0 {
                t.hash_drop(root);
                continue;
            }
            // des champs retirés : des nœuds se vident au milieu
            let gone: Vec<String> = model.keys().filter(|f| f.len() % 3 == 0).cloned().collect();
            for f in gone {
                assert!(t.hash_remove(root, f.as_bytes()));
                model.remove(&f);
            }
            kh.push((root, model));
        }
        let mut ks = Vec::new();
        for (k, (root, model)) in sets.into_iter().enumerate() {
            if k % 2 == 1 { t.set_drop(root) } else { ks.push((root, model)) }
        }
        let span_before = t.nodes.last_occupied().unwrap() + 1;
        let before = t.nodes.len();
        // propriétaire simulé (oxidart) : la racine `src` est désignée par l'entrée qui la
        // tient ; l'indice vaut ici le rang de cette entrée
        let mut owners: Vec<u32> = kh.iter().map(|(r, _)| *r).chain(ks.iter().map(|(r, _)| *r)).collect();
        for (k, &r) in owners.iter().enumerate() {
            if k < kh.len() { t.set_hash_root_hint(r, k as u32) } else { t.set_set_root_hint(r, k as u32) }
        }
        let mut c = t.compact_want(true).expect("passe");
        let mut roots_moved = 0;
        while t
            .compact_step(&mut c, 64, |hint, src, dst| {
                let ok = owners.get(hint as usize) == Some(&src);
                if ok {
                    owners[hint as usize] = dst;
                    roots_moved += 1;
                }
                ok
            })
            .is_none()
        {}
        assert!(roots_moved > 0, "des racines ont bougé");
        for (k, (root, _)) in kh.iter_mut().enumerate() {
            *root = owners[k];
        }
        for (k, (root, _)) in ks.iter_mut().enumerate() {
            *root = owners[kh.len() + k];
        }
        assert_eq!(t.nodes.len(), before, "aucun nœud perdu");
        assert!(c.moved > 0);
        let span_after = t.nodes.last_occupied().unwrap() + 1;
        assert!(span_after < span_before, "fin de slab {span_after} (avant {span_before})");
        for (root, model) in &kh {
            assert_eq!(t.hash_check(*root), model.len());
            let mut got = BTreeMap::new();
            t.hash_for_each(*root, |f, v| {
                got.insert(String::from_utf8(f.to_vec()).unwrap(), String::from_utf8(v.to_vec()).unwrap());
            });
            assert_eq!(&got, model);
        }
        for (root, model) in &ks {
            assert_eq!(t.set_check(*root), model.len());
            let mut got = BTreeSet::new();
            t.set_for_each(*root, |m| {
                got.insert(String::from_utf8(m.to_vec()).unwrap());
            });
            assert_eq!(&got, model);
        }
    }
}
