use std::{
    collections::VecDeque,
    mem::{ManuallyDrop, MaybeUninit},
};

use hislab::HiSlab;
use radixox_lib::shared_byte::SharedByte;
use valart::ValTrees;

use crate::zcommand::InnerZCommand;

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
}

impl Tag {
    pub(crate) fn redis_type(self) -> RedisType {
        match self {
            Tag::None => RedisType::None,
            Tag::Int | Tag::Bytes => RedisType::String,
            Tag::Hash => RedisType::Hash,
            Tag::List => RedisType::List,
            Tag::Set => RedisType::Set,
            Tag::ZSet => RedisType::ZSet,
        }
    }
}

// ─── ValUnion ─────────────────────────────────────────────────────────────────

pub(crate) union ValUnion {
    pub(crate) integer: i64,
    pub(crate) bytes: ManuallyDrop<SharedByte>,
    pub(crate) idx: u32,
}

// ─── Static slabs ─────────────────────────────────────────────────────────────

/// Hash et Set : arbres valart. Le nœud oxidart stocke l'idx de la racine
/// dans `ValUnion::idx`.
static mut VALART: MaybeUninit<ValTrees> = MaybeUninit::uninit();
static mut ZSET_SLAB: MaybeUninit<HiSlab<InnerZCommand>> = MaybeUninit::uninit();
static mut LIST_SLAB: MaybeUninit<HiSlab<VecDeque<SharedByte>>> = MaybeUninit::uninit();

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
fn zset_slab() -> &'static mut HiSlab<InnerZCommand> {
    #[allow(static_mut_refs)]
    unsafe {
        ZSET_SLAB.assume_init_mut()
    }
}
#[inline]
fn list_slab() -> &'static mut HiSlab<VecDeque<SharedByte>> {
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

fn hash_to_pairs(root: u32) -> Vec<(SharedByte, SharedByte)> {
    let trees = valart();
    let mut pairs = Vec::with_capacity(trees.hash_count(root) as usize);
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
    let mut members = Vec::with_capacity(trees.set_count(root) as usize);
    trees.set_for_each(root, |m| members.push(m.clone()));
    members
}

pub(crate) fn alloc_zset(val: InnerZCommand) -> u32 {
    zset_slab().insert(val)
}
pub(crate) fn free_zset(idx: u32) {
    zset_slab().remove(idx);
}
pub(crate) fn zset_ref(idx: u32) -> &'static InnerZCommand {
    zset_slab().get(idx).unwrap()
}
pub(crate) fn zset_mut(idx: u32) -> &'static mut InnerZCommand {
    zset_slab().get_mut(idx).unwrap()
}

/// Parcourt tous les ZSet du slab (section ZSet de la save).
pub(crate) fn for_each_zset(f: impl FnMut(u32, &InnerZCommand)) {
    zset_slab().for_each_occupied(f);
}

/// Parcourt toutes les List du slab (section List de la save).
pub(crate) fn for_each_list(f: impl FnMut(u32, &VecDeque<SharedByte>)) {
    list_slab().for_each_occupied(f);
}

pub(crate) fn alloc_list(val: VecDeque<SharedByte>) -> u32 {
    list_slab().insert(val)
}
pub(crate) fn free_list(idx: u32) {
    list_slab().remove(idx);
}
pub(crate) fn list_ref(idx: u32) -> &'static VecDeque<SharedByte> {
    list_slab().get(idx).unwrap()
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
        Value::Hash(h) => (Tag::Hash, ValUnion { idx: hash_from_pairs(h) }),
        Value::Set(s) => (Tag::Set, ValUnion { idx: set_from_members(s) }),
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
            Tag::ZSet => Value::ZSet(zset_ref(val.idx).clone()),
            Tag::List => Value::List(list_ref(val.idx).clone()),
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
            Tag::ZSet => Value::ZSet(zset_slab().remove(val.idx).expect("zset slot")),
            Tag::List => Value::List(list_slab().remove(val.idx).expect("list slot")),
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
            Tag::ZSet => free_zset(val.idx),
            Tag::List => free_list(val.idx),
            Tag::Int | Tag::None => {}
        }
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

    /// Idx du ZSet dans son slab.
    pub fn zset_idx(self) -> Result<u32, RedisType> {
        match self.tag {
            Tag::ZSet => Ok(unsafe { self.val.idx }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_zset(self) -> Result<&'static InnerZCommand, RedisType> {
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
    ZSet(InnerZCommand),
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

    pub fn as_zset(&self) -> Result<&InnerZCommand, RedisType> {
        match self {
            Value::ZSet(z) => Ok(z),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_zset_mut(&mut self) -> Result<&mut InnerZCommand, RedisType> {
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
