//! valart : Hash et Set de RadixOx, sur des tries de hash dans une HiSlab
//! unique (nœuds de 128 B, union [`ValNode`]).
//!
//! Voir `set.rs` pour la structure et ses invariants ; `hash.rs` en est le
//! miroir avec une valeur par entrée.

use std::mem::{ManuallyDrop, offset_of};

use hislab::HiSlab;
use radixox_lib::save_barrier::SaveBarrier;
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
    /// supprimés. Détruits à `finish_save`.
    deferred_bytes: Vec<SharedByte>,
    deferred_drops: Vec<(NodeType, u32)>,
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
        let nodes = &self.nodes;
        self.save.begin_with(|buf| {
            nodes.copy_occupancy(buf);
        });
        self.set_collisions.snapshot_t0();
        self.hash_collisions.snapshot_t0();
    }

    /// Termine la save : images libérées, libérations différées exécutées.
    /// TODO : vider par tranches (SAVE_DESIGN.md §2.B).
    pub fn finish_save(&mut self) {
        self.save.finish();
        self.set_collisions.clear_t0();
        self.hash_collisions.clear_t0();
        self.deferred_bytes.clear();
        for (kind, root) in std::mem::take(&mut self.deferred_drops) {
            match kind {
                NodeType::Hash => self.hash_drop_now(root),
                NodeType::Set => self.set_drop_now(root),
                NodeType::ZSet => unreachable!(),
            }
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
