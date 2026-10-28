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
pub mod list;
pub mod maxmem;
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
pub use zset::try_bzpop;
pub use zset::{
    cmd_zadd, cmd_zcard, cmd_zcount, cmd_zincrby, cmd_zmscore, cmd_zpop, cmd_zrandmember, cmd_zrange, cmd_zrank, cmd_zrem,
    cmd_zremrange, cmd_zscore,
};

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

// ── Lectures de plage géantes : job (§ commandes bornées) ──────────────────

thread_local! {
    /// Lecture de plage trop grande pour une seule fois : le job préparé par l'exécution
    /// (qui n'a rien répondu), repris par le cœur, qui parque la connexion.
    static DEFERRED: std::cell::Cell<Option<Box<jobs::RangeJob>>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn defer(job: jobs::RangeJob) {
    DEFERRED.with(|d| d.set(Some(Box::new(job))));
}

/// Job laissé par la dernière commande exécutée (LRANGE, ZRANGE et variantes).
pub fn take_deferred() -> Option<Box<jobs::RangeJob>> {
    DEFERRED.with(|d| d.take())
}

/// À partir de cette taille, une réponse faite d'une seule valeur ne passe pas par le tampon
/// de sortie : seul son en-tête `$len\r\n` y est écrit, la valeur part ensuite directement
/// depuis l'arbre (clone du `SharedByte`, sans copie), RUNTIME_ARCH.md §6.2 quater.
pub const BIG_REPLY: usize = 1 << 20;

thread_local! {
    /// Grande valeur laissée par la dernière commande, à envoyer après `out`.
    static BIG_TAIL: std::cell::Cell<Option<SharedByte>> = const { std::cell::Cell::new(None) };
    /// Le dépôt n'est actif qu'autour des exécutions du cœur ([`with_big_tail`]) : les
    /// autres appelants (rejeu du journal, oracle des tests) veulent la réponse entière.
    static BIG_TAIL_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `f` avec le dépôt des grandes valeurs actif ; rend aussi la valeur déposée, à envoyer
/// après ce que `f` a écrit.
#[inline(always)]
pub fn with_big_tail<R>(f: impl FnOnce() -> R) -> (R, Option<SharedByte>) {
    BIG_TAIL_ON.with(|on| on.set(true));
    let r = f();
    BIG_TAIL_ON.with(|on| on.set(false));
    (r, BIG_TAIL.with(|t| t.take()))
}

/// Encode la réponse ; une grande valeur seule est déposée au lieu d'être recopiée.
#[inline(always)]
fn encode_reply(out: &mut Vec<u8>, frame: Frame) {
    match frame {
        Frame::BulkString(b) if b.len() >= BIG_REPLY && BIG_TAIL_ON.with(|on| on.get()) => {
            out.push(b'$');
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
            BIG_TAIL.with(|t| t.set(Some(b)));
        }
        frame => radixox_lib::shared_frame::extend_encode(out, &frame),
    }
}

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
        | Cmd::BgSaveReserve(_)
        | Cmd::Config(_)
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
        | Cmd::BPop { .. }
        | Cmd::BLMove { .. }
        | Cmd::BZPop { .. }
        | Cmd::Shutdown { .. }
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
        Cmd::ZSetOp { op, dest: None, keys, weights, agg, with_scores } => {
            zset::zsetop_reply(art, op, &keys, &weights, agg, with_scores, out);
            return Executed::Done;
        }
        Cmd::ZRange { key, range, rev, limit, with_scores } => {
            zset::zrange_reply(art, key, range, rev, limit, with_scores, out);
            return Executed::Done;
        }
        Cmd::ZScan { key, cursor, pattern, count, noscores } => {
            scan::cmd_zscan(art, client.id, key, cursor, pattern, count, noscores, out);
            return Executed::Done;
        }
        Cmd::Invalid { name, err } => Frame::Error(err.message(name)),
        cmd => execute_sync(cmd, art),
    };
    encode_reply(out, frame);
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
        | Cmd::ZIncrBy { key: k, .. }
        | Cmd::ZRank { key: k, .. }
        | Cmd::ZCount { key: k, .. }
        | Cmd::ZPop { key: k, .. }
        | Cmd::ZRemRange { key: k, .. }
        | Cmd::ZMScore { key: k, .. }
        | Cmd::ZRandMember { key: k, .. }
        | Cmd::LPush { key: k, .. }
        | Cmd::LPop { key: k, .. }
        | Cmd::LLen(k)
        | Cmd::LRange { key: k, .. }
        | Cmd::LIndex { key: k, .. }
        | Cmd::LSet { key: k, .. }
        | Cmd::LInsert { key: k, .. }
        | Cmd::LRem { key: k, .. }
        | Cmd::LTrim { key: k, .. }
        | Cmd::LPos { key: k, .. } => push(k),
        Cmd::Del(keys) | Cmd::Exists(keys) => keys.iter().for_each(|k| push(k)),
        Cmd::MGet(keys) => keys.iter().for_each(|k| push(k)),
        Cmd::MSet(pairs) => pairs.iter().for_each(|(k, _)| push(k)),
        _ => return false,
    }
    true
}

/// Champ (Hash), membre (Set, ZSet) d'une commande de [`hint_keys`] : la descente
/// AMAC de sa clé enchaîne sur sa descente dans le conteneur (préchargement
/// seul). Le premier quand il y en a plusieurs. ZSet : ce qui suit l'index
/// ([`oxidart::ZProbe`]).
pub fn hint_field<'c>(cmd: &'c Cmd<'_>) -> Option<(&'c [u8], oxidart::ZProbe)> {
    use oxidart::ZProbe::{Add, Index, Tree};
    match cmd {
        Cmd::HGet { field, .. } | Cmd::HExists { field, .. } => Some((field, Index)),
        Cmd::HIncrBy { field, .. } | Cmd::SIsMember { member: field, .. } => Some((field, Index)),
        Cmd::HSet { fields, .. } => fields.first().map(|(f, _)| (f.as_slice(), Index)),
        Cmd::HDel { fields, .. } | Cmd::HMGet { fields, .. } => fields.first().map(|f| (f.as_slice(), Index)),
        Cmd::SAdd { members, .. } | Cmd::SRem { members, .. } => members.first().map(|m| (m.as_slice(), Index)),
        Cmd::ZScore { member, .. } => Some((member, Index)),
        Cmd::ZMScore { members, .. } => members.first().map(|m| (*m, Index)),
        Cmd::ZRank { member, .. } => Some((member, Tree)),
        Cmd::ZIncrBy { member, .. } => Some((member, Tree)),
        Cmd::ZRem { members, .. } => members.first().map(|m| (m.as_slice(), Tree)),
        Cmd::ZAdd { members, .. } => members.first().map(|(s, m)| (m.as_slice(), Add(*s))),
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
        Cmd::ZRange { key, range, rev, limit, with_scores } => {
            art.with_hint(key, h, |art| zset::zrange_reply(art, key, range, rev, limit, with_scores, out));
            return;
        }
        cmd => execute_with_hint(cmd, art, keys[0], h),
    };
    encode_reply(out, frame);
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
        Cmd::ZAdd { key, flags, members } => cmd_zadd(art, key, flags, &members),
        Cmd::ZCard(key) => cmd_zcard(art, key),
        Cmd::ZRange { key, range, rev, limit, with_scores } => cmd_zrange(art, key, range, rev, limit, with_scores),
        Cmd::ZRank { key, member, rev, with_score } => cmd_zrank(art, key, member, rev, with_score),
        Cmd::ZCount { key, range } => cmd_zcount(art, key, range),
        Cmd::ZPop { key, max, count } => cmd_zpop(art, key, max, count),
        Cmd::ZRemRange { key, range } => cmd_zremrange(art, key, range),
        Cmd::ZMScore { key, members } => cmd_zmscore(art, key, &members),
        Cmd::ZRandMember { key, count, with_scores } => cmd_zrandmember(art, key, count, with_scores),
        Cmd::ZScore { key, member } => cmd_zscore(art, key, member),
        Cmd::ZRem { key, members } => cmd_zrem(art, key, &members),
        Cmd::ZIncrBy { key, delta, member } => cmd_zincrby(art, key, delta, member),
        Cmd::ZSetOp { op, dest, keys, weights, agg, with_scores } => zset::cmd_zsetop(art, op, dest, &keys, &weights, agg, with_scores),
        Cmd::ZInterCard { keys, limit } => zset::cmd_zintercard(art, &keys, limit),
        // ── List ─────────────────────────────────────────────────────────────
        Cmd::LPush { key, vals, right, exists } => list::cmd_lpush(art, key, &vals, right, exists),
        Cmd::LPop { key, right, count } => list::cmd_lpop(art, key, right, count),
        Cmd::LLen(key) => list::cmd_llen(art, key),
        Cmd::LRange { key, start, stop } => list::cmd_lrange(art, key, start, stop),
        Cmd::LIndex { key, index } => list::cmd_lindex(art, key, index),
        Cmd::LSet { key, index, val } => list::cmd_lset(art, key, index, val),
        Cmd::LInsert { key, after, pivot, val } => list::cmd_linsert(art, key, after, pivot, val),
        Cmd::LRem { key, count, val } => list::cmd_lrem(art, key, count, val),
        Cmd::LTrim { key, start, stop } => list::cmd_ltrim(art, key, start, stop),
        Cmd::LPos { key, val, rank, count, maxlen } => list::cmd_lpos(art, key, val, rank, count, maxlen),
        Cmd::LMove { src, dst, from_right, to_right } => list::cmd_lmove(art, src, dst, from_right, to_right),
        // les bloquantes passent par le cœur (attente, service direct) ; ici, sans attente
        Cmd::BPop { keys, right, .. } => list::try_bpop(art, &keys, right).unwrap_or(Frame::NullArray),
        Cmd::BLMove { src, dst, from_right, to_right, .. } => {
            list::try_blmove(art, src, dst, from_right, to_right).unwrap_or(Frame::NullArray)
        }
        Cmd::BZPop { keys, max, .. } => zset::try_bzpop(art, &keys, max).unwrap_or(Frame::NullArray),
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
        | Cmd::ZScan { .. }
        | Cmd::MGet(_)
        | Cmd::Unlink(_)
        | Cmd::BgSave
        | Cmd::BgSaveReserve(_)
        | Cmd::Config(_)
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
        | Cmd::Shutdown { .. }
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

/// Section `# Memory` d'`INFO` (MEMORY_ARCH.md §16.8) : objets externes, grands objets (§18), pool.
pub fn info_memory(out: &mut String, gc_enabled: bool, gc_running: bool, gc_max_slice_us: u64) {
    use radixox_lib::raw_alloc::external;
    use std::fmt::Write as _;
    let _ = write!(
        out,
        "# Memory\r\nmem_external_bytes:{}\r\nmem_external_objects:{}\r\nmem_bigbuf_bytes:{}\r\n",
        external::bytes(),
        external::objects(),
        radixox_lib::big_buf::bytes(),
    );
    // Objets à poignée (membres des grands ZSet, MEMORY_ARCH.md §19.5).
    let (handles, table) = radixox_lib::handle::stats();
    let _ = write!(out, "mem_handles_live:{handles}\r\nmem_handles_table_bytes:{table}\r\n");
    // Grands objets et épinglés (MEMORY_ARCH.md §18.7) : absent tant que rien n'y est alloué.
    if let Some(l) = radixox_lib::large::stats() {
        let _ = write!(
            out,
            "mem_large_held_bytes:{}\r\nmem_large_dirty_bytes:{}\r\nmem_large_released_bytes:{}\r\n\
             mem_large_top_bytes:{}\r\nmem_slab_held_bytes:{}\r\nmem_slab_live_bytes:{}\r\n",
            l.held_bytes, l.dirty_bytes, l.released_bytes, l.top_bytes, l.slab_held_bytes, l.slab_live_bytes,
        );
    }
    if cfg!(not(radixox_sysalloc)) {
        use radixox_lib::raw_alloc::heap;
        let _ = write!(out, "mem_heap_bytes:{}\r\nmem_heap_peak_bytes:{}\r\n", heap::bytes(), heap::peak());
    }
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
    // Compteurs au-delà de 127 détenteurs (MEMORY_ARCH.md §10.2 ter).
    let (slots, copies) = radixox_lib::shared_byte::rc_overflow_stats();
    let _ = write!(out, "mem_rc_overflow_slots:{slots}\r\nmem_rc_overflow_copies:{copies}\r\n");
}

/// Total résident compté, contre le RSS du processus (MEMORY_ARCH.md §10.1) : pages
/// résidentes (`mincore`) de chaque région à nous — pool, `large` (tas Rust et slabs de
/// classes compris), chaque HiSlab, table des poignées, tables par bloc et par granule —,
/// plus `BigBuf` et le repli libc compté. `mem_untracked_bytes` = `RssAnon` − ce total :
/// pile, binaire, ce que la libc tient pour elle (~1–2 Mo, fixes) ; s'il grandit avec les
/// données, une région manque.
pub fn info_memory_total(out: &mut String, slabs: &[(&str, (u64, u64, u64))]) {
    use std::fmt::Write as _;
    let mut slabs_total = 0u64;
    for &(name, (touched, live, resident)) in slabs {
        let _ = write!(
            out,
            "mem_hislab_{name}_bytes:{resident}\r\nmem_hislab_{name}_touched_bytes:{touched}\r\nmem_hislab_{name}_live_bytes:{live}\r\n"
        );
        slabs_total += resident;
    }
    let pool = radixox_lib::pool::resident();
    let large = radixox_lib::large::resident();
    let handles = radixox_lib::handle::table_resident();
    let direct = radixox_lib::raw_alloc::heap::direct_bytes();
    let _ = write!(
        out,
        "mem_hislab_bytes:{slabs_total}\r\nmem_pool_resident_bytes:{pool}\r\nmem_large_resident_bytes:{large}\r\nmem_handles_table_resident_bytes:{handles}\r\nmem_direct_bytes:{direct}\r\n"
    );
    let tracked = slabs_total + pool + large + handles + direct + radixox_lib::big_buf::bytes();
    let (rss, anon) = proc_rss();
    let _ = write!(
        out,
        "mem_tracked_bytes:{tracked}\r\nmem_rss_bytes:{rss}\r\nmem_rss_anon_bytes:{anon}\r\nmem_untracked_bytes:{}\r\n\
         mem_charged_bytes:{}\r\nmem_charged_peak_bytes:{}\r\n",
        anon as i64 - tracked as i64,
        radixox_lib::pages::charged(),
        radixox_lib::pages::peak()
    );
}

/// `(VmRSS, RssAnon)` du processus en octets (`/proc/self/status`, lu à la demande).
fn proc_rss() -> (u64, u64) {
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else { return (0, 0) };
    let kb = |key: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map_or(0, |k| k * 1024)
    };
    (kb("VmRSS:"), kb("RssAnon:"))
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
