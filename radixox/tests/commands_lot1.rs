//! Lot 1 de la couverture Redis (COMMANDS_PLAN.md §3) : réponses, erreurs et cas limites
//! relevés sur Valkey 9.1 (même scénario envoyé aux deux serveurs, réponses comparées).
//! Écarts voulus, documentés dans COMMANDS_PLAN.md : INCRBYFLOAT en décimal exact (Redis
//! montre le bruit de son `long double`), TTL à la seconde.

mod common;

use std::sync::OnceLock;

use redis::Value as V;

const PORT: u16 = 16460;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

/// Réponse brute (erreurs comprises, en `Err(message)`).
fn q(c: &mut redis::Connection, args: &[&str]) -> Result<V, String> {
    let mut cmd = redis::cmd(args[0]);
    for a in &args[1..] {
        cmd.arg(*a);
    }
    cmd.query::<V>(c).map_err(|e| {
        // « ERR msg » / « WRONGTYPE msg » : code + détail
        match (e.code(), e.detail()) {
            (Some(code), Some(d)) => format!("{code} {d}"),
            _ => e.to_string(),
        }
    })
}

fn int(c: &mut redis::Connection, args: &[&str]) -> i64 {
    match q(c, args) {
        Ok(V::Int(n)) => n,
        r => panic!("{args:?} : entier attendu, {r:?}"),
    }
}

fn bulk(c: &mut redis::Connection, args: &[&str]) -> Option<String> {
    match q(c, args) {
        Ok(V::BulkString(b)) => Some(String::from_utf8(b).unwrap()),
        Ok(V::Nil) => None,
        r => panic!("{args:?} : bulk attendue, {r:?}"),
    }
}

fn err(c: &mut redis::Connection, args: &[&str]) -> String {
    match q(c, args) {
        Err(e) => e,
        r => panic!("{args:?} : erreur attendue, {r:?}"),
    }
}

fn strings(c: &mut redis::Connection, args: &[&str]) -> Vec<String> {
    match q(c, args) {
        Ok(V::Array(v)) => v
            .into_iter()
            .map(|x| match x {
                V::BulkString(b) => String::from_utf8(b).unwrap(),
                V::Int(n) => n.to_string(),
                o => panic!("{o:?}"),
            })
            .collect(),
        r => panic!("{args:?} : tableau attendu, {r:?}"),
    }
}

fn del(c: &mut redis::Connection, keys: &[&str]) {
    let _: i64 = redis::cmd("DEL").arg(keys).query(c).unwrap();
}

const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";

// ── Strings ──────────────────────────────────────────────────────────────────

#[test]
fn append_strlen() {
    let mut c = server();
    del(&mut c, &["l1:a", "l1:an", "l1:at", "l1:al", "l1:none"]);
    assert_eq!(int(&mut c, &["APPEND", "l1:a", "hello"]), 5);
    assert_eq!(int(&mut c, &["APPEND", "l1:a", " world"]), 11);
    assert_eq!(bulk(&mut c, &["GET", "l1:a"]).as_deref(), Some("hello world"));
    assert_eq!(int(&mut c, &["STRLEN", "l1:a"]), 11);
    assert_eq!(int(&mut c, &["STRLEN", "l1:none"]), 0);
    // APPEND "" sur une clé absente la crée (comme Redis)
    assert_eq!(int(&mut c, &["APPEND", "l1:none", ""]), 0);
    assert_eq!(int(&mut c, &["EXISTS", "l1:none"]), 1);
    // un entier reste un nombre après APPEND
    let _ = q(&mut c, &["SET", "l1:an", "10"]);
    assert_eq!(int(&mut c, &["APPEND", "l1:an", "5"]), 3);
    assert_eq!(int(&mut c, &["INCR", "l1:an"]), 106);
    // le TTL est gardé
    let _ = q(&mut c, &["SET", "l1:at", "v", "EX", "100"]);
    assert_eq!(int(&mut c, &["APPEND", "l1:at", "x"]), 2);
    assert!(int(&mut c, &["TTL", "l1:at"]) > 90);
    let _ = q(&mut c, &["LPUSH", "l1:al", "a"]);
    assert_eq!(err(&mut c, &["APPEND", "l1:al", "x"]), WRONGTYPE);
    assert_eq!(err(&mut c, &["STRLEN", "l1:al"]), WRONGTYPE);
}

#[test]
fn getrange_bounds() {
    let mut c = server();
    let _ = q(&mut c, &["SET", "l1:gr", "This is a string"]);
    let cases: [(&str, &str, &str); 11] = [
        ("0", "3", "This"),
        ("-3", "-1", "ing"),
        ("0", "-1", "This is a string"),
        ("10", "100", "string"),
        ("5", "2", ""),
        ("-1", "-5", ""),
        ("-100", "-200", ""),
        ("-100", "2", "Thi"),
        ("100", "200", ""),
        ("-9223372036854775808", "9223372036854775807", "This is a string"),
        ("0", "0", "T"),
    ];
    for (a, b, want) in cases {
        assert_eq!(bulk(&mut c, &["GETRANGE", "l1:gr", a, b]).as_deref(), Some(want), "{a} {b}");
    }
    assert_eq!(bulk(&mut c, &["SUBSTR", "l1:gr", "0", "3"]).as_deref(), Some("This"));
    del(&mut c, &["l1:gr-none"]);
    assert_eq!(bulk(&mut c, &["GETRANGE", "l1:gr-none", "0", "-1"]).as_deref(), Some(""));
    let _ = q(&mut c, &["SET", "l1:gri", "12345"]);
    assert_eq!(bulk(&mut c, &["GETRANGE", "l1:gri", "1", "2"]).as_deref(), Some("23"));
    assert_eq!(err(&mut c, &["GETRANGE", "l1:gr", "0", "x"]), "ERR value is not an integer or out of range");
}

#[test]
fn setrange() {
    let mut c = server();
    del(&mut c, &["l1:sr", "l1:sr2"]);
    // valeur vide sur clé absente : 0, la clé n'est pas créée
    assert_eq!(int(&mut c, &["SETRANGE", "l1:sr", "0", ""]), 0);
    assert_eq!(int(&mut c, &["EXISTS", "l1:sr"]), 0);
    assert_eq!(int(&mut c, &["SETRANGE", "l1:sr2", "6", "Redis"]), 11);
    assert_eq!(bulk(&mut c, &["GET", "l1:sr2"]).as_deref(), Some("\0\0\0\0\0\0Redis"));
    let _ = q(&mut c, &["SET", "l1:sr", "Hello World", "EX", "100"]);
    assert_eq!(int(&mut c, &["SETRANGE", "l1:sr", "6", "Redis"]), 11);
    assert_eq!(bulk(&mut c, &["GET", "l1:sr"]).as_deref(), Some("Hello Redis"));
    assert_eq!(int(&mut c, &["SETRANGE", "l1:sr", "13", "x"]), 14);
    assert_eq!(bulk(&mut c, &["GET", "l1:sr"]).as_deref(), Some("Hello Redis\0\0x"));
    assert_eq!(int(&mut c, &["SETRANGE", "l1:sr", "0", ""]), 14);
    assert!(int(&mut c, &["TTL", "l1:sr"]) > 90, "TTL gardé");
    assert_eq!(err(&mut c, &["SETRANGE", "l1:sr", "-1", "x"]), "ERR offset is out of range");
    assert_eq!(
        err(&mut c, &["SETRANGE", "l1:sr", "536870912", "x"]),
        "ERR string exceeds maximum allowed size (proto-max-bulk-len)"
    );
    let _ = q(&mut c, &["SADD", "l1:srs", "a"]);
    assert_eq!(err(&mut c, &["SETRANGE", "l1:srs", "0", ""]), WRONGTYPE);
}

#[test]
fn getset_getdel_getex() {
    let mut c = server();
    del(&mut c, &["l1:gs", "l1:gx", "l1:gh"]);
    assert_eq!(bulk(&mut c, &["GETSET", "l1:gs", "v1"]), None);
    let _ = q(&mut c, &["EXPIRE", "l1:gs", "100"]);
    assert_eq!(bulk(&mut c, &["GETSET", "l1:gs", "v2"]).as_deref(), Some("v1"));
    assert_eq!(int(&mut c, &["TTL", "l1:gs"]), -1, "GETSET retire le TTL");
    assert_eq!(bulk(&mut c, &["GETDEL", "l1:gs"]).as_deref(), Some("v2"));
    assert_eq!(bulk(&mut c, &["GETDEL", "l1:gs"]), None);
    let _ = q(&mut c, &["HSET", "l1:gh", "f", "v"]);
    assert_eq!(err(&mut c, &["GETDEL", "l1:gh"]), WRONGTYPE);
    assert_eq!(err(&mut c, &["GETSET", "l1:gh", "x"]), WRONGTYPE);
    assert_eq!(int(&mut c, &["EXISTS", "l1:gh"]), 1);

    let _ = q(&mut c, &["SET", "l1:gx", "v"]);
    assert_eq!(bulk(&mut c, &["GETEX", "l1:gx"]).as_deref(), Some("v"));
    assert_eq!(bulk(&mut c, &["GETEX", "l1:gx", "EX", "100"]).as_deref(), Some("v"));
    assert!(int(&mut c, &["TTL", "l1:gx"]) > 90);
    assert_eq!(bulk(&mut c, &["GETEX", "l1:gx", "PERSIST"]).as_deref(), Some("v"));
    assert_eq!(int(&mut c, &["TTL", "l1:gx"]), -1);
    assert_eq!(bulk(&mut c, &["GETEX", "l1:gx", "PX", "200000"]).as_deref(), Some("v"));
    assert!(int(&mut c, &["TTL", "l1:gx"]) > 190);
    assert_eq!(bulk(&mut c, &["GETEX", "l1:gx", "EXAT", "9999999999"]).as_deref(), Some("v"));
    assert!(int(&mut c, &["TTL", "l1:gx"]) > 1_000_000);
    // date passée : la valeur est rendue, la clé supprimée
    assert_eq!(bulk(&mut c, &["GETEX", "l1:gx", "PXAT", "1"]).as_deref(), Some("v"));
    assert_eq!(int(&mut c, &["EXISTS", "l1:gx"]), 0);
    assert_eq!(bulk(&mut c, &["GETEX", "l1:gx", "EX", "10"]), None);
    assert_eq!(err(&mut c, &["GETEX", "l1:gx", "EX", "0"]), "ERR invalid expire time in 'getex' command");
    assert_eq!(err(&mut c, &["GETEX", "l1:gx", "EX", "-1"]), "ERR invalid expire time in 'getex' command");
    assert_eq!(err(&mut c, &["GETEX", "l1:gx", "FOO", "10"]), "ERR syntax error");
    assert_eq!(err(&mut c, &["GETEX", "l1:gx", "EX", "10", "PX", "10"]), "ERR syntax error");
    assert_eq!(err(&mut c, &["GETEX", "l1:gh"]), WRONGTYPE);
}

#[test]
fn incrbyfloat() {
    let mut c = server();
    del(&mut c, &["l1:f", "l1:fh"]);
    assert_eq!(bulk(&mut c, &["INCRBYFLOAT", "l1:f", "0.1"]).as_deref(), Some("0.1"));
    // décimal exact (Redis : long double, même résultat ici)
    assert_eq!(bulk(&mut c, &["INCRBYFLOAT", "l1:f", "0.2"]).as_deref(), Some("0.3"));
    let _ = q(&mut c, &["SET", "l1:f", "10.50", "EX", "100"]);
    assert_eq!(bulk(&mut c, &["INCRBYFLOAT", "l1:f", "0.1"]).as_deref(), Some("10.6"));
    assert!(int(&mut c, &["TTL", "l1:f"]) > 90, "TTL gardé");
    let _ = q(&mut c, &["SET", "l1:f", "5.0e3"]);
    assert_eq!(bulk(&mut c, &["INCRBYFLOAT", "l1:f", "2.0e2"]).as_deref(), Some("5200"));
    assert_eq!(bulk(&mut c, &["INCRBYFLOAT", "l1:f", "-5200"]).as_deref(), Some("0"));
    assert_eq!(err(&mut c, &["INCRBYFLOAT", "l1:f", "abc"]), "ERR value is not a valid float");
    assert_eq!(err(&mut c, &["INCRBYFLOAT", "l1:f", "inf"]), "ERR increment would produce NaN or Infinity");
    let _ = q(&mut c, &["SET", "l1:f", " 1"]);
    assert_eq!(err(&mut c, &["INCRBYFLOAT", "l1:f", "1"]), "ERR value is not a valid float");
    let _ = q(&mut c, &["HSET", "l1:fh", "f", "v"]);
    assert_eq!(err(&mut c, &["INCRBYFLOAT", "l1:fh", "x"]), WRONGTYPE);
}

#[test]
fn msetnx_psetex_setex() {
    let mut c = server();
    del(&mut c, &["l1:ma", "l1:mb", "l1:mc", "l1:mx", "l1:ps"]);
    assert_eq!(int(&mut c, &["MSETNX", "l1:ma", "1", "l1:mb", "2"]), 1);
    assert_eq!(int(&mut c, &["MSETNX", "l1:mb", "3", "l1:mc", "4"]), 0);
    assert_eq!(int(&mut c, &["EXISTS", "l1:mc"]), 0, "aucune posée");
    assert_eq!(int(&mut c, &["MSETNX", "l1:mx", "1", "l1:mx", "2"]), 1);
    assert_eq!(bulk(&mut c, &["GET", "l1:mx"]).as_deref(), Some("2"));
    assert_eq!(err(&mut c, &["MSETNX", "l1:ma"]), "ERR wrong number of arguments for 'msetnx' command");
    let _ = q(&mut c, &["PSETEX", "l1:ps", "100000", "v"]);
    assert!(int(&mut c, &["PTTL", "l1:ps"]) > 90_000);
    assert_eq!(err(&mut c, &["PSETEX", "l1:ps", "0", "v"]), "ERR invalid expire time in 'psetex' command");
    assert_eq!(err(&mut c, &["SETEX", "l1:ps", "-1", "v"]), "ERR invalid expire time in 'setex' command");
}

// ── Clés et TTL ──────────────────────────────────────────────────────────────

#[test]
fn expire_options() {
    let mut c = server();
    let _ = q(&mut c, &["SET", "l1:e", "v"]);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "100", "XX"]), 0);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "100", "NX"]), 1);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "100", "NX"]), 0);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "50", "GT"]), 0);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "200", "GT"]), 1);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "300", "LT"]), 0);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "100", "XX", "LT"]), 1);
    assert!((95..=100).contains(&int(&mut c, &["TTL", "l1:e"])));
    let _ = q(&mut c, &["PERSIST", "l1:e"]);
    // sans TTL = infini : GT jamais, LT toujours
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "100", "GT"]), 0);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "100", "LT"]), 1);
    assert_eq!(
        err(&mut c, &["EXPIRE", "l1:e", "1", "NX", "XX"]),
        "ERR NX and XX, GT or LT options at the same time are not compatible"
    );
    assert_eq!(err(&mut c, &["EXPIRE", "l1:e", "1", "GT", "LT"]), "ERR GT and LT options at the same time are not compatible");
    assert_eq!(err(&mut c, &["EXPIRE", "l1:e", "1", "FOO"]), "ERR Unsupported option FOO");
    assert_eq!(
        err(&mut c, &["EXPIRE", "l1:e", "9223372036854775807"]),
        "ERR invalid expire time in 'expire' command"
    );
    // 0 ou négatif : la clé est supprimée
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "0"]), 1);
    assert_eq!(int(&mut c, &["EXISTS", "l1:e"]), 0);
    let _ = q(&mut c, &["SET", "l1:e", "v"]);
    assert_eq!(int(&mut c, &["PEXPIRE", "l1:e", "-1"]), 1);
    assert_eq!(int(&mut c, &["EXISTS", "l1:e"]), 0);
    assert_eq!(int(&mut c, &["EXPIRE", "l1:e", "10"]), 0);
}

#[test]
fn expireat_expiretime() {
    let mut c = server();
    let _ = q(&mut c, &["SET", "l1:ea", "v"]);
    assert_eq!(int(&mut c, &["EXPIRETIME", "l1:ea"]), -1);
    assert_eq!(int(&mut c, &["EXPIREAT", "l1:ea", "9999999999"]), 1);
    assert_eq!(int(&mut c, &["EXPIRETIME", "l1:ea"]), 9999999999);
    assert_eq!(int(&mut c, &["PEXPIRETIME", "l1:ea"]), 9999999999000);
    assert_eq!(int(&mut c, &["PEXPIREAT", "l1:ea", "9999999999500"]), 1);
    assert_eq!(int(&mut c, &["EXPIRETIME", "l1:ea"]), 10000000000, "arrondi comme Redis");
    assert_eq!(int(&mut c, &["EXPIREAT", "l1:ea", "1", "GT"]), 0);
    assert_eq!(int(&mut c, &["EXISTS", "l1:ea"]), 1);
    assert_eq!(int(&mut c, &["EXPIREAT", "l1:ea", "1", "LT"]), 1);
    assert_eq!(int(&mut c, &["EXISTS", "l1:ea"]), 0);
    assert_eq!(int(&mut c, &["EXPIRETIME", "l1:ea"]), -2);
    assert_eq!(int(&mut c, &["PEXPIRETIME", "l1:ea"]), -2);
}

#[test]
fn touch_rename_randomkey() {
    let mut c = server();
    del(&mut c, &["l1:ta", "l1:tb", "l1:tc"]);
    let _ = q(&mut c, &["MSET", "l1:ta", "1", "l1:tb", "2"]);
    assert_eq!(int(&mut c, &["TOUCH", "l1:ta", "l1:tb", "l1:tc", "l1:ta"]), 3);

    del(&mut c, &["l1:ra", "l1:rb", "l1:rc", "l1:rd", "l1:rh", "l1:rh2", "l1:rh3"]);
    let _ = q(&mut c, &["SET", "l1:ra", "1", "EX", "100"]);
    assert_eq!(q(&mut c, &["RENAME", "l1:ra", "l1:rb"]), Ok(V::Okay));
    assert_eq!(bulk(&mut c, &["GET", "l1:rb"]).as_deref(), Some("1"));
    assert!(int(&mut c, &["TTL", "l1:rb"]) > 90, "TTL suivi");
    assert_eq!(int(&mut c, &["EXISTS", "l1:ra"]), 0);
    assert_eq!(err(&mut c, &["RENAME", "l1:ra", "l1:rb"]), "ERR no such key");
    assert_eq!(err(&mut c, &["RENAMENX", "l1:ra", "l1:rb"]), "ERR no such key");
    assert_eq!(q(&mut c, &["RENAME", "l1:rb", "l1:rb"]), Ok(V::Okay));
    assert_eq!(int(&mut c, &["RENAMENX", "l1:rb", "l1:rb"]), 0);
    let _ = q(&mut c, &["SET", "l1:rc", "3"]);
    assert_eq!(int(&mut c, &["RENAMENX", "l1:rb", "l1:rc"]), 0);
    assert_eq!(int(&mut c, &["RENAMENX", "l1:rb", "l1:rd"]), 1);
    // écrase la destination quel que soit son type ; un gros type passe sans copie
    let _ = q(&mut c, &["HSET", "l1:rh", "f", "v"]);
    assert_eq!(q(&mut c, &["RENAME", "l1:rd", "l1:rh"]), Ok(V::Okay));
    assert_eq!(bulk(&mut c, &["GET", "l1:rh"]).as_deref(), Some("1"));
    let mut args = vec!["HSET", "l1:rh2"];
    let fields: Vec<String> = (0..500).map(|i| format!("f{i}")).collect();
    for f in &fields {
        args.push(f);
        args.push(f);
    }
    let _ = q(&mut c, &args);
    assert_eq!(q(&mut c, &["RENAME", "l1:rh2", "l1:rh3"]), Ok(V::Okay));
    assert_eq!(int(&mut c, &["HLEN", "l1:rh3"]), 500);
    assert_eq!(bulk(&mut c, &["HGET", "l1:rh3", "f499"]).as_deref(), Some("f499"));
    let _ = q(&mut c, &["HSET", "l1:rh3", "new", "x"]);
    assert_eq!(int(&mut c, &["HLEN", "l1:rh3"]), 501);

    // RANDOMKEY : une clé existante
    let k = bulk(&mut c, &["RANDOMKEY"]).expect("base non vide");
    assert_eq!(int(&mut c, &["EXISTS", &k]), 1, "{k}");
}

/// Le seul jeu de clés d'un serveur à part : RANDOMKEY les atteint toutes, rend nil vide.
#[test]
fn randomkey_reaches_every_key() {
    const P: u16 = 16461;
    common::start_server(P);
    let mut c = common::conn(P);
    assert_eq!(bulk(&mut c, &["RANDOMKEY"]), None);
    let keys: Vec<String> = (0..20).map(|i| format!("rk:{i}:{}", "x".repeat(i))).collect();
    for k in &keys {
        let _ = q(&mut c, &["SET", k, "v"]);
    }
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2000 {
        seen.insert(bulk(&mut c, &["RANDOMKEY"]).unwrap());
    }
    assert_eq!(seen.len(), keys.len(), "{seen:?}");
    let _ = q(&mut c, &["SHUTDOWN", "NOSAVE"]);
}

// ── Hash ─────────────────────────────────────────────────────────────────────

#[test]
fn hash_lot1() {
    let mut c = server();
    del(&mut c, &["l1:h", "l1:hs", "l1:hb"]);
    assert_eq!(int(&mut c, &["HSETNX", "l1:h", "f", "v"]), 1);
    assert_eq!(int(&mut c, &["HSETNX", "l1:h", "f", "w"]), 0);
    assert_eq!(bulk(&mut c, &["HGET", "l1:h", "f"]).as_deref(), Some("v"));
    assert_eq!(int(&mut c, &["HSTRLEN", "l1:h", "f"]), 1);
    assert_eq!(int(&mut c, &["HSTRLEN", "l1:h", "nofield"]), 0);
    assert_eq!(int(&mut c, &["HSTRLEN", "l1:h-none", "f"]), 0);
    assert_eq!(bulk(&mut c, &["HINCRBYFLOAT", "l1:h", "x", "0.1"]).as_deref(), Some("0.1"));
    assert_eq!(bulk(&mut c, &["HINCRBYFLOAT", "l1:h", "x", "0.2"]).as_deref(), Some("0.3"));
    assert_eq!(err(&mut c, &["HINCRBYFLOAT", "l1:h", "f", "1"]), "ERR hash value is not a float");
    assert_eq!(err(&mut c, &["HINCRBYFLOAT", "l1:h", "x", "inf"]), "ERR value is NaN or Infinity");
    assert_eq!(err(&mut c, &["HINCRBYFLOAT", "l1:h", "x", "y"]), "ERR value is not a valid float");
    let _ = q(&mut c, &["SET", "l1:hs", "v"]);
    assert_eq!(err(&mut c, &["HSETNX", "l1:hs", "f", "v"]), WRONGTYPE);
    assert_eq!(err(&mut c, &["HSTRLEN", "l1:hs", "f"]), WRONGTYPE);
    assert_eq!(err(&mut c, &["HINCRBYFLOAT", "l1:hs", "f", "1"]), WRONGTYPE);
    assert_eq!(err(&mut c, &["HRANDFIELD", "l1:hs"]), WRONGTYPE);
}

#[test]
fn hrandfield() {
    let mut c = server();
    del(&mut c, &["l1:hr", "l1:hrn", "l1:hrb"]);
    assert_eq!(bulk(&mut c, &["HRANDFIELD", "l1:hrn"]), None);
    assert_eq!(strings(&mut c, &["HRANDFIELD", "l1:hrn", "5"]), Vec::<String>::new());
    let _ = q(&mut c, &["HSET", "l1:hr", "a", "1", "b", "2", "c", "3"]);
    let mut all = strings(&mut c, &["HRANDFIELD", "l1:hr", "10", "WITHVALUES"]);
    all.sort();
    assert_eq!(all, ["1", "2", "3", "a", "b", "c"]);
    let two = strings(&mut c, &["HRANDFIELD", "l1:hr", "2"]);
    assert_eq!(two.len(), 2);
    assert_ne!(two[0], two[1], "distincts");
    assert_eq!(strings(&mut c, &["HRANDFIELD", "l1:hr", "-7"]).len(), 7, "répétitions");
    assert_eq!(strings(&mut c, &["HRANDFIELD", "l1:hr", "0"]), Vec::<String>::new());
    assert!(["a", "b", "c"].contains(&bulk(&mut c, &["HRANDFIELD", "l1:hr"]).unwrap().as_str()));
    assert_eq!(err(&mut c, &["HRANDFIELD", "l1:hr", "1", "FOO"]), "ERR syntax error");
    assert_eq!(
        err(&mut c, &["HRANDFIELD", "l1:hr", "-9223372036854775808"]),
        "ERR value is out of range, value must between -9223372036854775807 and 9223372036854775807"
    );
    assert_eq!(err(&mut c, &["HRANDFIELD", "l1:hr", "-5000000000000000000", "WITHVALUES"]), "ERR value is out of range");
    // pas d'allocation sans fin (abandon du serveur)
    assert_eq!(err(&mut c, &["HRANDFIELD", "l1:hr", "-5000000000000000000"]), "ERR value is out of range");
    // grand Hash (valart) : tirages distincts
    let mut args = vec!["HSET", "l1:hrb"];
    let fields: Vec<String> = (0..1000).map(|i| format!("f{i}")).collect();
    for f in &fields {
        args.push(f);
        args.push("v");
    }
    let _ = q(&mut c, &args);
    let mut got = strings(&mut c, &["HRANDFIELD", "l1:hrb", "100"]);
    got.sort();
    got.dedup();
    assert_eq!(got.len(), 100);
    assert_eq!(strings(&mut c, &["HRANDFIELD", "l1:hrb", "700"]).len(), 700);
}

// ── Set ──────────────────────────────────────────────────────────────────────

#[test]
fn set_lot1() {
    let mut c = server();
    del(&mut c, &["l1:s", "l1:sa", "l1:sb", "l1:sc", "l1:ss", "l1:sbig"]);
    let _ = q(&mut c, &["SADD", "l1:s", "a", "b", "c"]);
    assert_eq!(strings(&mut c, &["SMISMEMBER", "l1:s", "a", "x", "c"]), ["1", "0", "1"]);
    assert_eq!(strings(&mut c, &["SMISMEMBER", "l1:s-none", "a"]), ["0"]);
    assert_eq!(bulk(&mut c, &["SRANDMEMBER", "l1:s-none"]), None);
    assert_eq!(strings(&mut c, &["SRANDMEMBER", "l1:s-none", "3"]), Vec::<String>::new());
    let mut three = strings(&mut c, &["SRANDMEMBER", "l1:s", "10"]);
    three.sort();
    assert_eq!(three, ["a", "b", "c"]);
    assert_eq!(strings(&mut c, &["SRANDMEMBER", "l1:s", "-5"]).len(), 5);
    assert_eq!(strings(&mut c, &["SRANDMEMBER", "l1:s", "0"]), Vec::<String>::new());
    assert_eq!(err(&mut c, &["SRANDMEMBER", "l1:s", "1", "2"]), "ERR syntax error");
    let mut args = vec!["SADD", "l1:sbig"];
    let members: Vec<String> = (0..1000).map(|i| format!("m{i}")).collect();
    args.extend(members.iter().map(String::as_str));
    let _ = q(&mut c, &args);
    let mut got = strings(&mut c, &["SRANDMEMBER", "l1:sbig", "50"]);
    got.sort();
    got.dedup();
    assert_eq!(got.len(), 50);

    let _ = q(&mut c, &["SADD", "l1:sa", "x", "y"]);
    let _ = q(&mut c, &["SADD", "l1:sb", "z"]);
    assert_eq!(int(&mut c, &["SMOVE", "l1:sa", "l1:sb", "x"]), 1);
    assert_eq!(strings(&mut c, &["SMEMBERS", "l1:sa"]), ["y"]);
    assert_eq!(int(&mut c, &["SMOVE", "l1:sa", "l1:sb", "nope"]), 0);
    assert_eq!(int(&mut c, &["SMOVE", "l1:sa", "l1:sa", "y"]), 1);
    assert_eq!(int(&mut c, &["SMOVE", "l1:sa", "l1:sa", "nope"]), 0);
    // le dernier membre parti : la source disparaît
    assert_eq!(int(&mut c, &["SMOVE", "l1:sa", "l1:sc", "y"]), 1);
    assert_eq!(int(&mut c, &["EXISTS", "l1:sa"]), 0);
    let _ = q(&mut c, &["SET", "l1:ss", "v"]);
    assert_eq!(err(&mut c, &["SMOVE", "l1:sc", "l1:ss", "y"]), WRONGTYPE);
    assert_eq!(err(&mut c, &["SMOVE", "l1:ss", "l1:sc", "y"]), WRONGTYPE);
    // source absente : 0, même si la destination n'est pas un Set (comme Redis)
    assert_eq!(int(&mut c, &["SMOVE", "l1:s-none", "l1:ss", "y"]), 0);
    assert_eq!(err(&mut c, &["SMISMEMBER", "l1:ss", "a"]), WRONGTYPE);
}

// ── Serveur ──────────────────────────────────────────────────────────────────

#[test]
fn time_and_command_table() {
    let mut c = server();
    let t = strings(&mut c, &["TIME"]);
    assert_eq!(t.len(), 2);
    let secs: u64 = t[0].parse().unwrap();
    let us: u64 = t[1].parse().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(secs.abs_diff(now) <= 2 && us < 1_000_000);
    let n = int(&mut c, &["COMMAND", "COUNT"]);
    let list = strings(&mut c, &["COMMAND", "LIST"]);
    assert_eq!(list.len() as i64, n);
    for name in ["append", "getrange", "rename", "hrandfield", "smove", "time", "get"] {
        assert!(list.iter().any(|x| x == name), "{name}");
    }
    assert_eq!(err(&mut c, &["COMMAND", "COUNT", "x"]), "ERR wrong number of arguments for 'command|count' command");
}

/// FLUSHALL vide la base (une seule base : = FLUSHDB). Serveur à part.
#[test]
fn flushall() {
    const P: u16 = 16462;
    common::start_server(P);
    let mut c = common::conn(P);
    let _ = q(&mut c, &["MSET", "a", "1", "b", "2"]);
    assert_eq!(q(&mut c, &["FLUSHALL"]), Ok(V::Okay));
    assert_eq!(int(&mut c, &["DBSIZE"]), 0);
    assert_eq!(q(&mut c, &["FLUSHALL", "ASYNC"]), Ok(V::Okay));
    assert_eq!(err(&mut c, &["FLUSHALL", "FOO"]), "ERR syntax error");
    let _ = q(&mut c, &["SHUTDOWN", "NOSAVE"]);
}

/// Les écritures du lot 1 passent par l'AOF et se rejouent à l'identique (même
/// condensé après redémarrage).
#[test]
fn aof_replays_lot1_writes() {
    use std::process::{Command, Stdio};
    const P: u16 = 16463;
    let dir = std::env::temp_dir().join(format!("radixox-lot1-aof-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let start = || {
        let _ = Command::new("fuser").args(["-k", &format!("{P}/tcp")]).stderr(Stdio::null()).status();
        std::thread::sleep(std::time::Duration::from_millis(150));
        let child = Command::new(common::server_bin())
            .env("RADIXOX_PORT", P.to_string())
            .env("RADIXOX_DIR", &dir)
            .env("RADIXOX_SAVE", "")
            .env("RADIXOX_APPENDONLY", "yes")
            .env("RADIXOX_APPENDFSYNC", "always")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", P)).is_ok() {
                return child;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("serveur AOF pas prêt");
    };
    let mut child = start();
    let mut c = common::conn(P);
    let writes: &[&[&str]] = &[
        &["APPEND", "a", "hello"],
        &["APPEND", "a", " world"],
        &["SETRANGE", "a", "0", "J"],
        &["SET", "g", "old"],
        &["GETSET", "g", "new"],
        &["SET", "d", "x"],
        &["GETDEL", "d"],
        &["GETDEL", "d"],
        &["SET", "e", "v"],
        &["GETEX", "e", "EX", "1000"],
        &["INCRBYFLOAT", "f", "0.1"],
        &["INCRBYFLOAT", "f", "0.2"],
        &["MSETNX", "m1", "1", "m2", "2"],
        &["MSETNX", "m1", "9", "m3", "3"],
        &["PSETEX", "p", "500000", "v"],
        &["SET", "x", "v"],
        &["EXPIRE", "x", "500", "NX"],
        &["EXPIREAT", "x", "9999999999", "GT"],
        &["SET", "y", "v"],
        &["EXPIRE", "y", "0"],
        &["SET", "r1", "v"],
        &["RENAME", "r1", "r2"],
        &["RENAMENX", "r2", "a"],
        &["HSETNX", "h", "f", "v"],
        &["HINCRBYFLOAT", "h", "n", "1.5"],
        &["SADD", "s1", "a", "b"],
        &["SMOVE", "s1", "s2", "a"],
        &["SMOVE", "s1", "s2", "zz"],
    ];
    for w in writes {
        let _ = q(&mut c, w);
    }
    let digest = bulk(&mut c, &["DEBUG", "DIGEST"]).unwrap();
    let dbsize = int(&mut c, &["DBSIZE"]);
    let _ = q(&mut c, &["SHUTDOWN"]);
    let _ = child.wait();
    let mut child = start();
    let mut c = common::conn(P);
    assert_eq!(int(&mut c, &["DBSIZE"]), dbsize);
    assert_eq!(bulk(&mut c, &["DEBUG", "DIGEST"]).unwrap(), digest);
    assert_eq!(bulk(&mut c, &["GET", "a"]).as_deref(), Some("Jello world"));
    assert_eq!(bulk(&mut c, &["GET", "f"]).as_deref(), Some("0.3"));
    assert_eq!(int(&mut c, &["EXPIRETIME", "x"]), 9999999999);
    assert_eq!(int(&mut c, &["EXISTS", "y", "r1", "d", "m3"]), 0);
    let _ = q(&mut c, &["SHUTDOWN", "NOSAVE"]);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
