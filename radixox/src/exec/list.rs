//! Commandes List (ANALYSE_ART_UNIFIE.md §14) : réponses et erreurs de Redis.

#[allow(unused_imports)]
use crate::prelude::*;

use oxidart::OxidArt;
use oxidart::lcommand::{End, LPosOpts, ListError};
use oxidart::value::RedisType;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

fn wrongtype(_: RedisType) -> Frame {
    Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
}

#[inline]
fn end(right: bool) -> End {
    if right { End::Right } else { End::Left }
}

pub fn cmd_lpush(art: &mut OxidArt, key: &[u8], vals: &[SharedByte], right: bool, exists: bool) -> Frame {
    match art.cmd_lpush(key, end(right), vals, exists) {
        Ok(n) => Frame::Integer(n as i64),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_lpop(art: &mut OxidArt, key: &[u8], right: bool, count: Option<u64>) -> Frame {
    match art.cmd_lpop(key, end(right), count.unwrap_or(1)) {
        Ok(None) if count.is_some() => Frame::NullArray,
        Ok(None) => Frame::Null,
        Ok(Some(mut v)) => match count {
            None => v.pop().map_or(Frame::Null, Frame::BulkString),
            Some(_) => Frame::Array(v.into_iter().map(Frame::BulkString).collect()),
        },
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_llen(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_llen(key) {
        Ok(n) => Frame::Integer(n as i64),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_lrange(art: &mut OxidArt, key: &[u8], start: i64, stop: i64) -> Frame {
    // grande plage : par tranches (le cœur parque la connexion)
    if let Ok(Some((a, b))) = art.lrange_span(key, start, stop)
        && (b - a) as usize >= super::jobs::RANGE_JOB_MIN
    {
        super::defer(super::jobs::RangeJob::list(key, a, b));
        return Frame::Null;
    }
    match art.cmd_lrange(key, start, stop) {
        Ok(v) => Frame::Array(v.into_iter().map(Frame::BulkString).collect()),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_lindex(art: &mut OxidArt, key: &[u8], index: i64) -> Frame {
    match art.cmd_lindex(key, index) {
        Ok(v) => v.map_or(Frame::Null, Frame::BulkString),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_lset(art: &mut OxidArt, key: &[u8], index: i64, val: SharedByte) -> Frame {
    match art.cmd_lset(key, index, val) {
        Ok(()) => Frame::SimpleString("OK"),
        Err(ListError::WrongType(t)) => wrongtype(t),
        Err(ListError::NoSuchKey) => Frame::Error("ERR no such key".into()),
        Err(ListError::OutOfRange) => Frame::Error("ERR index out of range".into()),
    }
}

pub fn cmd_linsert(art: &mut OxidArt, key: &[u8], after: bool, pivot: &[u8], val: SharedByte) -> Frame {
    match art.cmd_linsert(key, after, pivot, val) {
        Ok(n) => Frame::Integer(n),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_lrem(art: &mut OxidArt, key: &[u8], count: i64, val: &[u8]) -> Frame {
    match art.cmd_lrem(key, count, val) {
        Ok(n) => Frame::Integer(n as i64),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_ltrim(art: &mut OxidArt, key: &[u8], start: i64, stop: i64) -> Frame {
    match art.cmd_ltrim(key, start, stop) {
        Ok(()) => Frame::SimpleString("OK"),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_lpos(art: &mut OxidArt, key: &[u8], val: &[u8], rank: i64, count: Option<u64>, maxlen: u64) -> Frame {
    match art.cmd_lpos(key, val, LPosOpts { rank, count, maxlen }) {
        Ok(v) if count.is_some() => Frame::Array(v.into_iter().map(|i| Frame::Integer(i as i64)).collect()),
        Ok(v) => v.first().map_or(Frame::Null, |&i| Frame::Integer(i as i64)),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_lmove(art: &mut OxidArt, src: &[u8], dst: &[u8], from_right: bool, to_right: bool) -> Frame {
    match art.cmd_lmove(src, dst, end(from_right), end(to_right)) {
        Ok(v) => v.map_or(Frame::Null, Frame::BulkString),
        Err(t) => wrongtype(t),
    }
}

/// BLPOP / BRPOP sans attente : la première clé non vide, `[clé, élément]` ; nul sinon
/// (l'appelant parque la connexion).
pub fn try_bpop(art: &mut OxidArt, keys: &[&[u8]], right: bool) -> Option<Frame> {
    for &k in keys {
        match art.cmd_lpop(k, end(right), 1) {
            Ok(Some(mut v)) if !v.is_empty() => {
                return Some(Frame::Array(vec![
                    Frame::BulkString(SharedByte::from_slice(k)),
                    Frame::BulkString(v.pop().unwrap()),
                ]));
            }
            Ok(_) => {}
            Err(t) => return Some(wrongtype(t)),
        }
    }
    None
}

/// BLMOVE sans attente : l'élément déplacé ; `None` si la source est vide.
pub fn try_blmove(art: &mut OxidArt, src: &[u8], dst: &[u8], from_right: bool, to_right: bool) -> Option<Frame> {
    match art.cmd_lmove(src, dst, end(from_right), end(to_right)) {
        Ok(Some(v)) => Some(Frame::BulkString(v)),
        Ok(None) => None,
        Err(t) => Some(wrongtype(t)),
    }
}
