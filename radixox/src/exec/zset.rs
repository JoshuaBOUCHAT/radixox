use oxidart::OxidArt;
use oxidart::error::TypeError;
use oxidart::zcommand::ZAddReply;
use radixox_lib::cmd::{ZAddFlags, ZRangeBy};
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_zadd(art: &mut OxidArt, key: &[u8], flags: ZAddFlags, score_members: &[(f64, SharedByte)]) -> Frame {
    // cmd_zadd peut créer le nœud zset : oxidart exige une clé possédée ici.
    match art.cmd_zadd_opts(SharedByte::from_slice(key), flags, score_members, None) {
        Ok(ZAddReply::Count(n)) => Frame::Integer(n as i64),
        Ok(ZAddReply::Score(Some(s))) => score_bulk(s),
        Ok(ZAddReply::Score(None)) => Frame::Null,
        Err(TypeError::NotAFloat) if flags.incr => Frame::Error("ERR resulting score is not a number (NaN)".into()),
        Err(e) => type_error(e),
    }
}

pub fn cmd_zcard(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_zcard(key) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zrange(
    art: &mut OxidArt,
    key: &[u8],
    range: ZRangeBy<'_>,
    rev: bool,
    limit: Option<(i64, i64)>,
    with_scores: bool,
) -> Frame {
    let (lo, hi) = match art.zrange_ranks(key, range, rev, limit) {
        Ok(Some(r)) => r,
        Ok(None) => return Frame::Array(Vec::new()),
        Err(t) => return wrongtype(t),
    };
    // grande plage : par tranches (le cœur parque la connexion)
    if hi - lo >= super::jobs::RANGE_JOB_MIN {
        super::defer(super::jobs::RangeJob::zset(key, lo, hi, rev, with_scores));
        return Frame::Null;
    }
    match art.zrange_at(key, lo, hi, rev, with_scores) {
        Ok(result) => Frame::Array(result.into_iter().map(Frame::BulkString).collect()),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_zscore(art: &mut OxidArt, key: &[u8], member: SharedByte) -> Frame {
    match art.cmd_zscore(key, member) {
        Ok(Some(score)) => Frame::BulkString(SharedByte::from_slice(score.to_string().as_bytes())),
        Ok(None) => Frame::Null,
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zrem(art: &mut OxidArt, key: &[u8], members: &[SharedByte]) -> Frame {
    match art.cmd_zrem(key, members) {
        Ok(removed) => Frame::Integer(removed as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zincrby(art: &mut OxidArt, key: &[u8], delta: f64, member: SharedByte) -> Frame {
    // cmd_zincrby peut créer le nœud zset : oxidart exige une clé possédée ici.
    match art.cmd_zincrby(SharedByte::from_slice(key), delta, member) {
        Ok(new_score) => {
            Frame::BulkString(SharedByte::from_slice(new_score.to_string().as_bytes()))
        }
        Err(TypeError::NotAFloat) => Frame::Error("ERR resulting score is not a number (NaN)".into()),
        Err(e) => type_error(e),
    }
}

fn type_error(e: TypeError) -> Frame {
    match e {
        TypeError::ValueNotSet => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        TypeError::NotAFloat => Frame::Error("ERR value is not a valid float".into()),
        TypeError::OutOfMemory => Frame::Error("OOM handle table full (RADIXOX_HANDLES_MAX)".into()),
        TypeError::NotAInt => Frame::Error("ERR internal error".into()),
    }
}

fn wrongtype(t: oxidart::value::RedisType) -> Frame {
    Frame::Error(format!(
        "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
        t.as_str()
    ))
}

fn score_bulk(s: f64) -> Frame {
    Frame::BulkString(oxidart::zset::format_score(s))
}

/// `[membre, score, …]` à plat (RESP2).
fn pairs(v: Vec<(SharedByte, f64)>, with_scores: bool) -> Frame {
    let mut out = Vec::with_capacity(v.len() * (1 + with_scores as usize));
    for (m, s) in v {
        out.push(Frame::BulkString(m));
        if with_scores {
            out.push(score_bulk(s));
        }
    }
    Frame::Array(out)
}

pub fn cmd_zrank(art: &mut OxidArt, key: &[u8], member: &[u8], rev: bool, with_score: bool) -> Frame {
    match art.cmd_zrank(key, member, rev) {
        Ok(Some((r, s))) if with_score => Frame::Array(vec![Frame::Integer(r as i64), score_bulk(s)]),
        Ok(Some((r, _))) => Frame::Integer(r as i64),
        Ok(None) => Frame::Null,
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_zcount(art: &mut OxidArt, key: &[u8], range: ZRangeBy<'_>) -> Frame {
    match art.cmd_zcount(key, range) {
        Ok(n) => Frame::Integer(n as i64),
        Err(t) => wrongtype(t),
    }
}

/// Sans count : `[membre, score]` (vide si absent) ; avec : à plat.
pub fn cmd_zpop(art: &mut OxidArt, key: &[u8], max: bool, count: Option<u64>) -> Frame {
    match art.cmd_zpop(key, max, count.unwrap_or(1)) {
        Ok(v) => pairs(v, true),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_zremrange(art: &mut OxidArt, key: &[u8], range: ZRangeBy<'_>) -> Frame {
    match art.cmd_zremrange(key, range) {
        Ok(n) => Frame::Integer(n as i64),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_zmscore(art: &mut OxidArt, key: &[u8], members: &[&[u8]]) -> Frame {
    match art.cmd_zmscore(key, members) {
        Ok(v) => Frame::Array(v.into_iter().map(|s| s.map_or(Frame::Null, score_bulk)).collect()),
        Err(t) => wrongtype(t),
    }
}

/// Sans count : un membre (nul si absent) ; avec : tableau (vide si absent).
pub fn cmd_zrandmember(art: &mut OxidArt, key: &[u8], count: Option<i64>, with_scores: bool) -> Frame {
    match art.cmd_zrandmember(key, count) {
        Ok(v) if count.is_none() => v.into_iter().next().map_or(Frame::Null, |(m, _)| Frame::BulkString(m)),
        Ok(v) => pairs(v, with_scores),
        Err(t) => wrongtype(t),
    }
}

/// BZPOPMIN / BZPOPMAX sans attente : la première clé non vide, `[clé, membre, score]` ;
/// `None` sinon (l'appelant parque la connexion).
pub fn try_bzpop(art: &mut OxidArt, keys: &[&[u8]], max: bool) -> Option<Frame> {
    for &k in keys {
        match art.cmd_zpop(k, max, 1) {
            Ok(mut v) if !v.is_empty() => {
                let (m, s) = v.pop().unwrap();
                return Some(Frame::Array(vec![Frame::BulkString(SharedByte::from_slice(k)), Frame::BulkString(m), score_bulk(s)]));
            }
            Ok(_) => {}
            Err(t) => return Some(wrongtype(t)),
        }
    }
    None
}
