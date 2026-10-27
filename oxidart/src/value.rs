use std::{
    collections::VecDeque,
    mem::{ManuallyDrop, MaybeUninit},
};

use hislab::HiSlab;
use radixox_lib::shared_byte::SharedByte;
use valart::ValTrees;

use crate::list::List;
use crate::small;
use crate::zset::ZSet;

// ─── Tag ─────────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    None = 0,
    Int = 1,
    Bytes = 2,
    Hash = 3,
    Set = 4,
    ZSet = 5,
    List = 6,
    /// Hash d'au plus `SMALL_MAX` champs : `ValUnion::ptr` vers un bloc
    /// `crate::small` de paires (champ, valeur).
    HashSmall = 7,
    /// Set d'au plus `SMALL_MAX` membres : `ValUnion::ptr` vers un bloc
    /// `crate::small`.
    SetSmall = 8,
}

impl Tag {
    pub(crate) fn redis_type(self) -> RedisType {
        match self {
            Tag::None => RedisType::None,
            Tag::Int | Tag::Bytes => RedisType::String,
            Tag::Hash | Tag::HashSmall => RedisType::Hash,
            Tag::List => RedisType::List,
            Tag::Set | Tag::SetSmall => RedisType::Set,
            Tag::ZSet => RedisType::ZSet,
        }
    }
}

// ─── ValUnion ─────────────────────────────────────────────────────────────────

pub(crate) union ValUnion {
    pub(crate) integer: i64,
    pub(crate) bytes: ManuallyDrop<SharedByte>,
    pub(crate) idx: u32,
    /// Bloc d'un petit conteneur (`Tag::HashSmall`, `Tag::SetSmall`).
    pub(crate) ptr: *mut u8,
}

// ─── Static slabs ─────────────────────────────────────────────────────────────

/// Hash et Set : arbres valart. Le nœud oxidart stocke l'idx de la racine
/// dans `ValUnion::idx`.
static mut VALART: MaybeUninit<ValTrees> = MaybeUninit::uninit();
static mut ZSET_SLAB: MaybeUninit<HiSlab<ZSet>> = MaybeUninit::uninit();
static mut LIST_SLAB: MaybeUninit<HiSlab<List>> = MaybeUninit::uninit();

static SLAB_INIT: std::sync::Once = std::sync::Once::new();

/// Initialize value slabs. Safe to call multiple times — only the first call takes effect.
pub(crate) fn init_slabs() {
    SLAB_INIT.call_once(|| unsafe {
        #[allow(static_mut_refs)]
        VALART.write(ValTrees::new());
        #[allow(static_mut_refs)]
        ZSET_SLAB.write(HiSlab::new(0, 100_000_000).expect("zset slab alloc"));
        #[allow(static_mut_refs)]
        LIST_SLAB.write(HiSlab::new(0, 100_000_000).expect("list slab alloc"));
    });
}

/// Arbres valart (Hash, Set). Même contrat que les slabs : mono-thread, et
/// l'appelant ne garde jamais deux références vivantes en même temps.
#[inline]
pub(crate) fn valart() -> &'static mut ValTrees {
    #[allow(static_mut_refs)]
    unsafe {
        VALART.assume_init_mut()
    }
}
#[inline]
fn zset_slab() -> &'static mut HiSlab<ZSet> {
    #[allow(static_mut_refs)]
    unsafe {
        ZSET_SLAB.assume_init_mut()
    }
}
#[inline]
fn list_slab() -> &'static mut HiSlab<List> {
    #[allow(static_mut_refs)]
    unsafe {
        LIST_SLAB.assume_init_mut()
    }
}

fn hash_from_pairs(pairs: Vec<(SharedByte, SharedByte)>) -> u32 {
    let trees = valart();
    let root = trees.hash_create();
    for (field, val) in pairs {
        trees.hash_set(root, field, val);
    }
    root
}

/// Hash neuf : petit bloc jusqu'à `SMALL_MAX` champs distincts, valart au-delà.
/// Un champ répété garde sa dernière valeur.
pub(crate) fn hash_into_raw(pairs: Vec<(SharedByte, SharedByte)>) -> (Tag, ValUnion) {
    let mut p = small::alloc::<2>(1);
    let mut rest = pairs.into_iter();
    for (f, v) in rest.by_ref() {
        unsafe {
            if let Some(i) = small::find::<2>(p, &f) {
                drop(small::remove_at::<2>(p, i));
            }
            if let Err([f, v]) = small::push::<2>(&mut p, [f, v]) {
                // trop de champs : tout part en valart
                let root = hash_from_pairs(
                    small::into_entries::<2>(p)
                        .into_iter()
                        .map(|[f, v]| (f, v))
                        .chain(std::iter::once((f, v)))
                        .chain(rest)
                        .collect(),
                );
                return (Tag::Hash, ValUnion { idx: root });
            }
        }
    }
    (Tag::HashSmall, ValUnion { ptr: p })
}

/// Set neuf : petit bloc jusqu'à `SMALL_MAX` membres distincts, valart au-delà.
pub(crate) fn set_into_raw(members: Vec<SharedByte>) -> (Tag, ValUnion) {
    let mut p = small::alloc::<1>(1);
    let mut rest = members.into_iter();
    for m in rest.by_ref() {
        unsafe {
            if small::find::<1>(p, &m).is_some() {
                continue;
            }
            if let Err([m]) = small::push::<1>(&mut p, [m]) {
                let root = set_from_members(
                    small::into_entries::<1>(p)
                        .into_iter()
                        .map(|[m]| m)
                        .chain(std::iter::once(m))
                        .chain(rest)
                        .collect(),
                );
                return (Tag::Set, ValUnion { idx: root });
            }
        }
    }
    (Tag::SetSmall, ValUnion { ptr: p })
}

fn hash_to_pairs(root: u32) -> Vec<(SharedByte, SharedByte)> {
    let trees = valart();
    let mut pairs = Vec::with_capacity(trees.hash_len(root) as usize);
    trees.hash_for_each(root, |k, v| pairs.push((k.clone(), v.clone())));
    pairs
}

fn set_from_members(members: Vec<SharedByte>) -> u32 {
    let trees = valart();
    let root = trees.set_create();
    for member in members {
        trees.set_add(root, member);
    }
    root
}

fn set_to_members(root: u32) -> Vec<SharedByte> {
    let trees = valart();
    let mut members = Vec::with_capacity(trees.set_card(root) as usize);
    trees.set_for_each(root, |m| members.push(m.clone()));
    members
}

pub(crate) fn alloc_zset(val: ZSet) -> u32 {
    zset_slab().insert(val)
}
/// Libère le ZSet `idx`. Pendant une save, différé (la vue T0 peut encore le lire) ;
/// sinon l'en-tête part tout de suite et le contenu d'un grand ZSet est rendu par tranches
/// (`zset::lazy`).
pub(crate) fn free_zset(idx: u32) {
    if crate::zset::save::defer_zset(idx) {
        return;
    }
    if let Some(mut z) = zset_slab().remove(idx) {
        crate::zset::lazy::release(&mut z);
    }
}
pub(crate) fn zset_ref(idx: u32) -> &'static ZSet {
    zset_slab().get(idx).unwrap()
}
/// Point de passage de toute modification d'un ZSet (en-tête ou petite forme) : barrière
/// de save.
pub(crate) fn zset_mut(idx: u32) -> &'static mut ZSet {
    crate::zset::save::before_header_write(idx, || crate::zset::save::ZImage::of(zset_ref(idx)));
    zset_slab().get_mut(idx).unwrap()
}

/// Bitmap de présence de la slab des ZSet (vue T0 de la save).
pub(crate) fn zset_slab_occupancy(buf: &mut Vec<u64>) {
    zset_slab().copy_occupancy(buf);
}

#[cfg(test)]
/// Parcourt tous les ZSet du slab (tests).
pub(crate) fn for_each_zset(f: impl FnMut(u32, &ZSet)) {
    zset_slab().for_each_occupied(f);
}

/// Précharge la case de l'en-tête du ZSet `idx` (chaînes AMAC).
#[inline]
pub(crate) fn prefetch_zset(idx: u32) {
    radixox_lib::prefetch::prefetch_l1(zset_slab().slot_ptr(idx) as *const u8);
}

/// Slab des ZSet (GC : blocs des petites formes).
#[inline]
pub(crate) fn zset_slab_mut() -> &'static mut HiSlab<ZSet> {
    zset_slab()
}

pub(crate) fn alloc_list(val: VecDeque<SharedByte>) -> u32 {
    list_slab().insert(List::from_values(val))
}
pub(crate) fn alloc_list_raw(l: List) -> u32 {
    list_slab().insert(l)
}
/// Libère la List `idx` : tout de suite si elle est petite, sinon par tranches
/// (`list::lazy`) ; pendant une save, après (la vue T0 peut encore la lire).
pub(crate) fn free_list(idx: u32) {
    crate::list::save::before_header_write(idx, || *list_ref(idx));
    if let Some(l) = list_slab().remove(idx)
        && let Some(l) = crate::list::save::defer_list(l)
    {
        crate::list::lazy::release(l);
    }
}
/// Bitmap de présence de la slab des en-têtes de List (vue T0 de la save).
pub(crate) fn list_slab_occupancy(buf: &mut Vec<u64>) {
    list_slab().copy_occupancy(buf);
}
pub(crate) fn list_ref(idx: u32) -> &'static List {
    list_slab().get(idx).unwrap()
}
/// Point de passage de toute modification d'un en-tête de List.
pub(crate) fn list_mut(idx: u32) -> &'static mut List {
    crate::list::save::before_header_write(idx, || *list_ref(idx));
    list_slab().get_mut(idx).unwrap()
}

/// Précharge la case de l'en-tête de la List `idx` (chaînes AMAC).
#[inline]
pub(crate) fn prefetch_list(idx: u32) {
    radixox_lib::prefetch::prefetch_l1(list_slab().slot_ptr(idx) as *const u8);
}

// ─── Tag+ValUnion ↔ Value conversions ────────────────────────────────────────

/// Converts an owned `Value` into `(Tag, ValUnion)`.
/// Hash/Set/ZSet/List are allocated into their slab; the returned idx owns the slot.
pub(crate) fn value_into_raw(val: Value) -> (Tag, ValUnion) {
    match val {
        Value::Int(n) => (Tag::Int, ValUnion { integer: n }),
        Value::String(b) => (
            Tag::Bytes,
            ValUnion {
                bytes: ManuallyDrop::new(b),
            },
        ),
        Value::Hash(h) => hash_into_raw(h),
        Value::Set(s) => set_into_raw(s),
        Value::ZSet(z) => (Tag::ZSet, ValUnion { idx: alloc_zset(z) }),
        Value::List(l) => (Tag::List, ValUnion { idx: alloc_list(l) }),
    }
}

/// Constructs an owned `Value` from a shared `(Tag, &ValUnion)`, cloning heap data.
///
/// # Safety
/// Tag must not be None.
pub(crate) unsafe fn value_from_raw_ref(tag: Tag, val: &ValUnion) -> Value {
    unsafe {
        match tag {
            Tag::None => panic!("value_from_raw_ref: Tag::None"),
            Tag::Int => Value::Int(val.integer),
            Tag::Bytes => Value::String((*val.bytes).clone()),
            Tag::Hash => Value::Hash(hash_to_pairs(val.idx)),
            Tag::Set => Value::Set(set_to_members(val.idx)),
            Tag::HashSmall => Value::Hash(
                small::entries::<2>(val.ptr)
                    .iter()
                    .map(|[f, v]| (f.clone(), v.clone()))
                    .collect(),
            ),
            Tag::SetSmall => Value::Set(small::entries::<1>(val.ptr).iter().map(|[m]| m.clone()).collect()),
            Tag::ZSet => Value::ZSet(zset_ref(val.idx).clone()),
            Tag::List => Value::List(list_ref(val.idx).iter().cloned().collect()),
        }
    }
}

/// Consumes `(Tag, ValUnion)` and returns the owned `Value`, removing any slab slot.
///
/// # Safety
/// Tag must not be None. The ValUnion must not be used after this call.
pub(crate) unsafe fn value_take_raw(tag: Tag, val: ValUnion) -> Value {
    unsafe {
        match tag {
            Tag::None => panic!("value_take_raw: Tag::None"),
            Tag::Int => Value::Int(val.integer),
            Tag::Bytes => Value::String(ManuallyDrop::into_inner(val.bytes)),
            Tag::Hash => {
                let pairs = hash_to_pairs(val.idx);
                valart().hash_drop(val.idx);
                Value::Hash(pairs)
            }
            Tag::Set => {
                let members = set_to_members(val.idx);
                valart().set_drop(val.idx);
                Value::Set(members)
            }
            Tag::HashSmall => Value::Hash(small::into_entries::<2>(val.ptr).into_iter().map(|[f, v]| (f, v)).collect()),
            Tag::SetSmall => Value::Set(small::into_entries::<1>(val.ptr).into_iter().map(|[m]| m).collect()),
            Tag::ZSet => {
                // pendant une save, le ZSet reste pour la vue T0 : copie
                let z = zset_ref(val.idx).clone();
                free_zset(val.idx);
                Value::ZSet(z)
            }
            Tag::List => {
                crate::list::save::before_header_write(val.idx, || *list_ref(val.idx));
                let mut l = list_slab().remove(val.idx).expect("list slot");
                let mut out = VecDeque::with_capacity(l.len() as usize);
                while let Some(v) = l.pop_front() {
                    out.push_back(v);
                }
                Value::List(out)
            }
        }
    }
}

/// Drops the resources owned by `(tag, val)` without constructing a Value.
///
/// # Safety
/// Tag+val must be consistent. After this call val is uninitialised for heap types.
pub(crate) unsafe fn drop_raw(tag: Tag, val: &mut ValUnion) {
    unsafe {
        match tag {
            Tag::Bytes => ManuallyDrop::drop(&mut val.bytes),
            Tag::Hash => valart().hash_drop(val.idx),
            Tag::Set => valart().set_drop(val.idx),
            Tag::HashSmall => small::free::<2>(val.ptr),
            Tag::SetSmall => small::free::<1>(val.ptr),
            Tag::ZSet => free_zset(val.idx),
            Tag::List => free_list(val.idx),
            Tag::Int | Tag::None => {}
        }
    }
}

// ─── RawVal ───────────────────────────────────────────────────────────────────

/// `(Tag, ValUnion)` possédé hors de l'arbre, libéré s'il n'est pas posé.
pub(crate) struct RawVal {
    pub(crate) tag: Tag,
    pub(crate) val: ValUnion,
}

impl RawVal {
    #[inline]
    pub(crate) fn new(tag: Tag, val: ValUnion) -> Self {
        Self { tag, val }
    }

    #[inline]
    pub(crate) fn from_value(val: Value) -> Self {
        let (tag, val) = value_into_raw(val);
        Self { tag, val }
    }

    /// Les octets d'une valeur `Bytes`.
    #[inline]
    pub(crate) fn as_bytes(&self) -> Option<&[u8]> {
        // SAFETY: tag et val construits ensemble.
        (self.tag == Tag::Bytes).then(|| unsafe { self.val.bytes.as_slice() })
    }

    /// Propriété transférée à l'appelant (rien n'est libéré).
    #[inline]
    pub(crate) fn into_parts(self) -> (Tag, ValUnion) {
        let me = ManuallyDrop::new(self);
        // SAFETY: `me` n'est jamais droppé.
        (me.tag, unsafe { std::ptr::read(&me.val) })
    }

    pub(crate) fn into_value(self) -> Value {
        let (tag, val) = self.into_parts();
        // SAFETY: tag et val cohérents, tag != None (valeur réelle).
        unsafe { value_take_raw(tag, val) }
    }
}

impl Drop for RawVal {
    fn drop(&mut self) {
        // SAFETY: tag et val construits ensemble.
        unsafe { drop_raw(self.tag, &mut self.val) };
    }
}

// ─── NodeValMut ───────────────────────────────────────────────────────────────

/// Mutable accessor into a node's value fields.
/// Keeps borrows tied to the node's lifetime — no free-floating `'static` leaks.
pub(crate) struct NodeValMut<'a> {
    pub(crate) tag: &'a mut Tag,
    pub(crate) val: &'a mut ValUnion,
}

impl<'a> NodeValMut<'a> {
    pub fn incr(&mut self, delta: i64) -> Result<i64, IntError> {
        let current = unsafe {
            match *self.tag {
                Tag::Int => self.val.integer,
                Tag::Bytes => {
                    let s = std::str::from_utf8((*self.val.bytes).as_slice())
                        .map_err(|_| IntError::NotAnInteger)?;
                    s.parse::<i64>().map_err(|_| IntError::NotAnInteger)?
                }
                _ => return Err(IntError::NotAnInteger),
            }
        };
        let new_val = current.checked_add(delta).ok_or(IntError::Overflow)?;
        if *self.tag == Tag::Bytes {
            unsafe { ManuallyDrop::drop(&mut self.val.bytes) };
        }
        *self.tag = Tag::Int;
        self.val.integer = new_val;
        Ok(new_val)
    }
}

// ─── NodeValRef ───────────────────────────────────────────────────────────────

/// Accès en lecture à la valeur d'un nœud. Les commandes qui ne modifient pas
/// le nœud oxidart (lectures, et écritures qui ne touchent que le conteneur :
/// valart, ZSet, List) passent par ici, pour ne pas déclencher de capture de
/// save (SAVE_DESIGN.md §2.C).
#[derive(Clone, Copy)]
pub(crate) struct NodeValRef<'a> {
    pub(crate) tag: Tag,
    pub(crate) val: &'a ValUnion,
}

impl<'a> NodeValRef<'a> {
    /// Racine valart du Hash.
    pub fn hash_root(self) -> Result<u32, RedisType> {
        match self.tag {
            Tag::Hash => Ok(unsafe { self.val.idx }),
            _ => Err(self.tag.redis_type()),
        }
    }

    /// Racine valart du Set.
    pub fn set_root(self) -> Result<u32, RedisType> {
        match self.tag {
            Tag::Set => Ok(unsafe { self.val.idx }),
            _ => Err(self.tag.redis_type()),
        }
    }

    /// Idx de l'en-tête de la List dans son slab.
    pub fn list_idx(self) -> Result<u32, RedisType> {
        match self.tag {
            Tag::List => Ok(unsafe { self.val.idx }),
            _ => Err(self.tag.redis_type()),
        }
    }

    /// Idx du ZSet dans son slab.
    pub fn zset_idx(self) -> Result<u32, RedisType> {
        match self.tag {
            Tag::ZSet => Ok(unsafe { self.val.idx }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_zset(self) -> Result<&'static ZSet, RedisType> {
        self.zset_idx().map(zset_ref)
    }
}

// ─── Public Value enum ────────────────────────────────────────────────────────

/// Valeur détachée de l'arbre. Pour Hash et Set, c'est une copie des entrées
/// (ordre sans signification) : les commandes travaillent directement sur les
/// arbres valart, ces variantes ne servent qu'à l'API générique `get`/`set`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(SharedByte),
    Int(i64),
    Hash(Vec<(SharedByte, SharedByte)>),
    List(VecDeque<SharedByte>),
    Set(Vec<SharedByte>),
    ZSet(ZSet),
}

impl Value {
    pub fn redis_type(&self) -> RedisType {
        match self {
            Value::String(_) | Value::Int(_) => RedisType::String,
            Value::Hash(_) => RedisType::Hash,
            Value::List(_) => RedisType::List,
            Value::Set(_) => RedisType::Set,
            Value::ZSet(_) => RedisType::ZSet,
        }
    }

    pub fn as_bytes(&self) -> Option<SharedByte> {
        match self {
            Value::String(b) => Some(b.clone()),
            Value::Int(n) => Some(SharedByte::from_slice(n.to_string().as_bytes())),
            _ => None,
        }
    }

    pub fn to_int(&self) -> Result<i64, IntError> {
        match self {
            Value::Int(n) => Ok(*n),
            Value::String(b) => {
                let s = std::str::from_utf8(b).map_err(|_| IntError::NotAnInteger)?;
                s.parse::<i64>().map_err(|_| IntError::NotAnInteger)
            }
            _ => Err(IntError::NotAnInteger),
        }
    }

    pub fn as_hash(&self) -> Result<&[(SharedByte, SharedByte)], RedisType> {
        match self {
            Value::Hash(h) => Ok(h),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_set(&self) -> Result<&[SharedByte], RedisType> {
        match self {
            Value::Set(s) => Ok(s),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_zset(&self) -> Result<&ZSet, RedisType> {
        match self {
            Value::ZSet(z) => Ok(z),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_zset_mut(&mut self) -> Result<&mut ZSet, RedisType> {
        match self {
            Value::ZSet(z) => Ok(z),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_list(&self) -> Result<&VecDeque<SharedByte>, RedisType> {
        match self {
            Value::List(l) => Ok(l),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_list_mut(&mut self) -> Result<&mut VecDeque<SharedByte>, RedisType> {
        match self {
            Value::List(l) => Ok(l),
            other => Err(other.redis_type()),
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(string: &str) -> Self {
        Self::String(SharedByte::from_slice(string.as_bytes()))
    }
}

// ─── RedisType ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedisType {
    None,
    String,
    Hash,
    List,
    Set,
    ZSet,
}

impl RedisType {
    pub fn as_str(self) -> &'static str {
        match self {
            RedisType::None => "none",
            RedisType::String => "string",
            RedisType::Hash => "hash",
            RedisType::List => "list",
            RedisType::Set => "set",
            RedisType::ZSet => "zset",
        }
    }
}

// ─── IntError ─────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum IntError {
    NotAnInteger,
    Overflow,
}
