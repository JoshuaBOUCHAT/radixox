//! Exécution des commandes, indépendante du runtime réseau : commandes sync
//! sur l'arbre, encodage des réponses construites sans `Frame`, état d'une
//! connexion cliente (AUTH, CLIENT, HELLO), curseurs SCAN. Partagé par le
//! serveur monoio (`bin/resp.rs`) et la future boucle io_uring
//! (RUNTIME_ARCH.md §13).

pub mod auth;
pub mod client;
mod hash;
pub mod scan;
mod sset;
pub mod string;
mod zset;

pub use hash::{
    cmd_hdel, cmd_hexists, cmd_hget, cmd_hgetall, cmd_hincrby, cmd_hkeys, cmd_hlen, cmd_hmget,
    cmd_hset, cmd_hvals,
};
pub use sset::{cmd_sadd, cmd_scard, cmd_sismember, cmd_smembers, cmd_spop, cmd_srem};
pub use zset::{cmd_zadd, cmd_zcard, cmd_zincrby, cmd_zrange, cmd_zrem, cmd_zscore};

use oxidart::OxidArt;
use radixox_lib::cmd::Cmd;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

use string::{
    cmd_dbsize, cmd_decr, cmd_decrby, cmd_del, cmd_exists, cmd_expire, cmd_flushdb, cmd_get,
    cmd_incr, cmd_incrby, cmd_mset, cmd_persist, cmd_pttl, cmd_set, cmd_setnx, cmd_ttl,
    cmd_type,
};

// ── Command execution ─────────────────────────────────────────────────────────

pub fn execute_sync(cmd: Cmd<'_>, art: &mut OxidArt) -> Frame {
    match cmd {
        // ── String / Keys ────────────────────────────────────────────────────
        Cmd::Get(key) => cmd_get(art, key),
        Cmd::Set { key, val, opts } => cmd_set(art, key, val, opts),
        Cmd::SetNx { key, val } => cmd_setnx(art, key, val),
        Cmd::MSet(pairs) => cmd_mset(art, &pairs),
        Cmd::Del(keys) => cmd_del(art, &keys),
        Cmd::Exists(keys) => cmd_exists(art, &keys),
        Cmd::Type(key) => cmd_type(art, key),
        // ── TTL ──────────────────────────────────────────────────────────────
        Cmd::Ttl(key) => cmd_ttl(art, key),
        Cmd::Pttl(key) => cmd_pttl(art, key),
        Cmd::Expire { key, dur } => cmd_expire(art, key, dur),
        Cmd::Persist(key) => cmd_persist(art, key),
        // ── Counters ─────────────────────────────────────────────────────────
        Cmd::Incr(key) => cmd_incr(art, key),
        Cmd::Decr(key) => cmd_decr(art, key),
        Cmd::IncrBy { key, delta } => cmd_incrby(art, key, delta),
        Cmd::DecrBy { key, delta } => cmd_decrby(art, key, delta),
        // ── Server ───────────────────────────────────────────────────────────
        Cmd::DbSize => cmd_dbsize(art),
        Cmd::FlushDb => cmd_flushdb(art),
        // ── Hash ─────────────────────────────────────────────────────────────
        Cmd::HSet { key, fields } => cmd_hset(art, key, &fields),
        Cmd::HGet { key, field } => cmd_hget(art, key, field),
        Cmd::HGetAll(key) => cmd_hgetall(art, key),
        Cmd::HDel { key, fields } => cmd_hdel(art, key, &fields),
        Cmd::HExists { key, field } => cmd_hexists(art, key, field),
        Cmd::HLen(key) => cmd_hlen(art, key),
        Cmd::HKeys(key) => cmd_hkeys(art, key),
        Cmd::HVals(key) => cmd_hvals(art, key),
        Cmd::HMGet { key, fields } => cmd_hmget(art, key, &fields),
        Cmd::HIncrBy { key, field, delta } => cmd_hincrby(art, key, field, delta),
        // ── Set ──────────────────────────────────────────────────────────────
        Cmd::SAdd { key, members } => cmd_sadd(art, key, &members),
        Cmd::SRem { key, members } => cmd_srem(art, key, &members),
        Cmd::SIsMember { key, member } => cmd_sismember(art, key, member),
        Cmd::SCard(key) => cmd_scard(art, key),
        Cmd::SMembers(key) => cmd_smembers(art, key),
        Cmd::SPop { key, count } => cmd_spop(art, key, count),
        // ── ZSet ─────────────────────────────────────────────────────────────
        Cmd::ZAdd { key, members } => cmd_zadd(art, key, &members),
        Cmd::ZCard(key) => cmd_zcard(art, key),
        Cmd::ZRange {
            key,
            start,
            stop,
            with_scores,
        } => cmd_zrange(art, key, start, stop, with_scores),
        Cmd::ZScore { key, member } => cmd_zscore(art, key, member),
        Cmd::ZRem { key, members } => cmd_zrem(art, key, &members),
        Cmd::ZIncrBy { key, delta, member } => cmd_zincrby(art, key, delta, member),
        // ── Should not reach here (handled in dispatch) ───────────────────
        Cmd::Ping(_)
        | Cmd::Quit
        | Cmd::Echo(_)
        | Cmd::Select(_)
        | Cmd::Subscribe(_)
        | Cmd::Unsubscribe(_)
        | Cmd::Publish { .. }
        | Cmd::Keys(_)
        | Cmd::KRange { .. }
        | Cmd::Scan { .. }
        | Cmd::MGet(_)
        | Cmd::Unlink(_)
        | Cmd::BgSave
        | Cmd::Save
        | Cmd::Info
        | Cmd::Auth { .. }
        | Cmd::Hello(_)
        | Cmd::Client(_)
        | Cmd::Command(_)
        | Cmd::HMSet { .. }
        | Cmd::Empty
        | Cmd::Invalid { .. } => unreachable!(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn encode_krange(buf: &mut Vec<u8>, range: &oxidart::krange::RangeOut<'_>) {
    use oxidart::krange::RangeVal;
    use radixox_lib::shared_frame::{encode_array_len, encode_bulk};
    encode_array_len(buf, 2 * range.len());
    for (key, val) in range.iter() {
        encode_bulk(buf, key);
        match val {
            RangeVal::Bytes(b) => encode_bulk(buf, b.as_slice()),
            RangeVal::Int(n) => encode_int_bulk(buf, n),
        }
    }
}

/// Réponse SCAN : [curseur, [clés…]], le curseur en bulk string décimale.
pub fn encode_scan(buf: &mut Vec<u8>, cursor: u64, page: &oxidart::krange::ScanPage) {
    use radixox_lib::shared_frame::{encode_array_len, encode_bulk};
    encode_array_len(buf, 2);
    encode_int_bulk(buf, cursor as i64);
    encode_array_len(buf, page.len());
    for key in page.iter() {
        encode_bulk(buf, key);
    }
}

pub fn encode_mget(buf: &mut Vec<u8>, vals: &[Option<oxidart::krange::RangeVal<'_>>]) {
    use oxidart::krange::RangeVal;
    use radixox_lib::shared_frame::{encode_array_len, encode_bulk};
    encode_array_len(buf, vals.len());
    for v in vals {
        match *v {
            Some(RangeVal::Bytes(b)) => encode_bulk(buf, b.as_slice()),
            Some(RangeVal::Int(n)) => encode_int_bulk(buf, n),
            None => buf.extend_from_slice(b"$-1\r\n"),
        }
    }
}

/// Entier en bulk string (GET/MGET d'une valeur Int), sans allocation.
pub fn encode_int_bulk(buf: &mut Vec<u8>, n: i64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut m = n.unsigned_abs();
    loop {
        i -= 1;
        tmp[i] = b'0' + (m % 10) as u8;
        m /= 10;
        if m == 0 {
            break;
        }
    }
    if n < 0 {
        i -= 1;
        tmp[i] = b'-';
    }
    radixox_lib::shared_frame::encode_bulk(buf, &tmp[i..]);
}

pub fn resp_pong(msg: Option<&[u8]>) -> Frame {
    match msg {
        Some(m) => Frame::BulkString(SharedByte::from_slice(m)),
        None => Frame::SimpleString(SharedByte::from_slice(b"PONG")),
    }
}

pub fn resp_ok() -> Frame {
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}
