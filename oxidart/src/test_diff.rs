//! Fuzz différentiel du nouvel arbre contre l'ancien (`oxidart-legacy`) :
//! mêmes opérations sur les deux, mêmes réponses. `check()` vérifie toute la
//! structure après chaque opération. Une save est lancée en cours de route
//! (mutations entre les tranches) et comparée à l'état T0 de l'oracle ; en fin
//! de graine, aller-retour save/reload dans les deux sens (format commun).

use std::time::Duration;

use oxidart_legacy::OxidArt as Old;
use radixox_lib::shared_byte::SharedByte;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::{OxidArt, TtlResult, Value};

// ─── Clés ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Keys {
    /// alphabet {a, b}, longueur 0..8 : chaînes de préfixes partout
    Ab,
    /// user:{n}, user:{n}:email
    User,
    /// octets quelconques, longueur 0..3 : nœuds larges (jusqu'à 256)
    Binary,
    /// long préfixe commun + hex : chemins sur le tas (> 7 octets)
    Long,
    Mix,
}

const ALL_KEYS: [Keys; 5] = [Keys::Ab, Keys::User, Keys::Binary, Keys::Long, Keys::Mix];

fn gen_key(rng: &mut StdRng, keys: Keys) -> Vec<u8> {
    match keys {
        Keys::Ab => {
            let len = rng.gen_range(0..8);
            (0..len).map(|_| if rng.gen_bool(0.5) { b'a' } else { b'b' }).collect()
        }
        Keys::User => {
            let n = rng.gen_range(0..300);
            match rng.gen_range(0..4) {
                0 => format!("user:{n}:email").into_bytes(),
                1 => format!("user:{}", n / 10).into_bytes(),
                _ => format!("user:{n}").into_bytes(),
            }
        }
        Keys::Binary => {
            let len = rng.gen_range(0..3);
            let first = if rng.gen_bool(0.7) { rng.r#gen::<u8>() } else { rng.gen_range(0..4) };
            (0..len).map(|i| if i == 0 { first } else { rng.r#gen() }).collect()
        }
        Keys::Long => {
            let mut k = b"session:0123456789abcdef".to_vec();
            k.truncate(rng.gen_range(0..k.len() + 1));
            let len = rng.gen_range(0..20);
            k.extend((0..len).map(|_| b"0123456789abcdef"[rng.gen_range(0..16)]));
            k
        }
        Keys::Mix => {
            let k = [Keys::Ab, Keys::User, Keys::Binary, Keys::Long][rng.gen_range(0..4)];
            gen_key(rng, k)
        }
    }
}

// ─── Valeurs canoniques (comparables entre les deux crates) ─────────────────

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum C {
    Str(Vec<u8>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    Set(Vec<Vec<u8>>),
    /// contenu comparé à part (ZRANGE)
    ZSet,
    List(Vec<Vec<u8>>),
}

fn canon_new(v: &Value) -> C {
    match v {
        Value::String(b) => C::Str(b.to_vec()),
        Value::Int(n) => C::Str(n.to_string().into_bytes()),
        Value::Hash(h) => {
            let mut h: Vec<_> = h.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
            h.sort();
            C::Hash(h)
        }
        Value::Set(s) => {
            let mut s: Vec<_> = s.iter().map(|m| m.to_vec()).collect();
            s.sort();
            C::Set(s)
        }
        Value::ZSet(_) => C::ZSet,
        Value::List(l) => C::List(l.iter().map(|i| i.to_vec()).collect()),
    }
}

fn canon_old(v: &oxidart_legacy::Value) -> C {
    use oxidart_legacy::Value as V;
    match v {
        V::String(b) => C::Str(b.to_vec()),
        V::Int(n) => C::Str(n.to_string().into_bytes()),
        V::Hash(h) => {
            let mut h: Vec<_> = h.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
            h.sort();
            C::Hash(h)
        }
        V::Set(s) => {
            let mut s: Vec<_> = s.iter().map(|m| m.to_vec()).collect();
            s.sort();
            C::Set(s)
        }
        V::ZSet(_) => C::ZSet,
        V::List(l) => C::List(l.iter().map(|i| i.to_vec()).collect()),
    }
}

fn ttl_new(t: TtlResult) -> Option<Option<u64>> {
    match t {
        TtlResult::KeyNotExist => None,
        TtlResult::KeyWithTtl(s) => Some(Some(s)),
        TtlResult::KeyWithoutTtl => Some(None),
    }
}

fn ttl_old(t: oxidart_legacy::TtlResult) -> Option<Option<u64>> {
    use oxidart_legacy::TtlResult as T;
    match t {
        T::KeyNotExist => None,
        T::KeyWithTtl(s) => Some(Some(s)),
        T::KeyWithoutTtl => Some(None),
    }
}

type Content = Vec<(Vec<u8>, C, Option<u64>)>;

fn content_new(art: &OxidArt, prefix: &[u8]) -> Content {
    let mut v: Content = art
        .getn(SharedByte::from_slice(prefix))
        .iter()
        .map(|(k, val)| {
            let ttl = ttl_new(art.get_ttl(k.clone())).expect("clé listée mais absente");
            (k.to_vec(), canon_new(val), ttl)
        })
        .collect();
    v.sort();
    v
}

fn content_old(art: &Old, prefix: &[u8]) -> Content {
    let mut v: Content = art
        .getn(SharedByte::from_slice(prefix))
        .iter()
        .map(|(k, val)| {
            let ttl = ttl_old(art.get_ttl(k.clone())).expect("clé listée mais absente");
            (k.to_vec(), canon_old(val), ttl)
        })
        .collect();
    v.sort();
    v
}

/// Dump décodé, en forme canonique (TTL restant au T0 du dump).
fn content_dump(buf: &[u8]) -> Content {
    let dump = crate::parse_dump(buf).expect("dump illisible");
    let mut v: Content = dump
        .entries
        .iter()
        .map(|e| (e.key.clone(), canon_new(&e.value), e.ttl))
        .collect();
    v.sort();
    v
}

/// État au T0 d'une save, TTL en secondes restantes à T0 (comme le dump).
fn content_at(art: &Old, prefix: &[u8], t0: u64, now: u64) -> Content {
    debug_assert_eq!(t0, now);
    content_old(art, prefix)
}

fn save_all(art: &mut OxidArt) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 97) {}
    drop(cur);
    art.finish_save();
    out
}

fn save_all_old(art: &mut Old) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 97) {}
    drop(cur);
    art.finish_save();
    out
}

/// Copie indépendante : comme les arguments d'une commande du serveur, que l'arbre est
/// seul à garder ensuite. Sans elle, les vecteurs du fuzz et l'oracle partagent chaque
/// valeur avec l'arbre et l'audit d'unicité n'a pas de sens.
fn fresh(b: &SharedByte) -> SharedByte {
    SharedByte::from_slice(b.as_slice())
}

fn deep(v: &[SharedByte]) -> Vec<SharedByte> {
    v.iter().map(fresh).collect()
}

fn deep_pairs(v: &[(SharedByte, SharedByte)]) -> Vec<(SharedByte, SharedByte)> {
    v.iter().map(|(a, b)| (fresh(a), fresh(b))).collect()
}

/// Après chaque opération : audit d'unicité du GC (MEMORY_ARCH.md §16.9), hors save
/// chaque `SharedByte` de l'arbre a `rc == 1`.
fn gc_check(art: &mut OxidArt, seed: u64, step: usize) {
    let shared = art.gc_audit_unique();
    assert!(shared.is_empty(), "SharedByte partagés (seed {seed}, pas {step}) : {:?}", &shared[..shared.len().min(5)]);
    if let Err(e) = radixox_lib::pool::verify_census(&art.gc_census()) {
        panic!("compteurs du pool (seed {seed}, pas {step}) : {e}");
    }
    // Avec le pool : une passe de GC complète après chaque opération (MEMORY_ARCH.md
    // §16.10, étape 6) ; tout objet que l'arbre est seul à tenir change d'adresse, un
    // pointeur périmé lit le motif 0xDB sous `radixox_gc_debug`.
    if cfg!(not(any(radixox_nopool, radixox_sysalloc))) {
        art.gc_full_pass();
        if let Err(e) = radixox_lib::pool::verify_census(&art.gc_census()) {
            panic!("compteurs du pool après GC (seed {seed}, pas {step}) : {e}");
        }
    }
}

fn sb(k: &[u8]) -> SharedByte {
    SharedByte::from_slice(k)
}

fn rbytes(r: Vec<SharedByte>) -> Vec<Vec<u8>> {
    r.into_iter().map(|b| b.to_vec()).collect()
}

fn sorted(mut v: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    v.sort();
    v
}

// ─── Fuzz ────────────────────────────────────────────────────────────────────

#[derive(Default, Debug)]
struct Stats {
    max_keys: usize,
    saves: usize,
    krange: usize,
    deln: usize,
    /// conteneurs vus en fin de graine : Hash valart, Hash petit, Set valart, Set petit
    containers: [usize; 4],
}

/// Save en cours sur le nouvel arbre : curseur, sortie, état T0 attendu.
struct Saving {
    cur: crate::SaveCursor,
    out: Vec<u8>,
    want: Content,
}

fn fuzz(seed: u64, ops: usize, keys: Keys, stats: &mut Stats) {
    let mut rng = StdRng::seed_from_u64(seed);
    let pool: Vec<SharedByte> = (0..6).map(|i| SharedByte::from_slice(format!("v-{i}"))).collect();
    // 12 champs / membres possibles : les petits conteneurs (≤ 8) passent en
    // valart, et en reviennent par suppression de la clé
    let fields: Vec<SharedByte> = ["f", "g", "field-long-name", "", "a", "b", "c", "d", "e", "h", "i", "j"]
        .iter()
        .map(|s| sb(s.as_bytes()))
        .collect();
    let members: Vec<SharedByte> = (0..12).map(|i| SharedByte::from_slice(format!("m-{i}"))).collect();
    let mut new = OxidArt::new();
    let mut old = Old::new();
    let mut now = 1_000;
    new.set_now(now);
    old.set_now(now);
    let save_at = rng.gen_range(ops / 4..ops / 2);
    let mut saving: Option<Saving> = None;

    for step in 0..ops {
        let key = gen_key(&mut rng, keys);
        let k = key.as_slice();
        let is_saving = saving.is_some();
        let op = rng.gen_range(0..32);
        match op {
            0..=5 => {
                let val = if rng.gen_bool(0.3) {
                    Value::Int(rng.gen_range(-50..50))
                } else {
                    Value::String(fresh(&pool[rng.gen_range(0..pool.len())]))
                };
                if rng.gen_bool(0.25) {
                    let d = Duration::from_secs(rng.gen_range(0..20));
                    new.set_ttl(sb(k), d, val.clone());
                    old.set_ttl(sb(k), d, to_old(&val));
                } else {
                    new.set(sb(k), val.clone());
                    old.set(sb(k), to_old(&val));
                }
            }
            6 => {
                let cond = [None, Some(true), Some(false)][rng.gen_range(0..3)];
                let ttl = rng.gen_bool(0.3).then(|| Duration::from_secs(rng.gen_range(0..20)));
                let val = Value::String(fresh(&pool[rng.gen_range(0..pool.len())]));
                let a = new.set_cond(k, ttl, val.clone(), cond);
                let b = old.set_cond(k, ttl, to_old(&val), cond);
                assert_eq!(a, b, "set_cond {key:?} {cond:?}");
            }
            7..=9 => {
                let a = new.get(k).map(|v| canon_new(&v));
                let b = old.get(k).map(|v| canon_old(&v));
                assert_eq!(a, b, "get {key:?}");
            }
            10..=11 => assert_eq!(new.del(k), old.del(k), "del {key:?}"),
            12 => {
                let a = new.take(k).map(|v| canon_new(&v));
                let b = old.take(k).map(|v| canon_old(&v));
                assert_eq!(a, b, "take {key:?}");
            }
            13 => {
                let d = Duration::from_secs(rng.gen_range(0..20));
                assert_eq!(new.expire(sb(k), d), old.expire(sb(k), d), "expire {key:?}");
            }
            14 => assert_eq!(new.persist(sb(k)), old.persist(sb(k)), "persist {key:?}"),
            15 => assert_eq!(ttl_new(new.get_ttl(sb(k))), ttl_old(old.get_ttl(sb(k))), "ttl {key:?}"),
            16 => {
                let d = rng.gen_range(-5..5);
                let a = format!("{:?}", new.incrby(sb(k), d));
                let b = format!("{:?}", old.incrby(sb(k), d));
                assert_eq!(a, b, "incrby {key:?}");
            }
            17 => {
                let n = rng.gen_range(1..5);
                let fv: Vec<(SharedByte, SharedByte)> = (0..n)
                    .map(|_| (fresh(&fields[rng.gen_range(0..fields.len())]), fresh(&pool[rng.gen_range(0..pool.len())])))
                    .collect();
                let ttl = rng.gen_bool(0.2).then(|| now + rng.gen_range(0..20));
                let a = format!("{:?}", new.cmd_hset(k, &fv, ttl));
                let b = format!("{:?}", old.cmd_hset(k, &deep_pairs(&fv), ttl));
                assert_eq!(a, b, "hset {key:?}");
            }
            18 => {
                let f = &fields[rng.gen_range(0..fields.len())];
                let a = new.cmd_hget(k, f).map(|v| v.map(|b| b.to_vec()));
                let b = old.cmd_hget(k, f).map(|v| v.map(|b| b.to_vec()));
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "hget");
                assert_eq!(format!("{:?}", new.cmd_hlen(k)), format!("{:?}", old.cmd_hlen(k)), "hlen");
                let a = new.cmd_hgetall(k).map(|v| sorted(rbytes(v)));
                let b = old.cmd_hgetall(k).map(|v| sorted(rbytes(v)));
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "hgetall {key:?}");
            }
            19 => {
                let fs = [fresh(&fields[rng.gen_range(0..fields.len())])];
                assert_eq!(format!("{:?}", new.cmd_hdel(k, &fs)), format!("{:?}", old.cmd_hdel(k, &fs)), "hdel");
            }
            20 => {
                let m = [fresh(&members[rng.gen_range(0..members.len())])];
                if rng.gen_bool(0.6) {
                    let a = format!("{:?}", new.cmd_sadd(k, &m, None));
                    assert_eq!(a, format!("{:?}", old.cmd_sadd(k, &deep(&m), None)), "sadd {key:?}");
                } else {
                    let a = format!("{:?}", new.cmd_srem(k, &m));
                    assert_eq!(a, format!("{:?}", old.cmd_srem(k, &m)), "srem {key:?}");
                }
                let a = new.cmd_smembers(k).map(|v| sorted(rbytes(v)));
                let b = old.cmd_smembers(k).map(|v| sorted(rbytes(v)));
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "smembers {key:?}");
                assert_eq!(format!("{:?}", new.cmd_scard(k)), format!("{:?}", old.cmd_scard(k)), "scard");
            }
            21 => {
                let m = fresh(&pool[rng.gen_range(0..pool.len())]);
                if rng.gen_bool(0.6) {
                    let sm = [(rng.gen_range(-3..3) as f64, m)];
                    let a = format!("{:?}", new.cmd_zadd(sb(k), &sm, None));
                    assert_eq!(a, format!("{:?}", old.cmd_zadd(sb(k), &sm.iter().map(|(s, m)| (*s, fresh(m))).collect::<Vec<_>>(), None)), "zadd {key:?}");
                } else {
                    let ms = [m];
                    let a = format!("{:?}", new.cmd_zrem(k, &ms));
                    assert_eq!(a, format!("{:?}", old.cmd_zrem(k, &deep(&ms))), "zrem {key:?}");
                }
                let a = new.cmd_zrange(k, 0, -1, true).map(rbytes);
                let b = old.cmd_zrange(k, 0, -1, true).map(rbytes);
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "zrange {key:?}");
                assert_eq!(format!("{:?}", new.cmd_zcard(k)), format!("{:?}", old.cmd_zcard(k)), "zcard");
            }
            22 => {
                let p = &key[..rng.gen_range(0..key.len() + 1)];
                assert_eq!(content_new(&new, p), content_old(&old, p), "getn {p:?}");
            }
            23 if !is_saving && rng.gen_bool(0.3) => {
                let p = &key[..rng.gen_range(0..key.len() + 1)];
                // nouvel arbre : seules les valeurs vivantes comptent (UNLINK)
                let live = content_old(&old, p).len();
                assert_eq!(new.deln(p), live, "deln {p:?}");
                old.deln(p);
                stats.deln += 1;
            }
            24 => {
                let p = &key[..rng.gen_range(0..key.len() + 1)];
                let from = gen_key(&mut rng, keys);
                let from: &[u8] = if rng.gen_bool(0.3) { b"" } else { &from };
                let count = rng.gen_range(1..40);
                let rev = rng.gen_bool(0.5);
                let a: Vec<_> = new.krange_owned(p, from, count, rev).iter().map(|(k, v)| (k.to_vec(), ov_new(v))).collect();
                let b: Vec<_> = old.krange_owned(p, from, count, rev).iter().map(|(k, v)| (k.to_vec(), ov_old(v))).collect();
                assert_eq!(a, b, "krange {p:?} from {from:?} count {count} rev {rev}");
                stats.krange += 1;
            }
            25 => {
                let n = rng.gen_range(1..80);
                let ks: Vec<Vec<u8>> = (0..n).map(|_| gen_key(&mut rng, keys)).collect();
                let refs: Vec<&[u8]> = ks.iter().map(Vec::as_slice).collect();
                if rng.gen_bool(0.5) {
                    let a: Vec<_> = new.mget_owned(&refs).iter().map(|v| v.as_ref().map(ov_new)).collect();
                    let b: Vec<_> = old.mget_owned(&refs).iter().map(|v| v.as_ref().map(ov_old)).collect();
                    assert_eq!(a, b, "mget");
                } else {
                    let (mut ea, mut eb) = (Vec::new(), Vec::new());
                    assert_eq!(new.exists_count(&refs, &mut ea), old.exists_count(&refs, &mut eb), "exists");
                    // les listes d'expirées croisées peuvent différer : pendant
                    // sa save, le nouvel arbre ne nettoie rien en lecture, l'oracle si
                    new.purge_expired(&refs, &ea);
                    old.purge_expired(&refs, &eb);
                    new.prefetch_keys(&refs);
                }
            }
            #[cfg(feature = "regex")]
            28 => {
                let glob = gen_glob(&mut rng, &key);
                let a = new.getn_glob(&glob).map(|v| {
                    let mut v: Vec<_> = v.iter().map(|(k, val)| (k.to_vec(), canon_new(val))).collect();
                    v.sort();
                    v
                });
                let b = old.getn_glob(&glob).map(|v| {
                    let mut v: Vec<_> = v.iter().map(|(k, val)| (k.to_vec(), canon_old(val))).collect();
                    v.sort();
                    v
                });
                assert_eq!(a.ok(), b.ok(), "glob {:?}", String::from_utf8_lossy(&glob));
            }
            26..=27 => {
                now += rng.gen_range(0..4);
                new.set_now(now);
                old.set_now(now);
            }
            _ => {}
        }
        if step == save_at {
            let mut out = Vec::new();
            let cur = new.save_start(&mut out);
            saving = Some(Saving { cur, out, want: content_at(&old, b"", now, now) });
        }
        if let Some(s) = saving.as_mut() {
            let budget = rng.gen_range(1..40);
            if new.save_step(&mut s.cur, &mut s.out, budget) {
                let s = saving.take().unwrap();
                drop(s.cur);
                new.finish_save();
                assert_eq!(content_dump(&s.out), s.want, "save sous mutations (seed {seed})");
                stats.saves += 1;
            }
        }
        new.check();
        gc_check(&mut new, seed, step);
        if step % 16 == 0 {
            assert_eq!(content_new(&new, b""), content_old(&old, b""), "contenu (seed {seed}, pas {step})");
        }
        stats.max_keys = stats.max_keys.max(old.getn(SharedByte::from_slice(b"")).len());
    }
    if let Some(mut s) = saving.take() {
        while !new.save_step(&mut s.cur, &mut s.out, 64) {}
        drop(s.cur);
        new.finish_save();
        assert_eq!(content_dump(&s.out), s.want, "save sous mutations (seed {seed})");
        stats.saves += 1;
    }

    {
        use crate::value::Tag;
        let mut path = Vec::new();
        let c = &mut stats.containers;
        new.walk(new.root_idx, &mut path, |_, tag, _, _| match tag {
            Tag::Hash => c[0] += 1,
            Tag::HashSmall => c[1] += 1,
            Tag::Set => c[2] += 1,
            Tag::SetSmall => c[3] += 1,
            _ => {}
        });
    }

    // aller-retour save/reload, dans les deux sens (format commun)
    let want = content_old(&old, b"");
    assert_eq!(content_new(&new, b""), want);
    let dump_new = save_all(&mut new);
    let dump_old = save_all_old(&mut old);
    assert_eq!(content_dump(&dump_new), content_dump(&dump_old), "dumps");
    let mut back = OxidArt::new();
    back.set_now(now);
    back.load_dump(&dump_old).expect("reload du dump ancien");
    back.check();
    assert!(back.gc_audit_unique().is_empty(), "SharedByte partagés après reload");
    assert_eq!(content_new(&back, b""), want, "reload v2 ← ancien");
    let mut back_old = Old::new();
    back_old.set_now(now);
    back_old.load_dump(&dump_new).expect("reload ancien ← v2");
    assert_eq!(content_old(&back_old, b""), want, "reload ancien ← v2");
    // reload = arbre canonique : même nombre de nœuds que l'original, une
    // fois ses clés expirées (pas encore nettoyées) supprimées
    let mut stale = Vec::new();
    let mut path = Vec::new();
    new.walk(new.root_idx, &mut path, |k, _, _, exp| {
        if crate::expired(exp, now) {
            stale.push(k.to_vec());
        }
    });
    for k in &stale {
        assert!(!new.del(k));
    }
    assert_eq!(back.check(), new.check(), "nœuds après reload");
}

/// Valeur de range/MGET par contenu (le `Debug` affiche le refcount, qui
/// diffère quand une save tient des clones).
fn ov_new(v: &crate::krange::OwnedVal) -> Vec<u8> {
    match v {
        crate::krange::OwnedVal::Bytes(b) => b.to_vec(),
        crate::krange::OwnedVal::Int(n) => n.to_string().into_bytes(),
    }
}

fn ov_old(v: &oxidart_legacy::krange::OwnedVal) -> Vec<u8> {
    match v {
        oxidart_legacy::krange::OwnedVal::Bytes(b) => b.to_vec(),
        oxidart_legacy::krange::OwnedVal::Int(n) => n.to_string().into_bytes(),
    }
}

/// Glob tiré d'une clé : morceaux littéraux, `*`, `?`, classes.
#[cfg(feature = "regex")]
fn gen_glob(rng: &mut StdRng, key: &[u8]) -> Vec<u8> {
    let mut g = Vec::new();
    for &b in key {
        match rng.gen_range(0..10) {
            0 => g.push(b'?'),
            1 => g.push(b'*'),
            2 => g.extend_from_slice(b"[a-z]"),
            _ => {
                if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
                    g.push(b'\\');
                }
                g.push(b);
            }
        }
    }
    if rng.gen_bool(0.5) {
        g.push(b'*');
    }
    g
}

fn to_old(v: &Value) -> oxidart_legacy::Value {
    match v {
        // copie : l'oracle ne partage rien avec l'arbre testé (audit d'unicité)
        Value::String(b) => oxidart_legacy::Value::String(fresh(b)),
        Value::Int(n) => oxidart_legacy::Value::Int(*n),
        _ => unreachable!("le fuzz ne pose que des String/Int par set"),
    }
}

#[test]
fn diff_vs_legacy() {
    for keys in ALL_KEYS {
        let mut stats = Stats::default();
        for seed in 0..16 {
            fuzz(seed, 1_500, keys, &mut stats);
        }
        eprintln!("{keys:?} : {stats:?}");
        assert!(stats.saves >= 16 && stats.krange > 100 && stats.deln > 5);
    }
}

/// Conteneurs : 5 clés, surtout des opérations Hash et Set, pour traverser
/// souvent la frontière petit bloc ↔ valart (> 8 entrées), avec une save sous
/// mutations et des reloads croisés.
fn fuzz_containers(seed: u64, ops: usize, seen: &mut [usize; 4]) {
    use crate::value::Tag;
    let mut rng = StdRng::seed_from_u64(seed);
    let names: Vec<SharedByte> = (0..14).map(|i| SharedByte::from_slice(format!("x{i}"))).collect();
    let vals: Vec<SharedByte> = (0..5).map(|i| SharedByte::from_slice(format!("val-{i}"))).collect();
    let keys: [&[u8]; 5] = [b"h", b"h:1", b"s", b"s:1", b"x"];
    let mut new = OxidArt::new();
    let mut old = Old::new();
    new.set_now(1_000);
    old.set_now(1_000);
    let save_at = rng.gen_range(ops / 4..ops / 2);
    let mut saving: Option<Saving> = None;
    for step in 0..ops {
        let k = keys[rng.gen_range(0..keys.len())];
        let pick = |rng: &mut StdRng, n: usize| -> Vec<SharedByte> {
            (0..rng.gen_range(1..=n)).map(|_| fresh(&names[rng.gen_range(0..names.len())])).collect()
        };
        match rng.gen_range(0..12) {
            0..=2 => {
                let fv: Vec<(SharedByte, SharedByte)> = pick(&mut rng, 4)
                    .into_iter()
                    .map(|f| (f, fresh(&vals[rng.gen_range(0..vals.len())])))
                    .collect();
                let ttl = rng.gen_bool(0.1).then(|| 1_000 + rng.gen_range(0..50));
                assert_eq!(format!("{:?}", new.cmd_hset(k, &fv, ttl)), format!("{:?}", old.cmd_hset(k, &deep_pairs(&fv), ttl)), "hset");
            }
            3 => {
                let fs = pick(&mut rng, 3);
                assert_eq!(format!("{:?}", new.cmd_hdel(k, &fs)), format!("{:?}", old.cmd_hdel(k, &fs)), "hdel");
            }
            4 => {
                let f = fresh(&names[rng.gen_range(0..names.len())]);
                let d = rng.gen_range(-3..3);
                assert_eq!(
                    format!("{:?}", new.cmd_hincrby(k, f.clone(), d).ok()),
                    format!("{:?}", old.cmd_hincrby(k, fresh(&f), d).ok()),
                    "hincrby"
                );
            }
            5..=6 => {
                let ms = pick(&mut rng, 4);
                assert_eq!(format!("{:?}", new.cmd_sadd(k, &ms, None)), format!("{:?}", old.cmd_sadd(k, &deep(&ms), None)), "sadd");
            }
            7 => {
                let ms = pick(&mut rng, 3);
                assert_eq!(format!("{:?}", new.cmd_srem(k, &ms)), format!("{:?}", old.cmd_srem(k, &deep(&ms))), "srem");
            }
            8 => {
                let f = &names[rng.gen_range(0..names.len())];
                let a = new.cmd_hget(k, f).map(|v| v.map(|b| b.to_vec()));
                let b = old.cmd_hget(k, f).map(|v| v.map(|b| b.to_vec()));
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "hget");
                assert_eq!(format!("{:?}", new.cmd_hlen(k)), format!("{:?}", old.cmd_hlen(k)), "hlen");
                assert_eq!(format!("{:?}", new.cmd_scard(k)), format!("{:?}", old.cmd_scard(k)), "scard");
                assert_eq!(
                    format!("{:?}", new.cmd_sismember(k, f.clone())),
                    format!("{:?}", old.cmd_sismember(k, f.clone())),
                    "sismember"
                );
            }
            9 => {
                // SPOP : aléatoire, comparé par cardinalité seulement, puis
                // l'oracle retire les mêmes membres
                if let Ok(crate::scommand::SPOPResult::Multiple(popped)) = new.cmd_spop(k, Some(b"2")) {
                    if !popped.is_empty() {
                        old.cmd_srem(k, &popped).unwrap();
                    }
                }
            }
            10 => {
                assert_eq!(new.del(k), old.del(k), "del");
            }
            _ => {
                new.set_now(new.now + rng.gen_range(0..5));
                old.set_now(new.now);
            }
        }
        if step == save_at {
            let mut out = Vec::new();
            let cur = new.save_start(&mut out);
            saving = Some(Saving { cur, out, want: content_old(&old, b"") });
        }
        if let Some(s) = saving.as_mut()
            && new.save_step(&mut s.cur, &mut s.out, rng.gen_range(1..8))
        {
            let s = saving.take().unwrap();
            drop(s.cur);
            new.finish_save();
            assert_eq!(content_dump(&s.out), s.want, "save sous mutations (seed {seed})");
        }
        new.check();
        gc_check(&mut new, seed, step);
        assert_eq!(content_new(&new, b""), content_old(&old, b""), "contenu (seed {seed}, pas {step})");
        let mut path = Vec::new();
        new.walk(new.root_idx, &mut path, |_, tag, _, _| match tag {
            Tag::Hash => seen[0] += 1,
            Tag::HashSmall => seen[1] += 1,
            Tag::Set => seen[2] += 1,
            Tag::SetSmall => seen[3] += 1,
            _ => {}
        });
    }
    if let Some(mut s) = saving.take() {
        while !new.save_step(&mut s.cur, &mut s.out, 64) {}
        drop(s.cur);
        new.finish_save();
        assert_eq!(content_dump(&s.out), s.want, "save sous mutations (seed {seed})");
    }
    let want = content_old(&old, b"");
    let dump_new = save_all(&mut new);
    let mut back = OxidArt::new();
    back.set_now(new.now);
    back.load_dump(&dump_new).expect("reload");
    back.check();
    assert!(back.gc_audit_unique().is_empty(), "SharedByte partagés après reload");
    assert_eq!(content_new(&back, b""), want, "reload des conteneurs");
    let mut back_old = Old::new();
    back_old.set_now(new.now);
    back_old.load_dump(&dump_new).expect("reload ancien ← v2");
    assert_eq!(content_old(&back_old, b""), want, "reload ancien ← v2");
}

#[test]
fn diff_containers_small_and_valart() {
    let mut seen = [0usize; 4];
    for seed in 0..40 {
        fuzz_containers(seed, 1_500, &mut seen);
    }
    eprintln!("conteneurs vus (Hash valart, Hash petit, Set valart, Set petit) : {seen:?}");
    assert!(seen.iter().all(|&n| n > 1_000), "{seen:?}");
}

/// Graines longues : gros arbres, nœuds larges, saves sur beaucoup de clés.
#[test]
fn diff_vs_legacy_long() {
    for keys in [Keys::Binary, Keys::Mix, Keys::Long] {
        let mut stats = Stats::default();
        for seed in 100..102 {
            fuzz(seed, 6_000, keys, &mut stats);
        }
        eprintln!("{keys:?} long : {stats:?}");
    }
}

#[test]
fn diff_eviction_keeps_live_content() {
    let mut rng = StdRng::seed_from_u64(7);
    let mut new = OxidArt::new();
    let mut old = Old::new();
    new.set_now(1_000);
    old.set_now(1_000);
    for _ in 0..5_000 {
        let k = gen_key(&mut rng, Keys::Mix);
        let v = Value::Int(rng.gen_range(0..100));
        let d = Duration::from_secs(rng.gen_range(0..10));
        if rng.gen_bool(0.6) {
            new.set_ttl(sb(&k), d, v.clone());
            old.set_ttl(sb(&k), d, to_old(&v));
        } else {
            new.set(sb(&k), v.clone());
            old.set(sb(&k), to_old(&v));
        }
    }
    new.check();
    new.set_now(1_005);
    old.set_now(1_005);
    let mut evicted = 0;
    for _ in 0..200 {
        evicted += new.evict_expired();
        new.check();
    }
    assert!(evicted > 0);
    assert_eq!(content_new(&new, b""), content_old(&old, b""));
    // toutes les expirées finissent par partir : plus aucun nœud tagué expiré
    new.set_now(1_000_000);
    old.set_now(1_000_000);
    while new.evict_expired() > 0 {}
    for _ in 0..50 {
        new.evict_expired();
    }
    new.check();
    assert_eq!(content_new(&new, b""), content_old(&old, b""));
}

/// Bug de l'ancien arbre (corrigé dans la copie oracle, absent du nouveau) :
/// FLUSHDB (`deln("")`) vidait les enfants inline de la racine mais laissait son
/// débordement pointer sur des nœuds libérés.
#[test]
fn legacy_flush_leaves_dangling_root_overflow() {
    let mut old = Old::new();
    for b in 0..10u8 {
        old.set(sb(&[b'a' + b]), oxidart_legacy::Value::Int(b as i64));
    }
    assert_eq!(old.deln(b""), 10);
    let after = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        old.krange(b"", b"", usize::MAX, false).len()
    }));
    assert_eq!(after.ok(), Some(0));
    assert_eq!(old.get(b"h"), None);
    // le nouvel arbre, lui
    let mut new = OxidArt::new();
    for b in 0..10u8 {
        new.set(sb(&[b'a' + b]), Value::Int(b as i64));
    }
    assert_eq!(new.deln(b""), 10);
    assert_eq!(new.check(), 1);
    assert_eq!(new.get(b"h"), None);
}
