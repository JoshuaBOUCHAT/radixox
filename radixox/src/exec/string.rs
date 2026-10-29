#[allow(unused_imports)]
use crate::prelude::*;

use core::time::Duration;

use radixox_lib::shared_frame::SharedFrame as Frame;
use oxidart::{Hint, OxidArt, PREFETCH_BATCH, TtlResult, counter::CounterError, value::Value};
use radixox_lib::cmd::{SetCondition, SetOpts};
use radixox_lib::shared_byte::SharedByte;

pub fn cmd_get(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.get(key) {
        Some(val) => match val.as_bytes() {
            Some(b) => Frame::BulkString(b),
            None => Frame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".into(),
            ),
        },
        None => Frame::Null,
    }
}

pub fn cmd_set(art: &mut OxidArt, key: &[u8], val: SharedByte, opts: SetOpts) -> Frame {
    let value = Value::String(val);

    let if_exists = match opts.condition {
        SetCondition::Always => None,
        SetCondition::IfExists => Some(true),
        SetCondition::IfNotExists => Some(false),
    };

    if !art.set_cond(key, opts.ttl, value, if_exists) {
        return Frame::Null;
    }

    Frame::SimpleString("OK")
}

// Avec l'indice du moteur AMAC (`execute_hinted`) : mêmes réponses que sans.

pub fn cmd_get_hinted(art: &mut OxidArt, key: &[u8], hint: Hint) -> Frame {
    match art.get_hinted(key, hint) {
        Some(val) => match val.as_bytes() {
            Some(b) => Frame::BulkString(b),
            None => Frame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".into(),
            ),
        },
        None => Frame::Null,
    }
}

pub fn cmd_set_hinted(art: &mut OxidArt, key: &[u8], val: SharedByte, opts: SetOpts, hint: Hint) -> Frame {
    let if_exists = match opts.condition {
        SetCondition::Always => None,
        SetCondition::IfExists => Some(true),
        SetCondition::IfNotExists => Some(false),
    };
    if !art.set_cond_hinted(key, opts.ttl, Value::String(val), if_exists, hint) {
        return Frame::Null;
    }
    Frame::SimpleString("OK")
}

pub fn cmd_setnx_hinted(art: &mut OxidArt, key: &[u8], val: SharedByte, hint: Hint) -> Frame {
    Frame::Integer(art.set_cond_hinted(key, None, Value::String(val), Some(false), hint) as i64)
}

pub fn cmd_incrby_hinted(art: &mut OxidArt, key: &[u8], delta: i64, hint: Hint) -> Frame {
    match art.incrby_hinted(key, delta, hint) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

/// TTL (`ms = false`) ou PTTL.
pub fn cmd_ttl_hinted(art: &mut OxidArt, key: &[u8], ms: bool, hint: Hint) -> Frame {
    match art.ttl_hinted(key, hint) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer((if ms { secs * 1000 } else { secs }) as i64),
    }
}

pub fn cmd_type_hinted(art: &mut OxidArt, key: &[u8], hint: Hint) -> Frame {
    match art.get_hinted(key, hint) {
        Some(val) => Frame::SimpleString(val.redis_type().as_str()),
        None => Frame::SimpleString("none"),
    }
}

pub fn cmd_setnx(art: &mut OxidArt, key: &[u8], val: SharedByte) -> Frame {
    Frame::Integer(art.set_cond(key, None, Value::String(val), Some(false)) as i64)
}

fn counter_err(e: CounterError) -> Frame {
    match e {
        CounterError::NotAnInteger => {
            Frame::Error("ERR value is not an integer or out of range".into())
        }
        CounterError::Overflow => Frame::Error("ERR increment or decrement would overflow".into()),
    }
}

pub fn cmd_incr(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.incr(SharedByte::from_slice(key)) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub fn cmd_decr(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.decr(SharedByte::from_slice(key)) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub fn cmd_incrby(art: &mut OxidArt, key: &[u8], delta: i64) -> Frame {
    match art.incrby(SharedByte::from_slice(key), delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub fn cmd_decrby(art: &mut OxidArt, key: &[u8], delta: i64) -> Frame {
    match art.decrby(SharedByte::from_slice(key), delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub fn cmd_del(art: &mut OxidArt, keys: &[&[u8]]) -> Frame {
    let mut count = 0i64;
    // descentes préchargées ensemble, puis suppressions dans un cache chaud
    for chunk in keys.chunks(PREFETCH_BATCH) {
        art.prefetch_keys(chunk);
        for key in chunk {
            if art.del(key) {
                count += 1;
            }
        }
    }
    Frame::Integer(count)
}

pub fn cmd_ttl(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.get_ttl(SharedByte::from_slice(key)) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer(secs as i64),
    }
}

pub fn cmd_pttl(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.get_ttl(SharedByte::from_slice(key)) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer((secs * 1000) as i64),
    }
}

pub fn cmd_expire(art: &mut OxidArt, key: &[u8], dur: Duration) -> Frame {
    if art.expire(SharedByte::from_slice(key), dur) { Frame::Integer(1) } else { Frame::Integer(0) }
}

pub fn cmd_persist(art: &mut OxidArt, key: &[u8]) -> Frame {
    if art.persist(SharedByte::from_slice(key)) { Frame::Integer(1) } else { Frame::Integer(0) }
}

pub fn cmd_exists(art: &mut OxidArt, keys: &[&[u8]]) -> Frame {
    let mut expired = Vec::new();
    let n = art.exists_count(keys, &mut expired);
    if !expired.is_empty() {
        art.purge_expired(keys, &expired);
    }
    Frame::Integer(n as i64)
}

pub fn cmd_mset(art: &mut OxidArt, pairs: &[(&[u8], SharedByte)]) -> Frame {
    let mut keys: [&[u8]; PREFETCH_BATCH] = [&[]; PREFETCH_BATCH];
    for chunk in pairs.chunks(PREFETCH_BATCH) {
        for (slot, (k, _)) in keys.iter_mut().zip(chunk) {
            *slot = k;
        }
        art.prefetch_keys(&keys[..chunk.len()]);
        for (k, v) in chunk {
            art.set(SharedByte::from_slice(k), Value::String(v.clone()));
        }
    }
    Frame::SimpleString("OK")
}

pub fn cmd_type(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.get(key) {
        Some(val) => {
            Frame::SimpleString(val.redis_type().as_str())
        }
        None => Frame::SimpleString("none"),
    }
}

pub fn cmd_dbsize(art: &mut OxidArt) -> Frame {
    let count = art.len() as i64;
    Frame::Integer(count)
}
