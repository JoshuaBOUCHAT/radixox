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
//   - SETNX key val  → SetNx (pas normalisé en SET NX : SETNX répond 1/0, SET NX OK/nil)
//   - SETEX key s v  → Set { opts: SetOpts { ttl: Some(Duration::from_secs(s)), .. } }
//   - HMSET          → HSet (alias déprécié, même sémantique)
//   - EXPIRE key s   → Expire { dur: Duration::from_secs(s) }
//   - PEXPIRE key ms → Expire { dur: Duration::from_millis(ms) }
// ---------------------------------------------------------------------------

pub enum Cmd<'a> {
    // --- Connection / admin -------------------------------------------
    /// Rien à exécuter, pas de réponse : ligne vide, `*0`.
    Empty,
    /// Commande reçue en entier mais inconnue ou mal formée : le serveur répond
    /// `err.message(name)` et la connexion continue.
    Invalid { name: &'a [u8], err: CmdError },
    /// INFO [section] — retourne des infos serveur (réponse minimale)
    Info,
    /// AUTH password / AUTH username password
    Auth { user: Option<&'a [u8]>, pass: &'a [u8] },
    /// HELLO [protover [AUTH user pass] [SETNAME name]] — arguments bruts,
    /// validés par le serveur (messages d'erreur de Redis).
    Hello(SmallVec<4, &'a [u8]>),
    /// CLIENT subcommand [args…] — arguments bruts.
    Client(SmallVec<4, &'a [u8]>),
    /// COMMAND [subcommand …] — arguments bruts.
    Command(SmallVec<4, &'a [u8]>),

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
    /// BGSAVE
    BgSave,
    /// SAVE (bloquante)
    Save,

    // --- String / clés -----------------------------------------------
    /// GET key
    Get(&'a [u8]),

    /// SET key value [EX secs | PX ms] [NX | XX]
    /// Couvre aussi SETEX (normalisé au parsing).
    Set {
        key: &'a [u8],
        val: SharedByte,
        opts: SetOpts,
    },

    /// SETNX key value — répond 1/0 (SET NX répond OK/nil).
    SetNx { key: &'a [u8], val: SharedByte },

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

    /// KRANGE prefix from count / KREVRANGE prefix from count : `count`
    /// entrées string sous `prefix`, triées, strictement après (resp. avant)
    /// `from` ("" = depuis le début / la fin). Réponse [k1, v1, k2, v2, …].
    KRange { prefix: &'a [u8], from: &'a [u8], count: usize, rev: bool },

    /// SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]. Le curseur reste
    /// brut : le serveur répond lui-même `ERR invalid cursor`.
    Scan { cursor: &'a [u8], pattern: Option<&'a [u8]>, count: Option<usize>, ty: Option<&'a [u8]> },

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
// Découpage du flux : RESP (`*N` + bulk strings) ou commande inline
// ---------------------------------------------------------------------------

/// Plus grosse bulk string acceptée (`proto-max-bulk-len` de Redis).
pub const MAX_BULK_LEN: usize = 512 << 20;
/// Plus grand nombre d'éléments d'une commande RESP (comme Redis).
pub const MAX_MULTIBULK_LEN: usize = 1 << 20;
/// Plus longue ligne inline (`PROTO_INLINE_MAX_SIZE` de Redis).
pub const MAX_INLINE_LEN: usize = 64 << 10;

/// Résultat du découpage d'un tampon.
pub enum Parse<'a> {
    /// Une commande, et les octets consommés (lignes vides et `*0` sautés compris).
    Done(Cmd<'a>, usize),
    /// Il manque des octets : rien n'est consommé, on attend la suite.
    Incomplete,
    /// Flux invalide : répondre `-ERR Protocol error: <msg>` et fermer la connexion.
    Invalid(&'static str),
}

/// En-tête `*N` ou `$N`.
enum Header {
    Num(usize),
    /// Négatif (`*-1`, `$-1`).
    Neg,
    Incomplete,
    Invalid,
}

/// Entier décimal d'en-tête terminé par `\r\n`. Débordement → `Invalid` : sans ce
/// contrôle (release = arithmétique modulaire), `$18446744073709551615`
/// faisait passer le test de longueur puis paniquer le découpage.
fn header(d: &[u8], pos: &mut usize) -> Header {
    let mut p = *pos;
    let neg = d.get(p) == Some(&b'-');
    if neg {
        p += 1;
    }
    let start = p;
    let mut n = 0usize;
    loop {
        match d.get(p) {
            None => return Header::Incomplete,
            Some(b'\r') => break,
            Some(&b) if b.is_ascii_digit() => {
                n = match n.checked_mul(10).and_then(|n| n.checked_add((b - b'0') as usize)) {
                    Some(n) => n,
                    None => return Header::Invalid,
                };
                p += 1;
            }
            Some(_) => return Header::Invalid,
        }
    }
    if p == start {
        return Header::Invalid;
    }
    match d.get(p + 1) {
        None => return Header::Incomplete,
        Some(b'\n') => {}
        Some(_) => return Header::Invalid,
    }
    *pos = p + 2;
    if neg { Header::Neg } else { Header::Num(n) }
}

/// Découpe une commande RESP qui commence à `d[pos] == b'*'`.
fn parse_multibulk<'a>(d: &'a [u8], mut pos: usize) -> Parse<'a> {
    pos += 1;
    let n = match header(d, &mut pos) {
        Header::Num(0) | Header::Neg => return Parse::Done(Cmd::Empty, pos),
        Header::Num(n) if n <= MAX_MULTIBULK_LEN => n,
        Header::Num(_) | Header::Invalid => return Parse::Invalid("invalid multibulk length"),
        Header::Incomplete => return Parse::Incomplete,
    };
    let mut raw: SmallVec<8, &[u8]> = SmallVec::new();
    for _ in 0..n {
        match d.get(pos) {
            None => return Parse::Incomplete,
            Some(b'$') => pos += 1,
            Some(_) => return Parse::Invalid("expected '$'"),
        }
        let len = match header(d, &mut pos) {
            Header::Num(len) if len <= MAX_BULK_LEN => len,
            Header::Num(_) | Header::Neg | Header::Invalid => {
                return Parse::Invalid("invalid bulk length");
            }
            Header::Incomplete => return Parse::Incomplete,
        };
        // `pos <= d.len()` (crlf lu juste avant) : pas de débordement possible.
        if d.len() - pos < len + 2 {
            return Parse::Incomplete;
        }
        if &d[pos + len..pos + len + 2] != b"\r\n" {
            return Parse::Invalid("expected CRLF after bulk string");
        }
        raw.push(&d[pos..pos + len]);
        pos += len + 2;
    }
    Parse::Done(dispatch(&raw), pos)
}

/// Commande inline (`SET k v\r\n` tapé dans telnet ou `nc`), comme Redis :
/// arguments séparés par des espaces, `"…"` ou `'…'` pour un argument qui en
/// contient. Les arguments empruntent la ligne : un échappement (`\n`, `\x41`…)
/// à l'intérieur de guillemets est refusé.
fn parse_inline<'a>(d: &'a [u8], pos: usize) -> Parse<'a> {
    let rest = &d[pos..];
    let Some(nl) = rest.iter().take(MAX_INLINE_LEN + 1).position(|&b| b == b'\n') else {
        return if rest.len() > MAX_INLINE_LEN {
            Parse::Invalid("too big inline request")
        } else {
            Parse::Incomplete
        };
    };
    let line = rest[..nl].strip_suffix(b"\r").unwrap_or(&rest[..nl]);
    let consumed = pos + nl + 1;
    let mut raw: SmallVec<8, &[u8]> = SmallVec::new();
    let mut i = 0;
    loop {
        while i < line.len() && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == line.len() {
            break;
        }
        let q = line[i];
        if q == b'"' || q == b'\'' {
            let start = i + 1;
            let Some(len) = line[start..].iter().position(|&b| b == q) else {
                return Parse::Invalid("unbalanced quotes in request");
            };
            let arg = &line[start..start + len];
            if arg.contains(&b'\\') {
                return Parse::Invalid("escapes are not supported in inline requests");
            }
            i = start + len + 1;
            // Un argument entre guillemets doit être suivi d'un espace (Redis).
            if i < line.len() && !line[i].is_ascii_whitespace() {
                return Parse::Invalid("unbalanced quotes in request");
            }
            raw.push(arg);
        } else {
            let start = i;
            while i < line.len() && !line[i].is_ascii_whitespace() {
                i += 1;
            }
            raw.push(&line[start..i]);
        }
    }
    if raw.is_empty() {
        // Ligne vide : ignorée, comme Redis (`redis-cli --pipe` envoie `\r\n`).
        return Parse::Done(Cmd::Empty, consumed);
    }
    Parse::Done(dispatch(&raw), consumed)
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

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

fn int(s: &[u8]) -> Result<i64, CmdError> {
    arg_i64(s).ok_or(CmdError::NotInt)
}

fn uint(s: &[u8]) -> Result<u64, CmdError> {
    arg_u64(s).ok_or(CmdError::NotInt)
}

fn float(s: &[u8]) -> Result<f64, CmdError> {
    arg_f64(s).ok_or(CmdError::NotFloat)
}

/// Construit une valeur possédée — réservé aux champs qui doivent survivre
/// à la commande (valeur stockée, ou donnée traversant une commande async).
fn sb(s: &[u8]) -> SharedByte {
    SharedByte::from_slice(s)
}

/// SCAN : options par paires (la dernière l'emporte, comme Redis) ; COUNT ≥ 1.
fn parse_scan<'a>(args: &[&'a [u8]]) -> Result<Cmd<'a>, CmdError> {
    let (mut pattern, mut count, mut ty) = (None, None, None);
    let mut i = 1;
    while i < args.len() {
        let (opt, val) = (args[i], *args.get(i + 1).ok_or(CmdError::Syntax)?);
        if opt.eq_ignore_ascii_case(b"match") {
            pattern = Some(val);
        } else if opt.eq_ignore_ascii_case(b"count") {
            match uint(val)? {
                0 => return Err(CmdError::Syntax),
                n => count = Some(n as usize),
            }
        } else if opt.eq_ignore_ascii_case(b"type") {
            ty = Some(val);
        } else {
            return Err(CmdError::Syntax);
        }
        i += 2;
    }
    Ok(Cmd::Scan { cursor: args[0], pattern, count, ty })
}

fn parse_set_opts(args: &[&[u8]]) -> Result<SetOpts, CmdError> {
    let mut opts = SetOpts::default();
    let mut i = 0;
    while i < args.len() {
        let s = args[i];
        if s.eq_ignore_ascii_case(b"ex") || s.eq_ignore_ascii_case(b"px") {
            i += 1;
            let n = uint(args.get(i).ok_or(CmdError::Syntax)?)?;
            opts.ttl = Some(if (s[0] | 0x20) == b'e' {
                Duration::from_secs(n)
            } else {
                Duration::from_millis(n)
            });
        } else if s.eq_ignore_ascii_case(b"nx") {
            opts.condition = SetCondition::IfNotExists;
        } else if s.eq_ignore_ascii_case(b"xx") {
            opts.condition = SetCondition::IfExists;
        } else {
            return Err(CmdError::Syntax);
        }
        i += 1;
    }
    Ok(opts)
}

/// Paires `champ valeur …` (HSET, HMSET).
fn field_pairs(args: &[&[u8]]) -> SmallVec<2, (SharedByte, SharedByte)> {
    let mut fields = SmallVec::new();
    for pair in args.chunks_exact(2) {
        fields.push((sb(pair[0]), sb(pair[1])));
    }
    fields
}

// ---------------------------------------------------------------------------
// Dispatch : nom exact, insensible à la casse → `Cmd`
// ---------------------------------------------------------------------------

/// Plus long nom de commande connu (`unsubscribe`) ; au-delà : inconnue.
const MAX_NAME_LEN: usize = 16;

/// Commande RESP complète → `Cmd`. Ne rend jamais d'erreur de découpage : une
/// commande inconnue ou mal formée devient `Cmd::Invalid`, à qui le serveur
/// répond une erreur, et la connexion continue.
fn dispatch<'a>(raw: &[&'a [u8]]) -> Cmd<'a> {
    let name = raw[0];
    match dispatch_inner(name, &raw[1..]) {
        Ok(cmd) => cmd,
        Err(err) => Cmd::Invalid { name, err },
    }
}

fn dispatch_inner<'a>(name: &'a [u8], args: &[&'a [u8]]) -> Result<Cmd<'a>, CmdError> {
    if name.len() > MAX_NAME_LEN {
        return Err(CmdError::Unknown);
    }
    let mut buf = [0u8; MAX_NAME_LEN];
    let lc = &mut buf[..name.len()];
    lc.copy_from_slice(name);
    lc.make_ascii_lowercase();

    let n = args.len();
    // Nombre d'arguments (nom exclu) : exactement / au moins / entre.
    macro_rules! exact {
        ($k:expr) => {
            if n != $k {
                return Err(CmdError::Arity);
            }
        };
    }
    macro_rules! at_least {
        ($k:expr) => {
            if n < $k {
                return Err(CmdError::Arity);
            }
        };
    }
    macro_rules! at_most {
        ($k:expr) => {
            if n > $k {
                return Err(CmdError::Arity);
            }
        };
    }
    macro_rules! between {
        ($a:expr, $b:expr) => {
            if n < $a || n > $b {
                return Err(CmdError::Arity);
            }
        };
    }
    macro_rules! borrowed {
        ($from:expr) => {{
            let mut v = SmallVec::new();
            for s in &args[$from..] {
                v.push(*s);
            }
            v
        }};
    }
    macro_rules! owned {
        ($from:expr) => {{
            let mut v = SmallVec::new();
            for s in &args[$from..] {
                v.push(sb(s));
            }
            v
        }};
    }

    Ok(match &*lc {
        // ── String / clés (les plus fréquentes d'abord) ──────────────────
        b"get" => { exact!(1); Cmd::Get(args[0]) }
        b"set" => {
            at_least!(2);
            let opts = parse_set_opts(&args[2..])?;
            Cmd::Set { key: args[0], val: sb(args[1]), opts }
        }
        b"setnx" => { exact!(2); Cmd::SetNx { key: args[0], val: sb(args[1]) } }
        b"setex" => {
            exact!(3);
            let ttl = Some(Duration::from_secs(uint(args[1])?));
            Cmd::Set { key: args[0], val: sb(args[2]), opts: SetOpts { ttl, condition: SetCondition::Always } }
        }
        b"mget" => { at_least!(1); Cmd::MGet(borrowed!(0)) }
        b"mset" => {
            if n == 0 || n % 2 != 0 {
                return Err(CmdError::Arity);
            }
            let mut pairs = SmallVec::new();
            for pair in args.chunks_exact(2) {
                pairs.push((pair[0], sb(pair[1])));
            }
            Cmd::MSet(pairs)
        }
        b"del" => { at_least!(1); Cmd::Del(borrowed!(0)) }
        b"unlink" => { at_least!(1); Cmd::Unlink(owned!(0)) }
        b"exists" => { at_least!(1); Cmd::Exists(borrowed!(0)) }
        b"type" => { exact!(1); Cmd::Type(args[0]) }
        b"keys" => { exact!(1); Cmd::Keys(sb(args[0])) }
        b"krange" | b"krevrange" => {
            exact!(3);
            Cmd::KRange { prefix: args[0], from: args[1], count: uint(args[2])? as usize, rev: lc.len() == 9 }
        }
        b"scan" => { at_least!(1); parse_scan(args)? }

        // ── TTL ──────────────────────────────────────────────────────────
        b"ttl" => { exact!(1); Cmd::Ttl(args[0]) }
        b"pttl" => { exact!(1); Cmd::Pttl(args[0]) }
        b"expire" => { exact!(2); Cmd::Expire { key: args[0], dur: Duration::from_secs(uint(args[1])?) } }
        b"pexpire" => { exact!(2); Cmd::Expire { key: args[0], dur: Duration::from_millis(uint(args[1])?) } }
        b"persist" => { exact!(1); Cmd::Persist(args[0]) }

        // ── Compteurs ────────────────────────────────────────────────────
        b"incr" => { exact!(1); Cmd::Incr(args[0]) }
        b"decr" => { exact!(1); Cmd::Decr(args[0]) }
        b"incrby" => { exact!(2); Cmd::IncrBy { key: args[0], delta: int(args[1])? } }
        b"decrby" => { exact!(2); Cmd::DecrBy { key: args[0], delta: int(args[1])? } }

        // ── Hash ─────────────────────────────────────────────────────────
        b"hset" | b"hmset" => {
            if n < 3 || n % 2 == 0 {
                return Err(CmdError::Arity);
            }
            let (key, fields) = (args[0], field_pairs(&args[1..]));
            // HMSET reste distinct : il répond +OK (jedis attend un status).
            if lc.len() == 4 { Cmd::HSet { key, fields } } else { Cmd::HMSet { key, fields } }
        }
        b"hget" => { exact!(2); Cmd::HGet { key: args[0], field: args[1] } }
        b"hgetall" => { exact!(1); Cmd::HGetAll(args[0]) }
        b"hdel" => { at_least!(2); Cmd::HDel { key: args[0], fields: owned!(1) } }
        b"hexists" => { exact!(2); Cmd::HExists { key: args[0], field: args[1] } }
        b"hlen" => { exact!(1); Cmd::HLen(args[0]) }
        b"hkeys" => { exact!(1); Cmd::HKeys(args[0]) }
        b"hvals" => { exact!(1); Cmd::HVals(args[0]) }
        b"hmget" => { at_least!(2); Cmd::HMGet { key: args[0], fields: owned!(1) } }
        b"hincrby" => { exact!(3); Cmd::HIncrBy { key: args[0], field: sb(args[1]), delta: int(args[2])? } }

        // ── Set ──────────────────────────────────────────────────────────
        b"sadd" => { at_least!(2); Cmd::SAdd { key: args[0], members: owned!(1) } }
        b"srem" => { at_least!(2); Cmd::SRem { key: args[0], members: owned!(1) } }
        b"sismember" => { exact!(2); Cmd::SIsMember { key: args[0], member: sb(args[1]) } }
        b"scard" => { exact!(1); Cmd::SCard(args[0]) }
        b"smembers" => { exact!(1); Cmd::SMembers(args[0]) }
        b"spop" => {
            between!(1, 2);
            let count = match args.get(1) {
                Some(c) => Some(uint(c)?),
                None => None,
            };
            Cmd::SPop { key: args[0], count }
        }

        // ── ZSet ─────────────────────────────────────────────────────────
        b"zadd" => {
            if n < 3 || n % 2 == 0 {
                return Err(CmdError::Arity);
            }
            let mut members = SmallVec::new();
            for pair in args[1..].chunks_exact(2) {
                members.push((float(pair[0])?, sb(pair[1])));
            }
            Cmd::ZAdd { key: args[0], members }
        }
        b"zcard" => { exact!(1); Cmd::ZCard(args[0]) }
        b"zrange" => {
            between!(3, 4);
            let with_scores = match args.get(3) {
                Some(s) if s.eq_ignore_ascii_case(b"withscores") => true,
                Some(_) => return Err(CmdError::Syntax),
                None => false,
            };
            Cmd::ZRange { key: args[0], start: int(args[1])?, stop: int(args[2])?, with_scores }
        }
        b"zscore" => { exact!(2); Cmd::ZScore { key: args[0], member: sb(args[1]) } }
        b"zrem" => { at_least!(2); Cmd::ZRem { key: args[0], members: owned!(1) } }
        b"zincrby" => { exact!(3); Cmd::ZIncrBy { key: args[0], delta: float(args[1])?, member: sb(args[2]) } }

        // ── Pub/Sub ──────────────────────────────────────────────────────
        b"subscribe" => { at_least!(1); Cmd::Subscribe(owned!(0)) }
        b"unsubscribe" => Cmd::Unsubscribe(owned!(0)),
        b"publish" => { exact!(2); Cmd::Publish { channel: sb(args[0]), message: sb(args[1]) } }

        // ── Connexion / serveur ──────────────────────────────────────────
        b"ping" => { at_most!(1); Cmd::Ping(args.first().copied()) }
        b"echo" => { exact!(1); Cmd::Echo(args[0]) }
        b"quit" => Cmd::Quit,
        b"auth" => match *args {
            [pass] => Cmd::Auth { user: None, pass },
            [user, pass] => Cmd::Auth { user: Some(user), pass },
            _ => return Err(CmdError::Arity),
        },
        b"hello" => Cmd::Hello(borrowed!(0)),
        b"client" => Cmd::Client(borrowed!(0)),
        b"command" => Cmd::Command(borrowed!(0)),
        b"select" => { exact!(1); Cmd::Select(uint(args[0])?) }
        b"info" => Cmd::Info,
        b"dbsize" => { exact!(0); Cmd::DbSize }
        b"flushdb" => {
            at_most!(1);
            match args.first() {
                Some(m) if !m.eq_ignore_ascii_case(b"async") && !m.eq_ignore_ascii_case(b"sync") => {
                    return Err(CmdError::Syntax);
                }
                _ => Cmd::FlushDb,
            }
        }
        b"bgsave" => { at_most!(1); Cmd::BgSave }
        b"save" => { exact!(0); Cmd::Save }

        _ => return Err(CmdError::Unknown),
    })
}

/// Erreur d'une commande reçue en entier mais impossible à exécuter. La
/// connexion continue : le serveur répond `-ERR …` (`message`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdError {
    /// Nom de commande inconnu.
    Unknown,
    /// Mauvais nombre d'arguments.
    Arity,
    /// Option inconnue ou combinaison invalide.
    Syntax,
    /// Entier attendu.
    NotInt,
    /// Flottant attendu.
    NotFloat,
}

impl CmdError {
    /// Message d'erreur RESP (sans le `-` initial), comme Redis.
    pub fn message(self, name: &[u8]) -> String {
        let name = String::from_utf8_lossy(name).to_ascii_lowercase();
        match self {
            CmdError::Unknown => format!("ERR unknown command '{name}'"),
            CmdError::Arity => format!("ERR wrong number of arguments for '{name}' command"),
            CmdError::Syntax => "ERR syntax error".into(),
            CmdError::NotInt => "ERR value is not an integer or out of range".into(),
            CmdError::NotFloat => "ERR value is not a valid float".into(),
        }
    }
}

impl<'a> Cmd<'a> {
    /// Découpe une commande au début de `d` (RESP ou inline).
    pub fn parse(d: &'a [u8]) -> Parse<'a> {
        let mut pos = 0;
        // Lignes vides ignorées, comme Redis : `redis-cli --pipe` envoie `\r\n`
        // avant son `ECHO` final.
        while d[pos..].starts_with(b"\r\n") {
            pos += 2;
        }
        match d.get(pos) {
            None => Parse::Incomplete,
            Some(b'*') => parse_multibulk(d, pos),
            Some(_) => parse_inline(d, pos),
        }
    }

    /// Une commande complète au début de `d`, ou `None` (incomplet ou invalide).
    pub fn from_slice(d: &'a [u8]) -> Option<Self> {
        match Self::parse(d) {
            Parse::Done(cmd, _) => Some(cmd),
            _ => None,
        }
    }

    /// `Cmd` à partir d'arguments déjà découpés (nom en premier).
    pub fn from_raw(parts: &[&'a [u8]]) -> Option<Self> {
        (!parts.is_empty()).then(|| dispatch(parts))
    }

    /// Change la façon d'interpréter les commandes suivantes de la connexion
    /// (authentification, mode abonné, fermeture) : dans un lot, la dernière
    /// exécutée pour sa connexion (RUNTIME_ARCH.md §2.1). Les commandes qui
    /// parquent la connexion (KEYS, UNLINK…) sont décidées par le serveur.
    pub fn is_barrier(&self) -> bool {
        matches!(
            self,
            Cmd::Auth { .. } | Cmd::Hello(_) | Cmd::Subscribe(_) | Cmd::Unsubscribe(_) | Cmd::Quit
        )
    }

    /// Autorisée avant `AUTH` quand un mot de passe est configuré.
    pub fn allowed_unauthenticated(&self) -> bool {
        matches!(self, Cmd::Auth { .. } | Cmd::Hello(_) | Cmd::Quit | Cmd::Empty)
    }

    /// Autorisée en mode abonné (Pub/Sub).
    pub fn allowed_in_pubsub(&self) -> bool {
        matches!(
            self,
            Cmd::Subscribe(_) | Cmd::Unsubscribe(_) | Cmd::Ping(_) | Cmd::Quit | Cmd::Empty
        )
    }

    /// Modifie la base (pour le futur collecteur d'effets, RUNTIME_ARCH.md §18).
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Cmd::Set { .. }
                | Cmd::SetNx { .. }
                | Cmd::MSet(_)
                | Cmd::Del(_)
                | Cmd::Unlink(_)
                | Cmd::Expire { .. }
                | Cmd::Persist(_)
                | Cmd::Incr(_)
                | Cmd::Decr(_)
                | Cmd::IncrBy { .. }
                | Cmd::DecrBy { .. }
                | Cmd::HSet { .. }
                | Cmd::HMSet { .. }
                | Cmd::HDel { .. }
                | Cmd::HIncrBy { .. }
                | Cmd::SAdd { .. }
                | Cmd::SRem { .. }
                | Cmd::SPop { .. }
                | Cmd::ZAdd { .. }
                | Cmd::ZRem { .. }
                | Cmd::ZIncrBy { .. }
                | Cmd::FlushDb
        )
    }

    /// Clé unique de la commande, pour le préchargement par lots
    /// (`OxidArt::prefetch_keys`). `None` : pas de clé, ou plusieurs.
    pub fn main_key(&self) -> Option<&'a [u8]> {
        match *self {
            Cmd::Get(k)
            | Cmd::Type(k)
            | Cmd::Ttl(k)
            | Cmd::Pttl(k)
            | Cmd::Persist(k)
            | Cmd::Incr(k)
            | Cmd::Decr(k)
            | Cmd::HGetAll(k)
            | Cmd::HLen(k)
            | Cmd::HKeys(k)
            | Cmd::HVals(k) => Some(k),
            Cmd::Set { key, .. }
            | Cmd::SetNx { key, .. }
            | Cmd::Expire { key, .. }
            | Cmd::IncrBy { key, .. }
            | Cmd::DecrBy { key, .. }
            | Cmd::HSet { key, .. }
            | Cmd::HMSet { key, .. }
            | Cmd::HGet { key, .. }
            | Cmd::HExists { key, .. } => Some(key),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cmd, CmdError, Parse};

    fn done(d: &[u8]) -> (Cmd<'_>, usize) {
        match Cmd::parse(d) {
            Parse::Done(cmd, n) => (cmd, n),
            Parse::Incomplete => panic!("incomplet : {:?}", d.escape_ascii().to_string()),
            Parse::Invalid(e) => panic!("invalide ({e}) : {:?}", d.escape_ascii().to_string()),
        }
    }

    fn invalid(d: &[u8]) -> bool {
        matches!(Cmd::parse(d), Parse::Invalid(_))
    }

    fn incomplete(d: &[u8]) -> bool {
        matches!(Cmd::parse(d), Parse::Incomplete)
    }

    #[test]
    fn blank_lines_before_a_command_are_skipped() {
        let d = b"\r\n\r\n*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\n";
        assert!(matches!(Cmd::from_slice(b"*1\r\n$4\r\nsave\r\n"), Some(Cmd::Save)));
        assert!(matches!(Cmd::from_slice(b"*3\r\n$4\r\nSADD\r\n$1\r\ns\r\n$1\r\nm\r\n"), Some(Cmd::SAdd { .. })));
        let (cmd, n) = done(d);
        assert!(matches!(cmd, Cmd::Echo(b"hi")));
        assert_eq!(n, d.len());
        // Lignes vides seules : rien à exécuter, on attend la suite.
        assert!(incomplete(b"\r\n"));
    }

    /// Longueurs énormes ou qui débordent : refusées, sans panic (un paquet
    /// envoyé avant AUTH faisait tomber le serveur).
    #[test]
    fn huge_or_overflowing_lengths_do_not_panic() {
        let cases: [&[u8]; 6] = [
            b"*1\r\n$18446744073709551615\r\nGET\r\n",
            b"*1\r\n$18446744073709551610\r\nGET\r\n",
            b"*1\r\n$99999999999999999999999\r\nGET\r\n",
            b"*99999999999999999999999\r\n$3\r\nGET\r\n",
            b"*18446744073709551615\r\n$3\r\nGET\r\n",
            b"*2\r\n$3\r\nGET\r\n$9223372036854775807\r\nk\r\n",
        ];
        for d in cases {
            assert!(invalid(d), "{:?}", d.escape_ascii().to_string());
        }
    }

    /// Incomplet (on attend) et invalide (on ferme) ne se confondent plus : une
    /// frame mal formée laissait la connexion attendre indéfiniment.
    #[test]
    fn incomplete_and_invalid_are_distinct() {
        let full = b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n";
        for cut in 0..full.len() {
            assert!(incomplete(&full[..cut]), "coupé à {cut}");
        }
        assert!(matches!(done(full).0, Cmd::Get(b"key")));
        assert!(invalid(b"*2\r\n:3\r\nGET\r\n"));          // pas de '$'
        assert!(invalid(b"*x\r\n"));                       // longueur non numérique
        assert!(invalid(b"*1\r\n$3\r\nGETxx"));            // pas de CRLF après la valeur
        assert!(invalid(b"*1\r\n$-1\r\n"));                // bulk négative
        assert!(invalid(b"*1\r\n$\r\n"));                  // longueur vide
        assert!(invalid(b"*1\r\n$536870913\r\n"));         // > 512 Mo
        assert!(incomplete(b"*1\r\n$536870912\r\n"));      // = 512 Mo : on attend
        assert!(invalid(b"*1048577\r\n"));                 // > 1 M d'arguments
    }

    /// `*0` et `*-1` : rien à exécuter, consommés (comme Redis).
    #[test]
    fn empty_multibulk_is_skipped() {
        assert!(matches!(done(b"*0\r\n"), (Cmd::Empty, 4)));
        assert!(matches!(done(b"*-1\r\n*1\r\n$4\r\nPING\r\n"), (Cmd::Empty, 5)));
    }

    #[test]
    fn inline_commands() {
        let (cmd, n) = done(b"SET k v\r\nGET k\r\n");
        assert!(matches!(cmd, Cmd::Set { key: b"k", .. }));
        assert_eq!(n, 9);
        assert!(matches!(done(b"ping\n").0, Cmd::Ping(None)));
        assert!(matches!(done(b"  GET   k  \r\n").0, Cmd::Get(b"k")));
        match done(b"SET k \"a b\"\r\n").0 {
            Cmd::Set { val, .. } => assert_eq!(val.as_slice(), b"a b"),
            _ => panic!("SET attendu"),
        }
        assert!(matches!(done(b"ECHO 'x y'\r\n").0, Cmd::Echo(b"x y")));
        assert!(matches!(done(b"   \r\n"), (Cmd::Empty, 5)));
        assert!(incomplete(b"GET k"));
        assert!(invalid(b"ECHO \"abc\r\n"));               // guillemet non fermé
        assert!(invalid(b"ECHO \"a\"b\r\n"));              // guillemet collé
        assert!(invalid(b"ECHO \"a\\nb\"\r\n"));           // échappement refusé
        let long = vec![b'a'; super::MAX_INLINE_LEN + 1];
        assert!(invalid(&long));
        assert!(incomplete(&long[..super::MAX_INLINE_LEN]));
    }

    /// Le nom entier est comparé : avant, la dispatch ne regardait que la
    /// première lettre et la longueur (`DUMP k` exécutait `DECR k`).
    #[test]
    fn command_names_are_matched_exactly() {
        let unknown = |d: &[u8]| matches!(Cmd::from_slice(d), Some(Cmd::Invalid { err: CmdError::Unknown, .. }));
        assert!(unknown(b"DUMP k\r\n"));
        assert!(unknown(b"SETRANGE k 0 v\r\n"));
        assert!(unknown(b"FOOBARZ\r\n"));
        assert!(unknown(b"QXYZ\r\n"));
        assert!(unknown(b"EVAL x 0\r\n"));
        assert!(matches!(Cmd::from_slice(b"gEt k\r\n"), Some(Cmd::Get(b"k"))));
        assert!(matches!(
            Cmd::from_slice(b"KREVRANGE p \"\" 3\r\n"),
            Some(Cmd::KRange { prefix: b"p", from: b"", count: 3, rev: true })
        ));
    }

    #[test]
    fn argument_errors() {
        let err = |d: &[u8]| match Cmd::from_slice(d) {
            Some(Cmd::Invalid { name, err }) => err.message(name),
            _ => panic!("erreur attendue : {:?}", d.escape_ascii().to_string()),
        };
        assert_eq!(err(b"GET\r\n"), "ERR wrong number of arguments for 'get' command");
        assert_eq!(err(b"GET a b\r\n"), "ERR wrong number of arguments for 'get' command");
        assert_eq!(err(b"HSET h f\r\n"), "ERR wrong number of arguments for 'hset' command");
        assert_eq!(err(b"INCRBY k x\r\n"), "ERR value is not an integer or out of range");
        assert_eq!(err(b"ZADD z x m\r\n"), "ERR value is not a valid float");
        assert_eq!(err(b"SET k v EX\r\n"), "ERR syntax error");
        assert_eq!(err(b"SET k v FOO\r\n"), "ERR syntax error");
        assert_eq!(err(b"ZRANGE z 0 1 REV\r\n"), "ERR syntax error");
        assert_eq!(err(b"FooBar\r\n"), "ERR unknown command 'foobar'");
    }
}
