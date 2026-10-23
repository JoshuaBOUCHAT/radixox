//! valart : Hash et Set de RadixOx, sur des tries de hash dans une HiSlab
//! unique (nœuds de 128 B, union [`ValNode`]).
//!
//! Voir `set.rs` pour la structure et ses invariants ; `hash.rs` en est le
//! miroir avec une valeur par entrée.

use std::mem::{ManuallyDrop, offset_of};

use hislab::HiSlab;
use radixox_lib::save_barrier::{SaveBarrier, T0};
use radixox_lib::shared_byte::SharedByte;

mod common;
mod hash;
mod overflow;
mod set;

use common::CollisionBuckets;
use hash::HashNode;
pub use overflow::{HashOverflow, SetOverflow};
use set::SetNode;

// Toutes les variantes doivent porter leur `node_type` au même offset : c'est
// ce qui permet de lire le tag depuis l'union sans savoir quelle variante est
// active (common initial sequence, comme un tagged union en C).
const NODE_TYPE_OFFSET: usize = offset_of!(HashNode, node_type);
const _: () = assert!(NODE_TYPE_OFFSET == offset_of!(SetNode, node_type));

union ValNode {
    hash: ManuallyDrop<HashNode>,
    set: ManuallyDrop<SetNode>,
}

impl ValNode {
    /// Lit le tag sans passer par le champ `NodeType` typé : un `u8` n'a pas
    /// de bit pattern invalide, donc c'est toujours sound même si la
    /// variante "active" de l'union n'est pas celle qu'on relit ici.
    #[inline]
    fn tag_u8(&self) -> u8 {
        unsafe { *(self as *const ValNode as *const u8).add(NODE_TYPE_OFFSET) }
    }
    fn tag(&self) -> NodeType {
        unsafe { *(self as *const ValNode as *const NodeType).add(NODE_TYPE_OFFSET) }
    }
}

impl Drop for ValNode {
    fn drop(&mut self) {
        match self.tag_u8() {
            t if t == NodeType::Hash as u8 => unsafe { ManuallyDrop::drop(&mut self.hash) },
            t if t == NodeType::Set as u8 => unsafe { ManuallyDrop::drop(&mut self.set) },
            t if t == NodeType::ZSet as u8 => unreachable!("ZSet node not implemented yet"),
            _ => unreachable!("invalid NodeType tag"),
        }
    }
}

/// Curseur de sérialisation d'un conteneur (Hash ou Set), reprenable d'une
/// tranche à l'autre : seuls des idx `(nœud ou bucket, profondeur)` y
/// restent entre deux appels. Réutilisable (capacité gardée).
#[derive(Default)]
pub struct ValSer {
    stack: Vec<(u32, u8)>,
}

/// Prefetch des 128 B d'un nœud du slab (occupé ou non : la zone est mappée).
#[inline(always)]
fn prefetch_slot(nodes: &HiSlab<ValNode>, idx: u32) {
    let p = unsafe { nodes.get_unchecked(idx) } as *const ValNode as *const u8;
    radixox_lib::prefetch::prefetch_l1(p);
    radixox_lib::prefetch::prefetch_l1(unsafe { p.add(64) });
}

/// Image T0 d'un nœud valart (SAVE_DESIGN.md §2.A) : copie bit à bit, bloc
/// overflow copié, entrées non clonées. Sûr parce que valart ne détruit
/// aucune entrée pendant une save (liste différée, §2.B).
enum ValNodeImage {
    Hash(ManuallyDrop<HashNode>),
    Set(ManuallyDrop<SetNode>),
}

impl ValNodeImage {
    fn capture(node: &ValNode) -> Self {
        match node.tag() {
            NodeType::Hash => Self::Hash(unsafe { &node.hash }.image_copy()),
            NodeType::Set => Self::Set(unsafe { &node.set }.image_copy()),
            NodeType::ZSet => unreachable!("ZSet node not implemented yet"),
        }
    }
}

impl Drop for ValNodeImage {
    fn drop(&mut self) {
        match self {
            Self::Hash(h) => HashNode::free_image(h),
            Self::Set(s) => SetNode::free_image(s),
        }
    }
}

/// Ce que le GC voit dans valart (MEMORY_ARCH.md §16.6, §17.4) : les `SharedByte` (clés,
/// valeurs, membres) et les blocs de débordement (objets de structure alignés sur 8). Une
/// closure sur `&mut SharedByte` est un visiteur qui ignore les blocs.
pub trait ValVisitor {
    fn bytes(&mut self, sb: &mut SharedByte);
    /// Bloc de débordement : pointeur (sans tag) en place et taille ; peut être déplacé.
    fn block(&mut self, _ptr: &mut *mut u8, _size: usize) {}
}

impl<F: FnMut(&mut SharedByte)> ValVisitor for F {
    fn bytes(&mut self, sb: &mut SharedByte) {
        self(sb)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum NodeType {
    Hash,
    Set,
    ZSet,
}

pub struct ValTrees {
    nodes: HiSlab<ValNode>,
    /// Zones de collision (hors de l'union `ValNode`), atteintes quand les 7
    /// octets de dispatch du hash sont épuisés.
    set_collisions: CollisionBuckets<SharedByte>,
    hash_collisions: CollisionBuckets<(SharedByte, SharedByte)>,
    /// Seed du hash des membres. Globale à tous les Hash et Set, et à
    /// persister dans le dump : la position de chaque entrée en dépend.
    seed: u64,
    hasher: foldhash::quality::FixedState,
    /// Save progressive : barrière COW sur `nodes`.
    save: SaveBarrier<ValNodeImage>,
    /// Pendant une save : clés/valeurs sorties d'un nœud, et conteneurs
    /// supprimés. Détruits après la save, par tranches (`drain_deferred`).
    deferred_bytes: Vec<SharedByte>,
    deferred_drops: Vec<(NodeType, u32)>,
    /// Conteneur différé en cours de destruction (pile d'idx valart).
    draining: Option<(NodeType, Vec<(u32, usize)>)>,
    /// Tests : masque appliqué au hash pour forcer des collisions.
    #[cfg(test)]
    hash_mask: u64,
}

impl ValTrees {
    /// Virtual capacity par défaut du slab : 2^26 nœuds × 128 B = 8 GiB
    /// réservés en virtuel, committés à la demande.
    pub const DEFAULT_VIRTUAL_CAPACITY: u32 = 1 << 26;

    pub fn new() -> Self {
        use std::hash::BuildHasher;
        let seed = std::collections::hash_map::RandomState::new().hash_one(0u64);
        Self::with_seed(seed, Self::DEFAULT_VIRTUAL_CAPACITY)
    }

    /// Seed explicite : rechargement d'un dump, tests reproductibles.
    pub fn with_seed(seed: u64, virtual_capacity: u32) -> Self {
        Self {
            nodes: HiSlab::new(0, virtual_capacity).expect("valart: mmap du slab impossible"),
            set_collisions: CollisionBuckets::default(),
            hash_collisions: CollisionBuckets::default(),
            seed,
            hasher: foldhash::quality::FixedState::with_seed(seed),
            save: SaveBarrier::new(),
            deferred_bytes: Vec::new(),
            deferred_drops: Vec::new(),
            draining: None,
            #[cfg(test)]
            hash_mask: u64::MAX,
        }
    }

    // ─── Points de passage (SAVE_DESIGN.md §3) ──────────────────────────────
    //
    // `hash_node_mut` / `set_node_mut` sont les seuls accès `&mut` à un nœud,
    // `free_hash_node` / `free_set_node` les seules libérations : la barrière
    // de save y est branchée (`before_write`). Une clé ou valeur qui *sort*
    // d'un nœud (suppression, remplacement) passe par `dispose`. Les buckets
    // de collision n'ont pas de barrière (copiés au T0).
    //
    // Pendant une save, *tout* ce qui sort d'un nœud est différé, pas
    // seulement ce qui sort d'un nœud capturé : HDEL/SREM font remonter des
    // entrées d'une feuille (capturée) vers un ancêtre (peut-être déjà
    // sérialisé, donc non capturé) ; les détruire depuis l'ancêtre laisserait
    // l'image de la feuille pointer dans le vide.

    #[inline]
    fn before_write(&mut self, idx: u32) {
        let Self { nodes, save, .. } = self;
        save.before_write(idx, || {
            ValNodeImage::capture(unsafe { nodes.get_unchecked(idx) })
        });
    }

    /// GC (MEMORY_ARCH.md §16.6, §17.4) : visite les blocs de débordement et les `SharedByte` des slots occupés à partir
    /// de `from`, au plus `budget` slots ; rend le slot où reprendre, `None` à la fin.
    /// Les zones de collision (rares) sont visitées d'un coup, une fois les slots finis.
    ///
    /// Accès sans `before_write` : réécrire un pointeur vers les mêmes octets n'est pas
    /// une mutation. **Jamais pendant une save** : l'image d'un nœud copie ses
    /// `SharedByte` bit à bit, déplacer l'original laisserait l'image pointer sur un
    /// emplacement libéré.
    pub fn gc_visit(&mut self, from: u32, budget: u32, f: &mut impl ValVisitor) -> Option<u32> {
        debug_assert!(!self.save.is_active(), "GC de valart pendant une save");
        let mut at = from;
        for _ in 0..budget {
            let Some(i) = self.nodes.next_occupied(at) else {
                self.hash_collisions.for_each_mut(|(k, v)| {
                    f.bytes(k);
                    f.bytes(v);
                });
                self.set_collisions.for_each_mut(|m| f.bytes(m));
                return None;
            };
            let node = unsafe { self.nodes.get_unchecked_mut(i) };
            match node.tag() {
                NodeType::Hash => unsafe { &mut node.hash }.for_each_sb_mut(f),
                NodeType::Set => unsafe { &mut node.set }.for_each_sb_mut(f),
                NodeType::ZSet => unreachable!("ZSet node not implemented yet"),
            }
            at = i + 1;
        }
        Some(at)
    }

    /// Slots couverts par le slab des nœuds (budget du GC).
    pub fn slot_span(&self) -> u32 {
        self.nodes.slot_span()
    }

    /// Seul point où une clé/valeur sortie du nœud `node` est détruite.
    #[inline]
    fn dispose(&mut self, node: u32, bytes: SharedByte) {
        let _ = node;
        if self.save.is_active() {
            self.deferred_bytes.push(bytes);
        }
    }

    /// Démarre une save : fige la vue T0 du slab.
    pub fn begin_save(&mut self) {
        // Normalement déjà vide (le serveur attend la fin du vidage avant
        // d'accepter une nouvelle save) : filet de sécurité, d'un coup.
        while !self.drain_deferred(u32::MAX) {}
        let nodes = &self.nodes;
        self.save.begin_with(|buf| {
            nodes.copy_occupancy(buf);
        });
        self.set_collisions.snapshot_t0();
        self.hash_collisions.snapshot_t0();
    }

    /// Termine la save (fin normale ou abandon) et vide d'un coup la liste
    /// différée. Le serveur utilise plutôt `end_save` + `drain_deferred`.
    pub fn finish_save(&mut self) {
        self.end_save();
        while !self.drain_deferred(u32::MAX) {}
    }

    /// Ferme la barrière (images libérées). La liste différée reste à vider
    /// par [`drain_deferred`](Self::drain_deferred) : plus rien ne la
    /// référence (images détruites, conteneurs inaccessibles depuis oxidart)
    /// et elle ne grossit plus (hors save, tout est détruit tout de suite).
    pub fn end_save(&mut self) {
        self.save.finish();
        self.set_collisions.clear_t0();
        self.hash_collisions.clear_t0();
    }

    /// Une tranche du vidage de la liste différée (SAVE_DESIGN.md §2.B) :
    /// coût 1 par valeur, 1 par nœud et 1 par entrée d'un conteneur détruit ;
    /// un nœud est toujours détruit en entier. `true` quand tout est vidé.
    pub fn drain_deferred(&mut self, mut budget: u32) -> bool {
        debug_assert!(!self.save.is_active(), "vidage pendant une save");
        let n = (budget as usize).min(self.deferred_bytes.len());
        self.deferred_bytes.truncate(self.deferred_bytes.len() - n);
        budget -= n as u32;
        while budget > 0 {
            let (kind, mut stack) = match self.draining.take() {
                Some(d) => d,
                None => match self.deferred_drops.pop() {
                    Some((kind, root)) => (kind, vec![(root, 0)]),
                    None => break,
                },
            };
            match kind {
                NodeType::Hash => self.hash_drop_step(&mut stack, &mut budget),
                NodeType::Set => self.set_drop_step(&mut stack, &mut budget),
                NodeType::ZSet => unreachable!(),
            }
            if !stack.is_empty() {
                self.draining = Some((kind, stack));
            }
        }
        if self.deferred_bytes.capacity() > 0 && self.deferred_bytes.is_empty() && self.deferred_drops.is_empty() {
            // Une grosse liste ne garde pas sa mémoire jusqu'à la save suivante.
            self.deferred_bytes = Vec::new();
            self.deferred_drops = Vec::new();
        }
        self.deferred_bytes.is_empty() && self.deferred_drops.is_empty() && self.draining.is_none()
    }

    /// Taille de la liste différée : (valeurs, conteneurs dont un en cours).
    pub fn deferred_len(&self) -> (usize, usize) {
        (self.deferred_bytes.len(), self.deferred_drops.len() + self.draining.is_some() as usize)
    }

    /// Prefetch de la racine d'un conteneur (128 B), avant de le sérialiser.
    #[inline]
    pub fn prefetch_root(&self, root: u32) {
        prefetch_slot(&self.nodes, root);
    }

    /// Prefetch des entrées de la racine d'un conteneur, lue dans sa version
    /// T0 (à appeler quand la racine est en cache : `prefetch_root` une
    /// visite plus tôt). Au plus 8 entrées.
    pub fn prefetch_root_entries(&self, root: u32) {
        match self.save.peek(root) {
            Some(T0::Slab) => {
                let node = unsafe { self.nodes.get_unchecked(root) };
                match node.tag() {
                    NodeType::Hash => unsafe { &node.hash }.prefetch_entries(),
                    NodeType::Set => unsafe { &node.set }.prefetch_entries(),
                    NodeType::ZSet => {}
                }
            }
            Some(T0::Image(ValNodeImage::Hash(h))) => h.prefetch_entries(),
            Some(T0::Image(ValNodeImage::Set(s))) => s.prefetch_entries(),
            None => {}
        }
    }

    pub fn is_saving(&self) -> bool {
        self.save.is_active()
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Octets occupés dans le slab (nœuds vivants × 128 B). Le slab est un
    /// mmap : invisible pour l'allocateur global, à ajouter à part, comme les
    /// overflows (MiMalloc en direct).
    pub fn slab_bytes(&self) -> usize {
        self.nodes.count_occupied() * size_of::<ValNode>()
    }
}

impl Default for ValTrees {
    fn default() -> Self {
        Self::new()
    }
}

// Sondes de taille, vérifiées par le compilateur à chaque build.
const _: () = assert!(size_of::<SharedByte>() == 8);
const _: () = assert!(size_of::<HashNode>() == 128);
const _: () = assert!(size_of::<SetNode>() == 128);
const _: () = assert!(size_of::<ValNode>() == 128);
