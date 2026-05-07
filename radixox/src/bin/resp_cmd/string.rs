use std::time::Duration;

use crate::Frame;
use oxidart::{OxidArt, TtlResult, counter::CounterError, value::Value};
use radixox_lib::cmd::{SetCondition, SetOpts};
use radixox_lib::shared_byte::SharedByte;

pub(crate) fn cmd_get(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.get(&key) {
        Some(val) => match val.as_bytes() {
            Some(b) => Frame::BulkString(b),
            None => Frame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".into(),
            ),
        },
        None => Frame::Null,
    }
}

pub(crate) fn cmd_set(art: &mut OxidArt, key: SharedByte, val: SharedByte, opts: SetOpts) -> Frame {
    let value = Value::String(val);

    if !matches!(opts.condition, SetCondition::Always) {
        let key_exists = art.get(&key).is_some();
        match opts.condition {
            SetCondition::IfNotExists if key_exists => return Frame::Null,
            SetCondition::IfExists if !key_exists => return Frame::Null,
            _ => {}
        }
    }

    match opts.ttl {
        Some(duration) => art.set_ttl(key, duration, value),
        None => art.set(key, value),
    }

    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}

fn counter_err(e: CounterError) -> Frame {
    match e {
        CounterError::NotAnInteger => {
            Frame::Error("ERR value is not an integer or out of range".into())
        }
        CounterError::Overflow => Frame::Error("ERR increment or decrement would overflow".into()),
    }
}

pub(crate) fn cmd_incr(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.incr(key) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_decr(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.decr(key) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_incrby(art: &mut OxidArt, key: SharedByte, delta: i64) -> Frame {
    match art.incrby(key, delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_decrby(art: &mut OxidArt, key: SharedByte, delta: i64) -> Frame {
    match art.decrby(key, delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_del(art: &mut OxidArt, keys: &[SharedByte]) -> Frame {
    let mut count = 0i64;
    for key in keys {
        if art.del(key).is_some() {
            count += 1;
        }
    }
    Frame::Integer(count)
}

pub(crate) fn cmd_ttl(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.get_ttl(key) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer(secs as i64),
    }
}

pub(crate) fn cmd_pttl(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.get_ttl(key) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer((secs * 1000) as i64),
    }
}

pub(crate) fn cmd_expire(art: &mut OxidArt, key: SharedByte, dur: Duration) -> Frame {
    if art.expire(key, dur) { Frame::Integer(1) } else { Frame::Integer(0) }
}

pub(crate) fn cmd_persist(art: &mut OxidArt, key: SharedByte) -> Frame {
    if art.persist(key) { Frame::Integer(1) } else { Frame::Integer(0) }
}

pub(crate) fn cmd_exists(art: &mut OxidArt, keys: &[SharedByte]) -> Frame {
    let mut count = 0i64;
    for key in keys {
        if art.get(key).is_some() {
            count += 1;
        }
    }
    Frame::Integer(count)
}

pub(crate) fn cmd_mget(art: &mut OxidArt, keys: &[SharedByte]) -> Frame {
    let results: Vec<Frame> = keys
        .iter()
        .map(|key| match art.get(key) {
            Some(val) => match val.as_bytes() {
                Some(b) => Frame::BulkString(b),
                None => Frame::Null,
            },
            None => Frame::Null,
        })
        .collect();
    Frame::Array(results)
}

pub(crate) fn cmd_mset(art: &mut OxidArt, pairs: &[(SharedByte, SharedByte)]) -> Frame {
    for (k, v) in pairs {
        art.set(k.clone(), Value::String(v.clone()));
    }
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}

pub(crate) fn cmd_type(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.get(&key) {
        Some(val) => {
            Frame::SimpleString(SharedByte::from_slice(val.redis_type().as_str().as_bytes()))
        }
        None => Frame::SimpleString(SharedByte::from_slice(b"none")),
    }
}

pub(crate) fn cmd_dbsize(art: &mut OxidArt) -> Frame {
    let count = art.getn(SharedByte::from_slice(b"")).len() as i64;
    Frame::Integer(count)
}

pub(crate) fn cmd_flushdb(art: &mut OxidArt) -> Frame {
    art.deln(b"");
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}
