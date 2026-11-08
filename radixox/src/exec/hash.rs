#[allow(unused_imports)]
use crate::prelude::*;

use oxidart::OxidArt;
use oxidart::error::TypeError;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_hset(art: &mut OxidArt, key: &[u8], fields: &[(SharedByte, SharedByte)]) -> Frame {
    match art.cmd_hset(key, fields, None) {
        Ok(added) => Frame::Integer(added as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_hget(art: &mut OxidArt, key: &[u8], field: &[u8]) -> Frame {
    match art.cmd_hget(key, field) {
        Ok(Some(val)) => Frame::BulkString(val),
        Ok(None) => Frame::Null,
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hgetall(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_hgetall(key) {
        Ok(fields) => Frame::Array(fields.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hdel(art: &mut OxidArt, key: &[u8], fields: &[SharedByte]) -> Frame {
    match art.cmd_hdel(key, fields) {
        Ok(count) => Frame::Integer(count as i64),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hexists(art: &mut OxidArt, key: &[u8], field: &[u8]) -> Frame {
    match art.cmd_hexists(key, field) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hlen(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_hlen(key) {
        Ok(len) => Frame::Integer(len as i64),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hkeys(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_hkeys(key) {
        Ok(keys) => Frame::Array(keys.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hvals(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_hvals(key) {
        Ok(vals) => Frame::Array(vals.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hmget(art: &mut OxidArt, key: &[u8], fields: &[SharedByte]) -> Frame {
    match art.cmd_hmget(key, fields) {
        Ok(values) => Frame::Array(
            values
                .into_iter()
                .map(|v| match v {
                    Some(b) => Frame::BulkString(b),
                    None => Frame::Null,
                })
                .collect(),
        ),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hincrby(art: &mut OxidArt, key: &[u8], field: SharedByte, delta: i64) -> Frame {
    match art.cmd_hincrby(key, field, delta) {
        Ok(new_val) => Frame::Integer(new_val),
        Err(TypeError::NotAInt) => Frame::Error("ERR hash value is not an integer".into()),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(TypeError::NotAFloat | TypeError::OutOfMemory) => Frame::Error("ERR internal error".into()),
    }
}

// ── Lot 1 (COMMANDS_PLAN.md §3) ──────────────────────────────────────────────

fn wrongtype() -> Frame {
    Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
}

pub fn cmd_hsetnx(art: &mut OxidArt, key: &[u8], field: SharedByte, val: SharedByte) -> Frame {
    match art.cmd_hsetnx(key, field, val) {
        Ok(set) => Frame::Integer(set as i64),
        Err(_) => wrongtype(),
    }
}

pub fn cmd_hstrlen(art: &mut OxidArt, key: &[u8], field: &[u8]) -> Frame {
    match art.cmd_hstrlen(key, field) {
        Ok(n) => Frame::Integer(n as i64),
        Err(_) => wrongtype(),
    }
}

pub fn cmd_hincrbyfloat(art: &mut OxidArt, key: &[u8], field: SharedByte, incr: &[u8]) -> Frame {
    use super::float::{FloatErr, add};
    // ordre de Redis : l'incrément (valide, fini), puis le type, puis la valeur du champ
    match add(Some(b"0"), incr) {
        Err(FloatErr::Incr) => return Frame::Error("ERR value is not a valid float".into()),
        Err(FloatErr::NanInf) => return Frame::Error("ERR value is NaN or Infinity".into()),
        _ => {}
    }
    let cur = match art.cmd_hget(key, &field) {
        Err(_) => return wrongtype(),
        Ok(cur) => cur,
    };
    match add(cur.as_deref(), incr) {
        Ok(r) => {
            let r = SharedByte::from_slice(r);
            if art.cmd_hset(key, &[(field, r.clone())], None).is_err() {
                return wrongtype();
            }
            Frame::BulkString(r)
        }
        Err(FloatErr::Value) => Frame::Error("ERR hash value is not a float".into()),
        Err(FloatErr::Incr) => Frame::Error("ERR value is not a valid float".into()),
        Err(FloatErr::NanInf) => Frame::Error("ERR increment would produce NaN or Infinity".into()),
    }
}

pub fn cmd_hrandfield(art: &mut OxidArt, key: &[u8], count: Option<i64>, with_values: bool) -> Frame {
    if count.is_some_and(|c| c < -radixox_lib::cmd::MAX_RAND_REPEAT) && art.type_of(key) == oxidart::value::RedisType::Hash {
        return Frame::Error("ERR value is out of range".into());
    }
    let picked = match art.cmd_hrandfield(key, count) {
        Ok(p) => p,
        Err(_) => return wrongtype(),
    };
    if count.is_none() {
        return picked.into_iter().next().map_or(Frame::Null, |(f, _)| Frame::BulkString(f));
    }
    let mut out = Vec::with_capacity(picked.len() * (1 + with_values as usize));
    for (f, v) in picked {
        out.push(Frame::BulkString(f));
        if with_values {
            out.push(Frame::BulkString(v));
        }
    }
    Frame::Array(out)
}
