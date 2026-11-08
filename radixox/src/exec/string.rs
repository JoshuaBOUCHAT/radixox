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

// ── Lot 1 (COMMANDS_PLAN.md §3) : strings ───────────────────────────────────

/// Plus longue string (`proto-max-bulk-len` de Redis).
const MAX_STR: usize = radixox_lib::cmd::MAX_BULK_LEN;

fn wrongtype() -> Frame {
    Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
}

fn too_big() -> Frame {
    Frame::Error("ERR string exceeds maximum allowed size (proto-max-bulk-len)".into())
}

fn invalid_expire(name: &str) -> Frame {
    Frame::Error(format!("ERR invalid expire time in '{name}' command"))
}

pub fn cmd_append(art: &mut OxidArt, key: &[u8], val: SharedByte) -> Frame {
    match art.get_str_exp(key) {
        Err(_) => wrongtype(),
        Ok(None) => {
            let n = val.len();
            art.set_at(key, Value::String(val), None);
            Frame::Integer(n as i64)
        }
        Ok(Some((old, exp))) => {
            let n = old.len() + val.len();
            if n > MAX_STR {
                return too_big();
            }
            if val.is_empty() {
                return Frame::Integer(n as i64);
            }
            let mut v = Vec::with_capacity(n);
            v.extend_from_slice(&old);
            v.extend_from_slice(&val);
            art.set_at(key, Value::String(SharedByte::from_slice(v)), exp);
            Frame::Integer(n as i64)
        }
    }
}

pub fn cmd_strlen(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.get_str_exp(key) {
        Err(_) => wrongtype(),
        Ok(v) => Frame::Integer(v.map_or(0, |(s, _)| s.len()) as i64),
    }
}

pub fn cmd_getrange(art: &mut OxidArt, key: &[u8], start: i64, end: i64) -> Frame {
    let s = match art.get_str_exp(key) {
        Err(_) => return wrongtype(),
        Ok(None) => return Frame::BulkString(SharedByte::from_slice(b"")),
        Ok(Some((s, _))) => s,
    };
    // bornes de Redis (getrangeCommand)
    let len = s.len() as i64;
    if (start < 0 && end < 0 && start > end) || len == 0 {
        return Frame::BulkString(SharedByte::from_slice(b""));
    }
    let start = if start < 0 { (len + start).max(0) } else { start };
    let end = if end < 0 { (len + end).max(0) } else { end.min(len - 1) };
    if start > end {
        return Frame::BulkString(SharedByte::from_slice(b""));
    }
    Frame::BulkString(SharedByte::from_slice(&s[start as usize..=end as usize]))
}

pub fn cmd_setrange(art: &mut OxidArt, key: &[u8], off: i64, val: SharedByte) -> Frame {
    if off < 0 {
        return Frame::Error("ERR offset is out of range".into());
    }
    let (old, exp) = match art.get_str_exp(key) {
        Err(_) => return wrongtype(),
        Ok(None) if val.is_empty() => return Frame::Integer(0),
        Ok(None) => (SharedByte::from_slice(b""), None),
        Ok(Some((old, exp))) => {
            if val.is_empty() {
                return Frame::Integer(old.len() as i64);
            }
            (old, exp)
        }
    };
    let off = off as u64;
    if off + val.len() as u64 > MAX_STR as u64 {
        return too_big();
    }
    let off = off as usize;
    let n = old.len().max(off + val.len());
    let mut v = Vec::with_capacity(n);
    v.extend_from_slice(&old);
    v.resize(n, 0);
    v[off..off + val.len()].copy_from_slice(&val);
    art.set_at(key, Value::String(SharedByte::from_slice(v)), exp);
    Frame::Integer(n as i64)
}

pub fn cmd_getset(art: &mut OxidArt, key: &[u8], val: SharedByte) -> Frame {
    match art.get_str_exp(key) {
        Err(_) => wrongtype(),
        Ok(old) => {
            // GETSET retire le TTL (comme SET)
            art.set_at(key, Value::String(val), None);
            old.map_or(Frame::Null, |(s, _)| Frame::BulkString(s))
        }
    }
}

pub fn cmd_getdel(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.get_str_exp(key) {
        Err(_) => wrongtype(),
        Ok(None) => Frame::Null,
        Ok(Some((s, _))) => {
            art.del(key);
            Frame::BulkString(s)
        }
    }
}

pub fn cmd_getex(art: &mut OxidArt, key: &[u8], op: Option<radixox_lib::cmd::GetExOp>) -> Frame {
    use radixox_lib::cmd::GetExOp;
    // échéance vérifiée avant la clé, comme Redis
    let at = match op {
        Some(GetExOp::At(a)) => match a.resolve(art.now) {
            Some(r) => Some(r),
            None => return invalid_expire("getex"),
        },
        _ => None,
    };
    let s = match art.get_str_exp(key) {
        Err(_) => return wrongtype(),
        Ok(None) => return Frame::Null,
        Ok(Some((s, _))) => s,
    };
    match (op, at) {
        (Some(GetExOp::Persist), _) => {
            art.persist(SharedByte::from_slice(key));
        }
        (_, Some((at, passed))) => {
            art.expire_at(key, at, passed, radixox_lib::cmd::ExpireCond::Always);
        }
        _ => {}
    }
    Frame::BulkString(s)
}

pub fn cmd_incrbyfloat(art: &mut OxidArt, key: &[u8], incr: &[u8]) -> Frame {
    use super::float::{FloatErr, add};
    let (cur, exp) = match art.get_str_exp(key) {
        Err(_) => return wrongtype(),
        Ok(None) => (None, None),
        Ok(Some((s, exp))) => (Some(s), exp),
    };
    match add(cur.as_deref(), incr) {
        Ok(r) => {
            let r = SharedByte::from_slice(r);
            // garde le TTL
            art.set_at(key, Value::String(r.clone()), exp);
            Frame::BulkString(r)
        }
        Err(FloatErr::Value | FloatErr::Incr) => Frame::Error("ERR value is not a valid float".into()),
        Err(FloatErr::NanInf) => Frame::Error("ERR increment would produce NaN or Infinity".into()),
    }
}

pub fn cmd_msetnx(art: &mut OxidArt, pairs: &[(&[u8], SharedByte)]) -> Frame {
    if pairs.iter().any(|(k, _)| art.type_of(k) != oxidart::value::RedisType::None) {
        return Frame::Integer(0);
    }
    cmd_mset(art, pairs);
    Frame::Integer(1)
}

// ── Lot 1 : clés et TTL ──────────────────────────────────────────────────────

pub fn cmd_expire_opt(
    art: &mut OxidArt,
    key: &[u8],
    at: radixox_lib::cmd::ExpireArg,
    cond: radixox_lib::cmd::ExpireCond,
    name: &str,
) -> Frame {
    match at.resolve(art.now) {
        None => invalid_expire(name),
        Some((at, passed)) => Frame::Integer(art.expire_at(key, at, passed, cond) as i64),
    }
}

pub fn cmd_expiretime(art: &mut OxidArt, key: &[u8], ms: bool) -> Frame {
    Frame::Integer(match art.expire_time(key) {
        Err(()) => -2,
        Ok(None) => -1,
        Ok(Some(t)) if ms => t as i64 * 1000,
        Ok(Some(t)) => t as i64,
    })
}

pub fn cmd_touch(art: &mut OxidArt, keys: &[&[u8]]) -> Frame {
    Frame::Integer(keys.iter().filter(|k| art.touch_key(k)).count() as i64)
}

pub fn cmd_randomkey(art: &mut OxidArt) -> Frame {
    match art.random_key() {
        Some(k) => Frame::BulkString(SharedByte::from_slice(k)),
        None => Frame::Null,
    }
}

pub fn cmd_rename(art: &mut OxidArt, src: &[u8], dst: &[u8], nx: bool) -> Frame {
    match art.rename(src, dst, nx) {
        None => Frame::Error("ERR no such key".into()),
        Some(done) if nx => Frame::Integer(done as i64),
        Some(_) => Frame::SimpleString("OK"),
    }
}

/// TIME : `[secondes, microsecondes]` de l'horloge murale.
pub fn cmd_time() -> Frame {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Realtime);
    let bulk = |n: i64| Frame::BulkString(SharedByte::from_slice(n.to_string().as_bytes()));
    Frame::Array(vec![bulk(t.tv_sec), bulk(t.tv_nsec / 1000)])
}
