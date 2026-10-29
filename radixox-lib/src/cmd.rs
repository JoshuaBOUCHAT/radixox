#[allow(unused_imports)]
use crate::prelude::*;
use core::time::Duration;

use crate::small_vec::SmallVec;

use crate::shared_byte::SharedByte;

// ---------------------------------------------------------------------------
// SET options
// ---------------------------------------------------------------------------

/// Options de ZADD (NX, XX, GT, LT, CH, INCR), combinaisons déjà validées.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZAddFlags {
    pub nx: bool,
    pub xx: bool,
    pub gt: bool,
    pub lt: bool,
    pub ch: bool,
    pub incr: bool,
}

/// Borne de score : `1.5`, `(1.5` (exclue), `-inf`, `+inf`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreBound {
    pub value: f64,
    pub excl: bool,
}

/// Borne lexicographique : `-`, `+`, `[a` (incluse), `(a` (exclue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexBound<'a> {
    Min,
    Max,
    Incl(&'a [u8]),
    Excl(&'a [u8]),
}

/// Opération d'ensemble des ZSet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZOp {
    Union,
    Inter,
    Diff,
}

/// AGGREGATE de ZUNION / ZINTER.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZAggregate {
    Sum,
    Min,
    Max,
}

/// Ce que désigne une plage de ZSet ; bornes toujours dans l'ordre (min, max), même
/// pour les variantes REV (le parseur les remet dans l'ordre).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZRangeBy<'a> {
    /// Rangs (négatifs depuis la fin), dans le sens du parcours.
    Rank(i64, i64),
    Score(ScoreBound, ScoreBound),
    Lex(LexBound<'a>, LexBound<'a>),
}

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

/// `REPLICATE` : ajoute ou retire une cible de réplication.
#[derive(Debug)]
pub enum Replicate<'a> {
    Start { host: &'a [u8], port: u16, auth: Option<&'a [u8]> },
    Stop { host: &'a [u8], port: u16 },
}

/// `RXLOAD` : synchronisation complète, le dump en morceaux.
#[derive(Debug)]
pub enum RxLoad<'a> {
    /// Taille totale du dump.
    Begin(u64),
    Data(&'a [u8]),
    End,
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
    /// CONFIG GET / SET (maxmemory, maxmemory-policy… : MEMORY_ARCH.md §10.5), arguments
    /// bruts, traités par le cœur.
    Config(SmallVec<4, &'a [u8]>),
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
    /// BGSAVE RESERVE <octets> : la save ne démarre que si autant de mémoire est libre
    /// pour ses copies (MEMORY_ARCH.md §10.5).
    BgSaveReserve(u64),
    /// BGREWRITEAOF : réécriture de l'AOF (avec l'AOF, = BGSAVE).
    BgRewriteAof,
    /// SAVE (bloquante)
    Save,
    /// LASTSAVE : date (Unix) de la dernière save réussie
    LastSave,
    /// DEBUG DIGEST [at] : condensé du contenu (clés vivantes à `at`, défaut :
    /// l'horloge du serveur), pour comparer deux serveurs.
    DebugDigest(Option<u64>),

    // --- Réplication (DURABILITY_DESIGN.md §10) ----------------------
    /// REPLICATE host port [AUTH pass] | REPLICATE STOP host port
    Replicate(Replicate<'a>),
    /// REPLICAOF NO ONE (`None`) ; REPLICAOF host port est refusé (la
    /// réplication est poussée par le primaire).
    ReplicaOf(Option<(&'a [u8], &'a [u8])>),
    /// RXHELLO replid
    RxHello(&'a [u8]),
    /// RXLOAD BEGIN len | RXLOAD DATA octets | RXLOAD END
    RxLoad(RxLoad<'a>),
    /// RXBLOCK bloc
    RxBlock(&'a [u8]),
    /// RXTICK now : horloge du primaire, entre deux blocs.
    RxTick(u64),
    /// WAIT numreplicas timeout(ms)
    Wait { replicas: u64, timeout_ms: u64 },
    /// SHUTDOWN [NOSAVE|SAVE] [NOW] [FORCE] [ABORT] : `save` `None` = le réglage du
    /// serveur (comme un arrêt par signal).
    Shutdown { save: Option<bool>, force: bool, abort: bool },

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

    // --- List --------------------------------------------------------
    /// LPUSH / RPUSH (`right`) / LPUSHX / RPUSHX (`exists`) key element [element ...]
    LPush {
        key: &'a [u8],
        vals: SmallVec<4, SharedByte>,
        right: bool,
        exists: bool,
    },
    /// LPOP / RPOP key [count]
    LPop { key: &'a [u8], right: bool, count: Option<u64> },
    /// LLEN key
    LLen(&'a [u8]),
    /// LRANGE key start stop
    LRange { key: &'a [u8], start: i64, stop: i64 },
    /// LINDEX key index
    LIndex { key: &'a [u8], index: i64 },
    /// LSET key index element
    LSet { key: &'a [u8], index: i64, val: SharedByte },
    /// LINSERT key BEFORE|AFTER pivot element
    LInsert { key: &'a [u8], after: bool, pivot: &'a [u8], val: SharedByte },
    /// LREM key count element
    LRem { key: &'a [u8], count: i64, val: &'a [u8] },
    /// LTRIM key start stop
    LTrim { key: &'a [u8], start: i64, stop: i64 },
    /// LPOS key element [RANK rank] [COUNT n] [MAXLEN len] (`rank ≠ 0`)
    LPos { key: &'a [u8], val: &'a [u8], rank: i64, count: Option<u64>, maxlen: u64 },
    /// LMOVE src dst LEFT|RIGHT LEFT|RIGHT ; RPOPLPUSH src dst
    LMove { src: &'a [u8], dst: &'a [u8], from_right: bool, to_right: bool },
    /// BLPOP / BRPOP key [key ...] timeout (secondes)
    BPop { keys: SmallVec<4, &'a [u8]>, right: bool, timeout: f64 },
    /// BLMOVE src dst LEFT|RIGHT LEFT|RIGHT timeout ; BRPOPLPUSH src dst timeout
    BLMove { src: &'a [u8], dst: &'a [u8], from_right: bool, to_right: bool, timeout: f64 },
    /// BZPOPMIN / BZPOPMAX key [key ...] timeout
    BZPop { keys: SmallVec<4, &'a [u8]>, max: bool, timeout: f64 },

    // --- ZSet --------------------------------------------------------
    /// ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [score member ...]
    ZAdd {
        key: &'a [u8],
        flags: ZAddFlags,
        members: SmallVec<2, (f64, &'a [u8])>,
    },

    /// ZCARD key
    ZCard(&'a [u8]),

    /// ZRANGE key a b [BYSCORE|BYLEX] [REV] [LIMIT offset count] [WITHSCORES], et
    /// ZREVRANGE, ZRANGEBYSCORE, ZREVRANGEBYSCORE, ZRANGEBYLEX, ZREVRANGEBYLEX.
    /// `limit` : `(offset, count)`, `count < 0` = tout.
    ZRange {
        key: &'a [u8],
        range: ZRangeBy<'a>,
        rev: bool,
        limit: Option<(i64, i64)>,
        with_scores: bool,
    },

    /// ZRANK / ZREVRANK key member [WITHSCORE]
    ZRank {
        key: &'a [u8],
        member: &'a [u8],
        rev: bool,
        with_score: bool,
    },

    /// ZCOUNT key min max (score) ; ZLEXCOUNT key min max (`range` = `Lex`)
    ZCount { key: &'a [u8], range: ZRangeBy<'a> },

    /// ZPOPMIN / ZPOPMAX key [count]
    ZPop { key: &'a [u8], max: bool, count: Option<u64> },

    /// ZREMRANGEBYRANK / ZREMRANGEBYSCORE / ZREMRANGEBYLEX key a b
    ZRemRange { key: &'a [u8], range: ZRangeBy<'a> },

    /// ZMSCORE key member [member ...]
    ZMScore { key: &'a [u8], members: SmallVec<4, &'a [u8]> },

    /// ZRANDMEMBER key [count [WITHSCORES]]
    ZRandMember { key: &'a [u8], count: Option<i64>, with_scores: bool },

    /// ZSCORE key member
    ZScore { key: &'a [u8], member: SharedByte },

    /// ZREM key member [member ...]
    ZRem {
        key: &'a [u8],
        members: SmallVec<4, SharedByte>,
    },

    /// ZUNION / ZINTER / ZDIFF numkeys key [key ...] [WEIGHTS w ...] [AGGREGATE SUM|MIN|MAX]
    /// [WITHSCORES] ; variantes STORE (`dest`). `weights` vide : tous à 1.
    ZSetOp {
        op: ZOp,
        dest: Option<&'a [u8]>,
        keys: SmallVec<4, &'a [u8]>,
        weights: SmallVec<4, f64>,
        agg: ZAggregate,
        with_scores: bool,
    },

    /// ZINTERCARD numkeys key [key ...] [LIMIT limit] (0 : sans limite)
    ZInterCard { keys: SmallVec<4, &'a [u8]>, limit: u64 },

    /// ZSCAN key cursor [MATCH pattern] [COUNT count] [NOSCORES]
    ZScan { key: &'a [u8], cursor: &'a [u8], pattern: Option<&'a [u8]>, count: Option<usize>, noscores: bool },

    /// ZINCRBY key increment member
    ZIncrBy {
        key: &'a [u8],
        delta: f64,
        member: &'a [u8],
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

/// Parcours d'une frame : `Ok(fin)` (octets consommés depuis le début de `d`),
/// chaque argument remis à `arg`. `Ok` sans argument : rien à exécuter.
type Walk = Result<usize, Stop>;

/// Fin de parcours sans frame complète.
#[derive(Clone, Copy)]
enum Stop {
    Incomplete,
    Invalid(&'static str),
}

/// Commande RESP qui commence à `d[pos] == b'*'`.
fn walk_multibulk<'a>(d: &'a [u8], mut pos: usize, mut arg: impl FnMut(&'a [u8])) -> Walk {
    pos += 1;
    let n = match header(d, &mut pos) {
        Header::Num(0) | Header::Neg => return Ok(pos),
        Header::Num(n) if n <= MAX_MULTIBULK_LEN => n,
        Header::Num(_) | Header::Invalid => return Err(Stop::Invalid("invalid multibulk length")),
        Header::Incomplete => return Err(Stop::Incomplete),
    };
    for _ in 0..n {
        match d.get(pos) {
            None => return Err(Stop::Incomplete),
            Some(b'$') => pos += 1,
            Some(_) => return Err(Stop::Invalid("expected '$'")),
        }
        let len = match header(d, &mut pos) {
            Header::Num(len) if len <= MAX_BULK_LEN => len,
            Header::Num(_) | Header::Neg | Header::Invalid => {
                return Err(Stop::Invalid("invalid bulk length"));
            }
            Header::Incomplete => return Err(Stop::Incomplete),
        };
        // `pos <= d.len()` (crlf lu juste avant) : pas de débordement possible.
        if d.len() - pos < len + 2 {
            return Err(Stop::Incomplete);
        }
        if &d[pos + len..pos + len + 2] != b"\r\n" {
            return Err(Stop::Invalid("expected CRLF after bulk string"));
        }
        arg(&d[pos..pos + len]);
        pos += len + 2;
    }
    Ok(pos)
}

/// Commande inline (`SET k v\r\n` tapé dans telnet ou `nc`), comme Redis :
/// arguments séparés par des espaces, `"…"` ou `'…'` pour un argument qui en
/// contient. Les arguments empruntent la ligne : un échappement (`\n`, `\x41`…)
/// à l'intérieur de guillemets est refusé.
fn walk_inline<'a>(d: &'a [u8], pos: usize, mut arg: impl FnMut(&'a [u8])) -> Walk {
    let rest = &d[pos..];
    let Some(nl) = rest.iter().take(MAX_INLINE_LEN + 1).position(|&b| b == b'\n') else {
        return Err(if rest.len() > MAX_INLINE_LEN {
            Stop::Invalid("too big inline request")
        } else {
            Stop::Incomplete
        });
    };
    let line = rest[..nl].strip_suffix(b"\r").unwrap_or(&rest[..nl]);
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
                return Err(Stop::Invalid("unbalanced quotes in request"));
            };
            let a = &line[start..start + len];
            if a.contains(&b'\\') {
                return Err(Stop::Invalid("escapes are not supported in inline requests"));
            }
            i = start + len + 1;
            // Un argument entre guillemets doit être suivi d'un espace (Redis).
            if i < line.len() && !line[i].is_ascii_whitespace() {
                return Err(Stop::Invalid("unbalanced quotes in request"));
            }
            arg(a);
        } else {
            let start = i;
            while i < line.len() && !line[i].is_ascii_whitespace() {
                i += 1;
            }
            arg(&line[start..i]);
        }
    }
    Ok(pos + nl + 1)
}

/// Une frame au début de `d` (lignes vides de tête sautées).
fn walk<'a>(d: &'a [u8], arg: impl FnMut(&'a [u8])) -> Walk {
    let mut pos = 0;
    // Lignes vides ignorées, comme Redis : `redis-cli --pipe` envoie `\r\n`
    // avant son `ECHO` final.
    while d[pos..].starts_with(b"\r\n") {
        pos += 2;
    }
    match d.get(pos) {
        None => Err(Stop::Incomplete),
        Some(b'*') => walk_multibulk(d, pos, arg),
        Some(_) => walk_inline(d, pos, arg),
    }
}

/// Longueur de la frame au début de `d`, sans la décoder (pas d'allocation) :
/// sert à découper le flux à l'arrivée ; la commande est parsée plus tard,
/// à son exécution.
pub enum FrameLen {
    /// Frame complète de `n` octets (peut ne rien contenir à exécuter).
    Done(usize),
    Incomplete,
    Invalid(&'static str),
}

pub fn frame_len(d: &[u8]) -> FrameLen {
    match walk(d, |_| {}) {
        Ok(n) => FrameLen::Done(n),
        Err(Stop::Incomplete) => FrameLen::Incomplete,
        Err(Stop::Invalid(msg)) => FrameLen::Invalid(msg),
    }
}

/// Frame RESP incomplète au début de `d` : borne basse de sa taille totale, d'après les
/// en-têtes déjà reçus (les données des bulks ne sont pas lues). `None` : pas une frame
/// `*N` incomplète. Sert au budget d'entrée (une frame plus grosse que lui ne peut pas
/// attendre qu'il se libère).
pub fn frame_need(d: &[u8]) -> Option<usize> {
    let mut pos = 0;
    while d[pos..].starts_with(b"\r\n") {
        pos += 2;
    }
    if d.get(pos) != Some(&b'*') {
        return None;
    }
    pos += 1;
    let n = match header(d, &mut pos) {
        Header::Num(n) if n > 0 && n <= MAX_MULTIBULK_LEN => n,
        Header::Incomplete => return Some(d.len()),
        _ => return None,
    };
    for _ in 0..n {
        if pos >= d.len() {
            return Some(pos.max(d.len()));
        }
        if d[pos] != b'$' {
            return None;
        }
        pos += 1;
        match header(d, &mut pos) {
            Header::Num(len) if len <= MAX_BULK_LEN => pos += len + 2,
            Header::Incomplete => return Some(d.len()),
            _ => return None,
        }
    }
    (pos > d.len()).then_some(pos)
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

/// NaN refusé, comme Redis (`ERR value is not a valid float`) : un score NaN casserait
/// l'ordre des ZSet.
fn arg_f64(s: &[u8]) -> Option<f64> {
    core::str::from_utf8(s).ok()?.parse().ok().filter(|f: &f64| !f.is_nan())
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

/// `LEFT` / `RIGHT` de LMOVE : vrai pour la droite.
fn side(s: &[u8]) -> Result<bool, CmdError> {
    if s.eq_ignore_ascii_case(b"right") {
        Ok(true)
    } else if s.eq_ignore_ascii_case(b"left") {
        Ok(false)
    } else {
        Err(CmdError::Syntax)
    }
}

/// Timeout des commandes bloquantes, en secondes (0 = sans fin).
fn timeout(s: &[u8]) -> Result<f64, CmdError> {
    let t = arg_f64(s)
        .filter(|t| t.is_finite())
        .ok_or(CmdError::Msg("ERR timeout is not a float or out of range"))?;
    if t < 0.0 {
        return Err(CmdError::Msg("ERR timeout is negative"));
    }
    Ok(t)
}

/// Borne de score de ZRANGEBYSCORE & co : `(` = exclue.
fn score_bound(s: &[u8]) -> Result<ScoreBound, CmdError> {
    let (excl, v) = match s.split_first() {
        Some((b'(', rest)) => (true, rest),
        _ => (false, s),
    };
    let value = arg_f64(v).ok_or(CmdError::Msg("ERR min or max is not a float"))?;
    Ok(ScoreBound { value, excl })
}

/// Borne lexicographique : `-`, `+`, `[x`, `(x`.
fn lex_bound(s: &[u8]) -> Result<LexBound<'_>, CmdError> {
    match s {
        b"-" => Ok(LexBound::Min),
        b"+" => Ok(LexBound::Max),
        [b'[', rest @ ..] => Ok(LexBound::Incl(rest)),
        [b'(', rest @ ..] => Ok(LexBound::Excl(rest)),
        _ => Err(CmdError::Msg("ERR min or max not valid string range item")),
    }
}

/// Construit une valeur possédée — réservé aux champs qui doivent survivre
/// à la commande (valeur stockée, ou donnée traversant une commande async).
fn port_num(s: &[u8]) -> Result<u16, CmdError> {
    u16::try_from(uint(s)?).map_err(|_| CmdError::NotInt)
}

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
            at_least!(3);
            let mut flags = ZAddFlags::default();
            let mut i = 1;
            while i < n {
                let a = args[i];
                match () {
                    _ if a.eq_ignore_ascii_case(b"nx") => flags.nx = true,
                    _ if a.eq_ignore_ascii_case(b"xx") => flags.xx = true,
                    _ if a.eq_ignore_ascii_case(b"gt") => flags.gt = true,
                    _ if a.eq_ignore_ascii_case(b"lt") => flags.lt = true,
                    _ if a.eq_ignore_ascii_case(b"ch") => flags.ch = true,
                    _ if a.eq_ignore_ascii_case(b"incr") => flags.incr = true,
                    _ => break,
                }
                i += 1;
            }
            let rest = &args[i..];
            if rest.is_empty() || rest.len() % 2 != 0 {
                return Err(CmdError::Syntax);
            }
            if flags.nx && flags.xx {
                return Err(CmdError::Msg("ERR XX and NX options at the same time are not compatible"));
            }
            if (flags.gt && flags.lt) || ((flags.gt || flags.lt) && flags.nx) {
                return Err(CmdError::Msg("ERR GT, LT, and/or NX options at the same time are not compatible"));
            }
            if flags.incr && rest.len() > 2 {
                return Err(CmdError::Msg("ERR INCR option supports a single increment-element pair"));
            }
            let mut members = SmallVec::new();
            for pair in rest.chunks_exact(2) {
                members.push((float(pair[0])?, pair[1]));
            }
            Cmd::ZAdd { key: args[0], flags, members }
        }
        // ── List ─────────────────────────────────────────────────────────
        b"lpush" | b"rpush" | b"lpushx" | b"rpushx" => {
            at_least!(2);
            Cmd::LPush { key: args[0], vals: owned!(1), right: lc[0] == b'r', exists: lc.len() == 6 }
        }
        b"lpop" | b"rpop" => {
            between!(1, 2);
            let count = match args.get(1) {
                Some(c) => Some(arg_u64(c).ok_or(CmdError::Msg("ERR value is out of range, must be positive"))?),
                None => None,
            };
            Cmd::LPop { key: args[0], right: lc[0] == b'r', count }
        }
        b"llen" => { exact!(1); Cmd::LLen(args[0]) }
        b"lrange" => { exact!(3); Cmd::LRange { key: args[0], start: int(args[1])?, stop: int(args[2])? } }
        b"lindex" => { exact!(2); Cmd::LIndex { key: args[0], index: int(args[1])? } }
        b"lset" => { exact!(3); Cmd::LSet { key: args[0], index: int(args[1])?, val: sb(args[2]) } }
        b"linsert" => {
            exact!(4);
            let after = if args[1].eq_ignore_ascii_case(b"after") {
                true
            } else if args[1].eq_ignore_ascii_case(b"before") {
                false
            } else {
                return Err(CmdError::Syntax);
            };
            Cmd::LInsert { key: args[0], after, pivot: args[2], val: sb(args[3]) }
        }
        b"lrem" => { exact!(3); Cmd::LRem { key: args[0], count: int(args[1])?, val: args[2] } }
        b"ltrim" => { exact!(3); Cmd::LTrim { key: args[0], start: int(args[1])?, stop: int(args[2])? } }
        b"lpos" => {
            at_least!(2);
            let (mut rank, mut count, mut maxlen) = (1i64, None, 0u64);
            let mut i = 2;
            while i < n {
                let val = *args.get(i + 1).ok_or(CmdError::Syntax)?;
                let opt = args[i];
                if opt.eq_ignore_ascii_case(b"rank") {
                    rank = int(val)?;
                    if rank == 0 {
                        return Err(CmdError::Msg(
                            "ERR RANK can't be zero: use 1 to start from the first match, 2 from the second ... or use negative to start from the end of the list",
                        ));
                    }
                } else if opt.eq_ignore_ascii_case(b"count") {
                    let c = int(val)?;
                    count = Some(u64::try_from(c).map_err(|_| CmdError::Msg("ERR COUNT can't be negative"))?);
                } else if opt.eq_ignore_ascii_case(b"maxlen") {
                    let m = int(val)?;
                    maxlen = u64::try_from(m).map_err(|_| CmdError::Msg("ERR MAXLEN can't be negative"))?;
                } else {
                    return Err(CmdError::Syntax);
                }
                i += 2;
            }
            Cmd::LPos { key: args[0], val: args[1], rank, count, maxlen }
        }
        b"lmove" => {
            exact!(4);
            Cmd::LMove { src: args[0], dst: args[1], from_right: side(args[2])?, to_right: side(args[3])? }
        }
        b"rpoplpush" => { exact!(2); Cmd::LMove { src: args[0], dst: args[1], from_right: true, to_right: false } }
        b"blpop" | b"brpop" => {
            at_least!(2);
            let mut keys = SmallVec::new();
            for k in &args[..n - 1] {
                keys.push(*k);
            }
            Cmd::BPop { keys, right: lc[1] == b'r', timeout: timeout(args[n - 1])? }
        }
        b"blmove" => {
            exact!(5);
            Cmd::BLMove {
                src: args[0],
                dst: args[1],
                from_right: side(args[2])?,
                to_right: side(args[3])?,
                timeout: timeout(args[4])?,
            }
        }
        b"brpoplpush" => {
            exact!(3);
            Cmd::BLMove { src: args[0], dst: args[1], from_right: true, to_right: false, timeout: timeout(args[2])? }
        }
        b"bzpopmin" | b"bzpopmax" => {
            at_least!(2);
            let mut keys = SmallVec::new();
            for k in &args[..n - 1] {
                keys.push(*k);
            }
            Cmd::BZPop { keys, max: &*lc == b"bzpopmax", timeout: timeout(args[n - 1])? }
        }

        b"zcard" => { exact!(1); Cmd::ZCard(args[0]) }
        b"zrange" => {
            at_least!(3);
            let (mut by_score, mut by_lex, mut rev, mut limit, mut with_scores) = (false, false, false, None, false);
            let mut i = 3;
            while i < n {
                let a = args[i];
                match () {
                    _ if a.eq_ignore_ascii_case(b"byscore") => by_score = true,
                    _ if a.eq_ignore_ascii_case(b"bylex") => by_lex = true,
                    _ if a.eq_ignore_ascii_case(b"rev") => rev = true,
                    _ if a.eq_ignore_ascii_case(b"withscores") => with_scores = true,
                    _ if a.eq_ignore_ascii_case(b"limit") && i + 2 < n => {
                        limit = Some((int(args[i + 1])?, int(args[i + 2])?));
                        i += 2;
                    }
                    _ => return Err(CmdError::Syntax),
                }
                i += 1;
            }
            if by_score && by_lex {
                return Err(CmdError::Syntax);
            }
            if limit.is_some() && !by_score && !by_lex {
                return Err(CmdError::Msg("ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX"));
            }
            if with_scores && by_lex {
                return Err(CmdError::Msg("ERR syntax error, WITHSCORES not supported in combination with BYLEX"));
            }
            // REV avec BYSCORE / BYLEX : la plage s'écrit max puis min
            let (lo, hi) = if rev && (by_score || by_lex) { (args[2], args[1]) } else { (args[1], args[2]) };
            let range = if by_score {
                ZRangeBy::Score(score_bound(lo)?, score_bound(hi)?)
            } else if by_lex {
                ZRangeBy::Lex(lex_bound(lo)?, lex_bound(hi)?)
            } else {
                ZRangeBy::Rank(int(args[1])?, int(args[2])?)
            };
            Cmd::ZRange { key: args[0], range, rev, limit, with_scores }
        }
        b"zrevrange" => {
            between!(3, 4);
            let with_scores = match args.get(3) {
                Some(s) if s.eq_ignore_ascii_case(b"withscores") => true,
                Some(_) => return Err(CmdError::Syntax),
                None => false,
            };
            Cmd::ZRange { key: args[0], range: ZRangeBy::Rank(int(args[1])?, int(args[2])?), rev: true, limit: None, with_scores }
        }
        b"zrangebyscore" | b"zrevrangebyscore" | b"zrangebylex" | b"zrevrangebylex" => {
            at_least!(3);
            let rev = lc.starts_with(b"zrev");
            let lex = lc.ends_with(b"lex");
            let (mut limit, mut with_scores) = (None, false);
            let mut i = 3;
            while i < n {
                let a = args[i];
                if a.eq_ignore_ascii_case(b"withscores") && !lex {
                    with_scores = true;
                } else if a.eq_ignore_ascii_case(b"limit") && i + 2 < n {
                    limit = Some((int(args[i + 1])?, int(args[i + 2])?));
                    i += 2;
                } else {
                    return Err(CmdError::Syntax);
                }
                i += 1;
            }
            let (lo, hi) = if rev { (args[2], args[1]) } else { (args[1], args[2]) };
            let range = if lex {
                ZRangeBy::Lex(lex_bound(lo)?, lex_bound(hi)?)
            } else {
                ZRangeBy::Score(score_bound(lo)?, score_bound(hi)?)
            };
            Cmd::ZRange { key: args[0], range, rev, limit, with_scores }
        }
        b"zrank" | b"zrevrank" => {
            between!(2, 3);
            let with_score = match args.get(2) {
                Some(s) if s.eq_ignore_ascii_case(b"withscore") => true,
                Some(_) => return Err(CmdError::Syntax),
                None => false,
            };
            Cmd::ZRank { key: args[0], member: args[1], rev: &*lc == b"zrevrank", with_score }
        }
        b"zcount" => {
            exact!(3);
            Cmd::ZCount { key: args[0], range: ZRangeBy::Score(score_bound(args[1])?, score_bound(args[2])?) }
        }
        b"zlexcount" => {
            exact!(3);
            Cmd::ZCount { key: args[0], range: ZRangeBy::Lex(lex_bound(args[1])?, lex_bound(args[2])?) }
        }
        b"zpopmin" | b"zpopmax" => {
            between!(1, 2);
            let count = match args.get(1) {
                Some(c) => Some(arg_u64(c).ok_or(CmdError::Msg("ERR value is out of range, must be positive"))?),
                None => None,
            };
            Cmd::ZPop { key: args[0], max: &*lc == b"zpopmax", count }
        }
        b"zremrangebyrank" => {
            exact!(3);
            Cmd::ZRemRange { key: args[0], range: ZRangeBy::Rank(int(args[1])?, int(args[2])?) }
        }
        b"zremrangebyscore" => {
            exact!(3);
            Cmd::ZRemRange { key: args[0], range: ZRangeBy::Score(score_bound(args[1])?, score_bound(args[2])?) }
        }
        b"zremrangebylex" => {
            exact!(3);
            Cmd::ZRemRange { key: args[0], range: ZRangeBy::Lex(lex_bound(args[1])?, lex_bound(args[2])?) }
        }
        b"zmscore" => { at_least!(2); Cmd::ZMScore { key: args[0], members: borrowed!(1) } }
        b"zrandmember" => {
            between!(1, 3);
            let count = match args.get(1) {
                Some(c) => Some(int(c)?),
                None => None,
            };
            let with_scores = match args.get(2) {
                Some(s) if s.eq_ignore_ascii_case(b"withscores") => true,
                Some(_) => return Err(CmdError::Syntax),
                None => false,
            };
            Cmd::ZRandMember { key: args[0], count, with_scores }
        }
        b"zscore" => { exact!(2); Cmd::ZScore { key: args[0], member: sb(args[1]) } }
        b"zrem" => { at_least!(2); Cmd::ZRem { key: args[0], members: owned!(1) } }
        b"zincrby" => { exact!(3); Cmd::ZIncrBy { key: args[0], delta: float(args[1])?, member: args[2] } }
        b"zunion" | b"zinter" | b"zdiff" | b"zunionstore" | b"zinterstore" | b"zdiffstore" => {
            let store = lc.ends_with(b"store");
            let op = match lc[1] {
                b'u' => ZOp::Union,
                b'i' => ZOp::Inter,
                _ => ZOp::Diff,
            };
            let need: &'static str = match &*lc {
                b"zunion" => "ERR at least 1 input key is needed for 'zunion' command",
                b"zinter" => "ERR at least 1 input key is needed for 'zinter' command",
                b"zdiff" => "ERR at least 1 input key is needed for 'zdiff' command",
                b"zunionstore" => "ERR at least 1 input key is needed for 'zunionstore' command",
                b"zinterstore" => "ERR at least 1 input key is needed for 'zinterstore' command",
                _ => "ERR at least 1 input key is needed for 'zdiffstore' command",
            };
            let start = store as usize;
            at_least!(start + 2);
            let nk = int(args[start])?;
            if nk <= 0 {
                return Err(CmdError::Msg(need));
            }
            let nk = nk as usize;
            let first = start + 1;
            if n < first + nk {
                return Err(CmdError::Syntax);
            }
            let mut keys = SmallVec::new();
            for k in &args[first..first + nk] {
                keys.push(*k);
            }
            let (mut weights, mut agg, mut with_scores) = (SmallVec::new(), ZAggregate::Sum, false);
            let mut i = first + nk;
            while i < n {
                let a = args[i];
                if op != ZOp::Diff && a.eq_ignore_ascii_case(b"weights") {
                    if n < i + 1 + nk {
                        return Err(CmdError::Syntax);
                    }
                    weights = SmallVec::new();
                    for w in &args[i + 1..i + 1 + nk] {
                        weights.push(arg_f64(w).ok_or(CmdError::Msg("ERR weight value is not a float"))?);
                    }
                    i += 1 + nk;
                } else if op != ZOp::Diff && a.eq_ignore_ascii_case(b"aggregate") {
                    let v = *args.get(i + 1).ok_or(CmdError::Syntax)?;
                    agg = if v.eq_ignore_ascii_case(b"sum") {
                        ZAggregate::Sum
                    } else if v.eq_ignore_ascii_case(b"min") {
                        ZAggregate::Min
                    } else if v.eq_ignore_ascii_case(b"max") {
                        ZAggregate::Max
                    } else {
                        return Err(CmdError::Syntax);
                    };
                    i += 2;
                } else if !store && a.eq_ignore_ascii_case(b"withscores") {
                    with_scores = true;
                    i += 1;
                } else {
                    return Err(CmdError::Syntax);
                }
            }
            Cmd::ZSetOp { op, dest: store.then(|| args[0]), keys, weights, agg, with_scores }
        }
        b"zintercard" => {
            at_least!(2);
            let nk = int(args[0])?;
            if nk <= 0 {
                return Err(CmdError::Msg("ERR at least 1 input key is needed for 'zintercard' command"));
            }
            let nk = nk as usize;
            if n < 1 + nk {
                return Err(CmdError::Syntax);
            }
            let mut keys = SmallVec::new();
            for k in &args[1..1 + nk] {
                keys.push(*k);
            }
            let limit = match &args[1 + nk..] {
                [] => 0,
                [l, v] if l.eq_ignore_ascii_case(b"limit") => {
                    arg_i64(v).and_then(|v| u64::try_from(v).ok()).ok_or(CmdError::Msg("ERR LIMIT can't be negative"))?
                }
                _ => return Err(CmdError::Syntax),
            };
            Cmd::ZInterCard { keys, limit }
        }
        b"zscan" => {
            at_least!(2);
            let (mut pattern, mut count, mut noscores) = (None, None, false);
            let mut i = 2;
            while i < n {
                let a = args[i];
                if a.eq_ignore_ascii_case(b"noscores") {
                    noscores = true;
                    i += 1;
                    continue;
                }
                let v = *args.get(i + 1).ok_or(CmdError::Syntax)?;
                if a.eq_ignore_ascii_case(b"match") {
                    pattern = Some(v);
                } else if a.eq_ignore_ascii_case(b"count") {
                    match int(v)? {
                        c if c < 1 => return Err(CmdError::Syntax),
                        c => count = Some(c as usize),
                    }
                } else {
                    return Err(CmdError::Syntax);
                }
                i += 2;
            }
            Cmd::ZScan { key: args[0], cursor: args[1], pattern, count, noscores }
        }

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
        b"config" => Cmd::Config(borrowed!(0)),
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
        b"bgsave" => match *args {
            [m, n] if m.eq_ignore_ascii_case(b"reserve") => Cmd::BgSaveReserve(uint(n)?),
            [] | [_] => Cmd::BgSave,
            _ => return Err(CmdError::Syntax),
        },
        b"bgrewriteaof" => { exact!(0); Cmd::BgRewriteAof }
        b"save" => { exact!(0); Cmd::Save }
        b"lastsave" => { exact!(0); Cmd::LastSave }
        b"debug" => match *args {
            [sub] if sub.eq_ignore_ascii_case(b"digest") => Cmd::DebugDigest(None),
            [sub, at] if sub.eq_ignore_ascii_case(b"digest") => Cmd::DebugDigest(Some(uint(at)?)),
            [] => return Err(CmdError::Arity),
            _ => return Err(CmdError::Syntax),
        },

        // ── Réplication ──────────────────────────────────────────────────
        b"replicate" => match *args {
            [stop, host, port] if stop.eq_ignore_ascii_case(b"stop") => {
                Cmd::Replicate(Replicate::Stop { host, port: port_num(port)? })
            }
            [host, port] => Cmd::Replicate(Replicate::Start { host, port: port_num(port)?, auth: None }),
            [host, port, kw, pass] if kw.eq_ignore_ascii_case(b"auth") => {
                Cmd::Replicate(Replicate::Start { host, port: port_num(port)?, auth: Some(pass) })
            }
            [_, _, _] | [_, _, _, _] => return Err(CmdError::Syntax),
            _ => return Err(CmdError::Arity),
        },
        b"replicaof" | b"slaveof" => match *args {
            [no, one] if no.eq_ignore_ascii_case(b"no") && one.eq_ignore_ascii_case(b"one") => Cmd::ReplicaOf(None),
            [host, port] => Cmd::ReplicaOf(Some((host, port))),
            _ => return Err(CmdError::Arity),
        },
        b"rxhello" => { exact!(1); Cmd::RxHello(args[0]) }
        b"rxload" => match *args {
            [sub, len] if sub.eq_ignore_ascii_case(b"begin") => Cmd::RxLoad(RxLoad::Begin(uint(len)?)),
            [sub, data] if sub.eq_ignore_ascii_case(b"data") => Cmd::RxLoad(RxLoad::Data(data)),
            [sub] if sub.eq_ignore_ascii_case(b"end") => Cmd::RxLoad(RxLoad::End),
            [] => return Err(CmdError::Arity),
            _ => return Err(CmdError::Syntax),
        },
        b"rxblock" => { exact!(1); Cmd::RxBlock(args[0]) }
        b"rxtick" => { exact!(1); Cmd::RxTick(uint(args[0])?) }
        b"wait" => { exact!(2); Cmd::Wait { replicas: uint(args[0])?, timeout_ms: uint(args[1])? } }
        b"shutdown" => {
            let (mut save, mut force, mut abort) = (None, false, false);
            for a in args {
                if a.eq_ignore_ascii_case(b"nosave") && save.is_none() {
                    save = Some(false);
                } else if a.eq_ignore_ascii_case(b"save") && save.is_none() {
                    save = Some(true);
                } else if a.eq_ignore_ascii_case(b"now") {
                    // pas d'attente des réplicas ici : sans effet
                } else if a.eq_ignore_ascii_case(b"force") {
                    force = true;
                } else if a.eq_ignore_ascii_case(b"abort") {
                    abort = true;
                } else {
                    return Err(CmdError::Syntax);
                }
            }
            if abort && (save.is_some() || force) {
                return Err(CmdError::Syntax);
            }
            Cmd::Shutdown { save, force, abort }
        }

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
    /// Message de Redis tel quel.
    Msg(&'static str),
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
            CmdError::Msg(m) => m.into(),
        }
    }
}

impl<'a> Cmd<'a> {
    /// Découpe une commande au début de `d` (RESP ou inline).
    pub fn parse(d: &'a [u8]) -> Parse<'a> {
        let mut raw: SmallVec<8, &'a [u8]> = SmallVec::new();
        match walk(d, |a| raw.push(a)) {
            // Ligne vide, `*0` : consommés, rien à exécuter (comme Redis).
            Ok(n) if raw.is_empty() => Parse::Done(Cmd::Empty, n),
            Ok(n) => Parse::Done(dispatch(&raw), n),
            Err(Stop::Incomplete) => Parse::Incomplete,
            Err(Stop::Invalid(msg)) => Parse::Invalid(msg),
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
        self.write_weight() > 0
    }

    /// Changements que la commande peut faire, compté par élément comme le
    /// `dirty` de Redis (MSET : ses paires, DEL : ses clés, HSET : ses champs) ;
    /// 0 pour une lecture. Majorant : une écriture sans effet compte aussi.
    /// FLUSHDB compte 1 ici, l'appelant ajoute la taille de la base.
    /// L'écriture peut faire grossir la mémoire (le `denyoom` de Redis) : refusée en
    /// `-OOM` au-delà de `maxmemory`. Les suppressions, pops et TTL passent toujours.
    pub fn may_grow(&self) -> bool {
        matches!(
            self,
            Cmd::Set { .. }
                | Cmd::SetNx { .. }
                | Cmd::MSet(_)
                | Cmd::Incr(_)
                | Cmd::Decr(_)
                | Cmd::IncrBy { .. }
                | Cmd::DecrBy { .. }
                | Cmd::HSet { .. }
                | Cmd::HMSet { .. }
                | Cmd::HIncrBy { .. }
                | Cmd::SAdd { .. }
                | Cmd::ZAdd { .. }
                | Cmd::ZIncrBy { .. }
                | Cmd::ZSetOp { dest: Some(_), .. }
                | Cmd::LPush { .. }
                | Cmd::LSet { .. }
                | Cmd::LInsert { .. }
                | Cmd::LMove { .. }
                | Cmd::BLMove { .. }
        )
    }

    pub fn write_weight(&self) -> u64 {
        let n = match self {
            Cmd::Set { .. }
            | Cmd::SetNx { .. }
            | Cmd::Expire { .. }
            | Cmd::Persist(_)
            | Cmd::Incr(_)
            | Cmd::Decr(_)
            | Cmd::IncrBy { .. }
            | Cmd::DecrBy { .. }
            | Cmd::HIncrBy { .. }
            | Cmd::ZIncrBy { .. }
            | Cmd::FlushDb => 1,
            Cmd::MSet(pairs) => pairs.len(),
            Cmd::Del(keys) => keys.len(),
            Cmd::Unlink(keys) => keys.len(),
            Cmd::HSet { fields, .. } | Cmd::HMSet { fields, .. } => fields.len(),
            Cmd::HDel { fields, .. } => fields.len(),
            Cmd::SAdd { members, .. } | Cmd::SRem { members, .. } => members.len(),
            Cmd::SPop { count, .. } => count.unwrap_or(1) as usize,
            Cmd::ZAdd { members, .. } => members.len(),
            Cmd::ZRem { members, .. } => members.len(),
            Cmd::ZPop { count, .. } => count.unwrap_or(1).max(1) as usize,
            Cmd::ZRemRange { .. } => 1,
            Cmd::ZSetOp { dest: Some(_), .. } => 1,
            Cmd::LPush { vals, .. } => vals.len(),
            Cmd::LPop { count, .. } => count.unwrap_or(1).max(1) as usize,
            Cmd::LSet { .. }
            | Cmd::LInsert { .. }
            | Cmd::LRem { .. }
            | Cmd::LTrim { .. }
            | Cmd::LMove { .. }
            | Cmd::BPop { .. }
            | Cmd::BLMove { .. }
            | Cmd::BZPop { .. } => 1,
            _ => 0,
        };
        n as u64
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
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::{Cmd, CmdError, FrameLen, Parse, frame_len, frame_need};

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
        assert_eq!(err(b"ZRANGE z 0 1 FOO\r\n"), "ERR syntax error");
        assert_eq!(
            err(b"ZRANGE z 0 1 LIMIT 0 1\r\n"),
            "ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX"
        );
        assert_eq!(err(b"ZADD z NX XX 1 m\r\n"), "ERR XX and NX options at the same time are not compatible");
        assert_eq!(err(b"ZADD z GT LT 1 m\r\n"), "ERR GT, LT, and/or NX options at the same time are not compatible");
        assert_eq!(err(b"ZADD z INCR 1 a 2 b\r\n"), "ERR INCR option supports a single increment-element pair");
        assert_eq!(err(b"ZCOUNT z x 1\r\n"), "ERR min or max is not a float");
        assert_eq!(err(b"ZLEXCOUNT z a b\r\n"), "ERR min or max not valid string range item");
        assert_eq!(err(b"ZADD z 1 m NaN n\r\n"), "ERR value is not a valid float");
        assert_eq!(err(b"FooBar\r\n"), "ERR unknown command 'foobar'");
    }

    /// `frame_len` (découpage à l'arrivée) et `parse` (à l'exécution) voient
    /// exactement les mêmes frontières, sur tous les préfixes.
    #[test]
    fn frame_len_agrees_with_parse() {
        let streams: [&[u8]; 6] = [
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nhello\r\n*2\r\n$3\r\nGET\r\n$1\r\nk\r\n",
            b"\r\n\r\nPING\r\nSET k \"a b\"\r\n*0\r\nGET k\n",
            b"*1\r\n$-1\r\n",
            b"ECHO \"abc\r\n",
            b"*2\r\n$3\r\nGET\r\n$9223372036854775807\r\nk\r\n",
            b"DUMP k\r\nFOO\r\n*1\r\n$4\r\nPING\r\n",
        ];
        for d in streams {
            for cut in 0..=d.len() {
                let part = &d[..cut];
                match (frame_len(part), Cmd::parse(part)) {
                    (FrameLen::Done(a), Parse::Done(_, b)) => assert_eq!(a, b),
                    (FrameLen::Incomplete, Parse::Incomplete) => {}
                    (FrameLen::Invalid(a), Parse::Invalid(b)) => assert_eq!(a, b),
                    _ => panic!("désaccord sur {:?}", part.escape_ascii().to_string()),
                }
            }
        }
    }

    /// Borne basse de la taille d'une frame incomplète : toujours ≤ la taille réelle,
    /// exacte dès que le dernier en-tête est reçu ; `None` pour une frame complète.
    #[test]
    fn frame_need_bounds_incomplete_frames() {
        let frame = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$10\r\n0123456789\r\n";
        for cut in 0..frame.len() {
            let part = &frame[..cut];
            let need = frame_need(part);
            if cut == 0 {
                assert_eq!(need, None);
                continue;
            }
            let need = need.unwrap_or_else(|| panic!("coupé à {cut}"));
            assert!(need >= cut && need <= frame.len(), "coupé à {cut} : {need}");
            if cut >= frame.len() - 12 {
                assert_eq!(need, frame.len(), "dernier en-tête reçu à {cut}");
            }
        }
        assert_eq!(frame_need(frame), None);
        assert_eq!(frame_need(b"PING\r\n"), None);
        assert_eq!(frame_need(b"*2\r\n$3\r\nSET\r\n$1048576\r\nxx"), Some(13 + 10 + 1048576 + 2));
    }
}
