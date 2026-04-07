mod common;

use std::collections::HashSet;
use std::sync::OnceLock;

use redis::Commands;

const PORT: u16 = 16380;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

// ── PING / ECHO ───────────────────────────────────────────────────────────────

#[test]
fn ping() {
    let mut c = server();
    let r: String = redis::cmd("PING").query(&mut c).unwrap();
    assert_eq!(r, "PONG");
}

#[test]
fn echo() {
    let mut c = server();
    let r: String = redis::cmd("ECHO").arg("hello world").query(&mut c).unwrap();
    assert_eq!(r, "hello world");
}

// ── TYPE ─────────────────────────────────────────────────────────────────────

#[test]
fn type_string() {
    let mut c = server();
    let k = "keys:type_str";
    let _: () = c.set(k, "v").unwrap();
    let t: String = redis::cmd("TYPE").arg(k).query(&mut c).unwrap();
    assert_eq!(t, "string");
}

#[test]
fn type_hash() {
    let mut c = server();
    let k = "keys:type_hash";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "f", "v").unwrap();
    let t: String = redis::cmd("TYPE").arg(k).query(&mut c).unwrap();
    assert_eq!(t, "hash");
}

#[test]
fn type_set() {
    let mut c = server();
    let k = "keys:type_set";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.sadd(k, "m").unwrap();
    let t: String = redis::cmd("TYPE").arg(k).query(&mut c).unwrap();
    assert_eq!(t, "set");
}

#[test]
fn type_zset() {
    let mut c = server();
    let k = "keys:type_zset";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "m", 1.0).unwrap();
    let t: String = redis::cmd("TYPE").arg(k).query(&mut c).unwrap();
    assert_eq!(t, "zset");
}

#[test]
fn type_missing_key() {
    let mut c = server();
    let k = "keys:type_none";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let t: String = redis::cmd("TYPE").arg(k).query(&mut c).unwrap();
    assert_eq!(t, "none");
}

// ── KEYS (glob) ───────────────────────────────────────────────────────────────

// Use a unique namespace so parallel tests don't interfere with KEYS scans.

#[test]
fn keys_prefix_glob() {
    let mut c = server();
    let prefix = "keys:glob_prefix_test";
    for i in 0..3 {
        let _: () = c.set(format!("{prefix}:{i}"), i).unwrap();
    }
    let found: HashSet<String> = redis::cmd("KEYS")
        .arg(format!("{prefix}:*"))
        .query(&mut c)
        .unwrap();
    assert_eq!(found.len(), 3);
    for i in 0..3 {
        assert!(found.contains(&format!("{prefix}:{i}")));
    }
}

#[test]
fn keys_question_mark_glob() {
    let mut c = server();
    let prefix = "keys:qmark";
    let _: () = c.set(format!("{prefix}:a"), 1).unwrap();
    let _: () = c.set(format!("{prefix}:b"), 2).unwrap();
    // "keys:qmark:?" matches single char after colon
    let found: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{prefix}:?"))
        .query(&mut c)
        .unwrap();
    assert_eq!(found.len(), 2);
}

#[test]
fn keys_exact_match() {
    let mut c = server();
    let k = "keys:exact_xyz_unique";
    let _: () = c.set(k, "v").unwrap();
    let found: Vec<String> = redis::cmd("KEYS").arg(k).query(&mut c).unwrap();
    assert_eq!(found, [k]);
}

#[test]
fn keys_no_match() {
    let mut c = server();
    let found: Vec<String> = redis::cmd("KEYS")
        .arg("keys:no_such_prefix_xyz_99999:*")
        .query(&mut c)
        .unwrap();
    assert!(found.is_empty());
}

// ── DBSIZE ────────────────────────────────────────────────────────────────────

#[test]
fn dbsize_positive_after_set() {
    let mut c = server();
    let _: () = c.set("keys:dbsize_probe", "v").unwrap();
    let size: i64 = redis::cmd("DBSIZE").query(&mut c).unwrap();
    assert!(size >= 1, "DBSIZE should be >= 1, got {size}");
}

// ── FLUSHDB ───────────────────────────────────────────────────────────────────

#[test]
fn flushdb_clears_all_keys() {
    // This test is sensitive — it flushes the whole DB.
    // Run last by using a separate, dedicated connection; isolate with a fresh server on a different port.
    let flush_port = 16384;
    static FLUSH_INIT: OnceLock<()> = OnceLock::new();
    FLUSH_INIT.get_or_init(|| common::start_server(flush_port));
    let mut c = common::conn(flush_port);

    let _: () = c.set("flushdb:a", "1").unwrap();
    let _: () = c.set("flushdb:b", "2").unwrap();
    let _: () = redis::cmd("FLUSHDB").query(&mut c).unwrap();
    let size: i64 = redis::cmd("DBSIZE").query(&mut c).unwrap();
    assert_eq!(size, 0);
}

// ── SELECT ────────────────────────────────────────────────────────────────────

#[test]
fn select_returns_ok() {
    let mut c = server();
    let r: String = redis::cmd("SELECT").arg(0).query(&mut c).unwrap();
    assert_eq!(r, "OK");
}
