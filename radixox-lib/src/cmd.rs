use std::time::Duration;

use crate::small_vec::SmallVec;

use crate::shared_byte::SharedByte;

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
// Mono-thread : plus de frontière Send à respecter entre parsing et exécution.
// Règle de représentation par champ :
//   - une clé / un champ / un membre qui ne sert qu'à une traversée ART
//     (lookup, comparaison, suppression) reste un `&'a [u8]` **emprunté**
//     directement dans le buffer de lecture — zéro alloc.
//   - une donnée destinée à être stockée durablement dans l'ART (valeur d'un
//     SET/HSET, membre d'un SADD/ZADD, ou toute donnée qui doit survivre à
//     une commande async elle-même stockée dans une structure partagée
//     persistante — Pub/Sub, UNLINK/KEYS) devient un `SharedByte` possédé.
//
// Règles de normalisation (au parsing) :
//   - SETNX key val  → Set { opts: SetOpts { condition: IfNotExists, .. } }
//   - SETEX key s v  → Set { opts: SetOpts { ttl: Some(Duration::from_secs(s)), .. } }
//   - HMSET          → HSet (alias déprécié, même sémantique)
//   - EXPIRE key s   → Expire { dur: Duration::from_secs(s) }
//   - PEXPIRE key ms → Expire { dur: Duration::from_millis(ms) }
// ---------------------------------------------------------------------------

pub enum Cmd<'a> {
    // --- Connection / admin -------------------------------------------
    /// Commande RESP valide mais non reconnue — répond ERR et avance le curseur.
    Unknown,
    /// INFO [section] — retourne des infos serveur (réponse minimale)
    Info,

    /// PING [message]
    Ping(Option<&'a [u8]>),
    /// QUIT
    Quit,
    /// ECHO message
    Echo(&'a [u8]),
    /// SELECT index
    Select(u64),
    /// DBSIZE
    DbSize,
    /// FLUSHDB
    FlushDb,

    // --- String / clés -----------------------------------------------
    /// GET key
    Get(&'a [u8]),

    /// SET key value [EX secs | PX ms] [NX | XX]
    /// Couvre aussi SETNX et SETEX (normalisés au parsing).
    Set {
        key: &'a [u8],
        val: SharedByte,
        opts: SetOpts,
    },

    /// MGET key [key ...]
    MGet(SmallVec<5, &'a [u8]>),

    /// MSET key value [key value ...]
    MSet(SmallVec<2, (&'a [u8], SharedByte)>),

    /// DEL key [key ...]
    Del(SmallVec<4, &'a [u8]>),

    /// UNLINK key [key ...]  (DEL async — même exécution côté ART)
    /// Commande async (traverse potentiellement un gros sous-arbre sur
    /// plusieurs yields) : owned, ne peut pas emprunter le read buffer.
    Unlink(SmallVec<4, SharedByte>),

    /// EXISTS key [key ...]
    Exists(SmallVec<4, &'a [u8]>),

    /// TYPE key
    Type(&'a [u8]),

    /// KEYS pattern — async (regex scan potentiellement long) : owned.
    Keys(SharedByte),

    // --- TTL ---------------------------------------------------------
    /// TTL key  (retourne secondes)
    Ttl(&'a [u8]),

    /// PTTL key  (retourne millisecondes)
    Pttl(&'a [u8]),

    /// EXPIRE key seconds  /  PEXPIRE key milliseconds
    /// Normalisé en Duration au parsing.
    Expire { key: &'a [u8], dur: Duration },

    /// PERSIST key
    Persist(&'a [u8]),

    // --- Compteurs ---------------------------------------------------
    /// INCR key
    Incr(&'a [u8]),

    /// DECR key
    Decr(&'a [u8]),

    /// INCRBY key delta
    IncrBy { key: &'a [u8], delta: i64 },

    /// DECRBY key delta
    DecrBy { key: &'a [u8], delta: i64 },

    // --- Hash --------------------------------------------------------
    /// HSET key field value [field value ...]
    HSet {
        key: &'a [u8],
        fields: SmallVec<2, (SharedByte, SharedByte)>,
    },

    /// HMSET key field value [field value ...] — alias déprécié de HSET, doit répondre +OK
    HMSet {
        key: &'a [u8],
        fields: SmallVec<2, (SharedByte, SharedByte)>,
    },

    /// HGET key field
    HGet { key: &'a [u8], field: &'a [u8] },

    /// HGETALL key
    HGetAll(&'a [u8]),

    /// HDEL key field [field ...]
    HDel {
        key: &'a [u8],
        fields: SmallVec<4, SharedByte>,
    },

    /// HEXISTS key field
    HExists { key: &'a [u8], field: &'a [u8] },

    /// HLEN key
    HLen(&'a [u8]),

    /// HKEYS key
    HKeys(&'a [u8]),

    /// HVALS key
    HVals(&'a [u8]),

    /// HMGET key field [field ...]
    HMGet {
        key: &'a [u8],
        fields: SmallVec<4, SharedByte>,
    },

    /// HINCRBY key field delta
    HIncrBy {
        key: &'a [u8],
        field: SharedByte,
        delta: i64,
    },

    // --- Set ---------------------------------------------------------
    /// SADD key member [member ...]
    SAdd {
        key: &'a [u8],
        members: SmallVec<4, SharedByte>,
    },

    /// SREM key member [member ...]
    SRem {
        key: &'a [u8],
        members: SmallVec<4, SharedByte>,
    },

    /// SISMEMBER key member
    SIsMember { key: &'a [u8], member: SharedByte },

    /// SCARD key
    SCard(&'a [u8]),

    /// SMEMBERS key
    SMembers(&'a [u8]),

    /// SPOP key [count]
    SPop { key: &'a [u8], count: Option<u64> },

    // --- ZSet --------------------------------------------------------
    /// ZADD key score member [score member ...]
    ZAdd {
        key: &'a [u8],
        members: SmallVec<2, (f64, SharedByte)>,
    },

    /// ZCARD key
    ZCard(&'a [u8]),

    /// ZRANGE key start stop [WITHSCORES]
    ZRange {
        key: &'a [u8],
        start: i64,
        stop: i64,
        with_scores: bool,
    },

    /// ZSCORE key member
    ZScore { key: &'a [u8], member: SharedByte },

    /// ZREM key member [member ...]
    ZRem {
        key: &'a [u8],
        members: SmallVec<4, SharedByte>,
    },

    /// ZINCRBY key increment member
    ZIncrBy {
        key: &'a [u8],
        delta: f64,
        member: SharedByte,
    },

    // --- Pub/Sub -----------------------------------------------------
    // Async, et le canal est retenu dans le registry Pub/Sub bien au-delà
    // de la durée de vie du buffer de lecture courant : owned.
    /// SUBSCRIBE channel [channel ...]
    Subscribe(SmallVec<5, SharedByte>),

    /// UNSUBSCRIBE [channel ...]
    Unsubscribe(SmallVec<5, SharedByte>),

    /// PUBLISH channel message
    Publish {
        channel: SharedByte,
        message: SharedByte,
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

/// Construit une valeur possédée — réservé aux champs qui doivent survivre
/// à la commande (valeur stockée, ou donnée traversant une commande async).
#[inline]
fn sb(s: &[u8]) -> SharedByte {
    SharedByte::from_slice(s)
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
fn dispatch<'a>(raw: &[&'a [u8]]) -> Option<Cmd<'a>> {
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
    macro_rules! multi_borrowed {
        ($v:expr) => {{
            let mut v = SmallVec::new();
            for s in args {
                v.push(*s);
            }
            $v(v)
        }};
    }
    macro_rules! multi_owned_from {
        ($from:expr, $v:expr) => {{
            let mut v = SmallVec::new();
            for s in &args[$from..] {
                v.push(sb(s));
            }
            $v(v)
        }};
    }

    match cmd[0] | 0x20 {
        // ── D : DEL DECR DECRBY DBSIZE ────────────────────────────────────
        b'd' => match cmd.len() {
            3 => { need!(1); Some(Cmd::Del(multi_borrowed!(|v| v))) }
            4 => { need!(1); Some(Cmd::Decr(args[0])) }
            6 => match cmd[1] | 0x20 {
                b'b' => Some(Cmd::DbSize),
                b'e' => { need!(2); Some(Cmd::DecrBy { key: args[0], delta: arg_i64(args[1])? }) }
                _ => None,
            },
            _ => None,
        },

        // ── E : ECHO EXISTS EXPIRE ────────────────────────────────────────
        b'e' => match cmd.len() {
            4 => { need!(1); Some(Cmd::Echo(args[0])) }
            6 => match cmd[2] | 0x20 {
                b'i' => { need!(1); Some(Cmd::Exists(multi_borrowed!(|v| v))) }   // EXISTS  e-x-I
                b'p' => { need!(2); Some(Cmd::Expire { key: args[0], dur: Duration::from_secs(arg_u64(args[1])?) }) } // EXPIRE e-x-P
                _ => None,
            },
            _ => None,
        },

        // ── F : FLUSHDB ───────────────────────────────────────────────────
        b'f' => if cmd.len() == 7 { Some(Cmd::FlushDb) } else { None },

        // ── G : GET ───────────────────────────────────────────────────────
        b'g' => if cmd.len() == 3 { need!(1); Some(Cmd::Get(args[0])) } else { None },

        // ── H : HSET HGET HGETALL HDEL HLEN HKEYS HVALS HMGET HMSET HEXISTS HINCRBY ──
        b'h' => match cmd.len() {
            4 => match cmd[1] | 0x20 {
                b's' => { // HSET
                    if args.len() < 3 || (args.len() - 1) % 2 != 0 { return None; }
                    let key = args[0];
                    let mut fields = SmallVec::new();
                    let mut i = 1;
                    while i < args.len() { fields.push((sb(args[i]), sb(args[i + 1]))); i += 2; }
                    Some(Cmd::HSet { key, fields })
                }
                b'g' => { need!(2); Some(Cmd::HGet { key: args[0], field: args[1] }) }
                b'd' => { need!(2); let key = args[0]; Some(Cmd::HDel { key, fields: multi_owned_from!(1, |v| v) }) }
                b'l' => { need!(1); Some(Cmd::HLen(args[0])) }
                _ => None,
            },
            5 => match cmd[1] | 0x20 {
                b'k' => { need!(1); Some(Cmd::HKeys(args[0])) }
                b'v' => { need!(1); Some(Cmd::HVals(args[0])) }
                b'm' => match cmd[2] | 0x20 {   // HM…
                    b'g' => { // HMGET
                        need!(2);
                        let key = args[0];
                        Some(Cmd::HMGet { key, fields: multi_owned_from!(1, |v| v) })
                    }
                    b's' => { // HMSET — gardé distinct pour que le serveur réponde +OK
                        if args.len() < 3 || (args.len() - 1) % 2 != 0 { return None; }
                        let key = args[0];
                        let mut fields = SmallVec::new();
                        let mut i = 1;
                        while i < args.len() { fields.push((sb(args[i]), sb(args[i + 1]))); i += 2; }
                        Some(Cmd::HMSet { key, fields })
                    }
                    _ => None,
                },
                _ => None,
            },
            7 => match cmd[1] | 0x20 {
                b'g' => { need!(1); Some(Cmd::HGetAll(args[0])) }  // HGETALL
                b'e' => { need!(2); Some(Cmd::HExists { key: args[0], field: args[1] }) } // HEXISTS
                b'i' => { need!(3); Some(Cmd::HIncrBy { key: args[0], field: sb(args[1]), delta: arg_i64(args[2])? }) } // HINCRBY
                _ => None,
            },
            _ => None,
        },

        // ── I : INCR INCRBY INFO ──────────────────────────────────────────
        b'i' => match cmd.len() {
            4 => match cmd[2] | 0x20 {
                b'c' => { need!(1); Some(Cmd::Incr(args[0])) }  // INCR
                b'f' => Some(Cmd::Info),                              // INFO
                _ => None,
            },
            6 => { need!(2); Some(Cmd::IncrBy { key: args[0], delta: arg_i64(args[1])? }) }
            _ => None,
        },

        // ── K : KEYS ──────────────────────────────────────────────────────
        b'k' => if cmd.len() == 4 { need!(1); Some(Cmd::Keys(sb(args[0]))) } else { None },

        // ── M : MGET MSET ─────────────────────────────────────────────────
        b'm' => if cmd.len() == 4 {
            match cmd[1] | 0x20 {
                b'g' => { need!(1); Some(Cmd::MGet(multi_borrowed!(|v| v))) }
                b's' => {
                    if args.is_empty() || args.len() % 2 != 0 { return None; }
                    let mut pairs = SmallVec::new();
                    let mut i = 0;
                    while i < args.len() { pairs.push((args[i], sb(args[i + 1]))); i += 2; }
                    Some(Cmd::MSet(pairs))
                }
                _ => None,
            }
        } else { None },

        // ── P : PING PTTL PERSIST PEXPIRE PUBLISH ────────────────────────
        b'p' => match cmd.len() {
            4 => match cmd[1] | 0x20 {
                b'i' => Some(Cmd::Ping(args.first().copied())), // PING
                b't' => { need!(1); Some(Cmd::Pttl(args[0])) }   // PTTL
                _ => None,
            },
            7 => match cmd[1] | 0x20 {
                b'e' => match cmd[2] | 0x20 {
                    b'r' => { need!(1); Some(Cmd::Persist(args[0])) } // PERSIST  p-e-R
                    b'x' => { need!(2); Some(Cmd::Expire { key: args[0], dur: Duration::from_millis(arg_u64(args[1])?) }) } // PEXPIRE p-e-X
                    _ => None,
                },
                b'u' => { need!(2); Some(Cmd::Publish { channel: sb(args[0]), message: sb(args[1]) }) } // PUBLISH
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
                Some(Cmd::Set { key: args[0], val: sb(args[1]), opts })
            }
            4 => match cmd[1] | 0x20 {
                b'a' => { need!(2); let key = args[0]; Some(Cmd::SAdd { key, members: multi_owned_from!(1, |v| v) }) }
                b'r' => { need!(2); let key = args[0]; Some(Cmd::SRem { key, members: multi_owned_from!(1, |v| v) }) }
                b'p' => { need!(1); Some(Cmd::SPop { key: args[0], count: args.get(1).and_then(|s| arg_u64(s)) }) }
                _ => None,
            },
            5 => match cmd[1] | 0x20 {
                b'c' => { need!(1); Some(Cmd::SCard(args[0])) } // SCARD
                b'e' => match cmd[3] | 0x20 { // SET-x : s-e-t-?
                    b'e' => { need!(3); Some(Cmd::Set { key: args[0], val: sb(args[2]), opts: SetOpts { ttl: Some(Duration::from_secs(arg_u64(args[1])?)), condition: SetCondition::Always } }) } // SETEX
                    b'n' => { need!(2); Some(Cmd::Set { key: args[0], val: sb(args[1]), opts: SetOpts { ttl: None, condition: SetCondition::IfNotExists } }) } // SETNX
                    _ => None,
                },
                _ => None,
            },
            6 => { need!(1); Some(Cmd::Select(arg_u64(args[0])?)) } // SELECT
            8 => { need!(1); Some(Cmd::SMembers(args[0])) }     // SMEMBERS
            9 => match cmd[1] | 0x20 {
                b'i' => { need!(2); Some(Cmd::SIsMember { key: args[0], member: sb(args[1]) }) } // SISMEMBER
                b'u' => { need!(1); Some(Cmd::Subscribe(multi_owned_from!(0, |v| v))) }          // SUBSCRIBE
                _ => None,
            },
            _ => None,
        },

        // ── T : TTL TYPE ──────────────────────────────────────────────────
        b't' => match cmd.len() {
            3 => { need!(1); Some(Cmd::Ttl(args[0])) }
            4 => { need!(1); Some(Cmd::Type(args[0])) }
            _ => None,
        },

        // ── U : UNLINK UNSUBSCRIBE ────────────────────────────────────────
        b'u' => match cmd.len() {
            6  => { need!(1); Some(Cmd::Unlink(multi_owned_from!(0, |v| v))) }
            11 => Some(Cmd::Unsubscribe(multi_owned_from!(0, |v| v))),
            _ => None,
        },

        // ── Z : ZADD ZREM ZCARD ZRANGE ZSCORE ZINCRBY ────────────────────
        b'z' => match cmd.len() {
            4 => match cmd[1] | 0x20 {
                b'a' => { // ZADD
                    if args.len() < 3 || (args.len() - 1) % 2 != 0 { return None; }
                    let key = args[0];
                    let mut members = SmallVec::new();
                    let mut i = 1;
                    while i < args.len() { members.push((arg_f64(args[i])?, sb(args[i + 1]))); i += 2; }
                    Some(Cmd::ZAdd { key, members })
                }
                b'r' => { need!(2); let key = args[0]; Some(Cmd::ZRem { key, members: multi_owned_from!(1, |v| v) }) }
                _ => None,
            },
            5 => { need!(1); Some(Cmd::ZCard(args[0])) } // ZCARD
            6 => match cmd[1] | 0x20 {
                b'r' => { // ZRANGE
                    need!(3);
                    // WITHSCORES : len 10, commence par 'w'
                    let with_scores = args.get(3).map_or(false, |s| s.len() == 10 && (s[0] | 0x20) == b'w');
                    Some(Cmd::ZRange { key: args[0], start: arg_i64(args[1])?, stop: arg_i64(args[2])?, with_scores })
                }
                b's' => { need!(2); Some(Cmd::ZScore { key: args[0], member: sb(args[1]) }) } // ZSCORE
                _ => None,
            },
            7 => { need!(3); Some(Cmd::ZIncrBy { key: args[0], delta: arg_f64(args[1])?, member: sb(args[2]) }) } // ZINCRBY
            _ => None,
        },

        _ => None,
    }
}

impl<'a> Cmd<'a> {
    /// Parse a Cmd from already-decoded parts (cmd name + args as byte slices).
    pub fn from_raw(parts: &[&'a [u8]]) -> Option<Self> {
        if parts.is_empty() {
            return None;
        }
        dispatch(parts)
    }

    /// Parse from raw RESP2 bytes, returns (Cmd, bytes_consumed) or None if incomplete/invalid.
    pub fn parse(d: &'a [u8]) -> Option<(Self, usize)> {
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

        let cmd = dispatch(&raw).unwrap_or(Cmd::Unknown);
        Some((cmd, pos))
    }

    pub fn from_slice(d: &'a [u8]) -> Option<Self> {
        Self::parse(d).map(|(cmd, _)| cmd)
    }
}
