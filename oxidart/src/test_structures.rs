/// Comprehensive tests for Hash, Set, and ZSet commands.
///
/// Focus areas:
/// - Keys with common prefixes (ART path compression edge cases)
/// - Cross-key isolation: ops on one key must not affect neighbors
/// - Auto-cleanup of empty structures and its effect on siblings
/// - WRONGTYPE errors across all type combinations
/// - ZSet double-index consistency (BTreeSet + HashMap always in sync)
/// - Edge cases: empty structures, single element, large counts
use bytes::Bytes;

use crate::OxidArt;

// ───────────────────────────────────────────────────────── helpers ──────────

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

fn bv(items: &[&str]) -> Vec<Bytes> {
    items.iter().map(|s| b(s)).collect()
}

fn fv(pairs: &[(&str, &str)]) -> Vec<(Bytes, Bytes)> {
    pairs.iter().map(|(f, v)| (b(f), b(v))).collect()
}

fn sm(pairs: &[(&str, f64)]) -> Vec<(f64, Bytes)> {
    pairs.iter().map(|(m, s)| (*s, b(m))).collect()
}

// ═══════════════════════════════════════════════════════════════════════════
// HASH TESTS
// ═══════════════════════════════════════════════════════════════════════════

// ──────────────────────────────────────────────────── basic ─────────────

#[test]
fn hash_hset_hget_basic() {
    let mut art = OxidArt::new();
    let added = art.cmd_hset(b"key", &fv(&[("f1", "v1"), ("f2", "v2")]), None).unwrap();
    assert_eq!(added, 2);

    assert_eq!(art.cmd_hget(b"key", b"f1").unwrap(), Some(b("v1")));
    assert_eq!(art.cmd_hget(b"key", b"f2").unwrap(), Some(b("v2")));
    assert_eq!(art.cmd_hget(b"key", b"absent").unwrap(), None);
}

#[test]
fn hash_hset_update_does_not_increment_added() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("f", "old")]), None).unwrap();
    let added = art.cmd_hset(b"k", &fv(&[("f", "new")]), None).unwrap();
    assert_eq!(added, 0, "update must not count as new field");
    assert_eq!(art.cmd_hget(b"k", b"f").unwrap(), Some(b("new")));
}

#[test]
fn hash_hset_mixed_add_update() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("exists", "v")]), None).unwrap();
    let added = art.cmd_hset(b"k", &fv(&[("exists", "new"), ("fresh", "v2")]), None).unwrap();
    assert_eq!(added, 1);
}

#[test]
fn hash_hgetall_order() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("z", "1"), ("a", "2"), ("m", "3")]), None).unwrap();
    let all = art.cmd_hgetall(b"k").unwrap();
    // BTreeMap → lexicographic order: a, m, z
    assert_eq!(all, vec![b("a"), b("2"), b("m"), b("3"), b("z"), b("1")]);
}

#[test]
fn hash_hgetall_missing_key() {
    let mut art = OxidArt::new();
    assert!(art.cmd_hgetall(b"nope").unwrap().is_empty());
}

#[test]
fn hash_hdel_basic() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("a", "1"), ("b", "2")]), None).unwrap();
    let deleted = art.cmd_hdel(b"k", &bv(&["a"])).unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(art.cmd_hget(b"k", b"a").unwrap(), None);
    assert_eq!(art.cmd_hget(b"k", b"b").unwrap(), Some(b("2")));
}

#[test]
fn hash_hdel_absent_field() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("f", "v")]), None).unwrap();
    assert_eq!(art.cmd_hdel(b"k", &bv(&["nope"])).unwrap(), 0);
}

#[test]
fn hash_hdel_missing_key() {
    let mut art = OxidArt::new();
    assert_eq!(art.cmd_hdel(b"nope", &bv(&["f"])).unwrap(), 0);
}

#[test]
fn hash_hdel_auto_cleanup_on_last_field() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("only", "v")]), None).unwrap();
    art.cmd_hdel(b"k", &bv(&["only"])).unwrap();
    // Key should no longer exist
    assert!(art.cmd_hgetall(b"k").unwrap().is_empty());
    assert_eq!(art.cmd_hlen(b"k").unwrap(), 0);
}

#[test]
fn hash_hdel_many_fields() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("a","1"),("b","2"),("c","3"),("d","4")]), None).unwrap();
    let del = art.cmd_hdel(b"k", &bv(&["a","c","z"])).unwrap();
    assert_eq!(del, 2);
    assert_eq!(art.cmd_hlen(b"k").unwrap(), 2);
}

#[test]
fn hash_hexists() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("f","v")]), None).unwrap();
    assert!(art.cmd_hexists(b"k", b"f").unwrap());
    assert!(!art.cmd_hexists(b"k", b"missing").unwrap());
    assert!(!art.cmd_hexists(b"nope", b"f").unwrap());
}

#[test]
fn hash_hlen_hkeys_hvals_hmget() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("a","1"),("b","2"),("c","3")]), None).unwrap();
    assert_eq!(art.cmd_hlen(b"k").unwrap(), 3);

    let keys = art.cmd_hkeys(b"k").unwrap();
    assert_eq!(keys, vec![b("a"), b("b"), b("c")]);

    let vals = art.cmd_hvals(b"k").unwrap();
    assert_eq!(vals, vec![b("1"), b("2"), b("3")]);

    let mg = art.cmd_hmget(b"k", &bv(&["a","c","z"])).unwrap();
    assert_eq!(mg, vec![Some(b("1")), Some(b("3")), None]);
}

#[test]
fn hash_hincrby_basic() {
    let mut art = OxidArt::new();
    // non-existent field starts at 0
    let v = art.cmd_hincrby(b"k", b"counter", 5).unwrap();
    assert_eq!(v, 5);
    let v2 = art.cmd_hincrby(b"k", b"counter", -3).unwrap();
    assert_eq!(v2, 2);
}

#[test]
fn hash_hincrby_on_string_field() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("n", "10")]), None).unwrap();
    assert_eq!(art.cmd_hincrby(b"k", b"n", 5).unwrap(), 15);
}

#[test]
fn hash_hincrby_non_numeric_field_errors() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("f", "notanumber")]), None).unwrap();
    assert!(art.cmd_hincrby(b"k", b"f", 1).is_err());
}

// ──────────────────────────────────────────────────── key isolation ─────────

/// Many hashes with common prefix — ART path compression must not mix them up.
#[test]
fn hash_many_similar_keys_isolation() {
    let mut art = OxidArt::new();

    // Insert 50 hashes: user:0 .. user:49, each with unique field/value
    for i in 0u32..50 {
        let key = format!("user:{i}");
        let field = format!("field{i}");
        let value = format!("value{i}");
        art.cmd_hset(key.as_bytes(), &fv(&[(&field, &value)]), None).unwrap();
    }

    // Verify each key has exactly its own data
    for i in 0u32..50 {
        let key = format!("user:{i}");
        let field = format!("field{i}");
        let expected = format!("value{i}");

        // Correct field present
        assert_eq!(
            art.cmd_hget(key.as_bytes(), field.as_bytes()).unwrap(),
            Some(Bytes::copy_from_slice(expected.as_bytes())),
            "wrong value for user:{i}"
        );
        // Length is exactly 1
        assert_eq!(art.cmd_hlen(key.as_bytes()).unwrap(), 1, "wrong len for user:{i}");
    }
}

#[test]
fn hash_delete_one_preserves_siblings() {
    let mut art = OxidArt::new();
    for i in 0u32..10 {
        let key = format!("h:{i}");
        art.cmd_hset(key.as_bytes(), &fv(&[("f", &i.to_string())]), None).unwrap();
    }

    // Delete h:5
    art.cmd_hdel(b"h:5", &bv(&["f"])).unwrap();

    // All others must still have their values
    for i in 0u32..10 {
        if i == 5 { continue; }
        let key = format!("h:{i}");
        let expected = i.to_string();
        assert_eq!(
            art.cmd_hget(key.as_bytes(), b"f").unwrap(),
            Some(Bytes::copy_from_slice(expected.as_bytes())),
            "h:{i} corrupted after deleting h:5"
        );
    }
}

#[test]
fn hash_overwrite_field_does_not_affect_others() {
    let mut art = OxidArt::new();
    art.cmd_hset(b"k", &fv(&[("a","1"),("b","2"),("c","3")]), None).unwrap();
    art.cmd_hset(b"k", &fv(&[("b","999")]), None).unwrap();
    assert_eq!(art.cmd_hget(b"k", b"a").unwrap(), Some(b("1")));
    assert_eq!(art.cmd_hget(b"k", b"b").unwrap(), Some(b("999")));
    assert_eq!(art.cmd_hget(b"k", b"c").unwrap(), Some(b("3")));
}

// ──────────────────────────────────────────────────── WRONGTYPE ─────────

#[test]
fn hash_wrongtype_on_string_key() {
    use crate::value::Value;
    let mut art = OxidArt::new();
    art.set(Bytes::from_static(b"str"), Value::from_str("hello"));

    assert!(art.cmd_hget(b"str", b"f").is_err());
    assert!(art.cmd_hgetall(b"str").is_err());
    assert!(art.cmd_hdel(b"str", &bv(&["f"])).is_err());
    assert!(art.cmd_hexists(b"str", b"f").is_err());
    assert!(art.cmd_hlen(b"str").is_err());
    assert!(art.cmd_hkeys(b"str").is_err());
    assert!(art.cmd_hvals(b"str").is_err());
    assert!(art.cmd_hmget(b"str", &bv(&["f"])).is_err());
    // hset returns TypeError, not RedisType
    assert!(art.cmd_hset(b"str", &fv(&[("f","v")]), None).is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// SET TESTS
// ═══════════════════════════════════════════════════════════════════════════

// ──────────────────────────────────────────────────── basic ─────────────

#[test]
fn set_sadd_basic() {
    let mut art = OxidArt::new();
    let added = art.cmd_sadd(b"s", &bv(&["a","b","c"]), None).unwrap();
    assert_eq!(added, 3);
    assert_eq!(art.cmd_scard(b"s").unwrap(), 3);
}

#[test]
fn set_sadd_deduplication() {
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["a","b"]), None).unwrap();
    let added = art.cmd_sadd(b"s", &bv(&["b","c"]), None).unwrap();
    assert_eq!(added, 1, "only c is new");
    assert_eq!(art.cmd_scard(b"s").unwrap(), 3);
}

#[test]
fn set_sismember() {
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["x","y"]), None).unwrap();
    assert!(art.cmd_sismember(b"s", b"x").unwrap());
    assert!(!art.cmd_sismember(b"s", b"z").unwrap());
    assert!(!art.cmd_sismember(b"nope", b"x").unwrap());
}

#[test]
fn set_smembers_sorted() {
    let mut art = OxidArt::new();
    // Insert in reverse order — BTreeSet must return lexicographic order
    art.cmd_sadd(b"s", &bv(&["z","m","a","b"]), None).unwrap();
    let members = art.cmd_smembers(b"s").unwrap();
    assert_eq!(members, vec![b("a"), b("b"), b("m"), b("z")]);
}

#[test]
fn set_smembers_empty_key() {
    let mut art = OxidArt::new();
    assert!(art.cmd_smembers(b"nope").unwrap().is_empty());
}

#[test]
fn set_srem_basic() {
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["a","b","c"]), None).unwrap();
    let removed = art.cmd_srem(b"s", &bv(&["a","z"])).unwrap();
    assert_eq!(removed, 1, "only a existed");
    assert!(!art.cmd_sismember(b"s", b"a").unwrap());
    assert!(art.cmd_sismember(b"s", b"b").unwrap());
}

#[test]
fn set_srem_missing_key() {
    let mut art = OxidArt::new();
    assert_eq!(art.cmd_srem(b"nope", &bv(&["x"])).unwrap(), 0);
}

#[test]
fn set_srem_auto_cleanup() {
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["only"]), None).unwrap();
    art.cmd_srem(b"s", &bv(&["only"])).unwrap();
    assert_eq!(art.cmd_scard(b"s").unwrap(), 0);
    assert!(art.cmd_smembers(b"s").unwrap().is_empty());
}

#[test]
fn set_spop_single() {
    use crate::scommand::SPOPResult;
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["a","b","c"]), None).unwrap();
    let res = art.cmd_spop(b"s", None).unwrap();
    let popped = match res {
        SPOPResult::Single(Some(v)) => v,
        _ => panic!("expected Single(Some)"),
    };
    assert!(!art.cmd_sismember(b"s", &popped).unwrap());
    assert_eq!(art.cmd_scard(b"s").unwrap(), 2);
}

#[test]
fn set_spop_count() {
    use crate::scommand::SPOPResult;
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["a","b","c","d","e"]), None).unwrap();
    let res = art.cmd_spop(b"s", Some(b"3")).unwrap();
    let popped = match res {
        SPOPResult::Multiple(v) => v,
        _ => panic!("expected Multiple"),
    };
    assert_eq!(popped.len(), 3);
    assert_eq!(art.cmd_scard(b"s").unwrap(), 2);
}

#[test]
fn set_spop_count_exceeds_cardinality() {
    use crate::scommand::SPOPResult;
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["a","b"]), None).unwrap();
    let res = art.cmd_spop(b"s", Some(b"10")).unwrap();
    let popped = match res {
        SPOPResult::Multiple(v) => v,
        _ => panic!("expected Multiple"),
    };
    assert_eq!(popped.len(), 2, "can't pop more than cardinality");
    assert_eq!(art.cmd_scard(b"s").unwrap(), 0);
}

#[test]
fn set_spop_invalid_count_errors() {
    let mut art = OxidArt::new();
    art.cmd_sadd(b"s", &bv(&["a"]), None).unwrap();
    assert!(art.cmd_spop(b"s", Some(b"notanumber")).is_err());
    assert!(art.cmd_spop(b"s", Some(b"0")).is_err());
}

// ──────────────────────────────────────────────────── key isolation ─────────

#[test]
fn set_many_similar_keys_isolation() {
    let mut art = OxidArt::new();

    for i in 0u32..50 {
        let key = format!("set:{i}");
        let member = format!("member{i}");
        art.cmd_sadd(key.as_bytes(), &[Bytes::copy_from_slice(member.as_bytes())], None).unwrap();
    }

    for i in 0u32..50 {
        let key = format!("set:{i}");
        let member = format!("member{i}");
        let wrong = format!("member{}", i + 1);

        assert!(art.cmd_sismember(key.as_bytes(), member.as_bytes()).unwrap(),
            "set:{i} should contain member{i}");
        assert!(!art.cmd_sismember(key.as_bytes(), wrong.as_bytes()).unwrap(),
            "set:{i} should NOT contain member{}", i + 1);
        assert_eq!(art.cmd_scard(key.as_bytes()).unwrap(), 1,
            "set:{i} should have cardinality 1");
    }
}

#[test]
fn set_delete_one_preserves_siblings() {
    let mut art = OxidArt::new();
    for i in 0u32..10 {
        let key = format!("s:{i}");
        art.cmd_sadd(key.as_bytes(), &bv(&["m"]), None).unwrap();
    }

    art.cmd_srem(b"s:5", &bv(&["m"])).unwrap();

    for i in 0u32..10 {
        if i == 5 { continue; }
        let key = format!("s:{i}");
        assert_eq!(art.cmd_scard(key.as_bytes()).unwrap(), 1,
            "s:{i} cardinality corrupted after removing s:5");
    }
}

// ──────────────────────────────────────────────────── WRONGTYPE ─────────

#[test]
fn set_wrongtype_on_string_key() {
    use crate::value::Value;
    let mut art = OxidArt::new();
    art.set(Bytes::from_static(b"str"), Value::from_str("hello"));

    assert!(art.cmd_srem(b"str", &bv(&["x"])).is_err());
    assert!(art.cmd_smembers(b"str").is_err());
    assert!(art.cmd_sismember(b"str", b"x").is_err());
    assert!(art.cmd_scard(b"str").is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// ZSET TESTS
// ═══════════════════════════════════════════════════════════════════════════

// ──────────────────────────────────────────────────── basic ─────────────

#[test]
fn zset_zadd_basic() {
    let mut art = OxidArt::new();
    let added = art.cmd_zadd(b"z", &sm(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]), None).unwrap();
    assert_eq!(added, 3);
    assert_eq!(art.cmd_zcard(b"z").unwrap(), 3);
}

#[test]
fn zset_zadd_update_does_not_increment_added() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a", 1.0)]), None).unwrap();
    let added = art.cmd_zadd(b"z", &sm(&[("a", 99.0)]), None).unwrap();
    assert_eq!(added, 0, "score update must not count as new");
    assert_eq!(art.cmd_zscore(b"z", b"a").unwrap(), Some(99.0));
}

#[test]
fn zset_zscore_basic() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("m", 3.14)]), None).unwrap();
    assert_eq!(art.cmd_zscore(b"z", b"m").unwrap(), Some(3.14));
    assert_eq!(art.cmd_zscore(b"z", b"absent").unwrap(), None);
    assert_eq!(art.cmd_zscore(b"nope", b"m").unwrap(), None);
}

#[test]
fn zset_zrange_ascending_order() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("c", 3.0), ("a", 1.0), ("b", 2.0)]), None).unwrap();
    let r = art.cmd_zrange(b"z", 0, -1, false).unwrap();
    assert_eq!(r, vec![b("a"), b("b"), b("c")]);
}

#[test]
fn zset_zrange_with_scores() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a", 1.0), ("b", 2.0)]), None).unwrap();
    let r = art.cmd_zrange(b"z", 0, -1, true).unwrap();
    assert_eq!(r, vec![b("a"), b("1"), b("b"), b("2")]);
}

#[test]
fn zset_zrange_partial() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a",1.0),("b",2.0),("c",3.0),("d",4.0)]), None).unwrap();
    let r = art.cmd_zrange(b"z", 1, 2, false).unwrap();
    assert_eq!(r, vec![b("b"), b("c")]);
}

#[test]
fn zset_zrange_negative_indices() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a",1.0),("b",2.0),("c",3.0)]), None).unwrap();

    // -1 = last
    let r = art.cmd_zrange(b"z", -1, -1, false).unwrap();
    assert_eq!(r, vec![b("c")]);

    // -2..-1 = last two
    let r = art.cmd_zrange(b"z", -2, -1, false).unwrap();
    assert_eq!(r, vec![b("b"), b("c")]);
}

#[test]
fn zset_zrange_empty_range() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a",1.0),("b",2.0)]), None).unwrap();
    // start > stop
    let r = art.cmd_zrange(b"z", 5, 2, false).unwrap();
    assert!(r.is_empty());
}

#[test]
fn zset_zrange_missing_key() {
    let mut art = OxidArt::new();
    assert!(art.cmd_zrange(b"nope", 0, -1, false).unwrap().is_empty());
}

#[test]
fn zset_zrem_basic() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a",1.0),("b",2.0)]), None).unwrap();
    let removed = art.cmd_zrem(b"z", &bv(&["a","z"])).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(art.cmd_zcard(b"z").unwrap(), 1);
    assert_eq!(art.cmd_zscore(b"z", b"a").unwrap(), None);
    assert_eq!(art.cmd_zscore(b"z", b"b").unwrap(), Some(2.0));
}

#[test]
fn zset_zrem_missing_key() {
    let mut art = OxidArt::new();
    assert_eq!(art.cmd_zrem(b"nope", &bv(&["x"])).unwrap(), 0);
}

#[test]
fn zset_zrem_auto_cleanup() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("only", 1.0)]), None).unwrap();
    art.cmd_zrem(b"z", &bv(&["only"])).unwrap();
    assert_eq!(art.cmd_zcard(b"z").unwrap(), 0);
    assert!(art.cmd_zrange(b"z", 0, -1, false).unwrap().is_empty());
}

#[test]
fn zset_zincrby_basic() {
    let mut art = OxidArt::new();
    // Non-existent member starts at 0
    let s = art.cmd_zincrby(b"z", 5.0, b"m").unwrap();
    assert_eq!(s, 5.0);
    let s2 = art.cmd_zincrby(b"z", -2.0, b"m").unwrap();
    assert_eq!(s2, 3.0);
}

#[test]
fn zset_zincrby_updates_order() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a",1.0),("b",2.0),("c",3.0)]), None).unwrap();
    // Bump a's score above c
    art.cmd_zincrby(b"z", 10.0, b"a").unwrap();
    let r = art.cmd_zrange(b"z", 0, -1, false).unwrap();
    assert_eq!(r, vec![b("b"), b("c"), b("a")]);
}

// ──────────────────────────────────────────── double-index consistency ───────

/// After score updates via ZINCRBY, BTreeSet and HashMap must agree.
#[test]
fn zset_double_index_consistency_after_updates() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a",1.0),("b",2.0),("c",3.0)]), None).unwrap();

    // Update every member's score multiple times
    art.cmd_zincrby(b"z", 10.0, b"b").unwrap();
    art.cmd_zincrby(b"z", -5.0, b"c").unwrap();
    art.cmd_zincrby(b"z", 100.0, b"a").unwrap();
    art.cmd_zincrby(b"z", -100.0, b"a").unwrap();

    // ZRANGE order must match ZSCORE values
    let range = art.cmd_zrange(b"z", 0, -1, true).unwrap();
    // range = [member, score, member, score, ...]
    assert_eq!(range.len(), 6);
    let mut prev_score = f64::NEG_INFINITY;
    for chunk in range.chunks(2) {
        let score: f64 = std::str::from_utf8(&chunk[1]).unwrap().parse().unwrap();
        assert!(score >= prev_score, "scores must be ascending in ZRANGE");

        let via_zscore = art.cmd_zscore(b"z", &chunk[0]).unwrap().unwrap();
        assert!((via_zscore - score).abs() < 1e-9, "ZSCORE and ZRANGE disagree");
        prev_score = score;
    }
}

#[test]
fn zset_equal_scores_lexicographic_tiebreak() {
    let mut art = OxidArt::new();
    // Same score → lexicographic order by member
    art.cmd_zadd(b"z", &sm(&[("z",1.0),("a",1.0),("m",1.0)]), None).unwrap();
    let r = art.cmd_zrange(b"z", 0, -1, false).unwrap();
    assert_eq!(r, vec![b("a"), b("m"), b("z")]);
}

#[test]
fn zset_score_update_removes_old_sorted_entry() {
    let mut art = OxidArt::new();
    art.cmd_zadd(b"z", &sm(&[("a",1.0),("b",1.0),("c",1.0)]), None).unwrap();
    // Change b's score — must not leave a ghost entry at score 1.0
    art.cmd_zadd(b"z", &sm(&[("b", 5.0)]), None).unwrap();

    let r = art.cmd_zrange(b"z", 0, -1, false).unwrap();
    assert_eq!(r.len(), 3, "no ghost entries");
    assert_eq!(r, vec![b("a"), b("c"), b("b")]);
    assert_eq!(art.cmd_zcard(b"z").unwrap(), 3);
}

// ──────────────────────────────────────────────────── key isolation ─────────

#[test]
fn zset_many_similar_keys_isolation() {
    let mut art = OxidArt::new();

    for i in 0u32..50 {
        let key = format!("zs:{i}");
        let member = format!("m{i}");
        art.cmd_zadd(key.as_bytes(), &[(i as f64, Bytes::copy_from_slice(member.as_bytes()))], None).unwrap();
    }

    for i in 0u32..50 {
        let key = format!("zs:{i}");
        let member = format!("m{i}");
        let score = art.cmd_zscore(key.as_bytes(), member.as_bytes()).unwrap();
        assert_eq!(score, Some(i as f64), "wrong score for zs:{i}");
        assert_eq!(art.cmd_zcard(key.as_bytes()).unwrap(), 1, "wrong card for zs:{i}");
    }
}

#[test]
fn zset_delete_one_preserves_siblings() {
    let mut art = OxidArt::new();
    for i in 0u32..10 {
        let key = format!("z:{i}");
        art.cmd_zadd(key.as_bytes(), &sm(&[("m", i as f64)]), None).unwrap();
    }

    art.cmd_zrem(b"z:5", &bv(&["m"])).unwrap();

    for i in 0u32..10 {
        if i == 5 { continue; }
        let key = format!("z:{i}");
        assert_eq!(art.cmd_zcard(key.as_bytes()).unwrap(), 1,
            "z:{i} cardinality corrupted");
        assert_eq!(art.cmd_zscore(key.as_bytes(), b"m").unwrap(), Some(i as f64),
            "z:{i} score corrupted");
    }
}

// ──────────────────────────────────────────────────── WRONGTYPE ─────────

#[test]
fn zset_wrongtype_on_string_key() {
    use crate::value::Value;
    let mut art = OxidArt::new();
    art.set(Bytes::from_static(b"str"), Value::from_str("hello"));

    assert!(art.cmd_zcard(b"str").is_err());
    assert!(art.cmd_zrange(b"str", 0, -1, false).is_err());
    assert!(art.cmd_zscore(b"str", b"m").is_err());
    assert!(art.cmd_zrem(b"str", &bv(&["m"])).is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// CROSS-TYPE ISOLATION
// ═══════════════════════════════════════════════════════════════════════════

/// Hash, Set, and ZSet at similar keys must not interfere with each other.
#[test]
fn cross_type_similar_keys_no_bleed() {
    let mut art = OxidArt::new();

    // hash at user:1, set at user:10, zset at user:100
    art.cmd_hset(b"user:1", &fv(&[("name","Alice")]), None).unwrap();
    art.cmd_sadd(b"user:10", &bv(&["tag1","tag2"]), None).unwrap();
    art.cmd_zadd(b"user:100", &sm(&[("score", 42.0)]), None).unwrap();

    // Each key must hold only its own type
    assert_eq!(art.cmd_hget(b"user:1", b"name").unwrap(), Some(b("Alice")));
    assert_eq!(art.cmd_scard(b"user:10").unwrap(), 2);
    assert_eq!(art.cmd_zscore(b"user:100", b"score").unwrap(), Some(42.0));

    // WRONGTYPE errors across the three keys
    assert!(art.cmd_hget(b"user:10", b"name").is_err(), "set key must reject hget");
    assert!(art.cmd_scard(b"user:1").is_err(),           "hash key must reject scard");
    assert!(art.cmd_zcard(b"user:10").is_err(),          "set key must reject zcard");
}

#[test]
fn cross_type_set_overwrites_with_string() {
    use crate::value::Value;
    let mut art = OxidArt::new();

    art.cmd_hset(b"k", &fv(&[("f","v")]), None).unwrap();
    // SET erases the hash, replaces with string
    art.set(Bytes::from_static(b"k"), Value::from_str("stringnow"));

    assert!(art.cmd_hget(b"k", b"f").is_err(), "k must now be string, not hash");
}

#[test]
fn cross_type_mixed_tree_deep() {
    let mut art = OxidArt::new();

    // Deeply interleaved keys of different types
    for i in 0u32..20 {
        let hk = format!("prefix:h:{i}");
        let sk = format!("prefix:s:{i}");
        let zk = format!("prefix:z:{i}");

        art.cmd_hset(hk.as_bytes(), &fv(&[("n", &i.to_string())]), None).unwrap();
        art.cmd_sadd(sk.as_bytes(), &[Bytes::copy_from_slice(i.to_string().as_bytes())], None).unwrap();
        art.cmd_zadd(zk.as_bytes(), &[(i as f64, b("m"))], None).unwrap();
    }

    // Verify each key
    for i in 0u32..20 {
        let hk = format!("prefix:h:{i}");
        let sk = format!("prefix:s:{i}");
        let zk = format!("prefix:z:{i}");
        let is = i.to_string();

        assert_eq!(art.cmd_hget(hk.as_bytes(), b"n").unwrap(),
            Some(Bytes::copy_from_slice(is.as_bytes())), "hash prefix:h:{i}");
        assert!(art.cmd_sismember(sk.as_bytes(), is.as_bytes()).unwrap(),
            "set prefix:s:{i}");
        assert_eq!(art.cmd_zscore(zk.as_bytes(), b"m").unwrap(), Some(i as f64),
            "zset prefix:z:{i}");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// STRESS: sequential add/delete cycles
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn hash_add_delete_cycle_many_keys() {
    let mut art = OxidArt::new();

    // Fill
    for i in 0u32..30 {
        let k = format!("k:{i}");
        art.cmd_hset(k.as_bytes(), &fv(&[("f",&i.to_string())]), None).unwrap();
    }
    // Delete odd keys
    for i in (1u32..30).step_by(2) {
        let k = format!("k:{i}");
        art.cmd_hdel(k.as_bytes(), &bv(&["f"])).unwrap();
    }
    // Even keys must survive with correct values
    for i in (0u32..30).step_by(2) {
        let k = format!("k:{i}");
        let expected = i.to_string();
        assert_eq!(
            art.cmd_hget(k.as_bytes(), b"f").unwrap(),
            Some(Bytes::copy_from_slice(expected.as_bytes())),
            "k:{i} corrupted"
        );
    }
}

#[test]
fn set_add_delete_cycle_many_keys() {
    let mut art = OxidArt::new();

    for i in 0u32..30 {
        let k = format!("k:{i}");
        let m = Bytes::copy_from_slice(i.to_string().as_bytes());
        art.cmd_sadd(k.as_bytes(), &[m], None).unwrap();
    }
    for i in (1u32..30).step_by(2) {
        let k = format!("k:{i}");
        let m = Bytes::copy_from_slice(i.to_string().as_bytes());
        art.cmd_srem(k.as_bytes(), &[m]).unwrap();
    }
    for i in (0u32..30).step_by(2) {
        let k = format!("k:{i}");
        let m = i.to_string();
        assert!(art.cmd_sismember(k.as_bytes(), m.as_bytes()).unwrap(), "k:{i} corrupted");
    }
}

#[test]
fn zset_add_delete_cycle_many_keys() {
    let mut art = OxidArt::new();

    for i in 0u32..30 {
        let k = format!("k:{i}");
        art.cmd_zadd(k.as_bytes(), &[(i as f64, b("m"))], None).unwrap();
    }
    for i in (1u32..30).step_by(2) {
        let k = format!("k:{i}");
        art.cmd_zrem(k.as_bytes(), &bv(&["m"])).unwrap();
    }
    for i in (0u32..30).step_by(2) {
        let k = format!("k:{i}");
        assert_eq!(art.cmd_zscore(k.as_bytes(), b"m").unwrap(), Some(i as f64), "k:{i} corrupted");
    }
}
