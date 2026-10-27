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

thread_local! {
    /// SPOP tire au hasard (seed valart, disposition) : à couper quand on
    /// compare deux arbres soumis aux mêmes opérations.
    static NO_SPOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn random_op(art: &mut OxidArt, rng: &mut Rng) {
    let k = key(rng);
    match rng.below(20) {
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
        13 if !NO_SPOP.with(|c| c.get()) => {
            let _ = art.cmd_spop(k.as_bytes(), Some(b"3"));
        }
        13 => {}
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
        // grands ZSet (au-delà de 128 membres : index, B+tree, poignées), mutés et
        // supprimés pendant la save ; libération par tranches entre deux opérations
        16 | 17 => {
            let pairs: Vec<_> = (0..1 + rng.below(30))
                .map(|_| ((rng.below(40) as f64) - 20.0, sb(&format!("member:{}", rng.below(700)))))
                .collect();
            let _ = art.cmd_zadd(sb(&format!("bigz{}", rng.below(3))), &pairs, None);
        }
        18 => {
            let members: Vec<_> = (0..1 + rng.below(20)).map(|_| sb(&format!("member:{}", rng.below(700)))).collect();
            let _ = art.cmd_zrem(format!("bigz{}", rng.below(3)).as_bytes(), &members);
            let _ = art.cmd_zincrby(sb(&format!("bigz{}", rng.below(3))), 0.5, sb(&format!("member:{}", rng.below(700))));
        }
        19 if rng.below(8) == 0 => {
            art.del(format!("bigz{}", rng.below(3)).as_bytes());
            art.zset_free_step(64);
        }
        _ => {
            let _ = art.get(k.as_bytes());
            art.set_now(art.now + rng.below(2));
            art.zset_free_step(64);
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

#[test]
fn file_footer_detects_truncation_and_corruption() {
    use crate::save::format;
    use crate::check_footer;

    let mut art = OxidArt::new();
    art.set_now(1_000);
    for i in 0..500 {
        art.set(sb(&format!("k{i}")), Value::String(sb("v")));
    }
    let mut body = Vec::new();
    let mut cur = art.save_start(&mut body);
    while !art.save_step(&mut cur, &mut body, 64) {}
    let digest = cur.digest().value();
    drop(cur);
    art.finish_save();

    let mut file = body.clone();
    file.extend_from_slice(&format::footer(crc32fast::hash(&body), digest));
    assert_eq!(check_footer(&file).unwrap(), &body[..]);
    assert_eq!(parse_dump(check_footer(&file).unwrap()).unwrap().entries.len(), 500);

    // Tronqué : plus de pied valide.
    for cut in [0, 3, file.len() / 2, file.len() - 1] {
        assert!(check_footer(&file[..cut]).is_err(), "cut {cut}");
    }
    // Un octet modifié dans le corps.
    let mut bad = file.clone();
    bad[body.len() / 2] ^= 0x40;
    assert_eq!(check_footer(&bad), Err(crate::DumpError("crc invalide")));
}

// ─── Reload (SAVE_DESIGN.md §5, étape 8) ─────────────────────────────────────

fn dump_of(art: &mut OxidArt) -> Vec<u8> {
    dump_digest_of(art).0
}

/// Dump brut et son condensé (pied).
fn dump_digest_of(art: &mut OxidArt) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 1024) {}
    let digest = cur.digest().value();
    drop(cur);
    art.finish_save();
    (out, digest)
}

fn reload(buf: &[u8], now: u64) -> (OxidArt, crate::LoadStats) {
    let mut art = OxidArt::new();
    art.set_now(now);
    let stats = art.load_dump(buf).expect("reload");
    (art, stats)
}

/// Aller-retour : état aléatoire → dump → reload ; même contenu, même nombre
/// de nœuds (la construction en masse produit l'arbre canonique), puis les
/// deux arbres réagissent pareil à une même suite de mutations (structure
/// rechargée valide : parent_idx, overflow, compressions).
#[test]
fn reload_roundtrip_random() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for round in 0..30 {
        for _ in 0..500 {
            random_op(&mut art, &mut rng);
        }
        let buf = dump_of(&mut art);
        let (mut back, stats) = reload(&buf, art.now);
        let expect = expected(&art);
        assert_eq!(stats.keys as usize, expect.len(), "round {round}");
        assert_eq!(stats.expired, 0, "round {round}");
        assert_eq!(expected(&back), expect, "round {round}");
        // ≤ : l'original garde des clés expirées pas encore nettoyées.
        assert!(back.node_count() <= art.node_count(), "round {round} : nœuds");

        let seed = rng.next() | 1;
        let (mut r1, mut r2) = (Rng(seed), Rng(seed));
        NO_SPOP.with(|c| c.set(true));
        for op in 0..300 {
            random_op(&mut art, &mut r1);
            random_op(&mut back, &mut r2);
            assert!(expected(&back) == expected(&art), "round {round} : divergence à l'op {op}");
        }
        NO_SPOP.with(|c| c.set(false));
    }
}

/// Branchement découvert après coup (`abcd, abcx, ab_`), clé vide, clé
/// préfixe d'une autre, fan-out > inline (overflow léger et huge).
#[test]
fn reload_shapes() {
    let mut art = OxidArt::new();
    for k in ["abcd", "abcx", "ab_", "ab", "", "a", "abcdef", "z"] {
        art.set(sb(k), Value::String(sb(&format!("v-{k}"))));
    }
    for i in 0..90u8 {
        let c = (b'!' + i) as char;
        art.set(sb(&format!("fan:{c}")), Value::String(sb("x")));
        art.set(sb(&format!("fan:{c}:sub")), Value::Int(i as i64));
    }
    let _ = art.cmd_hset(b"h", &[(sb("f"), sb("v")), (sb("g"), sb("w"))], None);
    let _ = art.cmd_sadd(b"s", &[sb("m1"), sb("m2")], None);
    let buf = dump_of(&mut art);
    let (mut back, _) = reload(&buf, art.now);
    assert_eq!(expected(&back), expected(&art));
    assert_eq!(back.node_count(), art.node_count());
    for i in 0..90u8 {
        let c = (b'!' + i) as char;
        assert!(back.del(format!("fan:{c}").as_bytes()));
    }
    assert_eq!(back.get(b"fan:#:sub").map(|v| v.to_int().unwrap()), Some(2));
}

/// Clés expirées entre la save et le reload : sautées, sans casser le
/// chaînage lcp ; les TTL restants sont recalculés et l'évicteur retrouve
/// les clés à TTL (nœuds tagués, parent_idx corrects).
#[test]
fn reload_skips_expired_and_evicts() {
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for i in 0..2_000 {
        let k = format!("user:{i}:session");
        match i % 3 {
            0 => art.set(sb(&k), Value::String(sb("permanent"))),
            1 => art.set_ttl(sb(&k), Duration::from_secs(10), Value::String(sb("court"))),
            _ => art.set_ttl(sb(&k), Duration::from_secs(100), Value::String(sb("long"))),
        }
    }
    let buf = dump_of(&mut art);

    // 50 s plus tard : les TTL de 10 s ont expiré.
    art.set_now(1_050);
    let (mut back, stats) = reload(&buf, 1_050);
    let expect: Vec<_> = expected(&art);
    assert_eq!(stats.expired, 667);
    assert_eq!(stats.keys as usize, expect.len());
    assert_eq!(expected(&back), expect);
    assert_eq!(back.get_ttl(sb("user:2:session")), crate::TtlResult::KeyWithTtl(50));

    // Plus tard encore : l'évicteur vide les clés longues.
    back.set_now(1_200);
    while back.evict_expired() > 0 {}
    let rest = expected(&back);
    assert!(rest.iter().all(|(_, v, ttl)| ttl.is_none() && *v == crate::save::reference::DumpVal::Str(b"permanent".to_vec())));
    assert!(rest.len() >= 667, "{}", rest.len());
}

#[test]
fn reload_rejects_non_empty_tree_and_bad_dump() {
    let mut art = OxidArt::new();
    art.set(sb("a"), Value::String(sb("1")));
    let buf = dump_of(&mut art);
    assert!(art.load_dump(&buf).is_err());
    let mut fresh = OxidArt::new();
    assert!(fresh.load_dump(&buf[..buf.len() - 1]).is_err());
}

/// Source qui ne rend que de petits morceaux de taille variable : chaque
/// primitive du lecteur finit par chevaucher une frontière de lecture.
struct Dribble<'a> {
    data: &'a [u8],
    rng: Rng,
    max: u64,
}
impl std::io::Read for Dribble<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = (1 + self.rng.below(self.max) as usize).min(out.len()).min(self.data.len());
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

fn file_of(art: &mut OxidArt) -> Vec<u8> {
    let (mut file, digest) = dump_digest_of(art);
    let crc = crc32fast::hash(&file);
    file.extend_from_slice(&crate::save::format::footer(crc, digest));
    file
}

/// Reload d'un fichier par fragments (lectures de 1 à 13 octets, et une
/// valeur plus grande que la fenêtre de lecture) : même résultat qu'en un
/// bloc, crc du pied vérifié au fil de l'eau.
#[test]
fn reload_file_by_fragments() {
    let mut rng = Rng(0x51_7CC1_B727_220A);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for _ in 0..3_000 {
        random_op(&mut art, &mut rng);
    }
    // 3 MB : plus grand que READ_CHUNK (1 MB), agrandit la fenêtre.
    let big: Vec<u8> = (0..3 << 20).map(|i| (i % 251) as u8).collect();
    art.set(sb("grosse"), Value::String(SharedByte::from_slice(&big)));
    let file = file_of(&mut art);
    let expect = expected(&art);

    for max in [1, 13, 1 << 22] {
        let mut back = OxidArt::new();
        back.set_now(art.now);
        let src = Dribble { data: &file, rng: Rng(max | 1), max };
        let stats = back.load_file(src).expect("reload par fragments");
        assert_eq!(stats.keys as usize, expect.len(), "max {max}");
        assert_eq!(expected(&back), expect, "max {max}");
    }
}

#[test]
fn reload_file_rejects_truncation_corruption_and_trailing_bytes() {
    let mut art = OxidArt::new();
    for i in 0..2_000 {
        art.set(sb(&format!("k{i}")), Value::String(sb(&format!("valeur-{i}"))));
    }
    let file = file_of(&mut art);
    let load = |data: &[u8]| OxidArt::new().load_file(Dribble { data, rng: Rng(7), max: 4096 });
    assert!(load(&file).is_ok());
    for cut in [0, 10, file.len() / 2, file.len() - 8, file.len() - 1] {
        assert!(load(&file[..cut]).is_err(), "cut {cut}");
    }
    // Un octet de valeur modifié : le flux reste lisible, seul le crc le voit.
    let mut bad = file.clone();
    let at = file.windows(10).position(|w| w == b"valeur-100").unwrap() + 7;
    bad[at] ^= 0x01;
    match load(&bad) {
        Err(crate::LoadError::Dump(e)) => assert_eq!(e.0, "crc invalide"),
        other => panic!("{other:?}"),
    }
    let mut extra = file.clone();
    extra.push(0);
    assert!(load(&extra).is_err());
}

/// Fichier compressé en tranches minimales (un bloc par appel), relu par
/// fragments : même état. Le format est une frame zstd standard qui redonne
/// exactement le fichier brut.
#[test]
fn reload_compressed_file() {
    use crate::compress::{ZstdReader, ZstdStream};
    use std::io::Read;
    use std::time::Duration;

    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for _ in 0..3_000 {
        random_op(&mut art, &mut rng);
    }
    let raw = file_of(&mut art);
    let expect = expected(&art);

    for level in [1, 3, 9, 19] {
        let mut z = ZstdStream::new(level);
        let mut packed = Vec::new();
        let mut pos = 0;
        while pos < raw.len() {
            // Petits morceaux d'entrée et temps nul : au plus un bloc par appel.
            let end = raw.len().min(pos + 1 + rng.below(5000) as usize);
            pos += z.compress(&raw[pos..end], &mut packed, Duration::ZERO);
        }
        while !z.finish(&mut packed, Duration::ZERO) {}
        assert!(packed.len() < raw.len() / 2, "niveau {level} : {} -> {}", raw.len(), packed.len());

        let mut unpacked = Vec::new();
        ZstdReader::new(&packed[..]).read_to_end(&mut unpacked).unwrap();
        assert!(unpacked == raw, "niveau {level} : frame ≠ fichier brut");

        let mut back = OxidArt::new();
        back.set_now(art.now);
        let stats = back
            .load_file(Dribble { data: &packed, rng: Rng(level as u64 | 1), max: 97 })
            .expect("reload compressé");
        assert_eq!(stats.keys as usize, expect.len(), "niveau {level}");
        assert_eq!(expected(&back), expect, "niveau {level}");

        // Tronqué, ou suivi d'octets : refusé.
        assert!(OxidArt::new().load_file(&packed[..packed.len() - 3]).is_err());
        let mut extra = packed.clone();
        extra.push(0);
        assert!(OxidArt::new().load_file(&extra[..]).is_err());
    }
}

/// Fin de save : les feuilles des images (une copie de valeur chacune) ne sont
/// pas libérées d'un bloc par `end_save` mais mises de côté et libérées par
/// tranches dans `drain_deferred` (pic de ~9 ms sur 1 M clés sinon).
#[test]
fn image_leaves_are_freed_by_slices() {
    let mut art = OxidArt::new();
    for i in 0..2000 {
        art.set(sb(&format!("k:{i:04}")), Value::from_str("v"));
    }
    let expect = expected(&art);
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    // Écrasées avant le parcours : leurs nœuds sont capturés avec leurs feuilles.
    for i in 0..2000 {
        art.set(sb(&format!("k:{i:04}")), Value::from_str("w"));
    }
    while !art.save_step(&mut cur, &mut out, 64) {}
    drop(cur);
    art.end_save();
    let pending = art.save.leftover.len();
    assert!(pending > 1000, "feuilles mises de côté : {pending}");
    let mut slices = 0;
    while !art.drain_deferred(100) {
        slices += 1;
    }
    assert!(slices >= pending / 100 - 1, "libérées par tranches ({slices})");
    assert!(art.save.leftover.is_empty());
    assert_eq!(decoded(&out), expect, "vue T0 intacte");
    assert_eq!(art.get(b"k:0007"), Some(Value::from_str("w")));
}

// ─── Condensé du contenu (DURABILITY_DESIGN.md §6.4, test 3) ─────────────────

/// Le condensé calculé par le DFS de la save (tranches minimales, mutations
/// entre chaque tranche) est celui de l'état au T0, calculé par le parcours
/// de l'arbre : deux codes différents, même valeur.
#[test]
fn save_digest_is_the_t0_state_under_mutations() {
    let mut rng = Rng(0xD16E_57AA_0001_0001);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for round in 0..40 {
        for _ in 0..400 {
            random_op(&mut art, &mut rng);
        }
        let want = art.content_digest(art.now);
        let mut ops = Rng(rng.next() | 1);
        let mut out = Vec::new();
        let mut cur = art.save_start(&mut out);
        while !art.save_step(&mut cur, &mut out, 1 + ops.below(4) as u32) {
            for _ in 0..ops.below(6) {
                random_op(&mut art, &mut ops);
            }
        }
        let got = cur.digest();
        drop(cur);
        art.finish_save();
        assert_eq!(got, want, "round {round}");
    }
}

/// Reload d'un fichier v2 : condensé vérifié, y compris quand des clés ont
/// expiré entre la save et le reload (leurs termes viennent du décodage).
#[test]
fn reload_checks_digest_even_with_expired_keys() {
    let mut rng = Rng(0xD16E_57AA_0002_0001);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for _ in 0..3_000 {
        random_op(&mut art, &mut rng);
    }
    // Conteneurs valart (> 8 entrées), dont un qui expire.
    let fields: Vec<_> = (0..50).map(|i| (sb(&format!("f{i}")), sb(&format!("v{i}")))).collect();
    let members: Vec<_> = (0..50).map(|i| sb(&format!("m{i}"))).collect();
    art.cmd_hset(b"gros-hash", &fields, None).unwrap();
    art.cmd_sadd(b"gros-set", &members, Some(art.now + 1)).unwrap();
    let file = file_of(&mut art);
    for later in [0, 2, 10] {
        let mut back = OxidArt::new();
        back.set_now(art.now + later);
        let stats = back.load_file(&file[..]).expect("reload");
        assert!(stats.digest_checked);
        assert_eq!(stats.expired > 0, later > 0, "plus tard de {later} s");
    }
}

/// Une valeur altérée dans un dump dont le crc est recalculé : le flux reste
/// lisible, le crc ne voit rien, le condensé refuse.
#[test]
fn reload_refuses_altered_content_with_valid_crc() {
    use crate::save::format;
    let mut art = OxidArt::new();
    for i in 0..2_000 {
        art.set(sb(&format!("k{i}")), Value::String(sb(&format!("valeur-{i}"))));
    }
    let (mut body, digest) = dump_digest_of(&mut art);
    let at = body.windows(10).position(|w| w == b"valeur-100").unwrap() + 7;
    body[at] ^= 0x01;
    let mut file = body.clone();
    file.extend_from_slice(&format::footer(crc32fast::hash(&body), digest));
    match OxidArt::new().load_file(&file[..]) {
        Err(crate::LoadError::Dump(e)) => assert!(e.0.starts_with("condensé"), "{}", e.0),
        other => panic!("{other:?}"),
    }
}

/// Un dump v1 (sans position du journal ni condensé) reste lisible.
#[test]
fn reload_reads_version_1() {
    use crate::save::format;
    let mut rng = Rng(0xD16E_57AA_0003_0001);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for _ in 0..2_000 {
        random_op(&mut art, &mut rng);
    }
    // Les ZSet sont dans leur enregistrement depuis la v3 (en v1 / v2 : dans la section
    // du début) : sans ZSet, le corps est le même d'une version à l'autre.
    for i in 0..20 {
        art.del(format!("z{i}").as_bytes());
    }
    for i in 0..3 {
        art.del(format!("bigz{i}").as_bytes());
    }
    let (v2, _) = dump_digest_of(&mut art);
    // En-tête v2 = v1 + 36 octets (seq, replid, offset) après la graine.
    const AFTER_SEED: usize = 4 + 1 + 1 + 8 + 8;
    let mut v1 = v2[..AFTER_SEED].to_vec();
    v1[4] = format::VERSION_1;
    v1.extend_from_slice(&v2[AFTER_SEED + 36..]);
    let mut file = v1.clone();
    file.extend_from_slice(&crc32fast::hash(&v1).to_le_bytes());
    file.extend_from_slice(&format::FOOTER_MAGIC);
    let mut back = OxidArt::new();
    back.set_now(art.now);
    let stats = back.load_file(&file[..]).expect("reload v1");
    assert!(!stats.digest_checked);
    assert_eq!(expected(&back), expected(&art));
    assert_eq!(crate::check_footer(&file).unwrap(), &v1[..]);
}

/// La position du journal fait l'aller-retour par l'en-tête.
#[test]
fn journal_position_roundtrip() {
    let mut art = OxidArt::new();
    art.set(sb("a"), Value::String(sb("1")));
    let pos = crate::JournalPos { seq: 42, offset: 9_876_543, replid: [7; 20] };
    let mut out = Vec::new();
    let mut cur = art.save_start_at(&mut out, &pos);
    while !art.save_step(&mut cur, &mut out, 64) {}
    drop(cur);
    art.finish_save();
    assert_eq!(parse_dump(&out).unwrap().pos, pos);
    let mut back = OxidArt::new();
    assert_eq!(back.load_dump(&out).unwrap().pos, pos);
}

// ─── Écrasement sur place (MEMORY_ARCH.md §16.5) ─────────────────────────────

fn str_ptr(art: &mut OxidArt, k: &str) -> (*const u8, Vec<u8>) {
    match art.get(k.as_bytes()) {
        Some(Value::String(s)) => (s.as_ptr(), s.to_vec()),
        v => panic!("{k} : {v:?}"),
    }
}

fn hash_ptr(art: &mut OxidArt, k: &str, f: &str) -> (*const u8, Vec<u8>) {
    let v = art.cmd_hget(k.as_bytes(), f.as_bytes()).unwrap().unwrap();
    (v.as_ptr(), v.to_vec())
}

#[test]
fn overwrite_in_place_strings() {
    let mut art = OxidArt::new();
    // "user:" est la self-val du nœud "user:", "user:1" une feuille dessous.
    for k in ["user:", "user:1", "user:2"] {
        art.set(sb(k), Value::String(sb(&"a".repeat(100))));
    }
    for k in ["user:", "user:1"] {
        let (p, _) = str_ptr(&mut art, k);
        art.set(sb(k), Value::String(sb(&"b".repeat(100))));
        assert_eq!(str_ptr(&mut art, k), (p, b"b".repeat(100)), "{k} : même taille, en place");
        art.set(sb(k), Value::String(sb(&"c".repeat(40))));
        assert_eq!(str_ptr(&mut art, k), (p, b"c".repeat(40)), "{k} : plus court, en place");
        // 4 + 39 et 4 + 40 octets : même nombre de granules (2 o).
        art.set(sb(k), Value::String(sb(&"d".repeat(39))));
        assert_eq!(str_ptr(&mut art, k), (p, b"d".repeat(39)), "{k} : même nombre de granules");
        // La capacité est celle de la valeur courante (39 → 44 o) : 41 octets réallouent.
        art.set(sb(k), Value::String(sb(&"d".repeat(41))));
        let (p2, _) = str_ptr(&mut art, k);
        assert_ne!(p2, p, "{k} : un granule de plus");
        let p = p2;
        art.set(sb(k), Value::String(sb(&"d".repeat(41))));
        assert_eq!(str_ptr(&mut art, k).0, p);
        art.set(sb(k), Value::String(sb(&"e".repeat(200))));
        let (q, v) = str_ptr(&mut art, k);
        assert!(q != p && v == b"e".repeat(200), "{k} : plus long, nouvel objet");
        // TTL posé : la feuille change de forme, valeur remplacée normalement.
        art.set_ttl(sb(k), Duration::from_secs(60), Value::String(sb(&"f".repeat(200))));
        assert_eq!(str_ptr(&mut art, k).1, b"f".repeat(200));
        assert!(matches!(art.get_ttl(sb(k)), crate::TtlResult::KeyWithTtl(_)), "{k} : TTL posé");
        // SET sans TTL après : TTL retiré, même si la valeur est écrasée sur place.
        art.set(sb(k), Value::String(sb(&"g".repeat(200))));
        assert_eq!(str_ptr(&mut art, k).1, b"g".repeat(200));
        assert!(matches!(art.get_ttl(sb(k)), crate::TtlResult::KeyWithoutTtl), "{k} : TTL retiré");
    }
    // Partagé (une copie vit ailleurs) : jamais écrasé.
    let held = art.get(b"user:2");
    art.set(sb("user:2"), Value::String(sb(&"z".repeat(100))));
    assert_eq!(held, Some(Value::String(sb(&"a".repeat(100)))));
    assert_eq!(str_ptr(&mut art, "user:2").1, b"z".repeat(100));
}

#[test]
fn overwrite_in_place_hashes() {
    let mut art = OxidArt::new();
    // "small" reste un petit bloc, "big" passe en valart (> SMALL_MAX champs).
    for (k, n) in [("small", 4), ("big", 40)] {
        let fields: Vec<_> = (0..n).map(|i| (sb(&format!("f{i}")), sb(&"a".repeat(100)))).collect();
        art.cmd_hset(k.as_bytes(), &fields, None).unwrap();
        drop(fields); // sinon chaque valeur stockée est partagée (rc == 2) : jamais écrasée
        let (p, _) = hash_ptr(&mut art, k, "f3");
        art.cmd_hset(k.as_bytes(), &[(sb("f3"), sb(&"b".repeat(100)))], None).unwrap();
        assert_eq!(hash_ptr(&mut art, k, "f3"), (p, b"b".repeat(100)), "{k}");
        art.cmd_hset(k.as_bytes(), &[(sb("f3"), sb(&"c".repeat(300)))], None).unwrap();
        let (q, v) = hash_ptr(&mut art, k, "f3");
        assert!(q != p && v == b"c".repeat(300), "{k} : plus long, nouvel objet");
    }
}

/// Écrasements de même taille pendant une save : le dump garde les anciennes valeurs.
#[test]
fn overwrite_during_save_keeps_t0() {
    let mut art = OxidArt::new();
    art.set_now(1_000);
    for i in 0..300 {
        art.set(sb(&format!("key:{i}")), Value::String(sb(&format!("old-{i:04}-xxxxxxxx"))));
    }
    for (k, n) in [("small", 4), ("big", 40)] {
        let fields: Vec<_> = (0..n).map(|i| (sb(&format!("f{i}")), sb(&format!("old-{i:04}")))).collect();
        art.cmd_hset(k.as_bytes(), &fields, None).unwrap();
    }
    let expect = expected(&art);
    let mut round = 0;
    let got = save_all(&mut art, || 8, |a| {
        round += 1;
        for i in (round % 7..300).step_by(7) {
            a.set(sb(&format!("key:{i}")), Value::String(sb(&format!("new-{i:04}-{round:08}"))));
        }
        for (k, n) in [("small", 4), ("big", 40)] {
            let fields: Vec<_> = (0..n).map(|i| (sb(&format!("f{i}")), sb(&format!("n{round:03}{i:02}")))).collect();
            a.cmd_hset(k.as_bytes(), &fields, None).unwrap();
        }
    });
    assert_eq!(got, expect);
    assert!(round > 1, "mutations entre les tranches");
    // Après la save, plus rien ne retient les valeurs : écrasement sur place à nouveau.
    let (p, _) = str_ptr(&mut art, "key:1");
    art.set(sb("key:1"), Value::String(sb("new-0001-zzzzzzzz")));
    assert_eq!(str_ptr(&mut art, "key:1").0, p);
}

// ─── Énumérateurs du GC (MEMORY_ARCH.md §16.6) ───────────────────────────────

/// Les énumérateurs voient exactement les objets du pool que tient l'arbre : le
/// recensement égale les octets vivants ajoutés par l'arbre (sans pool : 0 et 0).
#[test]
fn gc_census_matches_tree_bytes() {
    let before = radixox_lib::pool::live_bytes();
    let mut art = OxidArt::new();
    for i in 0..200 {
        art.set(sb(&format!("s:{i}")), Value::String(sb(&"x".repeat(i * 7))));
    }
    art.set(sb("s:"), Value::String(sb("self-val"))); // self-val du nœud "s:"
    art.set(sb("big"), Value::String(sb(&"y".repeat(20_000)))); // externe
    for (k, n) in [("h:small", 3), ("h:big", 30)] {
        let fv: Vec<_> = (0..n).map(|i| (sb(&format!("f{i}")), sb(&format!("v{i}")))).collect();
        art.cmd_hset(k.as_bytes(), &fv, None).unwrap();
    }
    for (k, n) in [("set:small", 3), ("set:big", 30)] {
        let ms: Vec<_> = (0..n).map(|i| sb(&format!("m{i}"))).collect();
        art.cmd_sadd(k.as_bytes(), &ms, None).unwrap();
    }
    art.set(sb("l"), Value::List((0..10).map(|i| sb(&format!("e{i}"))).collect::<VecDeque<_>>()));
    assert!(art.gc_audit_unique().is_empty());
    let census = art.gc_census();
    let total = radixox_lib::pool::verify_census(&census).unwrap();
    assert_eq!(total, radixox_lib::pool::live_bytes() - before);
    if cfg!(not(any(radixox_nopool, radixox_sysalloc))) {
        // 201 chaînes + 2 × (3 + 30) champs et valeurs + 33 membres + 10 éléments, plus les
        // pages des nœuds, les 2 petits blocs Hash / Set et les débordements valart (§17) ;
        // l'égalité stricte porte sur les octets (ci-dessus)
        let mut pages = 0;
        art.map.for_each_occupied(|_, n| pages += (n.page_block() > 0) as usize);
        assert!(census.len() > 201 + 66 + 33 + 10 + pages + 2, "{}", census.len());
    }
    drop(art);
    assert_eq!(radixox_lib::pool::live_bytes(), before);
}

/// Passe de GC forcée : avec le pool, chaque valeur que l'arbre est seul à tenir change
/// d'adresse et garde ses octets ; une valeur tenue ailleurs (rc > 1) reste en place.
#[test]
fn gc_full_pass_moves_values_and_pins_shared() {
    let mut art = OxidArt::new();
    for i in 0..300 {
        art.set(sb(&format!("k:{i}")), Value::String(sb(&format!("valeur-{i}-{}", "z".repeat(i % 50)))));
    }
    let fv: Vec<_> = (0..20).map(|i| (sb(&format!("f{i}")), sb(&format!("v{i}")))).collect();
    art.cmd_hset(b"h", &fv, None).unwrap();
    drop(fv);
    let before: Vec<_> = (0..300).map(|i| str_ptr(&mut art, &format!("k:{i}"))).collect();
    let (hp, _) = hash_ptr(&mut art, "h", "f7");
    let held = art.get(b"k:5"); // rc == 2 pendant la passe
    let stats0 = radixox_lib::pool::gc_stats().unwrap_or_default();
    art.gc_full_pass();
    let stats1 = radixox_lib::pool::gc_stats().unwrap_or_default();
    for (i, (p, v)) in before.iter().enumerate() {
        let (q, w) = str_ptr(&mut art, &format!("k:{i}"));
        assert_eq!(&w, v, "k:{i} : octets");
        if cfg!(not(any(radixox_nopool, radixox_sysalloc))) {
            assert_eq!(q == *p, i == 5, "k:{i} : déplacée sauf l'épinglée");
        }
    }
    assert_eq!(hash_ptr(&mut art, "h", "f7").1, b"v7");
    if cfg!(not(any(radixox_nopool, radixox_sysalloc))) {
        assert_ne!(hash_ptr(&mut art, "h", "f7").0, hp, "valart aussi");
        assert!(stats1.evacuated_objects - stats0.evacuated_objects >= 300 - 1 + 40);
        assert!(stats1.pinned > stats0.pinned);
        assert_eq!(radixox_lib::pool::gc_stats().unwrap().marked_blocks, 0);
    }
    drop(held);
    assert!(radixox_lib::pool::verify_census(&art.gc_census()).is_ok());
}

/// Chemins sur le tas (`CompactPath` > 7 octets, MEMORY_ARCH.md §17) : la passe forcée les
/// déplace aussi, l'arbre reste exact et le recensement les compte.
#[test]
fn gc_full_pass_moves_heap_paths() {
    let mut art = OxidArt::new();
    let keys: Vec<String> =
        (0..2_000).map(|i| format!("{:x}-suffixe-assez-long-{i}", i * 7919)).collect();
    for k in &keys {
        art.set(sb(k), Value::String(sb("v")));
    }
    // préfixes communs longs : nœuds intermédiaires à préfixe sur le tas
    for i in 0..500 {
        art.set(sb(&format!("namespace-commun-tres-long:{i}:fin")), Value::String(sb("w")));
    }
    // conteneurs : petits blocs Hash / Set et débordements valart (objets de structure, §17.4)
    for (k, n) in [("hs", 3), ("hb", 60)] {
        let fv: Vec<_> = (0..n).map(|i| (sb(&format!("champ-{i}")), sb(&format!("valeur-{i}")))).collect();
        art.cmd_hset(k.as_bytes(), &fv, None).unwrap();
    }
    for (k, n) in [("ss", 3), ("sb", 60)] {
        let ms: Vec<_> = (0..n).map(|i| sb(&format!("membre-{i}"))).collect();
        art.cmd_sadd(k.as_bytes(), &ms, None).unwrap();
    }
    let census0 = art.gc_census();
    let s0 = radixox_lib::pool::gc_stats().unwrap_or_default();
    art.gc_full_pass();
    let s1 = radixox_lib::pool::gc_stats().unwrap_or_default();
    for k in &keys {
        assert_eq!(art.get(k.as_bytes()), Some(Value::String(sb("v"))), "{k}");
    }
    for i in 0..500 {
        assert_eq!(art.get(format!("namespace-commun-tres-long:{i}:fin").as_bytes()), Some(Value::String(sb("w"))));
    }
    assert_eq!(art.cmd_hlen(b"hb").unwrap(), 60);
    assert_eq!(art.cmd_hget(b"hb", b"champ-42").unwrap().unwrap().as_slice(), b"valeur-42");
    assert_eq!(art.cmd_hget(b"hs", b"champ-2").unwrap().unwrap().as_slice(), b"valeur-2");
    assert_eq!(art.cmd_scard(b"sb").unwrap(), 60);
    assert_eq!(art.cmd_scard(b"ss").unwrap(), 3);
    assert!(radixox_lib::pool::verify_census(&art.gc_census()).is_ok());
    if radixox_lib::pool::ENABLED {
        // chaque emplacement recensé a changé (tout était évacuable), et les chemins comptent
        let census1 = art.gc_census();
        assert_eq!(census0.len(), census1.len());
        assert!(census0.iter().zip(&census1).all(|(a, b)| a.0 != b.0), "tout a bougé");
        let path_bytes: usize = keys.iter().map(|k| (2 + k.len() + 1) & !1).sum::<usize>() / 2;
        assert!((s1.evacuated_bytes - s0.evacuated_bytes) as usize >= path_bytes);
    }
}

// ─── Compactage de la slab des nœuds (MEMORY_ARCH.md §17.2) ──────────────────

/// 200K clés (dont des TTL), 3/4 supprimées au hasard, compactage par petites tranches :
/// l'arbre reste exact (parents, radix, tags TTL : `check`), la fin de la slab est rendue.
#[test]
fn compaction_moves_nodes_and_releases_tail() {
    let mut rng = Rng(0xC0FFEE);
    let mut art = OxidArt::new();
    art.set_now(1_000);
    let keys: Vec<String> = (0..200_000).map(|_| format!("{:016x}{:08x}", rng.next(), rng.next() as u32)).collect();
    for (i, k) in keys.iter().enumerate() {
        let v = Value::String(sb(&format!("v{i}")));
        if i % 7 == 0 {
            art.set_ttl(sb(k), Duration::from_secs(3600), v);
        } else {
            art.set(sb(k), v);
        }
    }
    let kept: Vec<usize> = (0..keys.len()).filter(|&i| rng.below(4) == 0).collect();
    let keep: std::collections::HashSet<usize> = kept.iter().copied().collect();
    for (i, k) in keys.iter().enumerate() {
        if !keep.contains(&i) {
            art.del(k.as_bytes());
        }
    }
    art.check();
    let holes = art.slab_holes_bytes();
    assert!(holes > 3 << 20, "beaucoup de trous : {holes}");
    let mut c = art.compact_want(false).expect("déclenchement");
    let mut slices = 0;
    let report = loop {
        slices += 1;
        if let Some(r) = art.compact_step(&mut c, 512) {
            break r;
        }
    };
    assert!(slices > 10, "par tranches ({slices})");
    assert!(report.moved > 0 && report.released_bytes >= 2 << 20, "{report:?}");
    art.check();
    // Vidé jusqu'à `arrondi(occupés + 2 Mio de libre gardé, page)` : le reste des trous est
    // ce libre, que les prochaines insertions comblent sans faute de page.
    let page = (2u32 << 20) / 128;
    let last = art.map.last_occupied().unwrap();
    assert!(last < (art.map.len() + page).next_multiple_of(page), "fin vidée : dernier {last}");
    assert!(art.slab_holes_bytes() < holes, "trous rebouchés");
    for &i in &kept {
        assert_eq!(art.get(keys[i].as_bytes()), Some(Value::String(sb(&format!("v{i}")))), "{}", keys[i]);
        if i % 7 == 0 {
            assert!(matches!(art.get_ttl(sb(&keys[i])), crate::TtlResult::KeyWithTtl(_)));
        }
    }
    // L'arbre compacté accepte les écritures (les slots rendus refont des fautes de page).
    for k in keys.iter().take(1000) {
        art.set(sb(k), Value::String(sb("w")));
    }
    art.check();
    // Sous le seuil : rien ne se déclenche ; `force` compacte tout.
    assert!(art.compact_want(false).is_none());
}

/// Chaînes profondes créées en dernier, donc en fin de slab : nœuds et parents voisins,
/// déplacés en même temps par le pipeline (MEMORY_ARCH.md §17.2), dans tous les ordres de
/// validation. Tranches minuscules : le pipeline se vide et repart sans cesse.
#[test]
fn compaction_pipeline_moves_parents_and_children_together() {
    let mut art = OxidArt::new();
    // d'abord du remplissage, ensuite supprimé : les trous sont en bas
    let filler: Vec<String> = (0..60_000).map(|i| format!("f{i:05}x")).collect();
    for k in &filler {
        art.set(sb(k), Value::String(sb("f")));
    }
    // puis des chaînes : chaque clé prolonge la précédente d'un caractère
    let mut chain = Vec::new();
    for root in 0..40 {
        let mut k = format!("chain{root}:");
        for d in 0..60 {
            k.push((b'a' + (d % 26) as u8) as char);
            art.set(sb(&k), Value::String(sb(&format!("{root}-{d}"))));
            chain.push((k.clone(), format!("{root}-{d}")));
        }
    }
    for k in &filler {
        art.del(k.as_bytes());
    }
    let mut c = art.compact_want(true).expect("trous");
    let mut slices = 0;
    while art.compact_step(&mut c, 7).is_none() {
        slices += 1;
    }
    assert!(slices > 100 && c.moved > 1000, "{slices} tranches, {} déplacés", c.moved);
    art.check();
    for (k, v) in &chain {
        assert_eq!(art.get(k.as_bytes()), Some(Value::String(sb(v))), "{k}");
    }
    assert_eq!(art.map.last_occupied().unwrap() + 1, art.map.len(), "slab dense");
}

/// Un dump v2 garde ses ZSet dans la section du début, l'enregistrement de la clé ne
/// porte que l'idx : toujours lisible (et rechargé en grande forme au-delà de 128).
#[test]
fn reload_reads_version_2_zset_section() {
    use crate::save::format;
    use radixox_lib::varint::{put_bytes, put_varint};
    let mut out = format::MAGIC.to_vec();
    out.push(format::VERSION_2);
    out.push(format::TTL_UNIT_SECONDS);
    out.extend_from_slice(&1_000u64.to_le_bytes()); // t0
    out.extend_from_slice(&0u64.to_le_bytes()); // graine
    out.extend_from_slice(&[0; 8 + 20 + 8]); // position du journal
    // section ZSet : deux ZSet, idx 7 (3 membres) et 9 (300 membres)
    put_varint(&mut out, 2);
    put_varint(&mut out, 7);
    put_varint(&mut out, 3);
    for (s, m) in [(2.0f64, "b"), (1.0, "a"), (2.0, "a2")] {
        out.extend_from_slice(&s.to_le_bytes());
        put_bytes(&mut out, m.as_bytes());
    }
    put_varint(&mut out, 9);
    put_varint(&mut out, 300);
    for i in 0..300 {
        out.extend_from_slice(&((i % 10) as f64).to_le_bytes());
        put_bytes(&mut out, format!("m{i:03}").as_bytes());
    }
    put_varint(&mut out, 0); // section List
    for (key, idx) in [("k", 7u64), ("l", 9)] {
        out.push(format::T_ZSET);
        put_varint(&mut out, 0);
        put_varint(&mut out, 1);
        out.extend_from_slice(key.as_bytes());
        put_varint(&mut out, idx);
    }
    out.push(format::T_END);
    let mut back = OxidArt::new();
    back.set_now(1_000);
    back.load_dump(&out).expect("reload v2");
    let names = |r: Vec<SharedByte>| r.iter().map(|b| String::from_utf8(b.to_vec()).unwrap()).collect::<Vec<_>>();
    assert_eq!(names(back.cmd_zrange(b"k", 0, -1, false).unwrap()), ["a", "a2", "b"]);
    assert_eq!(back.cmd_zcard(b"l").unwrap(), 300);
    assert_eq!(names(back.cmd_zrange(b"l", 0, 2, false).unwrap()), ["m000", "m010", "m020"]);
    // et la v3 qu'on réécrit se relit pareil
    let (v3, _) = dump_digest_of(&mut back);
    let (again, _) = reload(&v3, 1_000);
    assert_eq!(expected(&again), expected(&back));
}

/// AMAC, GC et save ne se gênent pas : entre deux tranches de la save, des fenêtres AMAC
/// (indices de clés et chaînes ZSet de la phase 1 bis) mutent les ZSet et des passes
/// complètes du GC déplacent leurs membres et leurs petits blocs. Le dump reste la vue T0,
/// et les réponses avec indice sont celles d'un arbre témoin sans indice ni save.
#[test]
fn amac_and_gc_during_save_keep_t0() {
    use crate::ZProbe;
    let mut rng = Rng(0x5EED_A3AC_0000_0001);
    let mut art = OxidArt::new();
    let mut plain = OxidArt::new();
    art.set_now(1_000);
    plain.set_now(1_000);
    let zkey = |rng: &mut Rng| format!("bigz{}", rng.below(4));
    let zmember = |rng: &mut Rng| format!("member:{}", rng.below(600));
    // une commande ZSet : (op, clé, membre, score)
    type Op = (u8, String, String, f64);
    let apply = |a: &mut OxidArt, (op, k, m, s): &Op| -> String {
        match op {
            0 | 1 => format!("{:?}", a.cmd_zadd(sb(k), &[(*s, sb(m))], None)),
            2 => format!("{:?}", a.cmd_zrem(k.as_bytes(), &[sb(m)])),
            3 => format!("{:?}", a.cmd_zincrby(sb(k), 1.0, sb(m))),
            4 => format!("{:?}", a.cmd_zscore(k.as_bytes(), sb(m))),
            _ => format!("{:?}", a.del(k.as_bytes())),
        }
    };
    let draw = |rng: &mut Rng| -> Op {
        let op = match rng.below(40) {
            0 => 5,
            n => (n % 5) as u8,
        };
        (op, zkey(rng), zmember(rng), rng.below(50) as f64 - 25.0)
    };
    for round in 0..12 {
        for _ in 0..3000 {
            let op = draw(&mut rng);
            assert_eq!(apply(&mut art, &op), apply(&mut plain, &op));
        }
        let expect = expected(&art);
        let mut wrng = Rng(rng.next() | 1);
        let got = save_all(&mut art, || 2, |a| {
            // une fenêtre AMAC de 1 à 32 commandes ZSet
            let ops: Vec<Op> = (0..1 + wrng.below(32)).map(|_| draw(&mut wrng)).collect();
            let keys: Vec<&[u8]> = ops.iter().map(|o| o.1.as_bytes()).collect();
            let mut hints = vec![crate::Hint::NONE; ops.len()];
            a.amac_descend(&keys, 8, &mut hints);
            let zp = |o: &Op| match o.0 {
                0 | 1 => ZProbe::Add(o.3),
                2 | 3 => ZProbe::Tree,
                _ => ZProbe::Index,
            };
            let items: Vec<_> = ops.iter().zip(&hints).map(|(o, h)| (*h, o.2.as_bytes(), zp(o))).collect();
            a.amac_fields(&items, 8);
            a.begin_window();
            let got: Vec<String> = ops.iter().zip(&hints).map(|(o, h)| a.with_hint(o.1.as_bytes(), *h, |a| apply(a, o))).collect();
            a.end_window();
            let want: Vec<String> = ops.iter().map(|o| apply(&mut plain, o)).collect();
            assert_eq!(got, want, "round {round}");
            if wrng.below(6) == 0 {
                a.gc_full_pass();
            }
            a.zset_free_step(32);
        });
        assert_eq!(got, expect, "round {round}");
        assert_eq!(expected(&art), expected(&plain), "round {round}");
        radixox_lib::handle::verify().unwrap();
    }
}
