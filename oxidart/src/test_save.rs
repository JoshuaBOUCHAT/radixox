//! Save progressive : la vue T0 lue par le parcours doit être exactement
//! l'état au `begin_save`, quelles que soient les mutations entrelacées.

use std::time::Duration;

use radixox_lib::shared_byte::SharedByte;

use std::collections::VecDeque;

use crate::save::reference::{decoded, expected, save_all};
use crate::value::Value;
use crate::{OxidArt, parse_dump};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn sb(s: &str) -> SharedByte {
    SharedByte::from_str(s)
}

/// Clés à préfixes communs (splits, recompressions, overflow d'enfants).
fn key(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 => format!("k{}", rng.below(300)),
        1 => format!("user:{}", rng.below(40)),
        2 => format!("user:{}:name", rng.below(40)),
        _ => format!("h{}", rng.below(30)),
    }
}

/// Valeurs > 7 octets de temps en temps (compression heap, SharedByte).
fn val(rng: &mut Rng) -> String {
    if rng.below(3) == 0 {
        format!("valeur-longue-{}", rng.next())
    } else {
        format!("v{}", rng.below(1000))
    }
}

fn random_op(art: &mut OxidArt, rng: &mut Rng) {
    let k = key(rng);
    match rng.below(16) {
        0..=2 => art.set(sb(&k), Value::String(sb(&val(rng)))),
        3 => art.set_ttl(sb(&k), Duration::from_secs(1 + rng.below(5)), Value::String(sb(&val(rng)))),
        4 => {
            art.del(k.as_bytes());
        }
        5 => {
            art.expire(sb(&k), Duration::from_secs(rng.below(4)));
        }
        6 => {
            art.persist(sb(&k));
        }
        7 => {
            let _ = art.incr(sb(&k));
        }
        8..=9 => {
            let pairs: Vec<_> = (0..1 + rng.below(4))
                .map(|_| (sb(&format!("f{}", rng.below(300))), sb(&val(rng))))
                .collect();
            let _ = art.cmd_hset(k.as_bytes(), &pairs, None);
        }
        10 => {
            let fields: Vec<_> = (0..1 + rng.below(6)).map(|_| sb(&format!("f{}", rng.below(300)))).collect();
            let _ = art.cmd_hdel(k.as_bytes(), &fields);
        }
        11 => {
            let members: Vec<_> = (0..1 + rng.below(4)).map(|_| sb(&format!("m{}", rng.below(300)))).collect();
            let _ = art.cmd_sadd(k.as_bytes(), &members, None);
        }
        12 => {
            let members: Vec<_> = (0..1 + rng.below(6)).map(|_| sb(&format!("m{}", rng.below(300)))).collect();
            let _ = art.cmd_srem(k.as_bytes(), &members);
        }
        13 => {
            let _ = art.cmd_spop(k.as_bytes(), Some(b"3"));
        }
        14 => {
            let prefix = format!("user:{}", rng.below(40));
            art.deln(prefix.as_bytes());
        }
        15 if rng.below(2) == 0 => {
            let pairs: Vec<_> = (0..1 + rng.below(4))
                .map(|_| (rng.below(100) as f64 / 4.0, sb(&format!("z{}", rng.below(50)))))
                .collect();
            let _ = art.cmd_zadd(sb(&format!("z{}", rng.below(20))), &pairs, None);
        }
        15 if rng.below(4) == 0 => {
            let items: VecDeque<_> = (0..rng.below(5)).map(|i| sb(&format!("item-{i}"))).collect();
            art.set(sb(&format!("l{}", rng.below(10))), Value::List(items));
        }
        _ => {
            let _ = art.get(k.as_bytes());
            art.set_now(art.now + rng.below(2));
        }
    }
}

#[test]
fn save_t0_view_matches_snapshot() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for round in 0..60 {
        for _ in 0..400 {
            random_op(&mut art, &mut rng);
        }
        let expect = expected(&art);
        let mut ops = Rng(rng.next() | 1);
        let n_ops = 1 + rng.below(8);
        // Budget 1 : un nœud par tranche, mutations entre chaque nœud.
        let got = save_all(&mut art, || 1, |a| {
            for _ in 0..ops.below(n_ops) {
                random_op(a, &mut ops);
            }
        });
        assert_eq!(got.len(), expect.len(), "round {round}");
        assert_eq!(got, expect, "round {round}");
    }
}

#[test]
fn save_heavy_mutation_before_traversal() {
    // Toutes les mutations avant le premier pas : un maximum d'images.
    let mut rng = Rng(77);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for _ in 0..3_000 {
        random_op(&mut art, &mut rng);
    }
    let expect = expected(&art);
    let mut first = true;
    let got = save_all(&mut art, || 64, |a| {
        if std::mem::take(&mut first) {
            for _ in 0..3_000 {
                random_op(a, &mut rng);
            }
        }
    });
    assert_eq!(got, expect);
    // Après la save, l'arbre vivant est cohérent et les libérations
    // différées ont été faites.
    let _ = expected(&art);
}

#[test]
fn save_flushdb_during_save() {
    let mut art = OxidArt::new();
    for i in 0..500 {
        art.set(sb(&format!("key:{i}")), Value::String(sb(&format!("valeur-{i}-xxxxxxxx"))));
        let _ = art.cmd_hset(format!("hash:{}", i % 10).as_bytes(), &[(sb(&format!("f{i}")), sb("v"))], None);
    }
    let expect = expected(&art);
    let mut first = true;
    let got = save_all(&mut art, || 16, |a| {
        if std::mem::take(&mut first) {
            assert!(a.deln(b"") > 0);
            assert!(expected(a).is_empty());
        }
    });
    assert_eq!(got, expect);
    assert!(expected(&art).is_empty());
}

/// Save complète en tranches de budget aléatoire, mutations aléatoires
/// entre les tranches ; le dump décodé doit être l'état T0, TTL compris.
#[test]
fn dump_roundtrip_with_tranches() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for round in 0..40 {
        for _ in 0..500 {
            random_op(&mut art, &mut rng);
        }
        let expect = expected(&art);
        let t0 = art.now;
        let mut out = Vec::new();
        let mut cur = art.save_start(&mut out);
        let mut ops = Rng(rng.next() | 1);
        let mut steps = 0;
        while !art.save_step(&mut cur, &mut out, 1 + ops.below(64) as u32) {
            steps += 1;
            for _ in 0..ops.below(4) {
                random_op(&mut art, &mut ops);
            }
        }
        art.finish_save();
        assert!(steps > 0 || expect.len() < 4, "round {round} : une seule tranche");
        assert_eq!(parse_dump(&out).unwrap().t0, t0);
        let got = decoded(&out);
        assert_eq!(got.len(), expect.len(), "round {round}");
        assert_eq!(got, expect, "round {round}");
    }
}

/// lcp : les clés d'un même sous-arbre ne répètent pas leur préfixe.
#[test]
fn dump_prefix_compression() {
    let mut art = OxidArt::new();
    for i in 0..1_000 {
        art.set(sb(&format!("utilisateur:{i:05}:nom")), Value::String(sb("x")));
    }
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 1024) {}
    art.finish_save();
    // Clé complète = 21 octets ; avec lcp, ~7 octets par enregistrement.
    assert!(out.len() < 1_000 * 12, "dump de {} octets", out.len());
    assert_eq!(decoded(&out), expected(&art));
}

#[test]
fn dump_rejects_corruption() {
    let mut art = OxidArt::new();
    art.set(sb("a"), Value::String(sb("1")));
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 1024) {}
    art.finish_save();
    assert!(parse_dump(&out).is_ok());
    assert!(parse_dump(&out[..out.len() - 1]).is_err());
    let mut bad = out.clone();
    bad[0] = b'X';
    assert!(parse_dump(&bad).is_err());
    let mut extra = out.clone();
    extra.push(0);
    assert!(parse_dump(&extra).is_err());
}
