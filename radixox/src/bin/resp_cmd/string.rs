use std::time::Duration;

use crate::Frame;
use oxidart::{OxidArt, TtlResult, counter::CounterError, value::Value};
use radixox_lib::shared_byte::SharedByte;

use crate::{SetCondition, parse_int, parse_set_options};

pub(crate) fn cmd_get(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'GET' command".into());
    }
    match art.get(&args[0]) {
        Some(val) => match val.as_bytes() {
            Some(b) => Frame::BulkString(b),
            None => Frame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".into(),
            ),
        },
        None => Frame::Null,
    }
}

pub(crate) fn cmd_set(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SET' command".into());
    }

    let key = args[0].clone();
    let val = Value::String(args[1].clone());
    let opts = match parse_set_options(&args[2..]) {
        Ok(o) => o,
        Err(e) => return e,
    };

    // Check condition before setting (skip lookup when Always)
    if !matches!(opts.condition, SetCondition::Always) {
        let key_exists = art.get(&key).is_some();
        match opts.condition {
            SetCondition::IfNotExists if key_exists => return Frame::Null,
            SetCondition::IfExists if !key_exists => return Frame::Null,
            _ => {}
        }
    }

    match opts.ttl {
        Some(duration) => art.set_ttl(key, duration, val),
        None => art.set(key, val),
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

pub(crate) fn cmd_incr(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'INCR' command".into());
    }
    match art.incr(args[0].clone()) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_decr(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'DECR' command".into());
    }
    match art.decr(args[0].clone()) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_incrby(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'INCRBY' command".into());
    }
    let delta: i64 = match parse_int(&args[1]) {
        Some(d) => d,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    match art.incrby(args[0].clone(), delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_decrby(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'DECRBY' command".into());
    }
    let delta: i64 = match parse_int(&args[1]) {
        Some(d) => d,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    match art.decrby(args[0].clone(), delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

pub(crate) fn cmd_del(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'DEL' command".into());
    }

    let mut count = 0i64;
    for key in args {
        if art.del(key).is_some() {
            count += 1;
        }
    }
    Frame::Integer(count)
}

pub(crate) fn cmd_ttl(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'TTL' command".into());
    }

    match art.get_ttl(args[0].clone()) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer(secs as i64),
    }
}

pub(crate) fn cmd_expire(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'EXPIRE' command".into());
    }

    let secs: u64 = match std::str::from_utf8(&args[1])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) => s,
        None => return Frame::Error("ERR value is not an integer".into()),
    };

    if art.expire(args[0].clone(), Duration::from_secs(secs)) {
        Frame::Integer(1)
    } else {
        Frame::Integer(0)
    }
}

pub(crate) fn cmd_persist(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'PERSIST' command".into());
    }

    if art.persist(args[0].clone()) {
        Frame::Integer(1)
    } else {
        Frame::Integer(0)
    }
}

pub(crate) fn cmd_exists(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'EXISTS' command".into());
    }

    let mut count = 0i64;
    for key in args {
        if art.get(key).is_some() {
            count += 1;
        }
    }
    Frame::Integer(count)
}

pub(crate) fn cmd_mget(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'MGET' command".into());
    }

    let results: Vec<Frame> = args
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

pub(crate) fn cmd_mset(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() || !args.len().is_multiple_of(2) {
        return Frame::Error("ERR wrong number of arguments for 'MSET' command".into());
    }

    for pair in args.chunks_exact(2) {
        art.set(pair[0].clone(), Value::String(pair[1].clone()));
    }

    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}

pub(crate) fn cmd_setnx(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SETNX' command".into());
    }

    let key = args[0].clone();
    if art.get(&key).is_some() {
        return Frame::Integer(0);
    }

    art.set(key, Value::String(args[1].clone()));
    Frame::Integer(1)
}

pub(crate) fn cmd_setex(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 3 {
        return Frame::Error("ERR wrong number of arguments for 'SETEX' command".into());
    }

    let key = args[0].clone();
    let secs: u64 = match parse_int(&args[1]) {
        Some(s) => s,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    let val = Value::String(args[2].clone());

    art.set_ttl(key, Duration::from_secs(secs), val);
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}

pub(crate) fn cmd_pttl(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'PTTL' command".into());
    }

    match art.get_ttl(args[0].clone()) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer((secs * 1000) as i64),
    }
}

pub(crate) fn cmd_pexpire(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'PEXPIRE' command".into());
    }

    let ms: u64 = match parse_int(&args[1]) {
        Some(m) => m,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };

    if art.expire(args[0].clone(), Duration::from_millis(ms)) {
        Frame::Integer(1)
    } else {
        Frame::Integer(0)
    }
}

pub(crate) fn cmd_echo(args: &[SharedByte]) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'ECHO' command".into());
    }
    Frame::BulkString(args[0].clone())
}

pub(crate) fn cmd_dbsize(art: &mut OxidArt) -> Frame {
    let count = art.getn(SharedByte::from_slice(b"")).len() as i64;
    Frame::Integer(count)
}

pub(crate) fn cmd_flushdb(art: &mut OxidArt) -> Frame {
    art.deln(b"");
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}

pub(crate) fn cmd_type(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'TYPE' command".into());
    }

    match art.get(&args[0]) {
        Some(val) => {
            Frame::SimpleString(SharedByte::from_slice(val.redis_type().as_str().as_bytes()))
        }
        None => Frame::SimpleString(SharedByte::from_slice(b"none")),
    }
}
