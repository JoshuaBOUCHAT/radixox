//! Save progressive : la vue T0 lue par le parcours doit être exactement
//! l'état au `begin_save`, quelles que soient les mutations entrelacées.

use std::time::Duration;

use radixox_lib::shared_byte::SharedByte;

use crate::OxidArt;
use crate::value::Value;

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
        let expect = art.snapshot();
        art.begin_save();
        let mut ops = Rng(rng.next() | 1);
        let n_ops = 1 + rng.below(8);
        let got = art.dump_t0(|a| {
            for _ in 0..ops.below(n_ops) {
                random_op(a, &mut ops);
            }
        });
        art.finish_save();
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
    let expect = art.snapshot();
    art.begin_save();
    for _ in 0..3_000 {
        random_op(&mut art, &mut rng);
    }
    let got = art.dump_t0(|_| {});
    art.finish_save();
    assert_eq!(got, expect);
    // Après la save, l'arbre vivant est cohérent et les libérations
    // différées ont été faites.
    let _ = art.snapshot();
}

#[test]
fn save_flushdb_during_save() {
    let mut art = OxidArt::new();
    for i in 0..500 {
        art.set(sb(&format!("key:{i}")), Value::String(sb(&format!("valeur-{i}-xxxxxxxx"))));
        let _ = art.cmd_hset(format!("hash:{}", i % 10).as_bytes(), &[(sb(&format!("f{i}")), sb("v"))], None);
    }
    let expect = art.snapshot();
    art.begin_save();
    assert!(art.deln(b"") > 0);
    assert!(art.snapshot().is_empty());
    let got = art.dump_t0(|_| {});
    art.finish_save();
    assert_eq!(got, expect);
    assert!(art.snapshot().is_empty());
}
