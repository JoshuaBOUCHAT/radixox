mod common;

use std::collections::HashSet;
use std::sync::OnceLock;

use redis::Commands;

const PORT: u16 = 16382;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

// ── SADD ─────────────────────────────────────────────────────────────────────

#[test]
fn sadd_single() {
    let mut c = server();
    let k = "set:sadd_single";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let added: i64 = c.sadd(k, "m").unwrap();
    assert_eq!(added, 1);
}

#[test]
fn sadd_multiple() {
    let mut c = server();
    let k = "set:sadd_multi";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let added: i64 = redis::cmd("SADD")
        .arg(k).arg("a").arg("b").arg("c")
        .query(&mut c).unwrap();
    assert_eq!(added, 3);
}

#[test]
fn sadd_duplicate_not_counted() {
    let mut c = server();
    let k = "set:sadd_dup";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.sadd(k, "m").unwrap();
    let again: i64 = c.sadd(k, "m").unwrap();
    assert_eq!(again, 0);
}

// ── SREM ─────────────────────────────────────────────────────────────────────

#[test]
fn srem_existing() {
    let mut c = server();
    let k = "set:srem_exist";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.sadd(k, "m").unwrap();
    let removed: i64 = c.srem(k, "m").unwrap();
    assert_eq!(removed, 1);
    let card: i64 = c.scard(k).unwrap();
    assert_eq!(card, 0);
}

#[test]
fn srem_missing() {
    let mut c = server();
    let k = "set:srem_miss";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.sadd(k, "m").unwrap();
    let removed: i64 = c.srem(k, "nosuch").unwrap();
    assert_eq!(removed, 0);
}

#[test]
fn srem_multiple() {
    let mut c = server();
    let k = "set:srem_multi";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(k).arg("a").arg("b").arg("c")
        .query(&mut c).unwrap();
    let removed: i64 = redis::cmd("SREM").arg(k).arg("a").arg("b").query(&mut c).unwrap();
    assert_eq!(removed, 2);
}

// ── SISMEMBER ─────────────────────────────────────────────────────────────────

#[test]
fn sismember_present() {
    let mut c = server();
    let k = "set:sismember_yes";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.sadd(k, "m").unwrap();
    let is_member: bool = c.sismember(k, "m").unwrap();
    assert!(is_member);
}

#[test]
fn sismember_absent() {
    let mut c = server();
    let k = "set:sismember_no";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.sadd(k, "m").unwrap();
    let is_member: bool = c.sismember(k, "other").unwrap();
    assert!(!is_member);
}

#[test]
fn sismember_empty_key() {
    let mut c = server();
    let k = "set:sismember_empty";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let is_member: bool = c.sismember(k, "m").unwrap();
    assert!(!is_member);
}

// ── SCARD ─────────────────────────────────────────────────────────────────────

#[test]
fn scard_empty() {
    let mut c = server();
    let k = "set:scard_empty";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let n: i64 = c.scard(k).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn scard_correct_count() {
    let mut c = server();
    let k = "set:scard_count";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(k).arg("a").arg("b").arg("c")
        .query(&mut c).unwrap();
    let n: i64 = c.scard(k).unwrap();
    assert_eq!(n, 3);
}

// ── SMEMBERS ─────────────────────────────────────────────────────────────────

#[test]
fn smembers_all() {
    let mut c = server();
    let k = "set:smembers";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(k).arg("a").arg("b").arg("c")
        .query(&mut c).unwrap();
    let members: HashSet<String> = c.smembers(k).unwrap();
    assert_eq!(members, ["a", "b", "c"].iter().map(|s| s.to_string()).collect());
}

#[test]
fn smembers_empty_key() {
    let mut c = server();
    let k = "set:smembers_empty";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let members: HashSet<String> = c.smembers(k).unwrap();
    assert!(members.is_empty());
}

// ── SPOP ─────────────────────────────────────────────────────────────────────

#[test]
fn spop_single_removes_element() {
    let mut c = server();
    let k = "set:spop_single";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.sadd(k, "only").unwrap();
    let popped: String = c.spop(k).unwrap();
    assert_eq!(popped, "only");
    let card: i64 = c.scard(k).unwrap();
    assert_eq!(card, 0);
}

#[test]
fn spop_empty_key_nil() {
    let mut c = server();
    let k = "set:spop_empty";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let popped: Option<String> = c.spop(k).unwrap();
    assert!(popped.is_none());
}

#[test]
fn spop_count_removes_n_elements() {
    let mut c = server();
    let k = "set:spop_count";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(k).arg("a").arg("b").arg("c").arg("d")
        .query(&mut c).unwrap();
    let popped: Vec<String> = redis::cmd("SPOP").arg(k).arg(2).query(&mut c).unwrap();
    assert_eq!(popped.len(), 2);
    let card: i64 = c.scard(k).unwrap();
    assert_eq!(card, 2);
}

// ── WRONGTYPE errors ──────────────────────────────────────────────────────────

#[test]
fn wrongtype_sadd_on_string() {
    let mut c = server();
    let k = "set:wrongtype";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: () = c.set(k, "val").unwrap();
    let err = c.sadd::<_, _, i64>(k, "m").unwrap_err();
    common::assert_wrongtype(&err);
}

#[test]
fn wrongtype_scard_on_zset() {
    let mut c = server();
    let k = "set:wrongtype_zset";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "m", 1.0).unwrap();
    let err = c.scard::<_, i64>(k).unwrap_err();
    common::assert_wrongtype(&err);
}
