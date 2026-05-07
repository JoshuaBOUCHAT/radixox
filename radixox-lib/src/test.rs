use std::mem::MaybeUninit;

use crate::shared_byte::SharedByte;
use crate::small_vec::SmallVec;
use crate::cmd::{Cmd, SetCondition};

#[test]
fn test_str() {
    let byte = SharedByte::from_slice(b"Salut !");
    let cpy = byte.clone();
    println!("The cpy value is: {}\n", cpy.as_str().expect("should work"))
}
#[test]
fn verify_niche() {
    assert_eq!(
        std::mem::size_of::<SharedByte>(),
        std::mem::size_of::<Option<SharedByte>>()
    );
}
const _: () =
    assert!(std::mem::size_of::<Option<SharedByte>>() == std::mem::size_of::<SharedByte>());
const _: () =
    assert!(std::mem::size_of::<MaybeUninit<SharedByte>>() == std::mem::size_of::<SharedByte>());

// ── SmallVec ─────────────────────────────────────────────────────────────────

#[test]
fn smallvec_inline_push_and_deref() {
    let mut v: SmallVec<4, u32> = SmallVec::new();
    v.push(10);
    v.push(20);
    v.push(30);
    assert_eq!(&*v, &[10, 20, 30]);
    assert_eq!(v.len(), 3);
}

#[test]
fn smallvec_fills_inline_capacity() {
    let mut v: SmallVec<3, u32> = SmallVec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    assert_eq!(&*v, &[1, 2, 3]);
}

#[test]
fn smallvec_promotes_to_heap_on_overflow() {
    let mut v: SmallVec<2, u32> = SmallVec::new();
    v.push(1);
    v.push(2);
    // promote happens here
    v.push(3);
    assert_eq!(&*v, &[1, 2, 3]);
}

#[test]
fn smallvec_heap_growth() {
    let mut v: SmallVec<2, u32> = SmallVec::new();
    for i in 0..20u32 {
        v.push(i);
    }
    let expected: Vec<u32> = (0..20).collect();
    assert_eq!(&*v, expected.as_slice());
}

#[test]
fn smallvec_empty_deref() {
    let v: SmallVec<4, u32> = SmallVec::new();
    assert_eq!(v.len(), 0);
    assert_eq!(&*v, &[] as &[u32]);
}

#[test]
fn smallvec_drop_runs_for_heap_items() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    let counter = Arc::new(AtomicUsize::new(0));
    {
        let mut v: SmallVec<1, Arc<AtomicUsize>> = SmallVec::new();
        v.push(Arc::clone(&counter));
        v.push(Arc::clone(&counter)); // triggers promote
        v.push(Arc::clone(&counter));
        // strong_count = 4 (counter + 3 in vec)
        assert_eq!(Arc::strong_count(&counter), 4);
    }
    // all three Arcs dropped → strong_count = 1
    assert_eq!(Arc::strong_count(&counter), 1);
}

#[test]
fn smallvec_drop_runs_for_inline_items() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    let counter = Arc::new(AtomicUsize::new(0));
    {
        let mut v: SmallVec<4, Arc<AtomicUsize>> = SmallVec::new();
        v.push(Arc::clone(&counter));
        v.push(Arc::clone(&counter));
        assert_eq!(Arc::strong_count(&counter), 3);
    }
    assert_eq!(Arc::strong_count(&counter), 1);
}

#[test]
fn smallvec_into_iter_yields_reverse_order() {
    // L'itérateur parcourt du dernier au premier.
    let mut v: SmallVec<4, u32> = SmallVec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    let collected: Vec<u32> = v.into_iter().collect();
    assert_eq!(collected, vec![3, 2, 1]);
}

#[test]
fn smallvec_into_iter_empty() {
    let v: SmallVec<4, u32> = SmallVec::new();
    let collected: Vec<u32> = v.into_iter().collect();
    assert!(collected.is_empty());
}

// ── Cmd parser ───────────────────────────────────────────────────────────────

fn resp(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[test]
fn cmd_ping_no_arg() {
    let r = resp(&[b"PING"]);
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::Ping(None))));
}

#[test]
fn cmd_ping_with_message() {
    let r = resp(&[b"PING", b"hello"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Ping(Some(msg))) => assert_eq!(&*msg, b"hello"),
        other => panic!("unexpected: {:?}", other.is_some()),
    }
}

#[test]
fn cmd_ping_lowercase() {
    let r = resp(&[b"ping"]);
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::Ping(None))));
}

#[test]
fn cmd_quit() {
    let r = resp(&[b"QUIT"]);
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::Quit)));
}

#[test]
fn cmd_get() {
    let r = resp(&[b"GET", b"mykey"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Get(k)) => assert_eq!(&*k, b"mykey"),
        _ => panic!("expected Get"),
    }
}

#[test]
fn cmd_set_simple() {
    let r = resp(&[b"SET", b"k", b"v"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Set { key, val, opts }) => {
            assert_eq!(&*key, b"k");
            assert_eq!(&*val, b"v");
            assert!(opts.ttl.is_none());
            assert!(matches!(opts.condition, SetCondition::Always));
        }
        _ => panic!("expected Set"),
    }
}

#[test]
fn cmd_set_ex() {
    let r = resp(&[b"SET", b"k", b"v", b"EX", b"60"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Set { opts, .. }) => {
            assert_eq!(opts.ttl, Some(std::time::Duration::from_secs(60)));
        }
        _ => panic!("expected Set with EX"),
    }
}

#[test]
fn cmd_set_px() {
    let r = resp(&[b"SET", b"k", b"v", b"PX", b"500"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Set { opts, .. }) => {
            assert_eq!(opts.ttl, Some(std::time::Duration::from_millis(500)));
        }
        _ => panic!("expected Set with PX"),
    }
}

#[test]
fn cmd_set_nx() {
    let r = resp(&[b"SET", b"k", b"v", b"NX"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Set { opts, .. }) => {
            assert!(matches!(opts.condition, SetCondition::IfNotExists));
        }
        _ => panic!("expected Set with NX"),
    }
}

#[test]
fn cmd_set_xx() {
    let r = resp(&[b"SET", b"k", b"v", b"XX"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Set { opts, .. }) => {
            assert!(matches!(opts.condition, SetCondition::IfExists));
        }
        _ => panic!("expected Set with XX"),
    }
}

#[test]
fn cmd_setnx() {
    let r = resp(&[b"SETNX", b"k", b"v"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Set { opts, .. }) => {
            assert!(matches!(opts.condition, SetCondition::IfNotExists));
            assert!(opts.ttl.is_none());
        }
        _ => panic!("expected Set (SETNX)"),
    }
}

#[test]
fn cmd_setex() {
    let r = resp(&[b"SETEX", b"k", b"10", b"v"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Set { opts, .. }) => {
            assert_eq!(opts.ttl, Some(std::time::Duration::from_secs(10)));
        }
        _ => panic!("expected Set (SETEX)"),
    }
}

#[test]
fn cmd_mget() {
    let r = resp(&[b"MGET", b"a", b"b", b"c"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::MGet(keys)) => {
            assert_eq!(keys.len(), 3);
            assert_eq!(&*keys[0], b"a");
            assert_eq!(&*keys[2], b"c");
        }
        _ => panic!("expected MGet"),
    }
}

#[test]
fn cmd_mset() {
    let r = resp(&[b"MSET", b"k1", b"v1", b"k2", b"v2"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::MSet(pairs)) => {
            assert_eq!(pairs.len(), 2);
            assert_eq!(&*pairs[0].0, b"k1");
            assert_eq!(&*pairs[0].1, b"v1");
        }
        _ => panic!("expected MSet"),
    }
}

#[test]
fn cmd_del_single() {
    let r = resp(&[b"DEL", b"k"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Del(keys)) => assert_eq!(keys.len(), 1),
        _ => panic!("expected Del"),
    }
}

#[test]
fn cmd_del_multiple() {
    let r = resp(&[b"DEL", b"a", b"b", b"c"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Del(keys)) => assert_eq!(keys.len(), 3),
        _ => panic!("expected Del"),
    }
}

#[test]
fn cmd_incr() {
    let r = resp(&[b"INCR", b"counter"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Incr(k)) => assert_eq!(&*k, b"counter"),
        _ => panic!("expected Incr"),
    }
}

#[test]
fn cmd_incrby() {
    let r = resp(&[b"INCRBY", b"counter", b"5"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::IncrBy { key, delta }) => {
            assert_eq!(&*key, b"counter");
            assert_eq!(delta, 5);
        }
        _ => panic!("expected IncrBy"),
    }
}

#[test]
fn cmd_decrby_negative() {
    let r = resp(&[b"DECRBY", b"counter", b"-3"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::DecrBy { delta, .. }) => assert_eq!(delta, -3),
        _ => panic!("expected DecrBy"),
    }
}

#[test]
fn cmd_expire() {
    let r = resp(&[b"EXPIRE", b"k", b"120"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Expire { key, dur }) => {
            assert_eq!(&*key, b"k");
            assert_eq!(dur, std::time::Duration::from_secs(120));
        }
        _ => panic!("expected Expire"),
    }
}

#[test]
fn cmd_pexpire() {
    let r = resp(&[b"PEXPIRE", b"k", b"2000"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Expire { dur, .. }) => {
            assert_eq!(dur, std::time::Duration::from_millis(2000));
        }
        _ => panic!("expected Expire (PEXPIRE)"),
    }
}

#[test]
fn cmd_hset() {
    let r = resp(&[b"HSET", b"myhash", b"field1", b"val1", b"field2", b"val2"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::HSet { key, fields }) => {
            assert_eq!(&*key, b"myhash");
            assert_eq!(fields.len(), 2);
            assert_eq!(&*fields[0].0, b"field1");
            assert_eq!(&*fields[1].1, b"val2");
        }
        _ => panic!("expected HSet"),
    }
}

#[test]
fn cmd_hmset_alias() {
    let r = resp(&[b"HMSET", b"h", b"f", b"v"]);
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::HSet { .. })));
}

#[test]
fn cmd_hget() {
    let r = resp(&[b"HGET", b"h", b"f"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::HGet { key, field }) => {
            assert_eq!(&*key, b"h");
            assert_eq!(&*field, b"f");
        }
        _ => panic!("expected HGet"),
    }
}

#[test]
fn cmd_sadd() {
    let r = resp(&[b"SADD", b"s", b"a", b"b"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::SAdd { key, members }) => {
            assert_eq!(&*key, b"s");
            assert_eq!(members.len(), 2);
        }
        _ => panic!("expected SAdd"),
    }
}

#[test]
fn cmd_zadd() {
    let r = resp(&[b"ZADD", b"z", b"1.5", b"m1", b"2.0", b"m2"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::ZAdd { key, members }) => {
            assert_eq!(&*key, b"z");
            assert_eq!(members.len(), 2);
            assert_eq!(members[0].0, 1.5f64);
            assert_eq!(&*members[0].1, b"m1");
        }
        _ => panic!("expected ZAdd"),
    }
}

#[test]
fn cmd_zrange_with_scores() {
    let r = resp(&[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::ZRange { start, stop, with_scores, .. }) => {
            assert_eq!(start, 0);
            assert_eq!(stop, -1);
            assert!(with_scores);
        }
        _ => panic!("expected ZRange"),
    }
}

#[test]
fn cmd_publish() {
    let r = resp(&[b"PUBLISH", b"chan", b"msg"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Publish { channel, message }) => {
            assert_eq!(&*channel, b"chan");
            assert_eq!(&*message, b"msg");
        }
        _ => panic!("expected Publish"),
    }
}

#[test]
fn cmd_subscribe_multi() {
    let r = resp(&[b"SUBSCRIBE", b"c1", b"c2", b"c3"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::Subscribe(channels)) => assert_eq!(channels.len(), 3),
        _ => panic!("expected Subscribe"),
    }
}

#[test]
fn cmd_invalid_returns_none() {
    assert!(Cmd::from_slice(b"").is_none());
    assert!(Cmd::from_slice(b"PING\r\n").is_none()); // pas un array RESP
    // SET sans valeur
    let r = resp(&[b"SET", b"k"]);
    assert!(Cmd::from_slice(&r).is_none());
    // HSET avec nombre de champs impair
    let r = resp(&[b"HSET", b"h", b"f1"]);
    assert!(Cmd::from_slice(&r).is_none());
    // Commande inconnue
    let r = resp(&[b"FOOBAR"]);
    assert!(Cmd::from_slice(&r).is_none());
}
