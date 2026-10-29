use std::mem::MaybeUninit;

use crate::shared_byte::SharedByte;
use crate::small_vec::{SmallVec, SmallVecIterator};
use crate::cmd::{Cmd, CmdError, SetCondition};

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

// ── SharedByte : en-tête compact (MEMORY_ARCH.md §16.2) ───────────────────────

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

#[test]
fn shared_byte_lengths_and_forms() {
    use crate::raw_alloc::external;
    let g = SharedByte::POOL_MAX_LEN;
    for len in [0, 1, 2, 3, g - 1, g, g + 1, 1 << 20] {
        let data = pattern(len);
        let (b0, o0) = (external::bytes(), external::objects());
        let sb = SharedByte::from_slice(&data);
        assert_eq!(sb.len(), len);
        assert_eq!(sb.as_slice(), &data[..]);
        assert_eq!(sb.rc(), 1);
        assert_eq!(sb.as_ptr() as usize % 2, 0, "données paires");
        assert_eq!(sb.is_external(), len > g, "len {len}");
        if len > g {
            assert_eq!(external::bytes() - b0, 6 + len as u64);
            assert_eq!(external::objects() - o0, 1);
        }
        drop(sb);
        assert_eq!((external::bytes(), external::objects()), (b0, o0));
    }
}

#[test]
fn shared_byte_forced_external() {
    use crate::raw_alloc::external;
    for len in [0, 1, 100, SharedByte::POOL_MAX_LEN] {
        let data = pattern(len);
        let o0 = external::objects();
        let sb = SharedByte::from_slice_external(&data);
        assert!(sb.is_external());
        assert_eq!((sb.len(), sb.as_slice()), (len, &data[..]));
        assert_eq!(external::objects() - o0, 1);

        // into_external : copie depuis la forme courte, rien depuis la longue
        let short = SharedByte::from_slice(&data);
        assert!(!short.is_external());
        let ext = short.into_external();
        assert!(ext.is_external());
        assert_eq!(ext.as_slice(), &data[..]);
        let p = ext.as_ptr();
        let same = ext.into_external();
        assert_eq!(same.as_ptr(), p);
        drop((sb, same));
        assert_eq!(external::objects(), o0);
    }
}

#[test]
fn shared_byte_to_uppercase() {
    for external in [false, true] {
        let mk = |d: &[u8]| {
            if external { SharedByte::from_slice_external(d) } else { SharedByte::from_slice(d) }
        };
        // non partagé : sur place
        let mut a = mk(b"get");
        let p = a.as_ptr();
        a.to_uppercase();
        assert_eq!((a.as_slice(), a.as_ptr()), (&b"GET"[..], p));

        // partagé : copie de même forme, l'original intact
        let mut b = mk(b"hset");
        let keep = b.clone();
        b.to_uppercase();
        assert_eq!(b.as_slice(), b"HSET");
        assert_eq!(keep.as_slice(), b"hset");
        assert_eq!((b.rc(), keep.rc()), (1, 1));
        assert_eq!(b.is_external(), external);
    }
}

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
fn smallvec_into_iter_yields_in_order() {
    // L'itérateur parcourt du premier au dernier.
    let mut v: SmallVec<4, u32> = SmallVec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    let collected: Vec<u32> = v.into_iter().collect();
    assert_eq!(collected, vec![1, 2, 3]);
}

/// Mode inline avec des éléments qui possèdent du heap : l'itérateur ne
/// doit pas lire l'ancien emplacement du vec (déplacé dans l'itérateur).
#[test]
fn smallvec_into_iter_inline_owned_items() {
    let mut v: SmallVec<4, String> = SmallVec::new();
    for i in 0..4 {
        v.push(format!("element-numero-{i}"));
    }
    let collected: Vec<String> = v.into_iter().collect();
    let expect: Vec<String> = (0..4).map(|i| format!("element-numero-{i}")).collect();
    assert_eq!(collected, expect);
}

#[test]
fn smallvec_into_iter_heap_mode() {
    let mut v: SmallVec<2, String> = SmallVec::new();
    for i in 0..20 {
        v.push(i.to_string());
    }
    let it = v.into_iter();
    assert_eq!(it.len(), 20);
    let collected: Vec<String> = it.collect();
    let expect: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    assert_eq!(collected, expect);
}

/// Itération partielle puis drop : chaque élément est droppé exactement une
/// fois (consommés par l'appelant, restants avec l'itérateur).
#[test]
fn smallvec_into_iter_partial_drops_rest() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    for n in [3usize, 4, 9] {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut v: SmallVec<4, Arc<AtomicUsize>> = SmallVec::new();
        for _ in 0..n {
            v.push(Arc::clone(&counter));
        }
        assert_eq!(Arc::strong_count(&counter), n + 1);
        let mut it = v.into_iter();
        let first = it.next().unwrap();
        let second = it.next().unwrap();
        assert_eq!(it.len(), n - 2);
        drop(it);
        assert_eq!(Arc::strong_count(&counter), 3, "n = {n}");
        drop((first, second));
        assert_eq!(Arc::strong_count(&counter), 1);
    }
}

/// Le vec est déplacé (renvoyé par une fonction) entre la création de
/// l'itérateur et sa consommation.
#[test]
fn smallvec_into_iter_survives_moves() {
    fn make() -> SmallVecIterator<4, u64> {
        let mut v: SmallVec<4, u64> = SmallVec::new();
        v.push(10);
        v.push(20);
        v.push(30);
        v.into_iter()
    }
    let it = make();
    let boxed = Box::new(it);
    assert_eq!(boxed.collect::<Vec<_>>(), vec![10, 20, 30]);
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
        Some(Cmd::SetNx { key, val }) => {
            assert_eq!(key, b"k");
            assert_eq!(val.as_ref(), b"v");
        }
        _ => panic!("expected SetNx"),
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
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::HMSet { .. })));
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
        Some(Cmd::ZAdd { key, members, .. }) => {
            assert_eq!(&*key, b"z");
            assert_eq!(members.len(), 2);
            assert_eq!(members[0].0, 1.5f64);
            assert_eq!(members[0].1, b"m1");
        }
        _ => panic!("expected ZAdd"),
    }
}

#[test]
fn cmd_zrange_with_scores() {
    let r = resp(&[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES"]);
    match Cmd::from_slice(&r) {
        Some(Cmd::ZRange { range, rev, with_scores, .. }) => {
            assert_eq!(range, crate::cmd::ZRangeBy::Rank(0, -1));
            assert!(!rev);
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
    // Incomplet : None (attend plus d'octets, ne peut pas avancer le curseur).
    assert!(Cmd::from_slice(b"").is_none());
    assert!(Cmd::from_slice(b"*1\r\n$4\r\nPI").is_none());
    // Inline : accepté, comme Redis.
    assert!(matches!(Cmd::from_slice(b"PING\r\n"), Some(Cmd::Ping(None))));

    // RESP complet mais commande non reconnue / arité invalide : Some(Cmd::Invalid)
    // — le curseur avance (le serveur répond ERR), sinon la connexion resterait
    // bloquée à vie sur une commande qui ne deviendra jamais valide.
    let r = resp(&[b"SET", b"k"]); // SET sans valeur
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::Invalid { err: CmdError::Arity, .. })));
    let r = resp(&[b"HSET", b"h", b"f1"]); // nombre de champs impair
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::Invalid { err: CmdError::Arity, .. })));
    let r = resp(&[b"FOOBAR"]); // commande inconnue
    assert!(matches!(Cmd::from_slice(&r), Some(Cmd::Invalid { err: CmdError::Unknown, .. })));
}

/// Au-delà de 127 détenteurs, le compte passe dans la table de débordement et en
/// revient : exact dans les deux sens, la case est rendue (MEMORY_ARCH.md §10.2 ter).
#[test]
fn shared_byte_rc_overflow_table() {
    use crate::shared_byte::rc_overflow_stats;
    let (used0, copies0) = rc_overflow_stats();
    let a = SharedByte::from_str("valeur partagée");
    let mut clones: Vec<SharedByte> = (0..100_000).map(|_| a.clone()).collect();
    assert_eq!(a.rc(), 100_001);
    assert_eq!(rc_overflow_stats(), (used0 + 1, copies0));
    assert!(clones.iter().all(|c| c.as_ptr() == a.as_ptr()), "partagé, pas copié");
    // Va-et-vient autour du seuil.
    clones.truncate(127);
    assert_eq!(a.rc(), 128);
    clones.pop();
    assert_eq!((a.rc(), rc_overflow_stats().0), (127, used0));
    clones.push(a.clone());
    assert_eq!((a.rc(), rc_overflow_stats().0), (128, used0 + 1));
    clones.pop();
    clones.pop();
    assert_eq!((a.rc(), rc_overflow_stats().0), (126, used0));
    drop(clones);
    assert_eq!(a.rc(), 1);
    assert_eq!(a.as_slice(), b"valeur partag\xc3\xa9e");
}

/// Table pleine (128 objets tenus chacun plus de 127 fois) : le clone suivant copie,
/// et tout est rendu à la fin.
#[test]
fn shared_byte_rc_overflow_full_copies() {
    use crate::shared_byte::rc_overflow_stats;
    let (used0, copies0) = rc_overflow_stats();
    assert_eq!(used0, 0);
    let objs: Vec<SharedByte> = (0..129u32).map(|i| SharedByte::from_slice(i.to_le_bytes())).collect();
    let mut held: Vec<SharedByte> = Vec::new();
    for o in &objs[..128] {
        held.extend((0..127).map(|_| o.clone()));
    }
    assert_eq!(rc_overflow_stats(), (128, copies0));
    held.extend((0..126).map(|_| objs[128].clone()));
    assert_eq!(objs[128].rc(), 127);
    let c = objs[128].clone(); // 128e détenteur : plus de case
    assert_eq!(rc_overflow_stats(), (128, copies0 + 1));
    assert_ne!(c.as_ptr(), objs[128].as_ptr());
    assert_eq!((c.rc(), objs[128].rc()), (1, 127));
    assert_eq!(&c[..], &128u32.to_le_bytes());
    drop(c);
    drop(held);
    assert_eq!(rc_overflow_stats().0, 0);
    assert!(objs.iter().all(|o| o.rc() == 1));
}
