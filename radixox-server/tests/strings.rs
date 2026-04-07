mod common;

use std::sync::OnceLock;

use redis::Commands;

const PORT: u16 = 16379;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

// ── GET / SET ─────────────────────────────────────────────────────────────────

#[test]
fn set_and_get() {
    let mut c = server();
    let k = "str:set_get";
    let _: () = c.set(k, "hello").unwrap();
    let v: String = c.get(k).unwrap();
    assert_eq!(v, "hello");
}

#[test]
fn get_missing_key_nil() {
    let mut c = server();
    let k = "str:get_nil";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let v: Option<String> = c.get(k).unwrap();
    assert!(v.is_none());
}

#[test]
fn set_overwrites() {
    let mut c = server();
    let k = "str:set_overwrite";
    let _: () = c.set(k, "first").unwrap();
    let _: () = c.set(k, "second").unwrap();
    let v: String = c.get(k).unwrap();
    assert_eq!(v, "second");
}

// ── SET with options ──────────────────────────────────────────────────────────

#[test]
fn set_ex_expires() {
    let mut c = server();
    let k = "str:set_ex";
    let _: () = redis::cmd("SET").arg(k).arg("v").arg("EX").arg(100).query(&mut c).unwrap();
    let ttl: i64 = c.ttl(k).unwrap();
    assert!(ttl > 0 && ttl <= 100);
}

#[test]
fn set_px_expires_millis() {
    let mut c = server();
    let k = "str:set_px";
    let _: () = redis::cmd("SET").arg(k).arg("v").arg("PX").arg(100_000).query(&mut c).unwrap();
    let pttl: i64 = c.pttl(k).unwrap();
    assert!(pttl > 0 && pttl <= 100_000);
}

#[test]
fn set_nx_only_when_absent() {
    let mut c = server();
    let k = "str:set_nx";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    // first SET NX should succeed
    let r1: Option<String> = redis::cmd("SET")
        .arg(k).arg("v1").arg("NX")
        .query(&mut c).unwrap();
    assert_eq!(r1.as_deref(), Some("OK"));
    // second SET NX should fail (nil)
    let r2: Option<String> = redis::cmd("SET")
        .arg(k).arg("v2").arg("NX")
        .query(&mut c).unwrap();
    assert!(r2.is_none());
    let v: String = c.get(k).unwrap();
    assert_eq!(v, "v1");
}

#[test]
fn set_xx_only_when_present() {
    let mut c = server();
    let k = "str:set_xx";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    // SET XX on absent key → nil
    let r1: Option<String> = redis::cmd("SET")
        .arg(k).arg("v").arg("XX")
        .query(&mut c).unwrap();
    assert!(r1.is_none());
    // create the key then SET XX → OK
    let _: () = c.set(k, "original").unwrap();
    let r2: Option<String> = redis::cmd("SET")
        .arg(k).arg("updated").arg("XX")
        .query(&mut c).unwrap();
    assert_eq!(r2.as_deref(), Some("OK"));
    let v: String = c.get(k).unwrap();
    assert_eq!(v, "updated");
}

// ── DEL ──────────────────────────────────────────────────────────────────────

#[test]
fn del_existing_key() {
    let mut c = server();
    let k = "str:del_exist";
    let _: () = c.set(k, "v").unwrap();
    let n: i64 = c.del(k).unwrap();
    assert_eq!(n, 1);
    let v: Option<String> = c.get(k).unwrap();
    assert!(v.is_none());
}

#[test]
fn del_missing_key() {
    let mut c = server();
    let k = "str:del_miss";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let n: i64 = c.del(k).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn del_multiple_keys() {
    let mut c = server();
    let (k1, k2, k3) = ("str:del_m1", "str:del_m2", "str:del_m3");
    let _: () = c.set(k1, "v").unwrap();
    let _: () = c.set(k2, "v").unwrap();
    let _: () = redis::cmd("DEL").arg(k3).query(&mut c).unwrap();
    let n: i64 = redis::cmd("DEL").arg(k1).arg(k2).arg(k3).query(&mut c).unwrap();
    assert_eq!(n, 2);
}

// ── EXISTS ────────────────────────────────────────────────────────────────────

#[test]
fn exists_present() {
    let mut c = server();
    let k = "str:exists_yes";
    let _: () = c.set(k, "v").unwrap();
    let n: i64 = c.exists(k).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn exists_absent() {
    let mut c = server();
    let k = "str:exists_no";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let n: i64 = c.exists(k).unwrap();
    assert_eq!(n, 0);
}

// ── MGET / MSET ───────────────────────────────────────────────────────────────

#[test]
fn mset_and_mget() {
    let mut c = server();
    let (k1, k2) = ("str:mset1", "str:mset2");
    let _: () = redis::cmd("MSET").arg(k1).arg("v1").arg(k2).arg("v2").query(&mut c).unwrap();
    let vals: Vec<String> = c.mget(&[k1, k2]).unwrap();
    assert_eq!(vals, ["v1", "v2"]);
}

#[test]
fn mget_with_missing_returns_nil() {
    let mut c = server();
    let (k1, k_miss) = ("str:mget_exists", "str:mget_miss_xyz");
    let _: () = c.set(k1, "found").unwrap();
    let _: () = redis::cmd("DEL").arg(k_miss).query(&mut c).unwrap();
    let vals: Vec<Option<String>> = c.mget(&[k1, k_miss]).unwrap();
    assert_eq!(vals, [Some("found".to_string()), None]);
}

// ── SETNX / SETEX ─────────────────────────────────────────────────────────────

#[test]
fn setnx_absent() {
    let mut c = server();
    let k = "str:setnx_absent";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let n: i64 = c.set_nx(k, "v").unwrap();
    assert_eq!(n, 1);
}

#[test]
fn setnx_present() {
    let mut c = server();
    let k = "str:setnx_present";
    let _: () = c.set(k, "existing").unwrap();
    let n: i64 = c.set_nx(k, "new").unwrap();
    assert_eq!(n, 0);
    let v: String = c.get(k).unwrap();
    assert_eq!(v, "existing");
}

#[test]
fn setex_sets_ttl() {
    let mut c = server();
    let k = "str:setex";
    let _: () = redis::cmd("SETEX").arg(k).arg(50).arg("v").query(&mut c).unwrap();
    let ttl: i64 = c.ttl(k).unwrap();
    assert!(ttl > 0 && ttl <= 50);
}

// ── TTL / PTTL / EXPIRE / PEXPIRE / PERSIST ──────────────────────────────────

#[test]
fn ttl_no_expiry_is_minus_one() {
    let mut c = server();
    let k = "str:ttl_noexp";
    let _: () = c.set(k, "v").unwrap();
    let ttl: i64 = c.ttl(k).unwrap();
    assert_eq!(ttl, -1);
}

#[test]
fn ttl_missing_key_is_minus_two() {
    let mut c = server();
    let k = "str:ttl_miss";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let ttl: i64 = c.ttl(k).unwrap();
    assert_eq!(ttl, -2);
}

#[test]
fn expire_and_persist() {
    let mut c = server();
    let k = "str:expire_persist";
    let _: () = c.set(k, "v").unwrap();
    let set: i64 = c.expire(k, 100).unwrap();
    assert_eq!(set, 1);
    let ttl: i64 = c.ttl(k).unwrap();
    assert!(ttl > 0);
    let cleared: i64 = c.persist(k).unwrap();
    assert_eq!(cleared, 1);
    let ttl2: i64 = c.ttl(k).unwrap();
    assert_eq!(ttl2, -1);
}

#[test]
fn pexpire_sets_ms_ttl() {
    let mut c = server();
    let k = "str:pexpire";
    let _: () = c.set(k, "v").unwrap();
    let set: i64 = redis::cmd("PEXPIRE").arg(k).arg(60_000).query(&mut c).unwrap();
    assert_eq!(set, 1);
    let pttl: i64 = c.pttl(k).unwrap();
    assert!(pttl > 0 && pttl <= 60_000);
}

// ── INCR / DECR / INCRBY / DECRBY ────────────────────────────────────────────

#[test]
fn incr_from_zero() {
    let mut c = server();
    let k = "str:incr_zero";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let v: i64 = c.incr(k, 1).unwrap();
    assert_eq!(v, 1);
}

#[test]
fn incr_existing() {
    let mut c = server();
    let k = "str:incr_exist";
    let _: () = c.set(k, "10").unwrap();
    let v: i64 = c.incr(k, 1).unwrap();
    assert_eq!(v, 11);
}

#[test]
fn decr() {
    let mut c = server();
    let k = "str:decr";
    let _: () = c.set(k, "5").unwrap();
    let v: i64 = c.decr(k, 1).unwrap();
    assert_eq!(v, 4);
}

#[test]
fn incrby() {
    let mut c = server();
    let k = "str:incrby";
    let _: () = redis::cmd("DEL").arg(k).query(&mut c).unwrap();
    let v: i64 = c.incr(k, 10).unwrap();
    assert_eq!(v, 10);
}

#[test]
fn decrby() {
    let mut c = server();
    let k = "str:decrby";
    let _: () = c.set(k, "20").unwrap();
    let v: i64 = c.decr(k, 7).unwrap();
    assert_eq!(v, 13);
}

#[test]
fn incr_not_integer_error() {
    let mut c = server();
    let k = "str:incr_notint";
    let _: () = c.set(k, "notanumber").unwrap();
    let err = c.incr::<_, _, i64>(k, 1).unwrap_err();
    assert!(err.to_string().contains("not an integer"), "expected integer error, got: {err}");
}
