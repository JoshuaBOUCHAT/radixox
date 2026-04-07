mod common;

use std::collections::HashMap;
use std::sync::OnceLock;

use redis::Commands;

const PORT: u16 = 16381;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

// ── HSET ─────────────────────────────────────────────────────────────────────

#[test]
fn hset_single_field() {
    let mut c = server();
    let k = "hash:hset_single";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let added: i64 = c.hset(k, "f1", "v1").unwrap();
    assert_eq!(added, 1);
}

#[test]
fn hset_multiple_fields() {
    let mut c = server();
    let k = "hash:hset_multi";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let added: i64 = redis::cmd("HSET")
        .arg(k)
        .arg("f1").arg("v1")
        .arg("f2").arg("v2")
        .arg("f3").arg("v3")
        .query(&mut c).unwrap();
    assert_eq!(added, 3);
}

#[test]
fn hset_overwrite_returns_zero() {
    let mut c = server();
    let k = "hash:hset_overwrite";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "f", "v1").unwrap();
    let again: i64 = c.hset(k, "f", "v2").unwrap();
    assert_eq!(again, 0);
}

// ── HMSET ────────────────────────────────────────────────────────────────────

#[test]
fn hmset_legacy_returns_ok() {
    let mut c = server();
    let k = "hash:hmset";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: () = redis::cmd("HMSET")
        .arg(k)
        .arg("a").arg("1")
        .arg("b").arg("2")
        .query(&mut c)
        .unwrap();
    let val: String = c.hget(k, "a").unwrap();
    assert_eq!(val, "1");
}

// ── HGET ─────────────────────────────────────────────────────────────────────

#[test]
fn hget_existing_field() {
    let mut c = server();
    let k = "hash:hget_exist";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "name", "alice").unwrap();
    let val: String = c.hget(k, "name").unwrap();
    assert_eq!(val, "alice");
}

#[test]
fn hget_missing_field_is_nil() {
    let mut c = server();
    let k = "hash:hget_nil";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "f", "v").unwrap();
    let val: Option<String> = c.hget(k, "nosuch").unwrap();
    assert!(val.is_none());
}

// ── HGETALL ───────────────────────────────────────────────────────────────────

#[test]
fn hgetall_returns_all_pairs() {
    let mut c = server();
    let k = "hash:hgetall";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("HSET")
        .arg(k).arg("x").arg("1").arg("y").arg("2")
        .query(&mut c).unwrap();
    let map: HashMap<String, String> = c.hgetall(k).unwrap();
    assert_eq!(map.get("x").map(String::as_str), Some("1"));
    assert_eq!(map.get("y").map(String::as_str), Some("2"));
    assert_eq!(map.len(), 2);
}

#[test]
fn hgetall_empty_key() {
    let mut c = server();
    let k = "hash:hgetall_empty";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let map: HashMap<String, String> = c.hgetall(k).unwrap();
    assert!(map.is_empty());
}

// ── HDEL ─────────────────────────────────────────────────────────────────────

#[test]
fn hdel_existing_field() {
    let mut c = server();
    let k = "hash:hdel_exist";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "f", "v").unwrap();
    let deleted: i64 = c.hdel(k, "f").unwrap();
    assert_eq!(deleted, 1);
    let val: Option<String> = c.hget(k, "f").unwrap();
    assert!(val.is_none());
}

#[test]
fn hdel_missing_field() {
    let mut c = server();
    let k = "hash:hdel_miss";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "f", "v").unwrap();
    let deleted: i64 = c.hdel(k, "nosuch").unwrap();
    assert_eq!(deleted, 0);
}

#[test]
fn hdel_multiple_fields() {
    let mut c = server();
    let k = "hash:hdel_multi";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("HSET")
        .arg(k).arg("a").arg("1").arg("b").arg("2").arg("c").arg("3")
        .query(&mut c).unwrap();
    let deleted: i64 = redis::cmd("HDEL").arg(k).arg("a").arg("b").query(&mut c).unwrap();
    assert_eq!(deleted, 2);
    let len: i64 = c.hlen(k).unwrap();
    assert_eq!(len, 1);
}

// ── HEXISTS ───────────────────────────────────────────────────────────────────

#[test]
fn hexists_present() {
    let mut c = server();
    let k = "hash:hexists_yes";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "f", "v").unwrap();
    let exists: bool = c.hexists(k, "f").unwrap();
    assert!(exists);
}

#[test]
fn hexists_absent() {
    let mut c = server();
    let k = "hash:hexists_no";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let exists: bool = c.hexists(k, "nosuch").unwrap();
    assert!(!exists);
}

// ── HLEN ─────────────────────────────────────────────────────────────────────

#[test]
fn hlen_correct() {
    let mut c = server();
    let k = "hash:hlen";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("HSET")
        .arg(k).arg("a").arg("1").arg("b").arg("2")
        .query(&mut c).unwrap();
    let len: i64 = c.hlen(k).unwrap();
    assert_eq!(len, 2);
}

// ── HKEYS / HVALS ─────────────────────────────────────────────────────────────

#[test]
fn hkeys_and_hvals() {
    let mut c = server();
    let k = "hash:hkeys_hvals";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("HSET")
        .arg(k).arg("x").arg("10").arg("y").arg("20")
        .query(&mut c).unwrap();
    let mut keys: Vec<String> = c.hkeys(k).unwrap();
    keys.sort();
    assert_eq!(keys, ["x", "y"]);
    let mut vals: Vec<String> = c.hvals(k).unwrap();
    vals.sort();
    assert_eq!(vals, ["10", "20"]);
}

// ── HMGET ─────────────────────────────────────────────────────────────────────

#[test]
fn hmget_mixed_present_absent() {
    let mut c = server();
    let k = "hash:hmget";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "a", "1").unwrap();
    let vals: Vec<Option<String>> = redis::cmd("HMGET")
        .arg(k).arg("a").arg("nosuch")
        .query(&mut c).unwrap();
    assert_eq!(vals, [Some("1".to_string()), None]);
}

// ── HINCRBY ──────────────────────────────────────────────────────────────────

#[test]
fn hincrby_creates_and_increments() {
    let mut c = server();
    let k = "hash:hincrby";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let v1: i64 = c.hincr(k, "counter", 5).unwrap();
    assert_eq!(v1, 5);
    let v2: i64 = c.hincr(k, "counter", -3).unwrap();
    assert_eq!(v2, 2);
}

#[test]
fn hincrby_not_integer_error() {
    let mut c = server();
    let k = "hash:hincrby_notint";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.hset(k, "f", "notanumber").unwrap();
    let err = c.hincr::<_, _, _, i64>(k, "f", 1).unwrap_err();
    // Server returns "ERR hash value is not an integer or out of range"
    assert!(err.to_string().contains("not an integer"), "expected integer error, got: {err}");
}

// ── WRONGTYPE errors ──────────────────────────────────────────────────────────

#[test]
fn wrongtype_hset_on_string() {
    let mut c = server();
    let k = "hash:wrongtype";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: () = c.set(k, "str").unwrap();
    let err = c.hset::<_, _, _, i64>(k, "f", "v").unwrap_err();
    common::assert_wrongtype(&err);
}
