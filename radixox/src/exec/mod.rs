//! Exécution des commandes, indépendante du runtime réseau : commandes sync
//! sur l'arbre, encodage des réponses construites sans `Frame`, état d'une
//! connexion cliente (AUTH, CLIENT, HELLO), curseurs SCAN, jobs, persistance.
//! Utilisé par le cœur de la boucle io_uring (`radixox::uring`).

pub mod aof;
pub mod auth;
pub mod autosave;
pub mod bgsave;
pub mod fs_task;
pub mod client;
pub mod jobs;
pub mod journal;
pub mod persist;
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
    cmd_dbsize, cmd_decr, cmd_decrby, cmd_del, cmd_exists, cmd_expire, cmd_get, cmd_get_hinted,
    cmd_incr, cmd_incrby, cmd_incrby_hinted, cmd_mset, cmd_persist, cmd_pttl, cmd_set,
    cmd_set_hinted, cmd_setnx, cmd_setnx_hinted, cmd_ttl, cmd_ttl_hinted, cmd_type,
    cmd_type_hinted,
};

// ── Exécution vers le tampon de sortie ──────────────────────────────────────

/// Commande dont l'exécution dépend du runtime réseau (fermeture, Pub/Sub,
/// jobs, fichiers) : rendue à l'appelant par [`execute_into`].
pub enum Executed<'a> {
    Done,
    Runtime(Cmd<'a>),
}

/// Exécute `cmd` et écrit sa réponse dans `out`, ou la rend à l'appelant si
/// elle dépend du runtime (`QUIT`, `SUBSCRIBE`, `UNSUBSCRIBE`, `PUBLISH`,
/// `KEYS`, `UNLINK`, `FLUSHDB`, `BGSAVE`, `SAVE`, `LASTSAVE`, `INFO`). Contrôle
/// d'AUTH compris.
pub fn execute_into<'a>(
    cmd: Cmd<'a>,
    art: &mut OxidArt,
    client: &mut client::Client,
    out: &mut Vec<u8>,
) -> Executed<'a> {
    use radixox_lib::shared_frame::extend_encode;
    if matches!(cmd, Cmd::Empty) {
        return Executed::Done;
    }
    if !client.authed && !cmd.allowed_unauthenticated() {
        extend_encode(out, &auth::noauth());
        return Executed::Done;
    }
    let frame = match cmd {
        Cmd::Quit
        | Cmd::Subscribe(_)
        | Cmd::Unsubscribe(_)
        | Cmd::Publish { .. }
        | Cmd::Keys(_)
        | Cmd::Unlink(_)
        | Cmd::FlushDb
        | Cmd::BgSave
        | Cmd::BgRewriteAof
        | Cmd::Save
        | Cmd::LastSave
        | Cmd::DebugDigest(_)
        | Cmd::Replicate(_)
        | Cmd::ReplicaOf(_)
        | Cmd::RxHello(_)
        | Cmd::RxLoad(_)
        | Cmd::RxBlock(_)
        | Cmd::RxTick(_)
        | Cmd::Wait { .. }
        | Cmd::Info => return Executed::Runtime(cmd),
        Cmd::Ping(msg) => resp_pong(msg),
        Cmd::Auth { user, pass } => auth::cmd_auth(user, pass, &mut client.authed),
        Cmd::Hello(args) => client::cmd_hello(&args, client),
        Cmd::Client(args) => client::cmd_client(&args, client),
        Cmd::Command(args) => client::cmd_command(&args),
        Cmd::Echo(msg) => Frame::BulkString(SharedByte::from_slice(msg)),
        // Un processus = une base (ARCHITECTURE.md §1) : comme Redis avec
        // `databases 1`.
        Cmd::Select(0) => resp_ok(),
        Cmd::Select(_) => Frame::Error("ERR DB index is out of range".into()),
        Cmd::HMSet { key, fields } => match cmd_hset(art, key, &fields) {
            // HMSET répond +OK (jedis attend un status, pas un entier).
            err @ Frame::Error(_) => err,
            _ => resp_ok(),
        },
        Cmd::MGet(keys) => {
            // Descentes entrelacées + valeurs par référence, encodées pendant
            // l'emprunt (ni clone ni Vec<Frame>) ; expirées croisées purgées après.
            let mut expired = Vec::new();
            {
                let vals = art.mget_ref(&keys, &mut expired);
                encode_mget(out, &vals);
            }
            if !expired.is_empty() {
                art.purge_expired(&keys, &expired);
            }
            return Executed::Done;
        }
        Cmd::KRange { prefix, from, count, rev } => {
            // Valeurs par référence (pas de clone/refcount), préchargées au parcours.
            let range = art.krange(prefix, from, count, rev);
            encode_krange(out, &range);
            return Executed::Done;
        }
        Cmd::Scan { cursor, pattern, count, ty } => {
            match scan::cmd_scan(art, client.id, cursor, pattern, count, ty) {
                Ok((next, page)) => encode_scan(out, next, &page),
                Err(e) => extend_encode(out, &e),
            }
            return Executed::Done;
        }
        Cmd::Invalid { name, err } => Frame::Error(err.message(name)),
        cmd => execute_sync(cmd, art),
    };
    extend_encode(out, &frame);
    Executed::Done
}

// ── Commandes avec indice (moteur AMAC, RUNTIME_ARCH.md §8bis) ───────────────

/// Clés d'une commande que [`execute_hinted`] sait exécuter, passées à
/// `push` dans l'ordre (`false`, rien passé : pas une telle commande). Son
/// effet sur l'arbre ne passe que par les primitives de la fenêtre AMAC
/// d'oxidart (`prefetch_batch.rs`).
pub fn hint_keys<'a>(cmd: &Cmd<'a>, mut push: impl FnMut(&'a [u8])) -> bool {
    match cmd {
        Cmd::Get(k)
        | Cmd::Type(k)
        | Cmd::Ttl(k)
        | Cmd::Pttl(k)
        | Cmd::Persist(k)
        | Cmd::Incr(k)
        | Cmd::Decr(k)
        | Cmd::Set { key: k, .. }
        | Cmd::SetNx { key: k, .. }
        | Cmd::Expire { key: k, .. }
        | Cmd::IncrBy { key: k, .. }
        | Cmd::DecrBy { key: k, .. } => push(k),
        // par `OxidArt::with_hint` (le conteneur ; le champ, par sa propre
        // descente valart)
        Cmd::HGetAll(k)
        | Cmd::HLen(k)
        | Cmd::HKeys(k)
        | Cmd::HVals(k)
        | Cmd::SCard(k)
        | Cmd::SMembers(k)
        | Cmd::ZCard(k)
        | Cmd::HSet { key: k, .. }
        | Cmd::HGet { key: k, .. }
        | Cmd::HDel { key: k, .. }
        | Cmd::HExists { key: k, .. }
        | Cmd::HMGet { key: k, .. }
        | Cmd::HIncrBy { key: k, .. }
        | Cmd::SAdd { key: k, .. }
        | Cmd::SRem { key: k, .. }
        | Cmd::SIsMember { key: k, .. }
        | Cmd::SPop { key: k, .. }
        | Cmd::ZAdd { key: k, .. }
        | Cmd::ZRange { key: k, .. }
        | Cmd::ZScore { key: k, .. }
        | Cmd::ZRem { key: k, .. }
        | Cmd::ZIncrBy { key: k, .. } => push(k),
        Cmd::Del(keys) | Cmd::Exists(keys) => keys.iter().for_each(|k| push(k)),
        Cmd::MGet(keys) => keys.iter().for_each(|k| push(k)),
        Cmd::MSet(pairs) => pairs.iter().for_each(|(k, _)| push(k)),
        _ => return false,
    }
    true
}

/// Champ (Hash) ou membre (Set) d'une commande de [`hint_keys`] : la descente
/// AMAC de sa clé enchaîne sur sa descente dans le conteneur (préchargement
/// seul). Le premier quand il y en a plusieurs.
pub fn hint_field<'c>(cmd: &'c Cmd<'_>) -> Option<&'c [u8]> {
    match cmd {
        Cmd::HGet { field, .. } | Cmd::HExists { field, .. } => Some(field),
        Cmd::HIncrBy { field, .. } | Cmd::SIsMember { member: field, .. } => Some(field),
        Cmd::HSet { fields, .. } => fields.first().map(|(f, _)| f.as_slice()),
        Cmd::HDel { fields, .. } | Cmd::HMGet { fields, .. } => fields.first().map(|f| f.as_slice()),
        Cmd::SAdd { members, .. } | Cmd::SRem { members, .. } => members.first().map(|m| m.as_slice()),
        _ => None,
    }
}

/// Comme [`execute_into`], avec `keys` = [`hint_keys`]`(cmd)` et l'indice de
/// la descente AMAC de chacune, dans une fenêtre ouverte
/// (`OxidArt::begin_window`).
#[inline(always)]
pub fn execute_hinted(cmd: Cmd<'_>, art: &mut OxidArt, keys: &[&[u8]], hints: &[oxidart::Hint], out: &mut Vec<u8>) {
    use radixox_lib::shared_frame::extend_encode;
    let h = hints[0];
    let frame = match cmd {
        Cmd::Get(key) => cmd_get_hinted(art, key, h),
        Cmd::Set { key, val, opts } => cmd_set_hinted(art, key, val, opts, h),
        Cmd::SetNx { key, val } => cmd_setnx_hinted(art, key, val, h),
        Cmd::Type(key) => cmd_type_hinted(art, key, h),
        Cmd::Ttl(key) => cmd_ttl_hinted(art, key, false, h),
        Cmd::Pttl(key) => cmd_ttl_hinted(art, key, true, h),
        Cmd::Expire { key, dur } => Frame::Integer(art.expire_hinted(key, dur, h) as i64),
        Cmd::Persist(key) => Frame::Integer(art.persist_hinted(key, h) as i64),
        Cmd::Incr(key) => cmd_incrby_hinted(art, key, 1, h),
        Cmd::Decr(key) => cmd_incrby_hinted(art, key, -1, h),
        Cmd::IncrBy { key, delta } => cmd_incrby_hinted(art, key, delta, h),
        Cmd::DecrBy { key, delta } => cmd_incrby_hinted(art, key, delta.wrapping_neg(), h),
        Cmd::Del(keys) => {
            let mut n = 0;
            for (j, (k, &h)) in keys.iter().zip(hints).enumerate() {
                prefetch_next(art, hints, j);
                n += art.del_hinted(k, h) as i64;
            }
            Frame::Integer(n)
        }
        Cmd::Exists(keys) => {
            let mut n = 0;
            for (j, (k, &h)) in keys.iter().zip(hints).enumerate() {
                prefetch_next(art, hints, j);
                n += art.exists_hinted(k, h) as i64;
            }
            Frame::Integer(n)
        }
        Cmd::MSet(pairs) => {
            for (j, ((k, v), &h)) in pairs.iter().zip(hints).enumerate() {
                prefetch_next(art, hints, j);
                art.set_cond_hinted(k, None, oxidart::value::Value::String(v.clone()), None, h);
            }
            resp_ok()
        }
        Cmd::MGet(keys) => {
            use oxidart::krange::RangeVal;
            radixox_lib::shared_frame::encode_array_len(out, keys.len());
            for (j, (k, &h)) in keys.iter().zip(hints).enumerate() {
                prefetch_next(art, hints, j);
                match art.str_hinted(k, h) {
                    Some(RangeVal::Bytes(b)) => radixox_lib::shared_frame::encode_bulk(out, b.as_slice()),
                    Some(RangeVal::Int(n)) => encode_int_bulk(out, n),
                    None => out.extend_from_slice(b"$-1\r\n"),
                }
            }
            return;
        }
        cmd => execute_with_hint(cmd, art, keys[0], h),
    };
    extend_encode(out, &frame);
}

/// Hash, Set, ZSet : la commande normale, sa clé partant de l'indice (hors du
/// chemin des commandes string, qui n'a pas à porter le cadre d'`execute_sync`).
#[inline(never)]
fn execute_with_hint(cmd: Cmd<'_>, art: &mut OxidArt, key: &[u8], hint: oxidart::Hint) -> Frame {
    art.with_hint(key, hint, |art| execute_sync(cmd, art))
}

/// Dans une commande à plusieurs clés : précharge la valeur de la clé j+2.
#[inline]
fn prefetch_next(art: &OxidArt, hints: &[oxidart::Hint], j: usize) {
    if let Some(&h) = hints.get(j + 2) {
        art.prefetch_hint_value(h);
    }
}

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
        Cmd::FlushDb
        | Cmd::Ping(_)
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
        | Cmd::BgRewriteAof
        | Cmd::Save
        | Cmd::LastSave
        | Cmd::DebugDigest(_)
        | Cmd::Replicate(_)
        | Cmd::ReplicaOf(_)
        | Cmd::RxHello(_)
        | Cmd::RxLoad(_)
        | Cmd::RxBlock(_)
        | Cmd::RxTick(_)
        | Cmd::Wait { .. }
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

/// Sections `# Server` et `# Stats` d'`INFO` (la persistance vient du cœur).
pub fn info_server(out: &mut String) {
    use std::fmt::Write as _;
    let _ = write!(
        out,
        "# Server\r\nredis_version:7.0.0\r\nradixox_version:{}\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
}

pub fn info_stats(out: &mut String) {
    use std::fmt::Write as _;
    let _ = write!(out, "# Stats\r\nscan_cursors:{}\r\n", scan::active_cursors());
}

/// Section `# Memory` d'`INFO` (MEMORY_ARCH.md §16.8) : objets externes (jemalloc), pool.
pub fn info_memory(out: &mut String, gc_enabled: bool, gc_running: bool, gc_max_slice_us: u64) {
    use radixox_lib::raw_alloc::external;
    use std::fmt::Write as _;
    let _ = write!(
        out,
        "# Memory\r\nmem_external_bytes:{}\r\nmem_external_objects:{}\r\nmem_external_bg_thread:{}\r\n",
        external::bytes(),
        external::objects(),
        external::background_thread() as u8
    );
    // Pool (MEMORY_ARCH.md §16.8) : absent tant qu'il n'a rien alloué, ou sous `--cfg radixox_nopool`.
    if let Some((st, reserve)) = radixox_lib::pool::stats() {
        let held = st.held_blocks * radixox_lib::pool::BLOCK as u64;
        let frag = if held == 0 { 0.0 } else { 1.0 - st.live_bytes as f64 / held as f64 };
        let _ = write!(
            out,
            "mem_pool_live_bytes:{}\r\nmem_pool_held_bytes:{held}\r\nmem_pool_fragmentation:{frag:.2}\r\n\
             mem_pool_pages_active:{}\r\nmem_pool_pages_empty:{}\r\nmem_pool_pages_released:{}\r\n\
             mem_pool_sync_faults:{}\r\nmem_pool_range_full:{}\r\nmem_pool_reserve_bytes:{reserve}\r\n",
            st.live_bytes,
            st.pages_active,
            st.pages_empty,
            st.pages_released,
            st.sync_faults,
            st.range_full,
        );
        let g = radixox_lib::pool::gc_stats().unwrap_or_default();
        let _ = write!(
            out,
            "gc_enabled:{}\r\ngc_running:{}\r\ngc_passes:{}\r\ngc_evacuated_objects:{}\r\n\
             gc_evacuated_bytes:{}\r\ngc_pinned:{}\r\ngc_unmarked_blocks:{}\r\ngc_fresh_pages:{}\r\n\
             gc_last_pass_ms:{}\r\ngc_marked_blocks:{}\r\ngc_reserve_bytes:{reserve}\r\n\
             gc_max_slice_us:{gc_max_slice_us}\r\n",
            gc_enabled as u8,
            gc_running as u8,
            g.passes,
            g.evacuated_objects,
            g.evacuated_bytes,
            g.pinned,
            g.unmarked_blocks,
            g.fresh_pages,
            g.last_pass_ms,
            g.marked_blocks,
        );
    }
}

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
        None => Frame::SimpleString("PONG"),
    }
}

pub fn resp_ok() -> Frame {
    Frame::SimpleString("OK")
}
