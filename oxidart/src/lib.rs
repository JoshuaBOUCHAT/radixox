//! # OxidArt
//!
//! A high-performance, compressed Adaptive Radix Tree (ART) implementation in Rust
//! for fast key-value storage operations.
//!
//! ## Features
//!
//! - **O(k) operations**: All operations (get, set, del) run in O(k) time where k is the key length
//! - **Path compression**: Minimizes memory usage by compressing single-child paths
//! - **Prefix operations**: Supports `getn` and `deln` for prefix-based queries and deletions
//! - **Zero-copy values**: Uses `bytes::Bytes` for efficient value handling
//!
//! ## Example
//!
//! ```rust,ignore
//! use oxidart::OxidArt;
//! use bytes::Bytes;
//! use std::time::Duration;
//!
//! let mut tree = OxidArt::new();
//!
//! // Insert key-value pairs
//! tree.set(SharedByte::from_str("hello"), SharedByte::from_str("world"));
//!
//! // Insert with TTL (requires `ttl` feature, enabled by default)
//! tree.set_now(1700000000); // Update internal clock
//! tree.set_ttl(SharedByte::from_str("session"), Duration::from_secs(3600), SharedByte::from_str("data"));
//!
//! // Retrieve a value
//! assert_eq!(tree.get(SharedByte::from_str("hello")), Some(SharedByte::from_str("world")));
//!
//! // Get all entries with a prefix
//! let entries = tree.getn(SharedByte::from_str("hello"));
//!
//! // Delete a key
//! let deleted = tree.del(b"hello");
//!
//! // Delete all keys with a prefix
//! let count = tree.deln(SharedByte::from_str("hello"));
//! ```
//!
//! ## Key Requirements
//!
//! Keys are arbitrary bytes (binary-safe): every byte value 0x00–0xFF, `\0` included.
//! Ordered operations (`krange`, prefix scans) use unsigned byte order (`memcmp`), like Redis.


pub mod compact;
pub mod compact_path;
pub mod compress;
pub mod digest;
pub mod gc;
pub mod error;
pub mod new_node;

pub mod hcommand;
pub mod krange;
pub mod lcommand;
pub mod list;
mod prefetch_batch;
pub use valart::ValCompaction;
pub use prefetch_batch::{AmacStats, Hint, PREFETCH_BATCH, ZProbe};
mod load;
pub(crate) mod node;
pub mod save;
pub mod scommand;
pub(crate) mod small;
pub mod value;
pub mod zcommand;
#[cfg(test)]
pub(crate) mod zset_inner;
pub mod zset;


pub mod counter;

#[cfg(feature = "regex")]
pub mod regex;

#[cfg(test)]
mod test;

#[cfg(test)]
mod test_structures;

#[cfg(test)]
mod test_save;

#[cfg(test)]
mod test_binary;

#[cfg(test)]
mod test_node;

#[cfg(test)]
mod test_diff;

#[cfg(test)]
mod test_detached;

use radixox_lib::hislab::TaggedHiSlab;
use radixox_lib::shared_byte::SharedByte;

use crate::compact_path::CompactPath;
use crate::node::{Entry, EntryRef, Leaf, Node};
use crate::save::{NodeImage, OxidSave};

pub use crate::load::{LoadError, LoadStats};
pub use crate::save::{Dump, DumpEntry, DumpError, JournalPos, SaveCursor, check_footer, parse_dump};
pub use crate::value::Value;
use crate::value::{
    NodeValMut, NodeValRef, RawVal, Tag, ValUnion, init_slabs, valart, value_from_raw_ref,
};

/// Result of a TTL lookup operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlResult {
    /// The key does not exist.
    KeyNotExist,
    /// The key exists and has a TTL (remaining seconds until expiration).
    KeyWithTtl(u64),
    /// The key exists but has no TTL (permanent).
    KeyWithoutTtl,
}

/// `parent_idx` d'une racine de sous-arbre détaché, pas encore libéré (la
/// racine de l'arbre a `u32::MAX`).
const DETACHED: u32 = u32::MAX - 1;

/// A compressed Adaptive Radix Tree for fast key-value storage.
///
/// Nœuds de 128 o à feuilles inline (voir `node.rs`). Une clé a un seul
/// emplacement possible. En partant d'un nœud de chemin `P` et du reste `R`
/// de la clé :
/// - `R` vide → self-val du nœud ;
/// - entrée `R[0]` enfant → le chemin de l'enfant doit prolonger la clé ;
/// - entrée `R[0]` feuille → clé trouvée si le suffixe vaut exactement `R[1..]`.
///
/// Tout nœud hors racine porte au moins deux choses (deux entrées, ou une
/// entrée et sa self-val). Sinon il disparaît : sans entrée il redevient une
/// feuille du parent, avec une seule entrée il fusionne avec elle.
///
/// Un nœud est tagué dans le slab (évicteur TTL) si et seulement si une de
/// ses valeurs (self-val ou feuille) a un TTL.
///
/// # Example
///
/// ```rust,ignore
/// use oxidart::OxidArt;
///
/// let mut tree = OxidArt::new();
/// tree.set(SharedByte::from_str("key"), Value::from_str("value"));
/// assert_eq!(tree.get(b"key"), Some(Value::from_str("value")));
/// ```
pub struct OxidArt {
    pub(crate) map: TaggedHiSlab<Node>,
    /// Current timestamp (seconds since UNIX epoch).
    /// The server is responsible for updating this via `set_now()`.
    pub now: u64,
    pub(crate) root_idx: u32,
    /// Save progressive : barrière COW sur `map` (SAVE_DESIGN.md).
    pub(crate) save: OxidSave,
    /// Valeurs stockées, expirées pas encore nettoyées comprises (comme le
    /// `DBSIZE` de Redis).
    pub(crate) key_count: usize,
    /// FLUSHDB faits ([`detach_all`](Self::detach_all)) : voir `Detached::flushes`.
    flushes: u64,
    /// Fenêtre AMAC ouverte ([`begin_window`](Self::begin_window)) : aucun
    /// nœud n'est libéré ni fusionné ; voir `prefetch_batch::WindowState`.
    pub(crate) window: Option<prefetch_batch::WindowState>,
    /// Compteurs du moteur AMAC (prototype).
    pub amac_stats: AmacStats,
    /// Curseur de l'évicteur TTL : prochain slot à examiner dans le bitmap tagué.
    evict_cursor: u32,
    /// Curseur de l'éviction `maxmemory` (bitmap d'occupation ou tagué selon la politique).
    mem_cursor: u32,
    _live: gc::LiveTree,
}

/// Un arbre détruit pendant une save (tests ; jamais en production) la termine : les slabs
/// de valart, des ZSet et des List sont globaux, leur état de save survivrait à l'arbre et
/// ferait échouer la save suivante d'un autre arbre (« vidage pendant une save »).
impl Drop for OxidArt {
    fn drop(&mut self) {
        if self.is_saving() {
            self.end_save();
        }
        // une passe : ce qui reste (save d'un autre arbre en cours) sera vidé par lui
        let _ = self.drain_deferred(u32::MAX);
    }
}

impl Default for OxidArt {
    fn default() -> Self {
        Self::new()
    }
}

/// Où vit une valeur.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slot {
    SelfVal,
    Leaf(u8),
}

/// Emplacement d'une valeur : nœud porteur + place dans ce nœud. Valable
/// jusqu'à la prochaine mutation de l'arbre.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Loc {
    pub(crate) node: u32,
    pub(crate) slot: Slot,
}

/// Résultat de la descente vers un préfixe.
pub(crate) enum PrefixHit {
    None,
    /// Toutes les clés du sous-arbre de ce nœud commencent par le préfixe.
    Node(u32),
    /// Seule cette feuille commence par le préfixe.
    Leaf { node: u32, radix: u8 },
}

/// Échéance passée. L'échéance 0 l'est toujours, même horloge à 0 : c'est la
/// marque de mort d'une fenêtre AMAC (`remove_in_window`).
#[inline]
pub(crate) fn expired(exp: Option<u64>, now: u64) -> bool {
    exp.is_some_and(|e| e < now.max(1))
}

/// `hay` commence-t-il par `needle` ? Les préfixes et suffixes comparés en
/// descente font presque toujours ≤ 7 octets (inline) : boucle courte inlinée
/// plutôt qu'un appel à `memcmp` à chaque étage.
#[inline(always)]
pub(crate) fn starts_with_fast(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n > hay.len() {
        return false;
    }
    if n > 16 {
        return &hay[..n] == needle;
    }
    let mut i = 0;
    while i < n {
        // SAFETY: i < n ≤ hay.len().
        if unsafe { *hay.get_unchecked(i) != *needle.get_unchecked(i) } {
            return false;
        }
        i += 1;
    }
    true
}

#[inline(always)]
pub(crate) fn eq_fast(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && starts_with_fast(a, b)
}

#[inline]
fn lcp(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[inline]
fn sfx_of(bytes: &[u8]) -> Option<CompactPath> {
    (!bytes.is_empty()).then(|| CompactPath::from_slice(bytes))
}

fn set_parent_radix(node: &mut Node, radix: u8) {
    node.exp_and_radix = match node.exp_and_radix.exp() {
        Some(exp) => ExpAndRadix::new(exp, radix),
        None => ExpAndRadix::no_expiracy(radix),
    };
}

fn leaf(raw: RawVal, exp: Option<u64>, sfx: Option<CompactPath>) -> Entry {
    let (tag, val) = raw.into_parts();
    Entry::Leaf(Leaf { tag, val, ttl: exp, sfx })
}

/// Pose `raw` en self-val d'un nœud qui n'en a pas.
fn put_self(node: &mut Node, raw: RawVal, exp: Option<u64>) {
    debug_assert!(!node.has_val());
    let (tag, val) = raw.into_parts();
    node.tag = tag;
    node.val = val;
    match exp {
        Some(exp) => node.exp_and_radix.set_exp(exp),
        None => node.exp_and_radix.set_no_expiracy(),
    }
}

/// Écrase sur place une self-val `Bytes` avec les octets de `raw` (`Bytes` aussi) et
/// pose le TTL ; `raw` reste à l'appelant. MEMORY_ARCH.md §16.5.
fn overwrite_self(node: &mut Node, raw: &RawVal, exp: Option<u64>) -> bool {
    let Some(src) = raw.as_bytes() else { return false };
    if node.tag != Tag::Bytes || !unsafe { (*node.val.bytes).overwrite_from(src) } {
        return false;
    }
    match exp {
        Some(exp) => node.exp_and_radix.set_exp(exp),
        None => node.exp_and_radix.set_no_expiracy(),
    }
    true
}

/// Retire la self-val (propriété à l'appelant).
fn take_self(node: &mut Node) -> (RawVal, Option<u64>) {
    debug_assert!(node.has_val());
    let raw = RawVal::new(node.tag, std::mem::replace(&mut node.val, ValUnion { integer: 0 }));
    let exp = node.exp_and_radix.exp();
    node.tag = Tag::None;
    node.exp_and_radix.set_no_expiracy();
    (raw, exp)
}

/// Une valeur posée sous un nœud neuf `n` : self-val si la clé s'arrête à
/// `common`, sinon feuille de radix `bytes[common]`.
fn place(n: &mut Node, bytes: &[u8], common: usize, raw: RawVal, exp: Option<u64>) {
    if bytes.len() == common {
        put_self(n, raw, exp);
    } else {
        n.insert(bytes[common], leaf(raw, exp, sfx_of(&bytes[common + 1..])));
    }
}

/// Décision d'un étage de `set_raw`.
enum Step {
    Absent,
    SameLeaf { live: bool },
    SplitLeaf,
    Descend(u32, usize),
    SplitChild(u32),
}

impl OxidArt {
    /// Creates a new empty `OxidArt` tree.
    pub fn new() -> Self {
        init_slabs();
        let map = TaggedHiSlab::new(20000, 25000000).expect("Can't allocate oxidart");
        let root_idx = map.insert(Node::new(CompactPath::new(), u32::MAX, 0));
        Self {
            map,
            root_idx,
            now: 0,
            save: OxidSave::default(),
            key_count: 0,
            flushes: 0,
            window: None,
            amac_stats: AmacStats::default(),
            evict_cursor: 0,
            mem_cursor: 0,
            _live: gc::LiveTree::new(),
        }
    }

    /// Nombre de clés stockées, en O(1). Les clés expirées pas encore
    /// supprimées comptent (comme le `DBSIZE` de Redis).
    #[inline]
    pub fn len(&self) -> usize {
        self.key_count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.key_count == 0
    }

    /// Updates the current timestamp. Call this periodically from your async runtime.
    #[inline]
    pub fn set_now(&mut self, now: u64) {
        self.now = now;
    }

    /// Met l'horloge du TTL à l'heure murale (secondes depuis l'époque Unix).
    /// La boucle du serveur l'appelle une fois par tour.
    #[inline]
    pub fn tick(&mut self) {
        self.now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before UNIX epoch")
            .as_secs();
    }

    /// Indique si une save est en cours. Pendant une save, les lectures ne
    /// modifient rien et l'évicteur est en pause (SAVE_DESIGN.md §2.C).
    #[inline]
    pub fn is_saving(&self) -> bool {
        self.save.barrier.is_active()
    }

    /// Une save tient les slabs globaux (ZSet, List, valart) : celle de cet arbre, ou celle
    /// d'un autre arbre du processus (tests à plusieurs nœuds). Une seule save à la fois
    /// par processus, comme Redis : la suivante attend.
    pub fn save_busy(&self) -> bool {
        self.is_saving() || zset::save::is_saving() || list::save::is_saving() || valart().is_saving()
    }

    /// Démarre une save : fige la vue T0 (bitmap de présence d'oxidart et
    /// de valart).
    pub fn begin_save(&mut self) {
        // Reliquat d'une save précédente pas encore vidé : d'un coup (rare).
        self.save.leftover.clear();
        let map = &self.map;
        self.save.barrier.begin_with(|buf| {
            map.copy_occupancy(buf);
        });
        self.save.t0 = self.now;
        valart().begin_save();
        zset::save::begin();
        list::save::begin();
    }

    /// Termine la save (fin normale ou abandon) : images libérées, liste
    /// différée valart vidée d'un coup. Côté serveur, préférer
    /// [`end_save`](Self::end_save) puis [`drain_deferred`](Self::drain_deferred)
    /// par tranches.
    pub fn finish_save(&mut self) {
        self.end_save();
        while !self.drain_deferred(u32::MAX) {}
    }

    /// Ferme la barrière (images libérées) sans vider la liste différée valart.
    pub fn end_save(&mut self) {
        self.save.barrier.finish();
        self.save.clear_images();
        valart().end_save();
        zset::save::end();
        list::save::end();
    }

    /// Une tranche de ce que la save laisse à libérer : feuilles des images
    /// oxidart, puis liste différée valart (suppressions faites pendant la
    /// save). `budget` : feuilles + coût valart (`ValTrees::drain_deferred`).
    /// `true` quand tout est libéré.
    pub fn drain_deferred(&mut self, budget: u32) -> bool {
        let rest = self.save.drain_leftover(budget);
        if !self.save.leftover.is_empty() {
            return false;
        }
        // Slabs globaux (ZSet, List, valart) : une save active est celle d'un autre arbre du
        // processus (tests à plusieurs nœuds) ; ses images peuvent encore lire ce qui est
        // différé. On attend sa fin.
        if zset::save::is_saving() || list::save::is_saving() || valart().is_saving() {
            return false;
        }
        let rest = zset::save::drain_step(rest);
        if !zset::save::deferred_empty() {
            return false;
        }
        let rest = list::save::drain_step(rest);
        if !list::save::deferred_empty() {
            return false;
        }
        valart().drain_deferred(rest)
    }

    /// Compactage de la slab valart (`valart::ValCompaction`) : une passe si ses trous
    /// dépassent le seuil (`force` : toujours). Jamais pendant une save ni en même temps
    /// qu'une passe du GC (au pilote d'y veiller).
    pub fn valart_compact_want(&self, force: bool) -> Option<valart::ValCompaction> {
        valart().compact_want(force)
    }

    /// Indices des racines valart (Hash, Set) que tient le nœud `idx` (valeur propre et
    /// feuilles) : le nœud vient d'être construit (chargement) ou déplacé (compactage).
    pub(crate) fn hint_containers(&self, idx: u32) {
        let node = unsafe { self.map.get_unchecked(idx) };
        let hint = |tag: Tag, val: &ValUnion| match tag {
            Tag::Hash => valart().set_hash_root_hint(unsafe { val.idx }, idx),
            Tag::Set => valart().set_set_root_hint(unsafe { val.idx }, idx),
            _ => {}
        };
        hint(node.tag, &node.val);
        for (_, e) in node.iter() {
            if let EntryRef::Leaf { tag, val, .. } = e {
                hint(tag, val);
            }
        }
    }

    /// Réparation des indices des racines valart avant une passe de compactage valart : les
    /// nœuds oxidart `from..`, au plus `budget`, reposent l'indice de chaque conteneur
    /// qu'ils tiennent (une feuille a pu changer de nœud depuis la création de la racine).
    /// Rend le nœud où reprendre, `None` à la fin. Jamais pendant une save (indice écrit
    /// seulement s'il change ; une racine écrite pendant une save serait capturée).
    pub fn valart_hint_repair(&self, from: u32, budget: u32) -> Option<u32> {
        let mut at = from;
        for _ in 0..budget {
            let i = self.map.next_occupied(at)?;
            self.hint_containers(i);
            at = i + 1;
        }
        Some(at)
    }

    /// Une tranche du compactage valart ; `Some((déplacés, octets rendus))` à la fin. Une
    /// racine ne bouge que si le nœud oxidart de son indice tient bien sa valeur (valeur
    /// propre ou feuille) : l'idx y est réécrit.
    pub fn valart_compact_step(&mut self, c: &mut valart::ValCompaction, budget: u32) -> Option<(u64, usize)> {
        let map = &mut self.map;
        valart().compact_step(c, budget, |hint, src, dst| {
            if hint == valart::NO_HINT || !map.is_occupied(hint) {
                return false;
            }
            // Sans `node_mut` : jamais pendant une save ; réécrire l'idx d'une racine déplacée
            // n'est pas une mutation logique.
            let node = unsafe { map.get_unchecked_mut(hint) };
            if matches!(node.tag, Tag::Hash | Tag::Set) && unsafe { node.val.idx } == src {
                node.val.idx = dst;
                return true;
            }
            let radix = node.iter().find_map(|(r, e)| match e {
                EntryRef::Leaf { tag: Tag::Hash | Tag::Set, val, .. } if unsafe { val.idx } == src => Some(r),
                _ => None,
            });
            match radix.and_then(|r| node.leaf_val_mut(r)) {
                Some((_, val)) => {
                    val.idx = dst;
                    true
                }
                None => false,
            }
        })
    }

    /// Octets de trous sous la fin de la slab valart.
    pub fn valart_holes_bytes(&self) -> u64 {
        valart().compact_holes_bytes()
    }

    /// Une tranche de la libération des grands ZSet et des grandes List supprimés
    /// (`zset::lazy`, `list::lazy`), au plus `budget` unités (nœuds, membres, blocs) ; rien
    /// pendant une save. Vrai s'il en reste.
    pub fn zset_free_step(&mut self, budget: u32) -> bool {
        let z = zset::lazy::step(budget);
        list::lazy::step(budget / 16) || z
    }

    /// Des grands ZSet ou de grandes List attendent d'être libérés, et ça peut avancer
    /// maintenant (pas de save en cours).
    pub fn zset_free_pending(&self) -> bool {
        (zset::lazy::pending() > 0 || list::lazy::pending() > 0) && !self.is_saving()
    }

    /// Nombre de nœuds qui ont une page de débordement.
    pub fn overflow_count(&self) -> usize {
        let mut n = 0usize;
        self.map.for_each_occupied(|_, node| n += (node.page_block() > 0) as usize);
        n
    }

    /// Returns the number of nodes currently allocated.
    pub fn node_count(&self) -> usize {
        let mut n = 0usize;
        self.map.for_each_occupied(|_, _| n += 1);
        n
    }

    // ─── Accès aux nœuds et points de passage (SAVE_DESIGN.md §3) ───────────
    //
    // Seules `node_mut` et `remove_node` donnent un `&mut Node` ou libèrent un
    // nœud : la barrière de save y est branchée. Modifier la page de
    // débordement d'un nœud, c'est modifier le nœud (elle n'est accessible que
    // par lui). Exception sans barrière (champs non écrits dans le dump) :
    // `set_parent`, et le tag TTL du slab (`map.tag` / `map.untag`).

    /// Nœud désigné par la racine ou par une entrée enfant d'un nœud vivant,
    /// lu sans consulter le bitmap d'occupation (une ligne de cache de moins
    /// par étage). Réservé aux descentes synchrones : aucun idx de l'arbre
    /// vivant n'est gardé d'un tour à l'autre (les jobs reprennent sur une clé).
    #[inline(always)]
    pub(crate) fn node(&self, idx: u32) -> &Node {
        debug_assert!(self.map.is_occupied(idx), "idx de nœud {idx} libre");
        // SAFETY: un nœud n'est libéré qu'après avoir été détaché de son
        // parent, dans le même appel synchrone ; une entrée enfant vivante
        // désigne donc toujours un slot occupé.
        unsafe { self.map.get_unchecked(idx) }
    }

    #[inline]
    pub(crate) fn node_mut(&mut self, idx: u32) -> &mut Node {
        self.before_write(idx);
        debug_assert!(self.map.is_occupied(idx), "idx de nœud {idx} libre");
        // SAFETY: comme `node`.
        unsafe { self.map.get_unchecked_mut(idx) }
    }

    /// Retire un nœud du slab (son tag TTL avec).
    pub(crate) fn remove_node(&mut self, idx: u32) -> Node {
        self.before_write(idx);
        self.map.remove(idx).expect("remove_node : slot libre")
    }

    /// Barrière : capture l'image T0 du nœud s'il est encore intact.
    #[inline]
    fn before_write(&mut self, idx: u32) {
        let Self { map, save, .. } = self;
        let OxidSave { barrier, childs, leaves, .. } = save;
        barrier.before_write(idx, || {
            let node = map.get(idx).expect("intact à T0 ⇒ encore occupé");
            NodeImage::capture(node, childs, leaves)
        });
    }

    /// Parent et radix ne sont pas sérialisés : pas de barrière.
    pub(crate) fn set_parent(&mut self, idx: u32, parent: u32, radix: u8) {
        // SAFETY: `idx` désigne un nœud vivant (enfant qu'on rattache).
        let node = unsafe { self.map.get_unchecked_mut(idx) };
        node.parent_idx = parent;
        set_parent_radix(node, radix);
    }

    /// Insère un nœud neuf, tagué s'il porte un TTL.
    fn insert_node(&mut self, node: Node) -> u32 {
        if node.has_any_ttl() {
            self.map.insert_tagged(node)
        } else {
            self.map.insert(node)
        }
    }

    /// Remet le tag TTL de `idx` en accord avec son contenu, `before` étant
    /// l'état avant la mutation (aucune lecture du bitmap de tags).
    #[inline]
    fn retag(&mut self, idx: u32, before: bool) {
        let after = self.node(idx).has_any_ttl();
        if after != before {
            if after {
                self.map.tag(idx);
            } else {
                self.map.untag(idx);
            }
        }
    }

    // ─── Descente ───────────────────────────────────────────────────────────

    /// Emplacement de la valeur de `key`, expirée ou non.
    #[inline]
    /// `(idx, cur)` est sur le chemin de `key` : la descente depuis la racine
    /// passe par `idx` avec `cur` octets consommés.
    #[cfg(debug_assertions)]
    fn on_path(&self, key: &[u8], idx: u32, cur: usize) -> bool {
        let (mut x, mut c) = (self.root_idx, 0);
        loop {
            if (x, c) == (idx, cur) {
                return true;
            }
            if c >= cur || c == key.len() {
                return false;
            }
            match self.node(x).get(key[c]) {
                Some(EntryRef::Child(ch)) => {
                    let cp: &[u8] = &self.node(ch).prefix;
                    if !starts_with_fast(&key[c + 1..], cp) {
                        return false;
                    }
                    c += 1 + cp.len();
                    x = ch;
                }
                _ => return false,
            }
        }
    }

    pub(crate) fn locate(&self, key: &[u8]) -> Option<Loc> {
        let mut idx = self.root_idx;
        let mut cur = 0;
        loop {
            let node = self.node(idx);
            if cur == key.len() {
                return node.has_val().then_some(Loc { node: idx, slot: Slot::SelfVal });
            }
            let r = key[cur];
            let rest = &key[cur + 1..];
            match node.get(r)? {
                EntryRef::Leaf { sfx, .. } => {
                    let s: &[u8] = sfx.map_or(&[], |p| p);
                    return eq_fast(s, rest).then_some(Loc { node: idx, slot: Slot::Leaf(r) });
                }
                EntryRef::Child(c) => {
                    let child = self.node(c);
                    child.prefetch_body();
                    let cp: &[u8] = &child.prefix;
                    if !starts_with_fast(rest, cp) {
                        return None;
                    }
                    cur += 1 + cp.len();
                    idx = c;
                }
            }
        }
    }

    /// Descente avec lecture de la valeur au passage (une seule recherche
    /// par étage) : emplacement, tag, valeur, échéance. Expirée ou non.
    #[inline]
    pub(crate) fn find_value(&self, key: &[u8]) -> Option<(Loc, Tag, &ValUnion, Option<u64>)> {
        let mut idx = self.root_idx;
        let mut cur = 0;
        loop {
            let node = self.node(idx);
            if cur == key.len() {
                if !node.has_val() {
                    return None;
                }
                let loc = Loc { node: idx, slot: Slot::SelfVal };
                return Some((loc, node.tag, &node.val, node.exp_and_radix.exp()));
            }
            let r = key[cur];
            let rest = &key[cur + 1..];
            match node.get(r)? {
                EntryRef::Leaf { tag, val, ttl, sfx } => {
                    let s: &[u8] = sfx.map_or(&[], |p| p);
                    if !eq_fast(s, rest) {
                        return None;
                    }
                    return Some((Loc { node: idx, slot: Slot::Leaf(r) }, tag, val, ttl));
                }
                EntryRef::Child(c) => {
                    let child = self.node(c);
                    child.prefetch_body();
                    let cp: &[u8] = &child.prefix;
                    if !starts_with_fast(rest, cp) {
                        return None;
                    }
                    cur += 1 + cp.len();
                    idx = c;
                }
            }
        }
    }

    /// Valeur à `loc` : tag, valeur, échéance.
    #[inline]
    pub(crate) fn loc_value(&self, loc: Loc) -> (Tag, &ValUnion, Option<u64>) {
        let node = self.node(loc.node);
        match loc.slot {
            Slot::SelfVal => (node.tag, &node.val, node.exp_and_radix.exp()),
            Slot::Leaf(r) => match node.get(r) {
                Some(EntryRef::Leaf { tag, val, ttl, .. }) => (tag, val, ttl),
                _ => unreachable!("loc_value : pas une feuille"),
            },
        }
    }

    /// Emplacement de la valeur vivante de `key`. Une valeur expirée est
    /// supprimée au passage (sauf pendant une save : seulement vue absente).
    /// Dans [`with_hint`](Self::with_hint), part de l'indice.
    pub(crate) fn live_loc(&mut self, key: &[u8]) -> Option<Loc> {
        if let Some(hint) = self.scoped_hint(key) {
            return self.live_hinted(key, hint);
        }
        let (loc, _, _, exp) = self.find_value(key)?;
        if expired(exp, self.now) {
            self.drop_expired(loc, key);
            return None;
        }
        Some(loc)
    }

    /// Retire une valeur expirée croisée en lecture (pas pendant une save) ;
    /// dans une fenêtre AMAC, sans changer la forme (`remove_in_window`).
    #[inline]
    pub(crate) fn drop_expired(&mut self, loc: Loc, key: &[u8]) {
        if self.is_saving() {
            return;
        }
        if self.window.is_some() {
            self.remove_in_window(loc);
            self.touch(key);
        } else {
            drop(self.remove_loc(loc));
        }
    }

    /// Valeur vivante à `key`, par référence (aucune capture de save).
    pub(crate) fn get_ref(&mut self, key: &[u8]) -> Option<NodeValRef<'_>> {
        let loc = self.live_loc(key)?;
        let (tag, val, _) = self.loc_value(loc);
        Some(NodeValRef { tag, val })
    }

    /// Accès en écriture à la valeur vivante de `key` (passe par la barrière).
    pub(crate) fn value_mut(&mut self, key: &[u8]) -> Option<NodeValMut<'_>> {
        let loc = self.live_loc(key)?;
        Some(self.val_mut_at(loc))
    }

    /// Accès en écriture à la valeur à `loc` (passe par la barrière).
    pub(crate) fn val_mut_at(&mut self, loc: Loc) -> NodeValMut<'_> {
        let node = self.node_mut(loc.node);
        match loc.slot {
            Slot::SelfVal => NodeValMut { tag: &mut node.tag, val: &mut node.val, node: loc.node },
            Slot::Leaf(r) => {
                let (tag, val) = node.leaf_val_mut(r).expect("feuille");
                NodeValMut { tag, val, node: loc.node }
            }
        }
    }

    /// Conteneur de type `want` à `key` : celui qui existe, ou un neuf créé
    /// par `create` et posé avec `ttl`. `Err` (type de la valeur en place) si
    /// la clé porte un autre type.
    pub(crate) fn ensure_container(
        &mut self,
        key: &[u8],
        ttl: Option<u64>,
        want: Tag,
        create: impl FnOnce() -> u32,
    ) -> Result<u32, Tag> {
        if let Some(v) = self.get_ref(key) {
            if v.tag != want {
                return Err(v.tag);
            }
            // SAFETY: conteneur ⇒ champ idx actif.
            return Ok(unsafe { v.val.idx });
        }
        let idx = create();
        self.set_raw(key, RawVal::new(want, ValUnion { idx }), ttl, None);
        Ok(idx)
    }

    // ─── Lecture ────────────────────────────────────────────────────────────

    /// Retrieves the value associated with the given key.
    ///
    /// Returns `Some(value)` if the key exists and is not expired, `None`
    /// otherwise. Expired entries are deleted on the way (except during a
    /// save).
    pub fn get(&mut self, key: &[u8]) -> Option<Value> {
        let (loc, tag, val, exp) = self.find_value(key)?;
        if expired(exp, self.now) {
            self.drop_expired(loc, key);
            return None;
        }
        // SAFETY: tag != None (valeur trouvée).
        Some(unsafe { value_from_raw_ref(tag, val) })
    }

    /// Returns the TTL status of a key.
    pub fn get_ttl(&self, key: SharedByte) -> TtlResult {
        let Some((_, _, _, exp)) = self.find_value(&key) else {
            return TtlResult::KeyNotExist;
        };
        match exp {
            Some(exp) if expired(Some(exp), self.now) => TtlResult::KeyNotExist,
            Some(exp) => TtlResult::KeyWithTtl(exp - self.now),
            None => TtlResult::KeyWithoutTtl,
        }
    }

    // ─── Écriture ───────────────────────────────────────────────────────────

    /// Inserts or updates a key-value pair (no TTL).
    pub fn set(&mut self, key: SharedByte, val: Value) {
        self.set_raw(&key, RawVal::from_value(val), None, None);
    }

    /// Inserts or updates a key-value pair with a time-to-live duration
    /// (from the current timestamp `self.now`).
    pub fn set_ttl(&mut self, key: SharedByte, ttl: std::time::Duration, val: Value) {
        let exp = self.now.saturating_add(ttl.as_secs());
        self.set_raw(&key, RawVal::from_value(val), Some(exp), None);
    }

    /// Inserts or updates a key-value pair, conditioned on prior existence, in
    /// a single tree traversal.
    ///
    /// * `if_exists = None` — always set.
    /// * `if_exists = Some(true)` — only if the key already exists (XX).
    /// * `if_exists = Some(false)` — only if the key does not exist (NX).
    ///
    /// Returns `true` if the value was written.
    pub fn set_cond(
        &mut self,
        key: &[u8],
        ttl: Option<std::time::Duration>,
        val: Value,
        if_exists: Option<bool>,
    ) -> bool {
        let exp = ttl.map(|d| self.now.saturating_add(d.as_secs()));
        self.set_raw(key, RawVal::from_value(val), exp, if_exists)
    }

    /// Pose `raw` (échéance `exp`) à `key` en une seule descente. Avec
    /// `cond = Some(want)`, n'écrit que si l'existence de la clé (vivante)
    /// vaut `want` ; sinon `raw` est libérée et la fonction renvoie `false`.
    /// Dans une fenêtre AMAC, la clé est notée touchée (elle a pu apparaître).
    pub(crate) fn set_raw(&mut self, key: &[u8], raw: RawVal, exp: Option<u64>, cond: Option<bool>) -> bool {
        self.set_raw_from(key, self.root_idx, 0, raw, exp, cond)
    }

    /// [`set_raw`](Self::set_raw) à partir du nœud `idx`, `cur` octets de `key`
    /// consommés (préfixe de `idx` compris) : un point du chemin de `key`, tel
    /// que la descente de la phase 1 d'AMAC le laisse sur une clé absente.
    pub(crate) fn set_raw_from(
        &mut self,
        key: &[u8],
        mut idx: u32,
        mut cur: usize,
        raw: RawVal,
        exp: Option<u64>,
        cond: Option<bool>,
    ) -> bool {
        #[cfg(debug_assertions)]
        assert!(self.on_path(key, idx, cur), "point d'insertion faux pour {}", key.escape_ascii());
        self.touch(key);
        let now = self.now;
        let ok = |exists: bool| cond.is_none_or(|want| want == exists);
        loop {
            if cur == key.len() {
                let node = self.node(idx);
                if !ok(node.has_val() && !expired(node.exp_and_radix.exp(), now)) {
                    return false;
                }
                if node.has_val() {
                    self.write_at(Loc { node: idx, slot: Slot::SelfVal }, raw, exp);
                    return true;
                }
                let before = node.has_any_ttl();
                self.key_count += 1;
                put_self(self.node_mut(idx), raw, exp);
                self.retag(idx, before);
                return true;
            }
            let r = key[cur];
            let rest = &key[cur + 1..];
            // décision prise sous emprunt, sans copie des chemins
            let step = match self.node(idx).get(r) {
                None => Step::Absent,
                Some(EntryRef::Leaf { sfx, ttl, .. }) => {
                    if eq_fast(sfx.map_or(&[][..], |p| p), rest) {
                        Step::SameLeaf { live: !expired(ttl, now) }
                    } else {
                        Step::SplitLeaf
                    }
                }
                Some(EntryRef::Child(c)) => {
                    let child = self.node(c);
                    child.prefetch_body();
                    let cp: &[u8] = &child.prefix;
                    if starts_with_fast(rest, cp) {
                        Step::Descend(c, cp.len())
                    } else {
                        Step::SplitChild(c)
                    }
                }
            };
            let exists = matches!(step, Step::SameLeaf { live: true });
            if !matches!(step, Step::Descend(..)) && !ok(exists) {
                return false;
            }
            match step {
                Step::Descend(c, plen) => {
                    cur += 1 + plen;
                    idx = c;
                    continue;
                }
                Step::Absent => {
                    self.key_count += 1;
                    let before = self.node(idx).has_any_ttl();
                    self.node_mut(idx).insert(r, leaf(raw, exp, sfx_of(rest)));
                    self.retag(idx, before);
                }
                Step::SameLeaf { .. } => self.write_at(Loc { node: idx, slot: Slot::Leaf(r) }, raw, exp),
                Step::SplitLeaf => {
                    self.key_count += 1;
                    self.split_leaf(idx, r, rest, raw, exp);
                }
                Step::SplitChild(c) => {
                    self.key_count += 1;
                    let cp = self.node(c).prefix.to_vec();
                    self.split_child(idx, r, c, &cp, rest, raw, exp);
                }
            }
            return true;
        }
    }

    /// Écrase la valeur présente à `loc` (expirée ou non) par `raw`, échéance
    /// `exp`. Ne change pas la forme : `loc` reste l'emplacement de la clé.
    pub(crate) fn write_at(&mut self, loc: Loc, raw: RawVal, exp: Option<u64>) {
        let idx = loc.node;
        let before = self.node(idx).has_any_ttl();
        match loc.slot {
            Slot::SelfVal => {
                // Après `node_mut` : un nœud intact capturé par la save tient un
                // clone (rc == 2), l'écrasement est refusé tout seul.
                let node = self.node_mut(idx);
                if !overwrite_self(node, &raw, exp) {
                    drop(take_self(node));
                    put_self(node, raw, exp);
                }
            }
            Slot::Leaf(r) => {
                // Écrasement sur place, après `node_mut` (voir la self-val).
                let in_place = raw.as_bytes().is_some_and(|src| self.node_mut(idx).overwrite_leaf_bytes(r, src, exp));
                if !in_place {
                    let (tag, val) = raw.into_parts();
                    let updated = self.node_mut(idx).update_leaf(r, tag, val, exp);
                    debug_assert!(updated);
                }
            }
        }
        self.retag(idx, before);
    }

    /// Promotion : la feuille `r` de `idx` et la nouvelle clé (reste `rest`)
    /// divergent → un nœud de préfixe commun les porte.
    fn split_leaf(&mut self, idx: u32, r: u8, rest: &[u8], raw: RawVal, exp: Option<u64>) {
        let before = self.node(idx).has_any_ttl();
        // L'entrée devient tout de suite un enfant (index provisoire, remplacé une fois
        // le nouveau nœud posé) : la page n'est ni réduite ni regrossie.
        let old = self.node_mut(idx).leaf_to_child(r, u32::MAX);
        let (tag, val, old_exp, old_sfx) = old.into_parts();
        let s: &[u8] = old_sfx.as_deref().unwrap_or(&[]);
        let common = lcp(s, rest);
        let mut n = Node::new(CompactPath::from_slice(&s[..common]), idx, r);
        let old_slot = if common == s.len() { Slot::SelfVal } else { Slot::Leaf(s[common]) };
        // L'ancienne feuille garde son suffixe, raccourci sur place : pas d'objet neuf, et
        // pas d'ancien objet entier laissé en trou pour le GC (une fois par insertion au
        // chargement d'UUID).
        match old_slot {
            Slot::SelfVal => put_self(&mut n, RawVal::new(tag, val), old_exp),
            Slot::Leaf(radix) => {
                let mut sfx = old_sfx.expect("suffixe plus long que le préfixe commun");
                let sfx = if common + 1 == sfx.len() {
                    None
                } else {
                    sfx.drop_front(common + 1);
                    Some(sfx)
                };
                n.insert(radix, leaf(RawVal::new(tag, val), old_exp, sfx));
            }
        }
        place(&mut n, rest, common, raw, exp);
        let n_idx = self.insert_node(n);
        let done = self.node_mut(idx).set_child_idx(r, n_idx);
        debug_assert!(done);
        self.retag(idx, before);
        if let Some(w) = &mut self.window {
            w.leaf_moved(Loc { node: idx, slot: Slot::Leaf(r) }, Loc { node: n_idx, slot: old_slot });
        }
    }

    /// Le préfixe `cp` de l'enfant `c` ne prolonge pas la clé : un nœud
    /// intermédiaire de préfixe commun s'intercale.
    #[allow(clippy::too_many_arguments)]
    fn split_child(&mut self, idx: u32, r: u8, c: u32, cp: &[u8], rest: &[u8], raw: RawVal, exp: Option<u64>) {
        let common = lcp(cp, rest);
        debug_assert!(common < cp.len());
        let mut n = Node::new(CompactPath::from_slice(&cp[..common]), idx, r);
        n.insert(cp[common], Entry::Child(c));
        place(&mut n, rest, common, raw, exp);
        let n_idx = self.insert_node(n);
        self.node_mut(c).prefix.drop_front(common + 1);
        if let Some(w) = &mut self.window {
            w.prefix_cut(c);
        }
        self.set_parent(c, n_idx, cp[common]);
        let done = self.node_mut(idx).set_child_idx(r, n_idx);
        debug_assert!(done);
    }

    /// Sets a TTL on an existing key.
    ///
    /// Returns `true` if the key exists and the TTL was set (also when it
    /// replaces a previous TTL, like Redis), `false` otherwise.
    pub fn expire(&mut self, key: SharedByte, ttl: std::time::Duration) -> bool {
        let Some(loc) = self.locate(&key) else {
            return false;
        };
        if expired(self.loc_value(loc).2, self.now) {
            return false;
        }
        let exp = self.now.saturating_add(ttl.as_secs());
        self.set_loc_exp(loc, Some(exp));
        true
    }

    /// Removes the TTL from a key, making it permanent.
    ///
    /// Returns `true` if the key exists and had a TTL, `false` otherwise.
    pub fn persist(&mut self, key: SharedByte) -> bool {
        let Some(loc) = self.locate(&key) else {
            return false;
        };
        let exp = self.loc_value(loc).2;
        // Expirée mais pas encore nettoyée : absente, ne pas la ressusciter.
        if exp.is_none() || expired(exp, self.now) {
            return false;
        }
        self.set_loc_exp(loc, None);
        true
    }

    pub(crate) fn set_loc_exp(&mut self, loc: Loc, exp: Option<u64>) {
        let before = self.node(loc.node).has_any_ttl();
        let node = self.node_mut(loc.node);
        match loc.slot {
            Slot::SelfVal => match exp {
                Some(e) => node.exp_and_radix.set_exp(e),
                None => node.exp_and_radix.set_no_expiracy(),
            },
            Slot::Leaf(r) => {
                node.set_ttl(r, exp);
            }
        }
        self.retag(loc.node, before);
    }

    // ─── Suppression ────────────────────────────────────────────────────────

    /// Deletes a key from the tree. Returns `true` if a live (non-expired)
    /// value was removed. Ne ramène pas la valeur ; voir
    /// [`OxidArt::take`] pour GETDEL.
    pub fn del(&mut self, key: &[u8]) -> bool {
        let Some((loc, _, _, exp)) = self.find_value(key) else {
            return false;
        };
        let live = !expired(exp, self.now);
        if self.window.is_some() {
            // fenêtre AMAC : sans changer la forme
            self.remove_in_window(loc);
            self.touch(key);
        } else {
            drop(self.remove_loc(loc));
        }
        live
    }

    /// Deletes a key and returns its live value (`None` if absent or expired).
    pub fn take(&mut self, key: &[u8]) -> Option<Value> {
        let (loc, _, _, exp) = self.find_value(key)?;
        let live = !expired(exp, self.now);
        let raw = self.remove_loc(loc);
        live.then(|| raw.into_value())
    }

    /// Retire la valeur à `loc` (propriété à l'appelant) puis rétablit
    /// l'invariant de forme.
    pub(crate) fn remove_loc(&mut self, loc: Loc) -> RawVal {
        let raw = self.take_loc(loc);
        self.normalize(loc.node);
        raw
    }

    /// Retire la valeur à `loc` sans normaliser (l'appelant le fait).
    pub(crate) fn take_loc(&mut self, loc: Loc) -> RawVal {
        self.key_count -= 1;
        let before = self.node(loc.node).has_any_ttl();
        let raw = match loc.slot {
            Slot::SelfVal => take_self(self.node_mut(loc.node)).0,
            Slot::Leaf(r) => {
                let Some(Entry::Leaf(l)) = self.node_mut(loc.node).remove(r) else {
                    unreachable!("remove_loc : pas une feuille");
                };
                let (tag, val, _, _) = l.into_parts();
                RawVal::new(tag, val)
            }
        };
        self.retag(loc.node, before);
        raw
    }

    /// Rétablit l'invariant « au moins deux choses » sur `x` après une
    /// suppression, en remontant par les `parent_idx` si le parent perd une
    /// entrée.
    pub(crate) fn normalize(&mut self, mut x: u32) {
        debug_assert!(self.window.is_none(), "normalize dans une fenêtre AMAC : la forme ne doit pas changer");
        while x != self.root_idx {
            let node = self.node(x);
            let (len, has_val) = (node.len(), node.has_val());
            let (p, r) = (node.parent_idx, node.exp_and_radix.parent_radix());
            match (len, has_val) {
                // rétrogradation : le nœud redevient une feuille du parent
                (0, true) => {
                    let mut gone = self.remove_node(x);
                    let (raw, exp) = take_self(&mut gone);
                    let sfx = sfx_of(&gone.prefix);
                    let before = self.node(p).has_any_ttl();
                    let parent = self.node_mut(p);
                    parent.remove(r);
                    parent.insert(r, leaf(raw, exp, sfx));
                    self.retag(p, before);
                    return;
                }
                // vide : disparaît, le parent perd une entrée
                (0, false) => {
                    self.remove_node(x);
                    let before = self.node(p).has_any_ttl();
                    self.node_mut(p).remove(r);
                    self.retag(p, before);
                    x = p;
                }
                // une seule entrée : fusion avec elle
                (1, false) => {
                    let q = node.radix_at(0);
                    let mut gone = self.remove_node(x);
                    let entry = gone.remove(q).expect("entrée");
                    let mut joined = gone.prefix.to_vec();
                    joined.push(q);
                    match entry {
                        Entry::Child(c) => {
                            joined.extend_from_slice(&self.node(c).prefix);
                            self.node_mut(c).prefix = CompactPath::from_slice(&joined);
                            self.set_parent(c, p, r);
                            let done = self.node_mut(p).set_child_idx(r, c);
                            debug_assert!(done);
                        }
                        Entry::Leaf(l) => {
                            let (tag, val, exp, sfx) = l.into_parts();
                            if let Some(sfx) = &sfx {
                                joined.extend_from_slice(sfx);
                            }
                            let before = self.node(p).has_any_ttl();
                            let parent = self.node_mut(p);
                            parent.remove(r);
                            parent.insert(r, leaf(RawVal::new(tag, val), exp, sfx_of(&joined)));
                            self.retag(p, before);
                        }
                    }
                    return;
                }
                _ => return,
            }
        }
    }

    // ─── Préfixes ───────────────────────────────────────────────────────────

    /// Descente vers `prefix`. `path` (vidé d'abord) reçoit la clé complète
    /// du nœud trouvé, ou de la feuille.
    pub(crate) fn find_prefix(&self, prefix: &[u8], path: &mut Vec<u8>) -> PrefixHit {
        path.clear();
        let mut idx = self.root_idx;
        let mut cur = 0;
        loop {
            if cur == prefix.len() {
                return PrefixHit::Node(idx);
            }
            let r = prefix[cur];
            let rest = &prefix[cur + 1..];
            let Some(entry) = self.node(idx).get(r) else {
                return PrefixHit::None;
            };
            path.push(r);
            match entry {
                EntryRef::Leaf { sfx, .. } => {
                    let s: &[u8] = sfx.map_or(&[], |p| p);
                    if !s.starts_with(rest) {
                        return PrefixHit::None;
                    }
                    path.extend_from_slice(s);
                    return PrefixHit::Leaf { node: idx, radix: r };
                }
                EntryRef::Child(c) => {
                    let cp: &[u8] = &self.node(c).prefix;
                    path.extend_from_slice(cp);
                    if rest.starts_with(cp) {
                        cur += 1 + cp.len();
                        idx = c;
                    } else if cp.starts_with(rest) {
                        return PrefixHit::Node(c);
                    } else {
                        return PrefixHit::None;
                    }
                }
            }
        }
    }

    /// Parcours DFS du sous-arbre de `start` (dont la clé complète est dans
    /// `path`) : `f(clé, tag, valeur, échéance)` pour chaque valeur, expirées
    /// comprises.
    pub(crate) fn walk<F>(&self, start: u32, path: &mut Vec<u8>, mut f: F)
    where
        F: FnMut(&[u8], Tag, &ValUnion, Option<u64>),
    {
        // (nœud, longueur de la clé du parent, radix vers le nœud)
        let mut stack: Vec<(u32, u32, u8)> = vec![(start, path.len() as u32, 0)];
        let mut first = true;
        while let Some((idx, depth, radix)) = stack.pop() {
            path.truncate(depth as usize);
            let node = self.node(idx);
            if !std::mem::take(&mut first) {
                path.push(radix);
                path.extend_from_slice(&node.prefix);
            }
            if node.has_val() {
                f(path, node.tag, &node.val, node.exp_and_radix.exp());
            }
            let base = path.len();
            for (r, e) in node.iter() {
                match e {
                    EntryRef::Child(c) => stack.push((c, base as u32, r)),
                    EntryRef::Leaf { tag, val, ttl, sfx } => {
                        path.push(r);
                        if let Some(sfx) = sfx {
                            path.extend_from_slice(sfx);
                        }
                        f(path, tag, val, ttl);
                        path.truncate(base);
                    }
                }
            }
        }
    }

    /// Returns all live key-value pairs whose key starts with `prefix`
    /// (every entry if the prefix is empty).
    pub fn getn(&self, prefix: SharedByte) -> Vec<(SharedByte, Value)> {
        let mut results = Vec::new();
        let mut path = Vec::new();
        let now = self.now;
        let mut push = |k: &[u8], tag: Tag, val: &ValUnion, exp: Option<u64>| {
            if !expired(exp, now) {
                // SAFETY: valeur réelle, tag cohérent.
                results.push((SharedByte::from_slice(k), unsafe { value_from_raw_ref(tag, val) }));
            }
        };
        match self.find_prefix(&prefix, &mut path) {
            PrefixHit::None => {}
            PrefixHit::Node(idx) => self.walk(idx, &mut path, push),
            PrefixHit::Leaf { node, radix } => {
                if let Some(EntryRef::Leaf { tag, val, ttl, .. }) = self.node(node).get(radix) {
                    push(&path, tag, val, ttl);
                }
            }
        }
        results
    }

    /// Deletes all keys that start with the given prefix (everything if it
    /// is empty). Returns the number of deleted live (non-expired) values.
    pub fn deln(&mut self, prefix: &[u8]) -> usize {
        let (mut stack, count) = self.cut_prefix(prefix);
        count + self.free_subtrees(&mut stack, usize::MAX, true)
    }

    /// Détache tout ce qui commence par `prefix` (tout si vide) : les clés
    /// disparaissent tout de suite, la mémoire est rendue ensuite par
    /// [`free_detached`](Self::free_detached), par tranches.
    pub fn detach_prefix(&mut self, prefix: &[u8], into: &mut Detached) {
        if into.stack.is_empty() {
            into.flushes = self.flushes;
        }
        let (stack, count) = self.cut_prefix(prefix);
        into.stack.extend(stack);
        into.freed += count;
    }

    /// Détache toute la base : comme `detach_prefix(b"")`, mais le compte de
    /// clés tombe à 0 tout de suite (FLUSHDB : `DBSIZE` juste après vaut 0).
    pub fn detach_all(&mut self, into: &mut Detached) {
        let (stack, count) = self.cut_prefix(b"");
        into.stack.extend(stack);
        into.freed += count;
        into.uncounted = true;
        // Les sous-arbres détachés avant, pas encore libérés, sont comptés
        // dans `key_count` : remis à 0 avec le reste.
        self.key_count = 0;
        self.flushes += 1;
    }

    /// Libère au plus `budget` nœuds détachés. `true` quand tout est libéré.
    /// Pendant une save, ne fait rien (le sous-arbre détaché reste dans la
    /// vue T0, SAVE_DESIGN.md §2.F) et rend `false`.
    pub fn free_detached(&mut self, d: &mut Detached, budget: usize) -> bool {
        if d.stack.is_empty() {
            return true;
        }
        if self.is_saving() {
            return false;
        }
        let counted = !d.uncounted && d.flushes == self.flushes;
        d.freed += self.free_subtrees(&mut d.stack, budget, counted);
        d.stack.is_empty()
    }

    /// Détache tout ce qui commence par `prefix` : renvoie les sous-arbres à
    /// libérer (déjà inaccessibles) et le nombre de valeurs déjà retirées.
    /// L'arbre est normalisé avant de rendre la main.
    pub(crate) fn cut_prefix(&mut self, prefix: &[u8]) -> (Vec<u32>, usize) {
        let mut path = Vec::new();
        match self.find_prefix(prefix, &mut path) {
            PrefixHit::None => (Vec::new(), 0),
            PrefixHit::Leaf { node, radix } => {
                let loc = Loc { node, slot: Slot::Leaf(radix) };
                let live = !expired(self.loc_value(loc).2, self.now);
                self.remove_loc(loc);
                (Vec::new(), live as usize)
            }
            PrefixHit::Node(idx) if idx == self.root_idx => {
                let now = self.now;
                let root = self.root_idx;
                let mut freed = 0;
                let mut kids = Vec::new();
                let mut removed = 0;
                let root_node = self.node_mut(root);
                if root_node.has_val() {
                    let (_, exp) = take_self(root_node);
                    freed += !expired(exp, now) as usize;
                    removed += 1;
                }
                while let Some(r) = (root_node.len() > 0).then(|| root_node.radix_at(0)) {
                    match root_node.remove(r).expect("entrée") {
                        Entry::Child(c) => kids.push(c),
                        Entry::Leaf(l) => {
                            freed += !expired(l.ttl, now) as usize;
                            removed += 1;
                        }
                    }
                }
                self.key_count -= removed;
                self.map.untag(root);
                for &c in &kids {
                    self.mark_detached(c);
                }
                (kids, freed)
            }
            PrefixHit::Node(idx) => {
                let node = self.node(idx);
                let (p, r) = (node.parent_idx, node.exp_and_radix.parent_radix());
                let before = self.node(p).has_any_ttl();
                self.node_mut(p).remove(r);
                self.retag(p, before);
                self.normalize(p);
                self.mark_detached(idx);
                (vec![idx], 0)
            }
        }
    }

    /// Libère au plus `budget` nœuds des sous-arbres détachés de `stack` ;
    /// renvoie le nombre de valeurs vivantes libérées. `counted` : leurs clés
    /// sont encore dans `key_count` (faux après [`detach_all`](Self::detach_all)).
    pub(crate) fn free_subtrees(&mut self, stack: &mut Vec<u32>, budget: usize, counted: bool) -> usize {
        let now = self.now;
        let mut count = 0;
        let mut done = 0;
        while done < budget {
            let Some(idx) = stack.pop() else {
                break;
            };
            let node = self.node(idx);
            count += (node.has_val() && !expired(node.exp_and_radix.exp(), now)) as usize;
            let mut removed = node.has_val() as usize;
            let first = stack.len();
            for (_, e) in node.iter() {
                match e {
                    EntryRef::Child(c) => stack.push(c),
                    EntryRef::Leaf { ttl, .. } => {
                        count += !expired(ttl, now) as usize;
                        removed += 1;
                    }
                }
            }
            if counted {
                self.key_count -= removed;
            }
            // Leur parent disparaît : ils deviennent des racines détachées.
            for i in first..stack.len() {
                self.mark_detached(stack[i]);
            }
            // le Drop du nœud libère ses feuilles, sa page et sa self-val
            drop(self.remove_node(idx));
            done += 1;
        }
        count
    }

    // ─── Éviction TTL ───────────────────────────────────────────────────────

    /// Nœuds tagués examinés par tranche de l'évicteur (≈ un défaut de cache chacun, de
    /// l'ordre de 50 µs).
    pub const EVICT_SLICE: u32 = 1024;

    /// Une tranche de l'évicteur TTL : balayage **linéaire** du bitmap tagué (nœuds qui
    /// portent au moins un TTL) depuis le curseur, au plus `budget` nœuds ; chaque nœud
    /// visité perd ses valeurs expirées. Pas de tirage : chaque nœud tagué est vu une fois
    /// par tour, la mémoire d'une clé expirée revient en au plus un tour. Rien pendant une
    /// save (SAVE_DESIGN.md §2.C), le curseur reste où il est.
    pub fn evict_step(&mut self, budget: u32) -> EvictStep {
        let mut st = EvictStep::default();
        if self.is_saving() {
            return st;
        }
        let mut at = self.evict_cursor;
        while st.visited < budget {
            let Some(idx) = self.map.next_tagged(at) else {
                st.lap_done = true;
                at = 0;
                break;
            };
            at = idx + 1;
            st.visited += 1;
            let n = self.evict_node(idx);
            st.evicted += n;
            st.hit += (n > 0) as u32;
        }
        self.evict_cursor = at;
        st
    }

    /// Un tour complet de l'évicteur TTL depuis le début (tests, appels forcés) ; rend le
    /// nombre de valeurs retirées.
    pub fn evict_expired(&mut self) -> usize {
        if self.is_saving() {
            return 0;
        }
        self.evict_cursor = 0;
        self.evict_step(u32::MAX).evicted
    }

    /// Racine d'un sous-arbre détaché (FLUSHDB, `UNLINK prefix*`) en attente
    /// de libération : il garde ses tags TTL, l'évicteur ne doit pas y
    /// toucher (ses clés sont déjà retirées, ou comptées par le job).
    fn mark_detached(&mut self, idx: u32) {
        // SAFETY: `idx` désigne un nœud vivant (sous-arbre pas encore libéré).
        unsafe { self.map.get_unchecked_mut(idx) }.parent_idx = DETACHED;
    }

    /// Le nœud est dans l'arbre (pas dans un sous-arbre détaché).
    fn attached(&self, mut idx: u32) -> bool {
        while idx != self.root_idx {
            match self.node(idx).parent_idx {
                DETACHED => return false,
                p => idx = p,
            }
        }
        true
    }

    /// Retire les valeurs expirées du nœud `idx` ; renvoie leur nombre.
    fn evict_node(&mut self, idx: u32) -> usize {
        let now = self.now;
        let node = self.node(idx);
        let self_exp = node.has_val() && expired(node.exp_and_radix.exp(), now);
        let mut radixes: smallvec::SmallVec<[u8; 16]> = smallvec::SmallVec::new();
        for (r, e) in node.iter() {
            if let EntryRef::Leaf { ttl, .. } = e
                && expired(ttl, now)
            {
                radixes.push(r);
            }
        }
        // La remontée jusqu'à la racine seulement s'il y a quelque chose à retirer : la
        // plupart des nœuds visités n'ont rien d'expiré.
        if !self_exp && radixes.is_empty() || !self.attached(idx) {
            return 0;
        }
        let node = self.node(idx);
        let before = node.has_any_ttl();
        let node = self.node_mut(idx);
        if self_exp {
            drop(take_self(node));
        }
        for &r in &radixes {
            drop(node.remove(r));
        }
        self.retag(idx, before);
        self.normalize(idx);
        let n = self_exp as usize + radixes.len();
        self.key_count -= n;
        n
    }

    // ─── Éviction maxmemory (MEMORY_ARCH.md §10.2 quater) ───────────────────

    /// Clé du nœud `idx` (sa valeur propre) dans `out` : radix et préfixes depuis la racine,
    /// en remontant les parents (comme la descente de [`Self::walk`], à l'envers).
    fn key_to_node(&self, idx: u32, out: &mut Vec<u8>) {
        let mut chain: smallvec::SmallVec<[u32; 16]> = smallvec::SmallVec::new();
        let mut i = idx;
        while i != self.root_idx {
            chain.push(i);
            i = self.node(i).parent_idx;
        }
        out.clear();
        for &i in chain.iter().rev() {
            let n = self.node(i);
            out.push(n.exp_and_radix.parent_radix());
            out.extend_from_slice(&n.prefix);
        }
    }

    /// Une tranche de l'éviction `maxmemory` : balayage **linéaire** des slots depuis le
    /// curseur (bitmap tagué si `volatile`, d'occupation sinon), au plus `scan` nœuds ; dans
    /// chaque nœud visité, ses clés (avec TTL si `volatile`) sont évincées, jusqu'à
    /// `max_keys`. `by_ttl` (`volatile-ttl`) : sur la fenêtre des `scan` nœuds tagués, le nœud
    /// qui porte l'échéance la plus proche. Chaque clé est rendue à `on_key` avant d'être
    /// retirée (journal : `DEL`, l'éviction n'est pas déterministe). Rien pendant une save :
    /// évincer une clé intacte forcerait une copie (MEMORY_ARCH.md §10.5).
    pub fn evict_for_memory(
        &mut self,
        volatile: bool,
        by_ttl: bool,
        scan: u32,
        max_keys: usize,
        on_key: &mut impl FnMut(&[u8]),
    ) -> EvictStep {
        let mut st = EvictStep::default();
        if self.is_saving() || self.window.is_some() {
            return st;
        }
        let next = |a: &Self, at: u32| if volatile { a.map.next_tagged(at) } else { a.map.next_occupied(at) };
        let mut at = self.mem_cursor;
        if by_ttl {
            // fenêtre : le nœud à l'échéance la plus proche
            let mut best: Option<(u64, u32)> = None;
            while st.visited < scan {
                let Some(idx) = next(self, at) else {
                    st.lap_done = true;
                    at = 0;
                    break;
                };
                at = idx + 1;
                st.visited += 1;
                let node = self.node(idx);
                let own = node.has_val().then(|| node.exp_and_radix.exp()).flatten();
                let min = node
                    .iter()
                    .filter_map(|(_, e)| match e {
                        EntryRef::Leaf { ttl, .. } => ttl,
                        _ => None,
                    })
                    .chain(own)
                    .min();
                if let Some(t) = min
                    && best.is_none_or(|(b, _)| t < b)
                {
                    best = Some((t, idx));
                }
            }
            if let Some((t, idx)) = best {
                let n = self.evict_entries(idx, |ttl| ttl.is_some_and(|x| x <= t), on_key);
                st.evicted += n;
                st.hit += (n > 0) as u32;
            }
        } else {
            while st.visited < scan && st.evicted < max_keys {
                let Some(idx) = next(self, at) else {
                    st.lap_done = true;
                    at = 0;
                    break;
                };
                at = idx + 1;
                st.visited += 1;
                let n = self.evict_entries(idx, |ttl| !volatile || ttl.is_some(), on_key);
                st.evicted += n;
                st.hit += (n > 0) as u32;
            }
        }
        self.mem_cursor = at;
        st
    }

    /// Évince les clés du nœud `idx` dont l'échéance passe `pick`, chacune rendue d'abord à
    /// `on_key`. Même retrait que [`Self::evict_node`].
    fn evict_entries(&mut self, idx: u32, pick: impl Fn(Option<u64>) -> bool, on_key: &mut impl FnMut(&[u8])) -> usize {
        let node = self.node(idx);
        let own = node.has_val() && pick(node.exp_and_radix.exp());
        let mut leaves: smallvec::SmallVec<[(u8, smallvec::SmallVec<[u8; 16]>); 16]> = smallvec::SmallVec::new();
        for (r, e) in node.iter() {
            if let EntryRef::Leaf { ttl, sfx, .. } = e
                && pick(ttl)
            {
                leaves.push((r, sfx.map(|s| s.iter().copied().collect()).unwrap_or_default()));
            }
        }
        if !own && leaves.is_empty() || !self.attached(idx) {
            return 0;
        }
        let mut key = Vec::new();
        self.key_to_node(idx, &mut key);
        let base = key.len();
        if own {
            on_key(&key);
        }
        for (r, sfx) in &leaves {
            key.truncate(base);
            key.push(*r);
            key.extend_from_slice(sfx);
            on_key(&key);
        }
        let before = self.node(idx).has_any_ttl();
        let node = self.node_mut(idx);
        if own {
            drop(take_self(node));
        }
        for (r, _) in &leaves {
            drop(node.remove(*r));
        }
        self.retag(idx, before);
        self.normalize(idx);
        let n = own as usize + leaves.len();
        self.key_count -= n;
        n
    }

    // ─── Invariants (tests) ─────────────────────────────────────────────────

    /// Vérifie toute la structure : formes de nœuds, parents, nœuds
    /// nécessaires, tags TTL, aucun nœud orphelin. Renvoie le nombre de nœuds.
    #[cfg(test)]
    pub(crate) fn check(&self) -> usize {
        let mut seen = 0;
        let mut stack = vec![(self.root_idx, None::<(u32, u8)>)];
        while let Some((idx, parent)) = stack.pop() {
            seen += 1;
            let node = self.node(idx);
            node.check();
            assert_eq!(self.map.is_tagged(idx), node.has_any_ttl(), "tag TTL du nœud {idx}");
            match parent {
                None => assert!(node.prefix.is_empty(), "préfixe sur la racine"),
                Some((p, r)) => {
                    assert_eq!(node.parent_idx, p, "parent_idx de {idx}");
                    assert_eq!(node.exp_and_radix.parent_radix(), r, "radix parent de {idx}");
                    let items = node.len() + node.has_val() as usize;
                    assert!(items >= 2, "nœud {idx} non nécessaire : {} entrées, val {}", node.len(), node.has_val());
                }
            }
            for (r, e) in node.iter() {
                if let EntryRef::Child(c) = e {
                    stack.push((c, Some((idx, r))));
                }
            }
        }
        assert_eq!(seen, self.node_count(), "nœuds orphelins dans le slab");
        let mut keys = 0;
        let mut path = Vec::new();
        self.walk(self.root_idx, &mut path, |_, _, _, _| keys += 1);
        assert_eq!(self.key_count, keys, "compteur de clés");
        seen
    }
}

/// Bilan d'une tranche de l'évicteur TTL ([`OxidArt::evict_step`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct EvictStep {
    /// Valeurs retirées.
    pub evicted: usize,
    /// Nœuds tagués examinés.
    pub visited: u32,
    /// Nœuds où au moins une valeur était expirée.
    pub hit: u32,
    /// Le curseur a atteint la fin du bitmap (et repart de 0).
    pub lap_done: bool,
}

#[derive(Clone, Copy, Default)]
#[repr(transparent)]
pub struct ExpAndRadix {
    inner: u64,
}
impl ExpAndRadix {
    const NO_EXPIRACY: u64 = 0x00FFFFFFFFFFFFFF;
    const RADIX_MASK: u64 = !Self::NO_EXPIRACY;
    const EXP_LENGTH: u64 = 56;
    pub const fn no_expiracy(parent_radix: u8) -> Self {
        Self {
            inner: ((parent_radix as u64) << 56) | Self::NO_EXPIRACY,
        }
    }
    pub fn exp(self) -> Option<u64> {
        let exp = self.inner & Self::NO_EXPIRACY;
        if exp == Self::NO_EXPIRACY {
            None
        } else {
            Some(exp)
        }
    }
    pub fn parent_radix(self) -> u8 {
        ((self.inner & Self::RADIX_MASK) >> Self::EXP_LENGTH) as u8
    }
    pub fn does_expire(self) -> bool {
        self.inner & Self::NO_EXPIRACY != Self::NO_EXPIRACY
    }
    ///this function panic if the 8 upper bit of the ttl provide is not at 0 because the niche is needed to store radix
    pub fn set_exp(&mut self, exp: u64) {
        assert!(exp & Self::RADIX_MASK == 0);
        self.inner = self.inner & Self::RADIX_MASK | exp
    }
    pub fn set_no_expiracy(&mut self) {
        self.inner |= Self::NO_EXPIRACY;
    }
    ///this function panic if the 8 upper bit of the ttl provide is not at 0 because the niche is needed to store radix
    pub fn new(exp: u64, parent_radix: u8) -> Self {
        assert!(exp & Self::RADIX_MASK == 0);
        Self {
            inner: ((parent_radix as u64) << Self::EXP_LENGTH) | exp,
        }
    }
}

/// Sous-arbres détachés de l'arbre (UNLINK `prefix*`, FLUSHDB), en attente de
/// libération : plus accessibles depuis la racine, ils n'appartiennent qu'à
/// ce curseur, donc leurs idx restent valides d'une tranche à l'autre.
#[derive(Default, Debug)]
pub struct Detached {
    stack: Vec<u32>,
    /// Valeurs vivantes (non expirées) retirées jusqu'ici.
    pub freed: usize,
    /// Clés déjà retirées de `key_count` (FLUSHDB).
    uncounted: bool,
    /// Nombre de FLUSHDB de l'arbre au détachement : un FLUSHDB plus récent a
    /// remis `key_count` à 0, ces clés n'y sont plus.
    flushes: u64,
}

impl Detached {
    /// Plus rien à libérer.
    pub fn is_done(&self) -> bool {
        self.stack.is_empty()
    }
}
