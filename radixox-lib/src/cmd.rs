use std::time::Duration;

use crate::small_vec::SmallVec;

use crate::shared_byte::OwnedByte;

// ---------------------------------------------------------------------------
// SET options
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SetCondition {
    Always,
    IfNotExists, // NX
    IfExists,    // XX
}

#[derive(Debug)]
pub struct SetOpts {
    /// EX/PX normalisé en Duration, None = pas de TTL.
    pub ttl: Option<Duration>,
    pub condition: SetCondition,
}

impl Default for SetOpts {
    fn default() -> Self {
        Self {
            ttl: None,
            condition: SetCondition::Always,
        }
    }
}

// ---------------------------------------------------------------------------
// Cmd
//
// Construit par le thread I/O après parsing RESP complet.
// Consommé par le thread Data pour exécuter les opérations ART.
//
// Règles de normalisation (I/O thread) :
//   - SETNX key val  → Set { opts: SetOpts { condition: IfNotExists, .. } }
//   - SETEX key s v  → Set { opts: SetOpts { ttl: Some(Duration::from_secs(s)), .. } }
//   - HMSET          → HSet (alias déprécié, même sémantique)
//   - EXPIRE key s   → Expire { dur: Duration::from_secs(s) }
//   - PEXPIRE key ms → Expire { dur: Duration::from_millis(ms) }
// ---------------------------------------------------------------------------

pub enum Cmd {
    // --- Connection / admin -------------------------------------------
    /// INFO [section] — retourne des infos serveur (réponse minimale)
    Info,

    /// PING [message]
    Ping(Option<OwnedByte>),
    /// QUIT
    Quit,
    /// ECHO message
    Echo(OwnedByte),
    /// SELECT index
    Select(u64),
    /// DBSIZE
    DbSize,
    /// FLUSHDB
    FlushDb,

    // --- String / clés -----------------------------------------------
    /// GET key
    Get(OwnedByte),

    /// SET key value [EX secs | PX ms] [NX | XX]
    /// Couvre aussi SETNX et SETEX (normalisés côté I/O).
    Set {
        key: OwnedByte,
        val: OwnedByte,
        opts: SetOpts,
    },

    /// MGET key [key ...]
    MGet(SmallVec<5, OwnedByte>),

    /// MSET key value [key value ...]
    MSet(SmallVec<2, (OwnedByte, OwnedByte)>),

    /// DEL key [key ...]
    Del(SmallVec<4, OwnedByte>),

    /// UNLINK key [key ...]  (DEL async — même exécution côté ART)
    Unlink(SmallVec<4, OwnedByte>),

    /// EXISTS key [key ...]
    Exists(SmallVec<4, OwnedByte>),

    /// TYPE key
    Type(OwnedByte),

    /// KEYS pattern
    Keys(OwnedByte),

    // --- TTL ---------------------------------------------------------
    /// TTL key  (retourne secondes)
    Ttl(OwnedByte),

    /// PTTL key  (retourne millisecondes)
    Pttl(OwnedByte),

    /// EXPIRE key seconds  /  PEXPIRE key milliseconds
    /// Normalisé en Duration sur l'I/O thread.
    Expire { key: OwnedByte, dur: Duration },

    /// PERSIST key
    Persist(OwnedByte),

    // --- Compteurs ---------------------------------------------------
    /// INCR key
    Incr(OwnedByte),

    /// DECR key
    Decr(OwnedByte),

    /// INCRBY key delta
    IncrBy { key: OwnedByte, delta: i64 },

    /// DECRBY key delta
    DecrBy { key: OwnedByte, delta: i64 },

    // --- Hash --------------------------------------------------------
    /// HSET key field value [field value ...]
    HSet {
        key: OwnedByte,
        fields: SmallVec<2, (OwnedByte, OwnedByte)>,
    },

    /// HMSET key field value [field value ...] — alias déprécié de HSET, doit répondre +OK
    HMSet {
        key: OwnedByte,
        fields: SmallVec<2, (OwnedByte, OwnedByte)>,
    },

    /// HGET key field
    HGet { key: OwnedByte, field: OwnedByte },

    /// HGETALL key
    HGetAll(OwnedByte),

    /// HDEL key field [field ...]
    HDel {
        key: OwnedByte,
        fields: SmallVec<4, OwnedByte>,
    },

    /// HEXISTS key field
    HExists { key: OwnedByte, field: OwnedByte },

    /// HLEN key
    HLen(OwnedByte),

    /// HKEYS key
    HKeys(OwnedByte),

    /// HVALS key
    HVals(OwnedByte),

    /// HMGET key field [field ...]
    HMGet {
        key: OwnedByte,
        fields: SmallVec<4, OwnedByte>,
    },

    /// HINCRBY key field delta
    HIncrBy {
        key: OwnedByte,
        field: OwnedByte,
        delta: i64,
    },

    // --- Set ---------------------------------------------------------
    /// SADD key member [member ...]
    SAdd {
        key: OwnedByte,
        members: SmallVec<4, OwnedByte>,
    },

    /// SREM key member [member ...]
    SRem {
        key: OwnedByte,
        members: SmallVec<4, OwnedByte>,
    },

    /// SISMEMBER key member
    SIsMember { key: OwnedByte, member: OwnedByte },

    /// SCARD key
    SCard(OwnedByte),

    /// SMEMBERS key
    SMembers(OwnedByte),

    /// SPOP key [count]
    SPop { key: OwnedByte, count: Option<u64> },

    // --- ZSet --------------------------------------------------------
    /// ZADD key score member [score member ...]
    ZAdd {
        key: OwnedByte,
        members: SmallVec<2, (f64, OwnedByte)>,
    },

    /// ZCARD key
    ZCard(OwnedByte),

    /// ZRANGE key start stop [WITHSCORES]
    ZRange {
        key: OwnedByte,
        start: i64,
        stop: i64,
        with_scores: bool,
    },

    /// ZSCORE key member
    ZScore { key: OwnedByte, member: OwnedByte },

    /// ZREM key member [member ...]
    ZRem {
        key: OwnedByte,
        members: SmallVec<4, OwnedByte>,
    },

    /// ZINCRBY key increment member
    ZIncrBy {
        key: OwnedByte,
        delta: f64,
        member: OwnedByte,
    },

    // --- Pub/Sub -----------------------------------------------------
    /// SUBSCRIBE channel [channel ...]
    Subscribe(SmallVec<5, OwnedByte>),

    /// UNSUBSCRIBE [channel ...]
    Unsubscribe(SmallVec<5, OwnedByte>),

    /// PUBLISH channel message
    Publish {
        channel: OwnedByte,
        message: OwnedByte,
    },
}
// ---------------------------------------------------------------------------
// RESP parser — single pass, no backtracking
// ---------------------------------------------------------------------------

fn parse_uint(d: &[u8], pos: &mut usize) -> Option<usize> {
    let mut n = 0usize;
    loop {
        match d.get(*pos)? {
            b'\r' => break,
            b if b.is_ascii_digit() => {
                n = n * 10 + (*b - b'0') as usize;
                *pos += 1;
            }
            _ => return None,
        }
    }
    Some(n)
}

fn expect_crlf(d: &[u8], pos: &mut usize) -> Option<()> {
    if d.get(*pos) == Some(&b'\r') && d.get(*pos + 1) == Some(&b'\n') {
        *pos += 2;
        Some(())
    } else {
        None
    }
}

fn arg_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut n = 0u64;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(n)
}

fn arg_i64(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let (neg, digits) = if s[0] == b'-' {
        (true, &s[1..])
    } else {
        (false, s)
    };
    if digits.is_empty() {
        return None;
    }
    let mut n = 0i64;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if neg { n.checked_neg()? } else { n })
}

fn arg_f64(s: &[u8]) -> Option<f64> {
    std::str::from_utf8(s).ok()?.parse().ok()
}

#[inline]
fn ob(s: &[u8]) -> OwnedByte {
    OwnedByte::from_slice(s)
}

fn parse_set_opts(args: &[&[u8]]) -> Option<SetOpts> {
    let mut opts = SetOpts::default();
    let mut i = 0;
    while i < args.len() {
        let s = args[i];
        if s.len() != 2 {
            return None;
        }
        match s[0] | 0x20 {
            b'e' => { i += 1; opts.ttl = Some(Duration::from_secs(arg_u64(*args.get(i)?)?)); }  // EX
            b'p' => { i += 1; opts.ttl = Some(Duration::from_millis(arg_u64(*args.get(i)?)?)); } // PX
            b'n' => opts.condition = SetCondition::IfNotExists, // NX
            b'x' => opts.condition = SetCondition::IfExists,    // XX
            _ => return None,
        }
        i += 1;
    }
    Some(opts)
}

// Trie de dispatch : cmd[0]|0x20 → len → cmd[1]|0x20 → cmd[2]|0x20 ...
// Chaque nœud discrimine sur le minimum de bytes nécessaires.
// Pas de eq_ignore_ascii_case : on masque le bit de casse (| 0x20) sur chaque byte.
fn dispatch(raw: &[&[u8]]) -> Option<Cmd> {
    let cmd = raw[0];
    let args = &raw[1..];

    // Toutes les commandes Redis font ≥ 3 caractères.
    if cmd.len() < 3 {
        return None;
    }

    macro_rules! need {
        ($n:expr) => {
            if args.len() < $n {
                return None;
            }
        };
    }
    macro_rules! multi {
        ($v:expr) => {{
            let mut v = SmallVec::new();
            for s in args {
                v.push(ob(s));
            }
            $v(v)
        }};
    }
    macro_rules! multi_from {
        ($from:expr, $v:expr) => {{
            let mut v = SmallVec::new();
            for s in &args[$from..] {
                v.push(ob(s));
            }
            $v(v)
        }};
    }

    match cmd[0] | 0x20 {
        // ── D : DEL DECR DECRBY DBSIZE ────────────────────────────────────
        b'd' => match cmd.len() {
            3 => { need!(1); Some(Cmd::Del(multi!(|v| v))) }
            4 => { need!(1); Some(Cmd::Decr(ob(args[0]))) }
            6 => match cmd[1] | 0x20 {
                b'b' => Some(Cmd::DbSize),
                b'e' => { need!(2); Some(Cmd::DecrBy { key: ob(args[0]), delta: arg_i64(args[1])? }) }
                _ => None,
            },
            _ => None,
        },

        // ── E : ECHO EXISTS EXPIRE ────────────────────────────────────────
        b'e' => match cmd.len() {
            4 => { need!(1); Some(Cmd::Echo(ob(args[0]))) }
            6 => match cmd[2] | 0x20 {
                b'i' => { need!(1); Some(Cmd::Exists(multi!(|v| v))) }   // EXISTS  e-x-I
                b'p' => { need!(2); Some(Cmd::Expire { key: ob(args[0]), dur: Duration::from_secs(arg_u64(args[1])?) }) } // EXPIRE e-x-P
                _ => None,
            },
            _ => None,
        },

        // ── F : FLUSHDB ───────────────────────────────────────────────────
        b'f' => if cmd.len() == 7 { Some(Cmd::FlushDb) } else { None },

        // ── G : GET ───────────────────────────────────────────────────────
        b'g' => if cmd.len() == 3 { need!(1); Some(Cmd::Get(ob(args[0]))) } else { None },

        // ── H : HSET HGET HGETALL HDEL HLEN HKEYS HVALS HMGET HMSET HEXISTS HINCRBY ──
        b'h' => match cmd.len() {
            4 => match cmd[1] | 0x20 {
                b's' => { // HSET
                    if args.len() < 3 || (args.len() - 1) % 2 != 0 { return None; }
                    let key = ob(args[0]);
                    let mut fields = SmallVec::new();
                    let mut i = 1;
                    while i < args.len() { fields.push((ob(args[i]), ob(args[i + 1]))); i += 2; }
                    Some(Cmd::HSet { key, fields })
                }
                b'g' => { need!(2); Some(Cmd::HGet { key: ob(args[0]), field: ob(args[1]) }) }
                b'd' => { need!(2); let key = ob(args[0]); Some(Cmd::HDel { key, fields: multi_from!(1, |v| v) }) }
                b'l' => { need!(1); Some(Cmd::HLen(ob(args[0]))) }
                _ => None,
            },
            5 => match cmd[1] | 0x20 {
                b'k' => { need!(1); Some(Cmd::HKeys(ob(args[0]))) }
                b'v' => { need!(1); Some(Cmd::HVals(ob(args[0]))) }
                b'm' => match cmd[2] | 0x20 {   // HM…
                    b'g' => { // HMGET
                        need!(2);
                        let key = ob(args[0]);
                        Some(Cmd::HMGet { key, fields: multi_from!(1, |v| v) })
                    }
                    b's' => { // HMSET — gardé distinct pour que le serveur réponde +OK
                        if args.len() < 3 || (args.len() - 1) % 2 != 0 { return None; }
                        let key = ob(args[0]);
                        let mut fields = SmallVec::new();
                        let mut i = 1;
                        while i < args.len() { fields.push((ob(args[i]), ob(args[i + 1]))); i += 2; }
                        Some(Cmd::HMSet { key, fields })
                    }
                    _ => None,
                },
                _ => None,
            },
            7 => match cmd[1] | 0x20 {
                b'g' => { need!(1); Some(Cmd::HGetAll(ob(args[0]))) }  // HGETALL
                b'e' => { need!(2); Some(Cmd::HExists { key: ob(args[0]), field: ob(args[1]) }) } // HEXISTS
                b'i' => { need!(3); Some(Cmd::HIncrBy { key: ob(args[0]), field: ob(args[1]), delta: arg_i64(args[2])? }) } // HINCRBY
                _ => None,
            },
            _ => None,
        },

        // ── I : INCR INCRBY INFO ──────────────────────────────────────────
        b'i' => match cmd.len() {
            4 => match cmd[2] | 0x20 {
                b'c' => { need!(1); Some(Cmd::Incr(ob(args[0]))) }  // INCR
                b'f' => Some(Cmd::Info),                              // INFO
                _ => None,
            },
            6 => { need!(2); Some(Cmd::IncrBy { key: ob(args[0]), delta: arg_i64(args[1])? }) }
            _ => None,
        },

        // ── K : KEYS ──────────────────────────────────────────────────────
        b'k' => if cmd.len() == 4 { need!(1); Some(Cmd::Keys(ob(args[0]))) } else { None },

        // ── M : MGET MSET ─────────────────────────────────────────────────
        b'm' => if cmd.len() == 4 {
            match cmd[1] | 0x20 {
                b'g' => { need!(1); Some(Cmd::MGet(multi!(|v| v))) }
                b's' => {
                    if args.is_empty() || args.len() % 2 != 0 { return None; }
                    let mut pairs = SmallVec::new();
                    let mut i = 0;
                    while i < args.len() { pairs.push((ob(args[i]), ob(args[i + 1]))); i += 2; }
                    Some(Cmd::MSet(pairs))
                }
                _ => None,
            }
        } else { None },

        // ── P : PING PTTL PERSIST PEXPIRE PUBLISH ────────────────────────
        b'p' => match cmd.len() {
            4 => match cmd[1] | 0x20 {
                b'i' => Some(Cmd::Ping(args.first().map(|s| ob(s)))), // PING
                b't' => { need!(1); Some(Cmd::Pttl(ob(args[0]))) }   // PTTL
                _ => None,
            },
            7 => match cmd[1] | 0x20 {
                b'e' => match cmd[2] | 0x20 {
                    b'r' => { need!(1); Some(Cmd::Persist(ob(args[0]))) } // PERSIST  p-e-R
                    b'x' => { need!(2); Some(Cmd::Expire { key: ob(args[0]), dur: Duration::from_millis(arg_u64(args[1])?) }) } // PEXPIRE p-e-X
                    _ => None,
                },
                b'u' => { need!(2); Some(Cmd::Publish { channel: ob(args[0]), message: ob(args[1]) }) } // PUBLISH
                _ => None,
            },
            _ => None,
        },

        // ── Q : QUIT ──────────────────────────────────────────────────────
        b'q' => if cmd.len() == 4 { Some(Cmd::Quit) } else { None },

        // ── S : SET SETNX SETEX SELECT SADD SREM SPOP SCARD SMEMBERS SISMEMBER SUBSCRIBE ──
        b's' => match cmd.len() {
            3 => { // SET
                need!(2);
                let opts = parse_set_opts(&args[2..])?;
                Some(Cmd::Set { key: ob(args[0]), val: ob(args[1]), opts })
            }
            4 => match cmd[1] | 0x20 {
                b'a' => { need!(2); let key = ob(args[0]); Some(Cmd::SAdd { key, members: multi_from!(1, |v| v) }) }
                b'r' => { need!(2); let key = ob(args[0]); Some(Cmd::SRem { key, members: multi_from!(1, |v| v) }) }
                b'p' => { need!(1); Some(Cmd::SPop { key: ob(args[0]), count: args.get(1).and_then(|s| arg_u64(s)) }) }
                _ => None,
            },
            5 => match cmd[1] | 0x20 {
                b'c' => { need!(1); Some(Cmd::SCard(ob(args[0]))) } // SCARD
                b'e' => match cmd[3] | 0x20 { // SET-x : s-e-t-?
                    b'e' => { need!(3); Some(Cmd::Set { key: ob(args[0]), val: ob(args[2]), opts: SetOpts { ttl: Some(Duration::from_secs(arg_u64(args[1])?)), condition: SetCondition::Always } }) } // SETEX
                    b'n' => { need!(2); Some(Cmd::Set { key: ob(args[0]), val: ob(args[1]), opts: SetOpts { ttl: None, condition: SetCondition::IfNotExists } }) } // SETNX
                    _ => None,
                },
                _ => None,
            },
            6 => { need!(1); Some(Cmd::Select(arg_u64(args[0])?)) } // SELECT
            8 => { need!(1); Some(Cmd::SMembers(ob(args[0]))) }     // SMEMBERS
            9 => match cmd[1] | 0x20 {
                b'i' => { need!(2); Some(Cmd::SIsMember { key: ob(args[0]), member: ob(args[1]) }) } // SISMEMBER
                b'u' => { need!(1); Some(Cmd::Subscribe(multi!(|v| v))) }                            // SUBSCRIBE
                _ => None,
            },
            _ => None,
        },

        // ── T : TTL TYPE ──────────────────────────────────────────────────
        b't' => match cmd.len() {
            3 => { need!(1); Some(Cmd::Ttl(ob(args[0]))) }
            4 => { need!(1); Some(Cmd::Type(ob(args[0]))) }
            _ => None,
        },

        // ── U : UNLINK UNSUBSCRIBE ────────────────────────────────────────
        b'u' => match cmd.len() {
            6  => { need!(1); Some(Cmd::Unlink(multi!(|v| v))) }
            11 => Some(Cmd::Unsubscribe(multi!(|v| v))),
            _ => None,
        },

        // ── Z : ZADD ZREM ZCARD ZRANGE ZSCORE ZINCRBY ────────────────────
        b'z' => match cmd.len() {
            4 => match cmd[1] | 0x20 {
                b'a' => { // ZADD
                    if args.len() < 3 || (args.len() - 1) % 2 != 0 { return None; }
                    let key = ob(args[0]);
                    let mut members = SmallVec::new();
                    let mut i = 1;
                    while i < args.len() { members.push((arg_f64(args[i])?, ob(args[i + 1]))); i += 2; }
                    Some(Cmd::ZAdd { key, members })
                }
                b'r' => { need!(2); let key = ob(args[0]); Some(Cmd::ZRem { key, members: multi_from!(1, |v| v) }) }
                _ => None,
            },
            5 => { need!(1); Some(Cmd::ZCard(ob(args[0]))) } // ZCARD
            6 => match cmd[1] | 0x20 {
                b'r' => { // ZRANGE
                    need!(3);
                    // WITHSCORES : len 10, commence par 'w'
                    let with_scores = args.get(3).map_or(false, |s| s.len() == 10 && (s[0] | 0x20) == b'w');
                    Some(Cmd::ZRange { key: ob(args[0]), start: arg_i64(args[1])?, stop: arg_i64(args[2])?, with_scores })
                }
                b's' => { need!(2); Some(Cmd::ZScore { key: ob(args[0]), member: ob(args[1]) }) } // ZSCORE
                _ => None,
            },
            7 => { need!(3); Some(Cmd::ZIncrBy { key: ob(args[0]), delta: arg_f64(args[1])?, member: ob(args[2]) }) } // ZINCRBY
            _ => None,
        },

        _ => None,
    }
}

impl Cmd {
    /// Parse a Cmd from already-decoded parts (cmd name + args as byte slices).
    pub fn from_raw(parts: &[&[u8]]) -> Option<Self> {
        if parts.is_empty() {
            return None;
        }
        dispatch(parts)
    }

    /// Parse from raw RESP2 bytes, returns (Cmd, bytes_consumed) or None if incomplete/invalid.
    pub fn parse(d: &[u8]) -> Option<(Self, usize)> {
        let mut pos = 0;

        if d.get(pos) != Some(&b'*') {
            return None;
        }
        pos += 1;
        let n = parse_uint(d, &mut pos)?;
        expect_crlf(d, &mut pos)?;
        if n == 0 {
            return None;
        }

        let mut raw: SmallVec<8, &[u8]> = SmallVec::new();
        for _ in 0..n {
            if d.get(pos) != Some(&b'$') {
                return None;
            }
            pos += 1;
            let len = parse_uint(d, &mut pos)?;
            expect_crlf(d, &mut pos)?;
            if pos + len > d.len() {
                return None;
            }
            raw.push(&d[pos..pos + len]);
            pos += len;
            expect_crlf(d, &mut pos)?;
        }

        dispatch(&raw).map(|cmd| (cmd, pos))
    }

    pub fn from_slice(d: &[u8]) -> Option<Self> {
        Self::parse(d).map(|(cmd, _)| cmd)
    }
}
