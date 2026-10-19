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
    drop(cur);
    art.finish_save();

    let mut file = body.clone();
    file.extend_from_slice(&format::footer(crc32fast::hash(&body)));
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
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 1024) {}
    drop(cur);
    art.finish_save();
    out
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
    let mut file = dump_of(art);
    let crc = crc32fast::hash(&file);
    file.extend_from_slice(&crate::save::format::footer(crc));
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
