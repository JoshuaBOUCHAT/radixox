//! Énumérateurs du GC (MEMORY_ARCH.md §16.6) : chaque `SharedByte` que tient l'arbre,
//! par un parcours linéaire des slabs qui ne connaît que le layout (pas de pointeur
//! arrière, pas de logique métier). Trois slabs : les nœuds d'oxidart (self-val, feuilles,
//! petits conteneurs), les nœuds de valart (champs et membres), les listes. Les ZSet ne
//! sont pas visités : leurs membres sont externes (§16.9).
//!
//! Le visiteur reçoit `&mut SharedByte` et ne doit le réécrire que vers les mêmes octets
//! (évacuation) : c'est pourquoi l'accès contourne la barrière de save (§8.5).

use radixox_lib::shared_byte::SharedByte;

use crate::OxidArt;
use crate::small;
use crate::value::{Tag, ValUnion, list_slab, valart};

/// Une passe du GC niveau A en cours (MEMORY_ARCH.md §16.6) : phase et slot où reprendre.
/// Le pool a marqué les blocs à évacuer ; chaque `SharedByte` visité est relocalisé s'il
/// en fait partie.
#[derive(Debug)]
pub struct GcPass {
    phase: Phase,
    at: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Oxidart,
    Valart,
    Lists,
}

impl Default for GcPass {
    fn default() -> Self {
        Self::new()
    }
}

impl GcPass {
    pub fn new() -> Self {
        Self { phase: Phase::Oxidart, at: 0 }
    }

    /// Une tranche : s'arrête au premier plafond atteint, `max_slots` slots visités ou
    /// `max_bytes` octets recopiés (travail borné, donc coût par tour prévisible). Vrai
    /// quand la passe est finie.
    pub fn step(&mut self, art: &mut OxidArt, max_slots: u32, max_bytes: usize) -> bool {
        /// Slots par appel aux énumérateurs : le plafond d'octets est vérifié entre deux.
        const CHUNK: u32 = 256;
        let mut moved = 0;
        let mut slots = 0;
        while slots < max_slots && moved < max_bytes {
            let n = CHUNK.min(max_slots - slots);
            slots += n;
            let f = &mut |sb: &mut SharedByte| moved += sb.gc_relocate();
            let next = match self.phase {
                Phase::Oxidart => art.gc_visit(self.at, n, f),
                // Testé à chaque tranche, pas seulement à l'entrée : une save peut démarrer
                // entre deux tranches, et l'image d'un nœud valart copie ses `SharedByte`
                // bit à bit. Valart sauté jusqu'à la passe suivante.
                Phase::Valart if valart().is_saving() => None,
                Phase::Valart => art.gc_visit_valart(self.at, n, f),
                Phase::Lists => art.gc_visit_lists(self.at, n, f),
            };
            if let Some(at) = next {
                self.at = at;
                continue;
            }
            self.at = 0;
            self.phase = match self.phase {
                Phase::Oxidart => Phase::Valart,
                Phase::Valart => Phase::Lists,
                Phase::Lists => return true,
            };
        }
        false
    }
}

/// Arbres vivants du processus. Valart, les listes et les ZSet sont des slabs globaux,
/// communs à tous les arbres : avec plusieurs arbres (tests à plusieurs nœuds), un audit
/// ne sait pas à qui appartient ce qu'il y voit.
static mut LIVE_TREES: u32 = 0;

/// Compte un arbre vivant, de sa création à sa destruction.
pub(crate) struct LiveTree(());

impl LiveTree {
    pub(crate) fn new() -> Self {
        unsafe { LIVE_TREES += 1 };
        Self(())
    }
}

impl Drop for LiveTree {
    fn drop(&mut self) {
        unsafe { LIVE_TREES -= 1 };
    }
}

/// Les `SharedByte` d'une valeur.
fn visit_val(tag: Tag, val: &mut ValUnion, f: &mut impl FnMut(&mut SharedByte)) {
    unsafe {
        match tag {
            Tag::Bytes => f(&mut *val.bytes),
            Tag::HashSmall => small::entries_mut::<2>(val.ptr).iter_mut().flatten().for_each(f),
            Tag::SetSmall => small::entries_mut::<1>(val.ptr).iter_mut().flatten().for_each(f),
            // Int, conteneurs par idx (visités dans leur slab)
            _ => {}
        }
    }
}

impl OxidArt {
    /// Slots occupés des nœuds à partir de `from`, au plus `budget` ; rend le slot où
    /// reprendre, `None` à la fin.
    pub fn gc_visit(&mut self, from: u32, budget: u32, f: &mut impl FnMut(&mut SharedByte)) -> Option<u32> {
        let mut at = from;
        for _ in 0..budget {
            let i = self.map.next_occupied(at)?;
            // SAFETY: slot occupé. Sans `node_mut`, donc sans barrière de save : réécrire un
            // pointeur vers les mêmes octets n'est pas une mutation (MEMORY_ARCH.md §8.5).
            // Seule exception au point de passage unique des mutations.
            let node = unsafe { self.map.get_unchecked_mut(i) };
            node.for_each_val_mut(|tag, val| visit_val(tag, val, f));
            at = i + 1;
        }
        Some(at)
    }

    /// Même contrat que [`Self::gc_visit`], sur les nœuds de valart. Jamais pendant une
    /// save (voir `ValTrees::gc_visit`).
    pub fn gc_visit_valart(&mut self, from: u32, budget: u32, f: &mut impl FnMut(&mut SharedByte)) -> Option<u32> {
        valart().gc_visit(from, budget, f)
    }

    /// Même contrat que [`Self::gc_visit`], sur les listes.
    pub fn gc_visit_lists(&mut self, from: u32, budget: u32, f: &mut impl FnMut(&mut SharedByte)) -> Option<u32> {
        let lists = list_slab();
        let mut at = from;
        for _ in 0..budget {
            let i = lists.next_occupied(at)?;
            unsafe { lists.get_unchecked_mut(i) }.iter_mut().for_each(&mut *f);
            at = i + 1;
        }
        Some(at)
    }

    /// Slots couverts par les trois slabs : `N` du budget sous charge (§16.6).
    pub fn gc_slot_span(&self) -> u64 {
        self.map.slot_span() as u64 + valart().slot_span() as u64 + list_slab().slot_span() as u64
    }

    /// Tous les `SharedByte` de l'arbre, d'un coup (tests, vérificateur). Valart sauté
    /// pendant une save de valart, comme le GC.
    pub fn gc_visit_all(&mut self, f: &mut impl FnMut(&mut SharedByte)) {
        let mut at = Some(0);
        while let Some(from) = at {
            at = self.gc_visit(from, u32::MAX, f);
        }
        if !valart().is_saving() {
            let mut at = Some(0);
            while let Some(from) = at {
                at = self.gc_visit_valart(from, u32::MAX, f);
            }
        }
        let mut at = Some(0);
        while let Some(from) = at {
            at = self.gc_visit_lists(from, u32::MAX, f);
        }
    }

    /// Audit d'unicité (MEMORY_ARCH.md §15, §16.9) : hors save, chaque `SharedByte` de
    /// l'arbre a `rc == 1`. Rend ceux qui ne l'ont pas (slab, octets, rc), pour le message.
    pub fn gc_audit_unique(&mut self) -> Vec<(&'static str, Vec<u8>, u16)> {
        let mut shared = Vec::new();
        // Les images d'une save tiennent des clones, encore après `end_save` tant que
        // `drain_deferred` n'a pas vidé leurs feuilles : partage légitime, temporaire.
        if self.is_saving() || !self.save.leftover.is_empty() {
            return shared;
        }
        // Slabs globaux : audités seulement si cet arbre est seul (toujours le cas en
        // production), et valart hors de sa propre save.
        let alone = unsafe { LIVE_TREES } == 1;
        let valart_saving = !alone || valart().is_saving();
        let check = |slab: &'static str| {
            move |sb: &mut SharedByte| {
                if sb.rc() != 1 {
                    Some((slab, sb.to_vec(), sb.rc()))
                } else {
                    None
                }
            }
        };
        let (a, b, c) = (check("oxidart"), check("valart"), check("listes"));
        let mut at = Some(0);
        while let Some(from) = at {
            at = self.gc_visit(from, u32::MAX, &mut |sb| shared.extend(a(sb)));
        }
        // valart est global : sa save peut être celle d'un autre arbre du processus (tests).
        let mut at = (!valart_saving).then_some(0);
        while let Some(from) = at {
            at = self.gc_visit_valart(from, u32::MAX, &mut |sb| shared.extend(b(sb)));
        }
        let mut at = alone.then_some(0);
        while let Some(from) = at {
            at = self.gc_visit_lists(from, u32::MAX, &mut |sb| shared.extend(c(sb)));
        }
        shared
    }

    /// Emplacements dans le pool de tous les `SharedByte` de l'arbre, pour
    /// `radixox_lib::pool::verify_census`.
    pub fn gc_census(&mut self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        self.gc_visit_all(&mut |sb: &mut SharedByte| out.extend(sb.pool_slot()));
        out
    }

    /// Passe complète forcée (tests, `radixox_gc_debug`) : tous les blocs fermés non vides
    /// marqués puis évacués ; ce qui reste marqué (objets épinglés) est démarqué. Sans
    /// pool : rien.
    pub fn gc_full_pass(&mut self) {
        use radixox_lib::pool;
        if pool::gc_begin(pool::Mark::All, std::time::Instant::now()) == 0 {
            return;
        }
        let mut pass = GcPass::new();
        while !pass.step(self, u32::MAX, usize::MAX) {}
        pool::gc_unmark_all();
    }
}
