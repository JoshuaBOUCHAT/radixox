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


pub mod compact_path;
pub mod compress;
pub mod digest;
pub mod error;
pub mod new_node;

pub mod hcommand;
pub mod krange;
mod prefetch_batch;
pub use prefetch_batch::PREFETCH_BATCH;
mod load;
pub(crate) mod node;
pub mod save;
pub mod scommand;
pub(crate) mod small;
pub mod value;
pub mod zcommand;
pub mod zset_inner;


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

use hislab::TaggedHiSlab;
use radixox_lib::shared_byte::SharedByte;
use rand::rngs::ThreadRng;

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

#[inline]
pub(crate) fn expired(exp: Option<u64>, now: u64) -> bool {
    exp.is_some_and(|e| e < now)
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
        valart().drain_deferred(rest)
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
    pub(crate) fn live_loc(&mut self, key: &[u8]) -> Option<Loc> {
        let (loc, _, _, exp) = self.find_value(key)?;
        if expired(exp, self.now) {
            if !self.is_saving() {
                drop(self.remove_loc(loc));
            }
            return None;
        }
        Some(loc)
    }

    /// Valeur vivante à `key`, par référence (aucune capture de save).
    pub(crate) fn get_ref(&mut self, key: &[u8]) -> Option<NodeValRef<'_>> {
        let (loc, _, _, exp) = self.find_value(key)?;
        if expired(exp, self.now) {
            if !self.is_saving() {
                drop(self.remove_loc(loc));
            }
            return None;
        }
        let (tag, val, _) = self.loc_value(loc);
        Some(NodeValRef { tag, val })
    }

    /// Accès en écriture à la valeur vivante de `key` (passe par la barrière).
    pub(crate) fn value_mut(&mut self, key: &[u8]) -> Option<NodeValMut<'_>> {
        let loc = self.live_loc(key)?;
        let node = self.node_mut(loc.node);
        match loc.slot {
            Slot::SelfVal => Some(NodeValMut { tag: &mut node.tag, val: &mut node.val }),
            Slot::Leaf(r) => {
                let (tag, val) = node.leaf_val_mut(r).expect("feuille");
                Some(NodeValMut { tag, val })
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
            if !self.is_saving() {
                drop(self.remove_loc(loc));
            }
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
            Some(exp) if exp < self.now => TtlResult::KeyNotExist,
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
    pub(crate) fn set_raw(&mut self, key: &[u8], raw: RawVal, exp: Option<u64>, cond: Option<bool>) -> bool {
        let now = self.now;
        let ok = |exists: bool| cond.is_none_or(|want| want == exists);
        let mut idx = self.root_idx;
        let mut cur = 0;
        loop {
            if cur == key.len() {
                let node = self.node(idx);
                if !ok(node.has_val() && !expired(node.exp_and_radix.exp(), now)) {
                    return false;
                }
                let before = node.has_any_ttl();
                let node = self.node_mut(idx);
                if node.has_val() {
                    drop(take_self(node));
                } else {
                    self.key_count += 1;
                }
                let node = self.node_mut(idx);
                put_self(node, raw, exp);
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
                Step::SameLeaf { .. } => {
                    let before = self.node(idx).has_any_ttl();
                    let (tag, val) = raw.into_parts();
                    let updated = self.node_mut(idx).update_leaf(r, tag, val, exp);
                    debug_assert!(updated);
                    self.retag(idx, before);
                }
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

    /// Promotion : la feuille `r` de `idx` et la nouvelle clé (reste `rest`)
    /// divergent → un nœud de préfixe commun les porte.
    fn split_leaf(&mut self, idx: u32, r: u8, rest: &[u8], raw: RawVal, exp: Option<u64>) {
        let before = self.node(idx).has_any_ttl();
        let Some(Entry::Leaf(old)) = self.node_mut(idx).remove(r) else {
            unreachable!("split_leaf sur une entrée qui n'est pas une feuille");
        };
        let (tag, val, old_exp, old_sfx) = old.into_parts();
        let s: &[u8] = old_sfx.as_deref().unwrap_or(&[]);
        let common = lcp(s, rest);
        let mut n = Node::new(CompactPath::from_slice(&s[..common]), idx, r);
        place(&mut n, s, common, RawVal::new(tag, val), old_exp);
        place(&mut n, rest, common, raw, exp);
        let n_idx = self.insert_node(n);
        self.node_mut(idx).insert(r, Entry::Child(n_idx));
        self.retag(idx, before);
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
        self.node_mut(c).prefix = CompactPath::from_slice(&cp[common + 1..]);
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

    fn set_loc_exp(&mut self, loc: Loc, exp: Option<u64>) {
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
        drop(self.remove_loc(loc));
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
        self.normalize(loc.node);
        raw
    }

    /// Rétablit l'invariant « au moins deux choses » sur `x` après une
    /// suppression, en remontant par les `parent_idx` si le parent perd une
    /// entrée.
    pub(crate) fn normalize(&mut self, mut x: u32) {
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

    const MAX_SAMPLE: usize = 20;
    const SAMPLE_SIZE: usize = 20;
    const THRESHOLD: usize = Self::SAMPLE_SIZE / 4; // 25% of 20

    /// Evicts expired entries using Redis-style probabilistic sampling.
    ///
    /// Un tirage choisit un nœud tagué (qui porte au moins un TTL) et retire
    /// toutes ses valeurs expirées. On recommence tant qu'au moins 25 % des
    /// nœuds tirés avaient une valeur expirée. Returns the number of evicted
    /// values.
    pub fn evict_expired(&mut self) -> usize {
        if self.is_saving() {
            return 0;
        }
        let mut rng = rand::thread_rng();
        let mut total_evicted = 0;
        for _ in 0..Self::MAX_SAMPLE {
            let (evicted, hit_nodes, sampled) = self.evict_cycle(&mut rng);
            total_evicted += evicted;
            if sampled < Self::SAMPLE_SIZE || hit_nodes < Self::THRESHOLD {
                break;
            }
        }
        total_evicted
    }

    fn evict_cycle(&mut self, rng: &mut ThreadRng) -> (usize, usize, usize) {
        let (mut evicted, mut hit_nodes, mut sampled) = (0, 0, 0);
        for _ in 0..Self::SAMPLE_SIZE {
            let Some((idx, _)) = self.map.random_tagged(rng) else {
                break;
            };
            sampled += 1;
            let n = self.evict_node(idx);
            evicted += n;
            hit_nodes += (n > 0) as usize;
        }
        (evicted, hit_nodes, sampled)
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
        if !self.attached(idx) {
            return 0;
        }
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
        if !self_exp && radixes.is_empty() {
            return 0;
        }
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
