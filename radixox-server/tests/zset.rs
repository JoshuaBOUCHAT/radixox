mod common;

use std::sync::OnceLock;

use redis::Commands;

const PORT: u16 = 16383;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

// ── ZADD ─────────────────────────────────────────────────────────────────────

#[test]
fn zadd_single_member() {
    let mut c = server();
    let k = "zset:zadd_single";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let added: i64 = c.zadd(k, "alpha", 1.0).unwrap();
    assert_eq!(added, 1);
}

#[test]
fn zadd_multiple_members() {
    let mut c = server();
    let k = "zset:zadd_multi";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let added: i64 = redis::cmd("ZADD")
        .arg(k)
        .arg(1.0)
        .arg("a")
        .arg(2.0)
        .arg("b")
        .arg(3.0)
        .arg("c")
        .query(&mut c)
        .unwrap();
    assert_eq!(added, 3);
}

#[test]
fn zadd_returns_new_members_count_only() {
    let mut c = server();
    let k = "zset:zadd_new_only";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "m", 1.0).unwrap();
    // re-adding the same member with a different score → 0 new, 0 returned
    let again: i64 = c.zadd(k, "m", 99.0).unwrap();
    assert_eq!(again, 0, "re-adding existing member should not count as new");
}

#[test]
fn zadd_updates_score() {
    let mut c = server();
    let k = "zset:zadd_update_score";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "m", 1.0).unwrap();
    let _: i64 = c.zadd(k, "m", 42.0).unwrap();
    let score: f64 = c.zscore(k, "m").unwrap();
    assert!((score - 42.0).abs() < f64::EPSILON);
}

// ── ZCARD ─────────────────────────────────────────────────────────────────────

#[test]
fn zcard_empty_key() {
    let mut c = server();
    let k = "zset:zcard_empty";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let n: i64 = c.zcard(k).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn zcard_after_adds() {
    let mut c = server();
    let k = "zset:zcard_adds";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("ZADD")
        .arg(k)
        .arg(1.0).arg("a")
        .arg(2.0).arg("b")
        .query(&mut c).unwrap();
    let n: i64 = c.zcard(k).unwrap();
    assert_eq!(n, 2);
}

// ── ZSCORE ────────────────────────────────────────────────────────────────────

#[test]
fn zscore_existing_member() {
    let mut c = server();
    let k = "zset:zscore_exist";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "x", 3.14).unwrap();
    let score: f64 = c.zscore(k, "x").unwrap();
    assert!((score - 3.14).abs() < 1e-9);
}

#[test]
fn zscore_missing_member_is_nil() {
    let mut c = server();
    let k = "zset:zscore_nil";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let score: Option<f64> = c.zscore(k, "nosuchkey").unwrap();
    assert!(score.is_none());
}

// ── ZRANGE ───────────────────────────────────────────────────────────────────

#[test]
fn zrange_asc() {
    let mut c = server();
    let k = "zset:zrange_asc";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("ZADD")
        .arg(k)
        .arg(3.0).arg("c")
        .arg(1.0).arg("a")
        .arg(2.0).arg("b")
        .query(&mut c).unwrap();
    let members: Vec<String> = c.zrange(k, 0, -1).unwrap();
    assert_eq!(members, ["a", "b", "c"]);
}

#[test]
fn zrange_partial() {
    let mut c = server();
    let k = "zset:zrange_partial";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("ZADD")
        .arg(k)
        .arg(1.0).arg("a")
        .arg(2.0).arg("b")
        .arg(3.0).arg("c")
        .query(&mut c).unwrap();
    let members: Vec<String> = c.zrange(k, 0, 1).unwrap();
    assert_eq!(members, ["a", "b"]);
}

#[test]
fn zrange_negative_index() {
    let mut c = server();
    let k = "zset:zrange_neg";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("ZADD")
        .arg(k)
        .arg(1.0).arg("a")
        .arg(2.0).arg("b")
        .arg(3.0).arg("c")
        .query(&mut c).unwrap();
    let members: Vec<String> = c.zrange(k, -2, -1).unwrap();
    assert_eq!(members, ["b", "c"]);
}

#[test]
fn zrange_empty_key() {
    let mut c = server();
    let k = "zset:zrange_empty";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let members: Vec<String> = c.zrange(k, 0, -1).unwrap();
    assert!(members.is_empty());
}

#[test]
fn zrange_withscores() {
    let mut c = server();
    let k = "zset:zrange_ws";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("ZADD")
        .arg(k)
        .arg(10.0).arg("a")
        .arg(20.0).arg("b")
        .query(&mut c).unwrap();
    let pairs: Vec<(String, f64)> = c.zrange_withscores(k, 0, -1).unwrap();
    assert_eq!(pairs, [("a".to_string(), 10.0), ("b".to_string(), 20.0)]);
}

// ── ZREM ─────────────────────────────────────────────────────────────────────

#[test]
fn zrem_existing_member() {
    let mut c = server();
    let k = "zset:zrem_exist";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "m", 1.0).unwrap();
    let removed: i64 = c.zrem(k, "m").unwrap();
    assert_eq!(removed, 1);
    let card: i64 = c.zcard(k).unwrap();
    assert_eq!(card, 0);
}

#[test]
fn zrem_missing_member() {
    let mut c = server();
    let k = "zset:zrem_miss";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "m", 1.0).unwrap();
    let removed: i64 = c.zrem(k, "nosuch").unwrap();
    assert_eq!(removed, 0);
}

#[test]
fn zrem_multiple_members() {
    let mut c = server();
    let k = "zset:zrem_multi";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = redis::cmd("ZADD")
        .arg(k)
        .arg(1.0).arg("a")
        .arg(2.0).arg("b")
        .arg(3.0).arg("c")
        .query(&mut c).unwrap();
    let removed: i64 = redis::cmd("ZREM").arg(k).arg("a").arg("b").query(&mut c).unwrap();
    assert_eq!(removed, 2);
    let card: i64 = c.zcard(k).unwrap();
    assert_eq!(card, 1);
}

// ── ZINCRBY ───────────────────────────────────────────────────────────────────

#[test]
fn zincrby_existing() {
    let mut c = server();
    let k = "zset:zincrby_exist";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: i64 = c.zadd(k, "m", 5.0).unwrap();
    let new_score: f64 = c.zincr(k, "m", 3.0).unwrap();
    assert!((new_score - 8.0).abs() < f64::EPSILON);
}

#[test]
fn zincrby_creates_member() {
    let mut c = server();
    let k = "zset:zincrby_new";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let new_score: f64 = c.zincr(k, "m", 7.5).unwrap();
    assert!((new_score - 7.5).abs() < f64::EPSILON);
    let card: i64 = c.zcard(k).unwrap();
    assert_eq!(card, 1);
}

// ── WRONGTYPE errors ──────────────────────────────────────────────────────────

#[test]
fn wrongtype_zadd_on_string() {
    let mut c = server();
    let k = "zset:wrongtype_zadd";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: () = c.set(k, "value").unwrap();
    let err = c.zadd::<_, _, _, i64>(k, "m", 1.0).unwrap_err();
    common::assert_wrongtype(&err);
}

#[test]
fn wrongtype_zcard_on_string() {
    let mut c = server();
    let k = "zset:wrongtype_zcard";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let _: () = c.set(k, "value").unwrap();
    let err = c.zcard::<_, i64>(k).unwrap_err();
    common::assert_wrongtype(&err);
}
