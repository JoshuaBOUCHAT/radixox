#[allow(unused_imports)]
use crate::prelude::*;

use oxidart::OxidArt;
use oxidart::error::TypeError;
use oxidart::scommand::SPOPResult;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_sadd(art: &mut OxidArt, key: &[u8], members: &[SharedByte]) -> Frame {
    match art.cmd_sadd(key, members, None) {
        Ok(count) => Frame::Integer(count as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_srem(art: &mut OxidArt, key: &[u8], members: &[SharedByte]) -> Frame {
    match art.cmd_srem(key, members) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_sismember(art: &mut OxidArt, key: &[u8], member: SharedByte) -> Frame {
    match art.cmd_sismember(key, member) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_scard(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_scard(key) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_smembers(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_smembers(key) {
        Ok(members) => Frame::Array(members.into_iter().map(Frame::BulkString).collect()),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_spop(art: &mut OxidArt, key: &[u8], count: Option<u64>) -> Frame {
    let count_str = count.map(|n| n.to_string());
    let count_bytes = count_str.as_deref().map(str::as_bytes);
    match art.cmd_spop(key, count_bytes) {
        Ok(SPOPResult::Single(opt)) => match opt {
            Some(val) => Frame::BulkString(val),
            None => Frame::Null,
        },
        Ok(SPOPResult::Multiple(vec)) => {
            Frame::Array(vec.into_iter().map(Frame::BulkString).collect())
        }
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(TypeError::NotAInt) => {
            Frame::Error("ERR value is not an integer or out of range".into())
        }
        Err(TypeError::NotAFloat | TypeError::OutOfMemory) => Frame::Error("ERR internal error".into()),
    }
}

// ── Lot 1 (COMMANDS_PLAN.md §3) ──────────────────────────────────────────────

fn wrongtype() -> Frame {
    Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
}

pub fn cmd_smismember(art: &mut OxidArt, key: &[u8], members: &[&[u8]]) -> Frame {
    match art.cmd_smismember(key, members) {
        Ok(v) => Frame::Array(v.into_iter().map(|b| Frame::Integer(b as i64)).collect()),
        Err(_) => wrongtype(),
    }
}

pub fn cmd_srandmember(art: &mut OxidArt, key: &[u8], count: Option<i64>) -> Frame {
    if count.is_some_and(|c| c < -radixox_lib::cmd::MAX_RAND_REPEAT) && art.type_of(key) == oxidart::value::RedisType::Set {
        return Frame::Error("ERR value is out of range".into());
    }
    match art.cmd_srandmember(key, count) {
        Err(_) => wrongtype(),
        Ok(v) if count.is_none() => v.into_iter().next().map_or(Frame::Null, Frame::BulkString),
        Ok(v) => Frame::Array(v.into_iter().map(Frame::BulkString).collect()),
    }
}

pub fn cmd_smove(art: &mut OxidArt, src: &[u8], dst: &[u8], member: SharedByte) -> Frame {
    match art.cmd_smove(src, dst, member) {
        Ok(moved) => Frame::Integer(moved as i64),
        Err(_) => wrongtype(),
    }
}
