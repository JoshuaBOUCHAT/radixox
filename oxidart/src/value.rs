use std::{
    collections::{BTreeSet, VecDeque},
    mem::{ManuallyDrop, MaybeUninit},
};

use hislab::HiSlab;
use radixox_lib::shared_byte::SharedByte;

use crate::{hcommand::InnerHCommand, zcommand::InnerZCommand};

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

static mut HASH_SLAB: MaybeUninit<HiSlab<InnerHCommand>> = MaybeUninit::uninit();
static mut SET_SLAB: MaybeUninit<HiSlab<BTreeSet<SharedByte>>> = MaybeUninit::uninit();
static mut ZSET_SLAB: MaybeUninit<HiSlab<InnerZCommand>> = MaybeUninit::uninit();
static mut LIST_SLAB: MaybeUninit<HiSlab<VecDeque<SharedByte>>> = MaybeUninit::uninit();

static SLAB_INIT: std::sync::Once = std::sync::Once::new();

/// Initialize value slabs. Safe to call multiple times — only the first call takes effect.
pub(crate) fn init_slabs() {
    SLAB_INIT.call_once(|| unsafe {
        #[allow(static_mut_refs)]
        HASH_SLAB.write(HiSlab::new(0, 100_000_000).expect("hash slab alloc"));
        #[allow(static_mut_refs)]
        SET_SLAB.write(HiSlab::new(0, 100_000_000).expect("set slab alloc"));
        #[allow(static_mut_refs)]
        ZSET_SLAB.write(HiSlab::new(0, 100_000_000).expect("zset slab alloc"));
        #[allow(static_mut_refs)]
        LIST_SLAB.write(HiSlab::new(0, 100_000_000).expect("list slab alloc"));
    });
}

#[inline]
fn hash_slab() -> &'static mut HiSlab<InnerHCommand> {
    #[allow(static_mut_refs)]
    unsafe {
        HASH_SLAB.assume_init_mut()
    }
}
#[inline]
fn set_slab() -> &'static mut HiSlab<BTreeSet<SharedByte>> {
    #[allow(static_mut_refs)]
    unsafe {
        SET_SLAB.assume_init_mut()
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

pub(crate) fn alloc_hash(val: InnerHCommand) -> u32 {
    hash_slab().insert(val)
}
pub(crate) fn free_hash(idx: u32) {
    hash_slab().remove(idx);
}
pub(crate) fn hash_ref(idx: u32) -> &'static InnerHCommand {
    hash_slab().get(idx).unwrap()
}
pub(crate) fn hash_mut(idx: u32) -> &'static mut InnerHCommand {
    hash_slab().get_mut(idx).unwrap()
}

pub(crate) fn alloc_set(val: BTreeSet<SharedByte>) -> u32 {
    set_slab().insert(val)
}
pub(crate) fn free_set(idx: u32) {
    set_slab().remove(idx);
}
pub(crate) fn set_ref(idx: u32) -> &'static BTreeSet<SharedByte> {
    set_slab().get(idx).unwrap()
}
pub(crate) fn set_mut(idx: u32) -> &'static mut BTreeSet<SharedByte> {
    set_slab().get_mut(idx).unwrap()
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

pub(crate) fn alloc_list(val: VecDeque<SharedByte>) -> u32 {
    list_slab().insert(val)
}
pub(crate) fn free_list(idx: u32) {
    list_slab().remove(idx);
}
pub(crate) fn list_ref(idx: u32) -> &'static VecDeque<SharedByte> {
    list_slab().get(idx).unwrap()
}
pub(crate) fn list_mut(idx: u32) -> &'static mut VecDeque<SharedByte> {
    list_slab().get_mut(idx).unwrap()
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
        Value::Hash(h) => (Tag::Hash, ValUnion { idx: alloc_hash(h) }),
        Value::Set(s) => (Tag::Set, ValUnion { idx: alloc_set(s) }),
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
            Tag::Hash => Value::Hash(hash_ref(val.idx).clone()),
            Tag::Set => Value::Set(set_ref(val.idx).clone()),
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
            Tag::Hash => Value::Hash(hash_slab().remove(val.idx).expect("hash slot")),
            Tag::Set => Value::Set(set_slab().remove(val.idx).expect("set slot")),
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
            Tag::Hash => free_hash(val.idx),
            Tag::Set => free_set(val.idx),
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

    pub fn as_hash(&self) -> Result<&InnerHCommand, RedisType> {
        match *self.tag {
            Tag::Hash => Ok(unsafe { hash_ref(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_hash_mut(&mut self) -> Result<&'static mut InnerHCommand, RedisType> {
        match *self.tag {
            Tag::Hash => Ok(unsafe { hash_mut(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_set(&self) -> Result<&BTreeSet<SharedByte>, RedisType> {
        match *self.tag {
            Tag::Set => Ok(unsafe { set_ref(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_set_mut(&mut self) -> Result<&'static mut BTreeSet<SharedByte>, RedisType> {
        match *self.tag {
            Tag::Set => Ok(unsafe { set_mut(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_zset(&self) -> Result<&InnerZCommand, RedisType> {
        match *self.tag {
            Tag::ZSet => Ok(unsafe { zset_ref(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_zset_mut(&mut self) -> Result<&'static mut InnerZCommand, RedisType> {
        match *self.tag {
            Tag::ZSet => Ok(unsafe { zset_mut(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_list(&self) -> Result<&VecDeque<SharedByte>, RedisType> {
        match *self.tag {
            Tag::List => Ok(unsafe { list_ref(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }

    pub fn as_list_mut(&mut self) -> Result<&mut VecDeque<SharedByte>, RedisType> {
        match *self.tag {
            Tag::List => Ok(unsafe { list_mut(self.val.idx) }),
            _ => Err(self.tag.redis_type()),
        }
    }
}

// ─── Public Value enum ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(SharedByte),
    Int(i64),
    Hash(InnerHCommand),
    List(VecDeque<SharedByte>),
    Set(BTreeSet<SharedByte>),
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

    pub fn as_hash(&self) -> Result<&InnerHCommand, RedisType> {
        match self {
            Value::Hash(h) => Ok(h),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_hash_mut(&mut self) -> Result<&mut InnerHCommand, RedisType> {
        match self {
            Value::Hash(h) => Ok(h),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_set(&self) -> Result<&BTreeSet<SharedByte>, RedisType> {
        match self {
            Value::Set(s) => Ok(s),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_set_mut(&mut self) -> Result<&mut BTreeSet<SharedByte>, RedisType> {
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
