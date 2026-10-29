//! Énumérateurs du GC (MEMORY_ARCH.md §16.6, §17) : chaque `SharedByte`, chemin sur le tas
//! et page de nœud que tient l'arbre,
//! par un parcours linéaire des slabs qui ne connaît que le layout (pas de pointeur
//! arrière, pas de logique métier). Quatre slabs : les nœuds d'oxidart (self-val, feuilles,
//! petits conteneurs), les nœuds de valart (champs et membres), les listes, les ZSet (bloc
//! de la petite forme). Les membres des grands ZSet sont des objets à poignée : évacués en
//! parcourant leurs blocs (phase `Handled`), sans visiter les ZSet (MEMORY_ARCH.md §19).
//!
//! Le visiteur reçoit `&mut SharedByte` et ne doit le réécrire que vers les mêmes octets
//! (évacuation) : c'est pourquoi l'accès contourne la barrière de save (§8.5).

use radixox_lib::shared_byte::SharedByte;

use crate::OxidArt;
use crate::compact_path::CompactPath;
use crate::small;
use crate::value::{Tag, ValUnion, valart, zset_slab_mut};

/// Ce que les énumérateurs donnent à voir : les `SharedByte` de l'arbre et ses chemins sur le
/// tas (`CompactPath`, MEMORY_ARCH.md §17). Une closure sur `&mut SharedByte` est un visiteur
/// qui ignore les chemins.
pub trait GcVisitor {
    fn bytes(&mut self, sb: &mut SharedByte);
    fn path(&mut self, _p: &mut CompactPath) {}
    /// Objet de structure aligné sur 8 (page d'un nœud, petit bloc Hash / Set, débordement
    /// valart) : pointeur en place et taille.
    fn page(&mut self, _ptr: &mut *mut u8, _size: usize) {}
}

impl<F: FnMut(&mut SharedByte)> GcVisitor for F {
    fn bytes(&mut self, sb: &mut SharedByte) {
        self(sb)
    }
}

/// Visiteur de la passe : relocalise tout ce qui est dans un bloc marqué.
struct Relocate {
    moved: usize,
}

impl GcVisitor for Relocate {
    fn bytes(&mut self, sb: &mut SharedByte) {
        self.moved += sb.gc_relocate();
    }
    fn path(&mut self, p: &mut CompactPath) {
        self.moved += p.gc_relocate();
    }
    fn page(&mut self, ptr: &mut *mut u8, size: usize) {
        // SAFETY: page d'un nœud, allouée par `struct_alloc`, seul le nœud la tient.
        self.moved += unsafe { radixox_lib::pool::relocate8(ptr, size) };
    }
}

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
    ZSets,
    /// Blocs à poignée marqués (membres des ZSet, MEMORY_ARCH.md §19.3) : parcourus
    /// eux-mêmes, aucune structure visitée ; `at` = rang dans la liste des blocs marqués.
    Handled,
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

    /// Une tranche : s'arrête au premier plafond atteint, `max_slots` slots visités,
    /// `max_bytes` octets recopiés ou `max_time` écoulé. Le temps borne vraiment la tranche :
    /// un slot coûte ~10–20 ns cache chaud mais un défaut de cache ou plus à froid (le petit
    /// bloc d'un Hash en plus) ; 10 000 slots froids ont pris 1,8 ms (YCSB 1M). Vrai quand
    /// la passe est finie.
    pub fn step(&mut self, art: &mut OxidArt, max_slots: u32, max_bytes: usize, max_time: std::time::Duration) -> bool {
        /// Slots par appel aux énumérateurs : octets et temps vérifiés entre deux (64 slots
        /// froids ≈ 10 µs).
        const CHUNK: u32 = 64;
        let t0 = std::time::Instant::now();
        let mut v = Relocate { moved: 0 };
        let mut slots = 0;
        while slots < max_slots && v.moved < max_bytes && (slots == 0 || t0.elapsed() < max_time) {
            let n = CHUNK.min(max_slots - slots);
            slots += n;
            let next = match self.phase {
                Phase::Oxidart => art.gc_visit(self.at, n, &mut v),
                // Testé à chaque tranche, pas seulement à l'entrée : une save peut démarrer
                // entre deux tranches, et l'image d'un nœud valart copie ses `SharedByte`
                // bit à bit. Valart sauté jusqu'à la passe suivante.
                Phase::Valart if valart().is_saving() => None,
                Phase::Valart => art.gc_visit_valart(self.at, n, &mut v),
                Phase::Lists => art.gc_visit_lists(self.at, n, &mut |sb| v.bytes(sb)),
                Phase::ZSets => art.gc_visit_zsets(self.at, n, &mut v),
                Phase::Handled => radixox_lib::handle::gc_step(self.at, max_bytes, &mut v.moved),
            };
            if let Some(at) = next {
                self.at = at;
                continue;
            }
            self.at = 0;
            self.phase = match self.phase {
                Phase::Oxidart => Phase::Valart,
                Phase::Valart => Phase::Lists,
                Phase::Lists => Phase::ZSets,
                Phase::ZSets => Phase::Handled,
                Phase::Handled => return true,
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
/// Les objets d'une valeur : son `SharedByte`, ou le petit bloc d'un Hash / Set (objet de
/// structure, déplacé d'abord) puis ses entrées, lues à sa nouvelle place.
fn visit_val(tag: Tag, val: &mut ValUnion, v: &mut impl GcVisitor) {
    unsafe {
        match tag {
            Tag::Bytes => v.bytes(&mut *val.bytes),
            Tag::HashSmall => {
                let size = small::block_bytes::<2>(val.ptr);
                v.page(&mut val.ptr, size);
                small::entries_mut::<2>(val.ptr).iter_mut().flatten().for_each(|sb| v.bytes(sb));
            }
            Tag::SetSmall => {
                let size = small::block_bytes::<1>(val.ptr);
                v.page(&mut val.ptr, size);
                small::entries_mut::<1>(val.ptr).iter_mut().flatten().for_each(|sb| v.bytes(sb));
            }
            // Int, conteneurs par idx (visités dans leur slab)
            _ => {}
        }
    }
}

impl OxidArt {
    /// Slots occupés des nœuds à partir de `from`, au plus `budget` ; rend le slot où
    /// reprendre, `None` à la fin.
    pub fn gc_visit(&mut self, from: u32, budget: u32, v: &mut impl GcVisitor) -> Option<u32> {
        let mut at = from;
        for _ in 0..budget {
            let i = self.map.next_occupied(at)?;
            // SAFETY: slot occupé. Sans `node_mut`, donc sans barrière de save : réécrire un
            // pointeur vers les mêmes octets n'est pas une mutation (MEMORY_ARCH.md §8.5).
            // Seule exception au point de passage unique des mutations.
            let node = unsafe { self.map.get_unchecked_mut(i) };
            // la page d'abord : valeurs et suffixes sont ensuite lus à sa nouvelle place
            if let Some((ptr, size)) = node.page_mut() {
                v.page(ptr, size);
            }
            node.for_each_val_mut(|tag, val| visit_val(tag, val, v));
            node.for_each_path_mut(|p| v.path(p));
            at = i + 1;
        }
        Some(at)
    }

    /// Même contrat que [`Self::gc_visit`], sur les nœuds de valart. Jamais pendant une
    /// save (voir `ValTrees::gc_visit`).
    pub fn gc_visit_valart(&mut self, from: u32, budget: u32, v: &mut impl GcVisitor) -> Option<u32> {
        /// Le visiteur d'oxidart vu par valart : les blocs de débordement sont des objets de
        /// structure comme les pages.
        struct Bridge<'a, V>(&'a mut V);
        impl<V: GcVisitor> valart::ValVisitor for Bridge<'_, V> {
            fn bytes(&mut self, sb: &mut SharedByte) {
                self.0.bytes(sb);
            }
            fn block(&mut self, ptr: &mut *mut u8, size: usize) {
                self.0.page(ptr, size);
            }
        }
        valart().gc_visit(from, budget, &mut Bridge(v))
    }

    /// Même contrat que [`Self::gc_visit`], sur les blocs des listes.
    pub fn gc_visit_lists(&mut self, from: u32, budget: u32, f: &mut impl FnMut(&mut SharedByte)) -> Option<u32> {
        crate::list::gc_visit(from, budget, f)
    }

    /// Même contrat que [`Self::gc_visit`], sur les ZSet : le bloc d'une petite forme est un
    /// objet de structure à un seul propriétaire.
    pub fn gc_visit_zsets(&mut self, from: u32, budget: u32, v: &mut impl GcVisitor) -> Option<u32> {
        let zsets = zset_slab_mut();
        let mut at = from;
        for _ in 0..budget {
            let i = zsets.next_occupied(at)?;
            if let Some((ptr, size)) = unsafe { zsets.get_unchecked_mut(i) }.small_block() {
                v.page(ptr, size);
            }
            at = i + 1;
        }
        Some(at)
    }

    /// Slots couverts par les slabs : `N` du budget sous charge (§16.6).
    pub fn gc_slot_span(&self) -> u64 {
        self.map.slot_span() as u64
            + valart().slot_span() as u64
            + crate::list::slot_span() as u64
            + zset_slab_mut().slot_span() as u64
    }

    /// Tous les `SharedByte` de l'arbre, d'un coup (tests, vérificateur). Valart sauté
    /// pendant une save de valart, comme le GC.
    pub fn gc_visit_all(&mut self, v: &mut impl GcVisitor) {
        let mut at = Some(0);
        while let Some(from) = at {
            at = self.gc_visit(from, u32::MAX, v);
        }
        if !valart().is_saving() {
            let mut at = Some(0);
            while let Some(from) = at {
                at = self.gc_visit_valart(from, u32::MAX, v);
            }
        }
        let mut at = Some(0);
        while let Some(from) = at {
            at = self.gc_visit_lists(from, u32::MAX, &mut |sb| v.bytes(sb));
        }
        let mut at = Some(0);
        while let Some(from) = at {
            at = self.gc_visit_zsets(from, u32::MAX, v);
        }
    }

    /// Audit d'unicité (MEMORY_ARCH.md §15, §16.9) : hors save, chaque `SharedByte` de
    /// l'arbre a `rc == 1`. Rend ceux qui ne l'ont pas (slab, octets, rc), pour le message.
    pub fn gc_audit_unique(&mut self) -> Vec<(&'static str, Vec<u8>, u32)> {
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
            at = self.gc_visit(from, u32::MAX, &mut |sb: &mut SharedByte| shared.extend(a(sb)));
        }
        // valart est global : sa save peut être celle d'un autre arbre du processus (tests).
        let mut at = (!valart_saving).then_some(0);
        while let Some(from) = at {
            at = self.gc_visit_valart(from, u32::MAX, &mut |sb: &mut SharedByte| shared.extend(b(sb)));
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
        struct Census(Vec<(usize, usize)>);
        impl GcVisitor for Census {
            fn bytes(&mut self, sb: &mut SharedByte) {
                self.0.extend(sb.pool_slot());
            }
            fn path(&mut self, p: &mut CompactPath) {
                self.0.extend(p.pool_slot());
            }
            fn page(&mut self, ptr: &mut *mut u8, size: usize) {
                self.0.extend(radixox_lib::pool::struct_slot(*ptr, size));
            }
        }
        let mut c = Census(Vec::new());
        self.gc_visit_all(&mut c);
        c.0
    }

    /// Passe complète forcée (tests, `radixox_gc_debug`) : tous les blocs fermés non vides
    /// marqués puis évacués ; ce qui reste marqué (objets épinglés) est démarqué. Sans
    /// pool : rien.
    pub fn gc_full_pass(&mut self) {
        use radixox_lib::pool;
        if pool::gc_begin(pool::Mark::All, radixox_lib::sys::Instant::now()) == 0 {
            return;
        }
        let mut pass = GcPass::new();
        while !pass.step(self, u32::MAX, usize::MAX, std::time::Duration::MAX) {}
        pool::gc_unmark_all();
    }
}
