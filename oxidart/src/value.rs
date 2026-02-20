use bytes::Bytes;

use std::collections::{BTreeSet, VecDeque};

use crate::{hcommand::InnerHCommand, zcommand::InnerZCommand};

// ---------------------------------------------------------------------------
// Value enum — replaces Bytes as the stored type in OxidArt
// ---------------------------------------------------------------------------

/// Redis-compatible value types stored in the radix tree.
///
/// No `None` variant — the tree uses `Option<Value>` which is free
/// thanks to niche optimization on the enum discriminant.
/// All variants ≤ 32 bytes → no Box, no indirection.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Raw string data.
    String(Bytes),
    /// Integer — INCR/DECR operate directly, zero parsing.
    /// Transparent: TYPE returns "string", GET formats on the fly.
    Int(i64),
    /// Hash: field → value pairs. BTreeMap for predictable O(log n) and ordered iteration.
    Hash(InnerHCommand),
    /// List: ordered collection, push/pop both ends.
    List(VecDeque<Bytes>),
    /// Set: unique members. BTreeSet for ordered iteration.
    Set(BTreeSet<Bytes>),
    /// Sorted set: members with f64 scores, sorted by (score, member).
    /// Uses InnerZCommand with Small/Large dynamic dispatch.
    /// InnerZCommand is ≤32 bytes (Small=Vec 24B, Large=Box 8B) — no outer Box needed.
    ZSet(InnerZCommand),
}

// ---------------------------------------------------------------------------
// Redis type system
// ---------------------------------------------------------------------------

/// Redis type family — for TYPE command and WRONGTYPE checks.
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

// ---------------------------------------------------------------------------
// Value — core
// ---------------------------------------------------------------------------

impl Value {
    /// Redis type. String and Int are both "string".
    #[inline]
    pub fn redis_type(&self) -> RedisType {
        match self {
            Value::String(_) | Value::Int(_) => RedisType::String,
            Value::Hash(_) => RedisType::Hash,
            Value::List(_) => RedisType::List,
            Value::Set(_) => RedisType::Set,
            Value::ZSet(_) => RedisType::ZSet,
        }
    }
}

// ---------------------------------------------------------------------------
// Value — string family (String + Int)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum IntError {
    NotAnInteger,
    Overflow,
}

impl Value {
    /// Get raw bytes. Int formats on the fly.
    pub fn as_bytes(&self) -> Option<Bytes> {
        match self {
            Value::String(b) => Some(b.clone()),
            Value::Int(n) => Some(Bytes::from(n.to_string())),
            _ => None,
        }
    }

    /// Try to read as i64. Works on Int (direct) and String (parse).
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

    /// Increment by delta. Converts String → Int on first call.
    pub fn incr(&mut self, delta: i64) -> Result<i64, IntError> {
        let current = match self {
            Value::Int(n) => *n,
            Value::String(b) => {
                let s = std::str::from_utf8(b).map_err(|_| IntError::NotAnInteger)?;
                s.parse::<i64>().map_err(|_| IntError::NotAnInteger)?
            }
            _ => return Err(IntError::NotAnInteger),
        };
        let new_val = current.checked_add(delta).ok_or(IntError::Overflow)?;
        *self = Value::Int(new_val);
        Ok(new_val)
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(string: &'static str) -> Self {
        Self::String(Bytes::from_static(string.as_bytes()))
    }
}

// ---------------------------------------------------------------------------
// Value — Hash
// ---------------------------------------------------------------------------

impl Value {
    pub fn as_hash_mut(&mut self) -> Result<&mut InnerHCommand, RedisType> {
        match self {
            Value::Hash(h) => Ok(h),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_hash(&self) -> Result<&InnerHCommand, RedisType> {
        match self {
            Value::Hash(h) => Ok(h),
            other => Err(other.redis_type()),
        }
    }
}

// ---------------------------------------------------------------------------
// Value — List
// ---------------------------------------------------------------------------

impl Value {
    pub fn as_list_mut(&mut self) -> Result<&mut VecDeque<Bytes>, RedisType> {
        match self {
            Value::List(l) => Ok(l),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_list(&self) -> Result<&VecDeque<Bytes>, RedisType> {
        match self {
            Value::List(l) => Ok(l),
            other => Err(other.redis_type()),
        }
    }
}

// ---------------------------------------------------------------------------
// Value — Set
// ---------------------------------------------------------------------------

impl Value {
    pub fn as_set_mut(&mut self) -> Result<&mut BTreeSet<Bytes>, RedisType> {
        match self {
            Value::Set(s) => Ok(s),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_set(&self) -> Result<&BTreeSet<Bytes>, RedisType> {
        match self {
            Value::Set(s) => Ok(s),
            other => Err(other.redis_type()),
        }
    }
}

// ---------------------------------------------------------------------------
// Value — ZSet
// ---------------------------------------------------------------------------

impl Value {
    pub fn as_zset_mut(&mut self) -> Result<&mut InnerZCommand, RedisType> {
        match self {
            Value::ZSet(z) => Ok(z),
            other => Err(other.redis_type()),
        }
    }

    pub fn as_zset(&self) -> Result<&InnerZCommand, RedisType> {
        match self {
            Value::ZSet(z) => Ok(z),
            other => Err(other.redis_type()),
        }
    }
}

// ---------------------------------------------------------------------------
// ZSet helpers
// ---------------------------------------------------------------------------
// With BTreeSet, insert/remove/find are all O(log n) via the set itself.
// No manual helpers needed - BTreeSet handles ordering automatically.
