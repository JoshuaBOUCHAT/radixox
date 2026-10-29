//! Clés binaires : tous les octets 0x00–0xFF (y compris `\0`, `\n`, octets non
//! UTF-8), à chaque niveau, jusqu'au palier dense (256 enfants d'un nœud).

#[allow(unused_imports)]
use crate::prelude::*;
#[allow(unused_imports)]
use std::{dbg, eprintln, print, println, thread_local};

use std::collections::BTreeMap;

use radixox_lib::shared_byte::SharedByte;

use crate::OxidArt;
use crate::krange::RangeVal;
use crate::value::Value;

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut x = seed;
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }
}

fn set(art: &mut OxidArt, k: &[u8], v: &[u8]) {
    art.set(SharedByte::from_slice(k), Value::String(SharedByte::from_slice(v)));
}

fn get(art: &mut OxidArt, k: &[u8]) -> Option<Vec<u8>> {
    art.get(k).and_then(|v| v.as_bytes()).map(|b| b.as_slice().to_vec())
}

fn listing(art: &OxidArt) -> Vec<(Vec<u8>, Vec<u8>)> {
    art.krange(b"", b"", usize::MAX, false)
        .iter()
        .map(|(k, v)| match v {
            RangeVal::Bytes(b) => (k.to_vec(), b.as_slice().to_vec()),
            RangeVal::Int(n) => (k.to_vec(), n.to_string().into_bytes()),
        })
        .collect()
}

/// Clés binaires aléatoires, préfixes partagés, octets « piégeux » surreprésentés.
fn binary_keys(rnd: &mut impl FnMut() -> u64, n: usize) -> Vec<Vec<u8>> {
    let tricky = [0x00u8, 0x01, b'\n', b'\r', b'*', b'?', b'[', b']', b'\\', b'^', b'-', 0x7F, 0x80, 0xC3, 0xFE, 0xFF];
    (0..n)
        .map(|_| {
            let mut k = match rnd() % 3 {
                0 => vec![0x00],
                1 => vec![0xFF, 0x00],
                _ => b"u:".to_vec(),
            };
            let len = rnd() as usize % 6;
            k.extend((0..len).map(|_| {
                if rnd() % 2 == 0 { tricky[rnd() as usize % tricky.len()] } else { rnd() as u8 }
            }));
            k
        })
        .collect()
}

/// Un nœud à 256 enfants (palier dense), plus `\0` et 0xFF en profondeur ;
/// lecture, suppression de la moitié, forme canonique identique à un arbre neuf.
#[test]
fn all_256_radix_under_one_node() {
    let mut art = OxidArt::new();
    let mut model = BTreeMap::new();
    for b in 0..=255u8 {
        for k in [vec![b'k', b], vec![b'k', b, 0x00], vec![b'k', b, 0xFF, b]] {
            let v = format!("v{b}:{}", k.len()).into_bytes();
            set(&mut art, &k, &v);
            model.insert(k, v);
        }
    }
    for (k, v) in &model {
        assert_eq!(get(&mut art, k).as_ref(), Some(v), "clé {:?}", k.escape_ascii().to_string());
    }
    assert_eq!(listing(&art), model.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
    art.assert_canonical();

    let mut rnd = rng(0xB1A5);
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    for k in keys {
        if rnd() % 2 == 0 {
            assert!(art.del(&k));
            model.remove(&k);
        }
    }
    let mut fresh = OxidArt::new();
    for (k, v) in &model {
        set(&mut fresh, k, v);
    }
    assert_eq!(listing(&art), listing(&fresh));
    assert_eq!(art.assert_canonical(), fresh.assert_canonical());
}

/// Ordre lexicographique d'octets non signés (memcmp) dans krange, et
/// pagination sur des clés binaires, contre une BTreeMap.
#[test]
fn krange_binary_order() {
    let mut rnd = rng(0x5EED_0001);
    let mut art = OxidArt::new();
    let mut model = BTreeMap::new();
    for k in binary_keys(&mut rnd, 3000) {
        let v = k.iter().rev().copied().collect::<Vec<u8>>();
        set(&mut art, &k, &v);
        model.insert(k, v);
    }
    let want: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(listing(&art), want);
    for prefix in [&b"\x00"[..], b"\xFF\x00", b"u:", b"u:\x00", b"u:\xFF", b""] {
        for rev in [false, true] {
            let mut from = Vec::new();
            let mut got = Vec::new();
            loop {
                let page: Vec<Vec<u8>> =
                    art.krange(prefix, &from, 41, rev).iter().map(|(k, _)| k.to_vec()).collect();
                let Some(last) = page.last() else { break };
                from = last.clone();
                got.extend(page);
            }
            let mut exp: Vec<Vec<u8>> = model.keys().filter(|k| k.starts_with(prefix)).cloned().collect();
            if rev {
                exp.reverse();
            }
            assert_eq!(got, exp, "prefix {:?} rev {rev}", prefix.escape_ascii().to_string());
        }
    }
    art.assert_canonical();
}

/// Aller-retour save → reload avec des clés binaires.
#[test]
fn save_reload_binary_keys() {
    let mut rnd = rng(0x5EED_0002);
    let mut art = OxidArt::new();
    for k in binary_keys(&mut rnd, 2000) {
        set(&mut art, &k, &k);
    }
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 1024) {}
    drop(cur);
    art.finish_save();

    let mut back = OxidArt::new();
    back.load_dump(&out).expect("reload");
    assert_eq!(listing(&back), listing(&art));
    assert_eq!(back.assert_canonical(), art.assert_canonical());
}

/// Champs de hash et membres de set binaires (arbre valart, propre à lui) :
/// 256 premiers octets différents, `\0`, préfixes communs ; contre une BTreeMap.
#[test]
fn valart_binary_fields_and_members() {
    let mut rnd = rng(0x5EED_0004);
    let mut art = OxidArt::new();
    let mut hash = BTreeMap::new();
    let mut set = std::collections::BTreeSet::new();
    let mut fields: Vec<Vec<u8>> = (0..=255u8).map(|b| vec![b]).collect();
    fields.extend((0..=255u8).map(|b| vec![b, 0x00]));
    fields.extend(binary_keys(&mut rnd, 400));
    for f in &fields {
        let v = f.iter().map(|b| b ^ 0x5A).collect::<Vec<u8>>();
        art.cmd_hset(b"h\x00\xFF", &[(SharedByte::from_slice(f), SharedByte::from_slice(&v))], None).unwrap();
        hash.insert(f.clone(), v);
        art.cmd_sadd(b"s\xFF\x00", &[SharedByte::from_slice(f)], None).unwrap();
        set.insert(f.clone());
    }
    let check = |art: &mut OxidArt, hash: &BTreeMap<Vec<u8>, Vec<u8>>, set: &std::collections::BTreeSet<Vec<u8>>| {
        for (f, v) in hash {
            assert_eq!(art.cmd_hget(b"h\x00\xFF", f).unwrap().map(|b| b.as_slice().to_vec()).as_ref(), Some(v));
        }
        assert_eq!(art.cmd_hlen(b"h\x00\xFF").unwrap() as usize, hash.len());
        let mut all: Vec<Vec<u8>> = art.cmd_smembers(b"s\xFF\x00").unwrap().iter().map(|m| m.as_slice().to_vec()).collect();
        all.sort();
        assert_eq!(all, set.iter().cloned().collect::<Vec<_>>());
        for m in set {
            assert!(art.cmd_sismember(b"s\xFF\x00", SharedByte::from_slice(m)).unwrap());
        }
    };
    check(&mut art, &hash, &set);
    for f in &fields {
        if rnd() % 2 == 0 {
            art.cmd_hdel(b"h\x00\xFF", &[SharedByte::from_slice(f)]).unwrap();
            hash.remove(f);
            art.cmd_srem(b"s\xFF\x00", &[SharedByte::from_slice(f)]).unwrap();
            set.remove(f);
        }
    }
    check(&mut art, &hash, &set);
}

/// TTL et évicteur actif sur des clés dont la radix est 0x00 ou 0xFF (la radix
/// du parent est rangée dans l'octet haut de ExpAndRadix) : tout ce qui expire
/// part, le reste reste lisible, forme canonique ensuite.
#[test]
fn ttl_and_eviction_on_binary_radix() {
    let mut art = OxidArt::new();
    art.set_now(1_000);
    let mut live = BTreeMap::new();
    for b in 0..=255u8 {
        for (i, k) in [vec![b], vec![0x00, b], vec![0xFF, b, 0x00], vec![0xFF, b, 0xFF]].into_iter().enumerate() {
            let v = SharedByte::from_slice(&k);
            if (b as usize + i) % 3 == 0 {
                art.set(SharedByte::from_slice(&k), Value::String(v));
                live.insert(k.clone(), k);
            } else {
                art.set_ttl(SharedByte::from_slice(&k), std::time::Duration::from_secs(5), Value::String(v));
            }
        }
    }
    art.set_now(1_000 + 60_000);
    while art.evict_expired() > 0 {}
    // ce que l'évicteur n'a pas tiré au sort part par suppression à la volée
    let all: Vec<Vec<u8>> = (0..=255u8)
        .flat_map(|b| [vec![b], vec![0x00, b], vec![0xFF, b, 0x00], vec![0xFF, b, 0xFF]])
        .collect();
    for k in &all {
        assert_eq!(get(&mut art, k).as_ref(), live.get(k), "clé {:?}", k.escape_ascii().to_string());
    }
    let mut fresh = OxidArt::new();
    for (k, v) in &live {
        set(&mut fresh, k, v);
    }
    assert_eq!(listing(&art), listing(&fresh));
    assert_eq!(art.assert_canonical(), fresh.assert_canonical());
}

/// Clés binaires longues (compressions sur le tas) qui partagent de longs
/// préfixes et ne diffèrent que d'un octet, y compris en fin de clé.
#[test]
fn long_binary_keys() {
    let mut rnd = rng(0x5EED_0005);
    let mut art = OxidArt::new();
    let mut model = BTreeMap::new();
    let base: Vec<u8> = (0..700).map(|_| rnd() as u8).collect();
    for _ in 0..600 {
        let cut = rnd() as usize % base.len();
        let mut k = base[..cut].to_vec();
        k.push(rnd() as u8);
        k.extend((0..rnd() % 40).map(|_| rnd() as u8));
        set(&mut art, &k, &k[..k.len().min(5)]);
        model.insert(k.clone(), k[..k.len().min(5)].to_vec());
    }
    assert_eq!(listing(&art), model.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
    art.assert_canonical();
    let mut out = Vec::new();
    let mut cur = art.save_start(&mut out);
    while !art.save_step(&mut cur, &mut out, 64) {}
    drop(cur);
    art.finish_save();
    let mut back = OxidArt::new();
    back.load_dump(&out).expect("reload");
    assert_eq!(listing(&back), listing(&art));
}

// ─── Globs (KEYS) ────────────────────────────────────────────────────────────

#[cfg(feature = "regex")]
mod glob {
use super::*;
use crate::regex::{GlobKind, glob_kind, glob_to_regex};

/// Référence : `stringmatchlen` de Redis, sans l'option nocase.
fn redis_match(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| redis_match(rest, &s[i..])),
        Some((b'?', rest)) => !s.is_empty() && redis_match(rest, &s[1..]),
        Some((b'[', _)) => {
            let Some((&c, srest)) = s.split_first() else { return false };
            let mut i = 1;
            let not = p.get(i) == Some(&b'^');
            if not {
                i += 1;
            }
            let mut hit = false;
            loop {
                if i >= p.len() {
                    break; // classe non fermée : fermée en fin de motif
                }
                if p[i] == b']' {
                    i += 1;
                    break;
                }
                if p[i] == b'\\' && i + 1 < p.len() {
                    i += 1;
                    hit |= p[i] == c;
                } else if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                    let mut j = i + 2;
                    if p[j] == b'\\' && j + 1 < p.len() {
                        j += 1;
                    }
                    let (a, b) = if p[i] <= p[j] { (p[i], p[j]) } else { (p[j], p[i]) };
                    hit |= a <= c && c <= b;
                    i = j;
                } else {
                    hit |= p[i] == c;
                }
                i += 1;
            }
            hit != not && redis_match(&p[i..], srest)
        }
        Some((b'\\', rest)) if !rest.is_empty() => {
            s.first() == Some(&rest[0]) && redis_match(&rest[1..], &s[1..])
        }
        Some((&b, rest)) => s.first() == Some(&b) && redis_match(rest, &s[1..]),
    }
}

/// getn_glob == filtre par la référence Redis, sur des clés binaires et des
/// motifs tirés au hasard (classes, intervalles, négations, échappements,
/// octets non UTF-8, `\n`).
#[test]
fn glob_matches_redis_reference() {
    let mut rnd = rng(0x5EED_0003);
    let mut art = OxidArt::new();
    let keys = binary_keys(&mut rnd, 1500);
    for k in &keys {
        set(&mut art, k, b"x");
    }
    // clés faites de métacaractères, pour que les motifs littéraux aient de quoi matcher
    let mut keys: Vec<Vec<u8>> = keys;
    for k in [&b"u:.+()|{2}$^"[..], b"u:[&&]", b"u:\\", b"", b"u:*?[]"] {
        set(&mut art, k, b"x");
        keys.push(k.to_vec());
    }
    keys.sort();
    keys.dedup();
    // métacaractères regex hors classe (doivent rester littéraux), opérations
    // ensemblistes de regex-syntax dans une classe (`&&`, `--`, `~~`, `[` imbriqué),
    // `\` en fin de motif et en fin de classe non fermée
    let atoms: [&[u8]; 30] = [
        b"*", b"?", b"u", b":", b"\x00", b"\xFF", b"\n", b"[a-z]", b"[^\x00]", b"[\x00-\x10]",
        b"[\\]]", b"\\*", b"\\[", b"[*?]", b"[\xFE-\x80]", b"\\\\",
        b".", b"+", b"(", b")", b"|", b"{2}", b"$", b"^",
        b"[u&&:]", b"[u--:]", b"[u~~:]", b"[[]", b"[a[b]", b"[^\\",
    ];
    let mut nonempty = 0;
    for _ in 0..1000 {
        let mut pat = Vec::new();
        for _ in 0..1 + rnd() % 5 {
            pat.extend_from_slice(atoms[rnd() as usize % atoms.len()]);
        }
        if rnd() % 10 == 0 {
            pat.push(b'\\'); // `\` final
        }
        let mut got: Vec<Vec<u8>> =
            art.getn_glob(&pat).expect("motif valide").into_iter().map(|(k, _)| k.as_slice().to_vec()).collect();
        got.sort();
        let want: Vec<Vec<u8>> = keys.iter().filter(|k| redis_match(&pat, k)).cloned().collect();
        assert_eq!(got, want, "motif {:?} → regex {}", pat.escape_ascii().to_string(), glob_to_regex(&pat));
        let direct: Vec<Vec<u8>> = keys.iter().filter(|k| crate::regex::glob_match(&pat, k)).cloned().collect();
        assert_eq!(direct, want, "glob_match, motif {:?}", pat.escape_ascii().to_string());
        nonempty += !want.is_empty() as u32;
    }
    assert!(nonempty >= 100, "trop peu de motifs qui matchent quelque chose : {nonempty}/1000");
}

/// `*` matche aussi une clé contenant `\n` (bug corrigé : le `.` des regex ne
/// matchait pas `\n` par défaut) ; cas limites des classes.
#[test]
fn glob_edge_cases() {
    let mut art = OxidArt::new();
    for k in [&b"a\nb"[..], b"a\xFFb", b"a\x00b", b"a]b", b"a-b", b"ab"] {
        set(&mut art, k, b"1");
    }
    let keys: Vec<&[u8]> = vec![b"a\nb", b"a\xFFb", b"a\x00b", b"a]b", b"a-b", b"ab"];
    // chaque cas vérifié aussi contre la référence Redis
    let count = |art: &OxidArt, p: &[u8]| {
        let n = art.getn_glob(p).unwrap().len();
        assert_eq!(n, keys.iter().filter(|k| redis_match(p, k)).count(), "référence, motif {:?}", p.escape_ascii().to_string());
        n
    };
    assert_eq!(count(&art, b"*"), 6);
    assert_eq!(count(&art, b"a?b"), 5); // un octet quelconque, \n compris
    assert_eq!(count(&art, b"a[]b"), 0); // classe vide : rien
    assert_eq!(count(&art, b"a[^]b"), 5); // négation vide : n'importe quel octet
    assert_eq!(count(&art, b"a[\\]]b"), 1); // `]` échappé dans une classe
    assert_eq!(count(&art, b"a[b-a]"), 1); // intervalle inversé = [a-b] : « ab »
    assert_eq!(count(&art, b"a[-]b"), 1); // `-` seul : littéral
    assert_eq!(count(&art, b"a[\x00-\x7F]b"), 4); // \n, \0, ], -
}

/// La clé vide : `*` la renvoie (chemin préfixe et chemin DFA), le motif vide
/// ne renvoie qu'elle.
#[test]
fn glob_empty_key() {
    let mut art = OxidArt::new();
    set(&mut art, b"", b"vide");
    set(&mut art, b"a", b"1");
    let keys = |v: Vec<(SharedByte, Value)>| {
        let mut k: Vec<Vec<u8>> = v.into_iter().map(|(k, _)| k.as_slice().to_vec()).collect();
        k.sort();
        k
    };
    assert_eq!(keys(art.getn_glob(b"*").unwrap()), vec![b"".to_vec(), b"a".to_vec()]);
    assert_eq!(keys(art.getn(SharedByte::from_slice(b""))), vec![b"".to_vec(), b"a".to_vec()]);
    assert_eq!(keys(art.getn_glob(b"").unwrap()), vec![b"".to_vec()]);
    assert_eq!(keys(art.getn_glob(b"?").unwrap()), vec![b"a".to_vec()]);
}

/// Un motif dont la DFA exploserait (`*a` suivi de N `?` : ~2^N états) doit
/// échouer vite au lieu de figer ou d'avaler la mémoire du serveur.
#[test]
fn glob_dfa_blowup_is_refused() {
    let mut art = OxidArt::new();
    set(&mut art, b"abc", b"1");
    let mut pat = b"*a".to_vec();
    pat.extend([b'?'; 40]);
    let t = std::time::Instant::now();
    assert!(art.getn_glob(&pat).is_err(), "motif pathologique accepté");
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "refus trop lent : {:?}", t.elapsed());
    // des motifs réalistes, même chargés, passent toujours sous la limite
    assert!(art.getn_glob(b"*a??").is_ok());
    for p in [
        &b"*user:*:[a-z]*:admin:?*:x[0-9][0-9]*"[..],
        b"tenant:*:session:[0-9a-f][0-9a-f]*:*:token:*",
        b"*[^:]:*:*:*:*:*:*:*:*:end",
        b"log:20??-??-??T??:??:??*",
    ] {
        assert!(art.getn_glob(p).is_ok(), "motif réaliste refusé : {:?}", p.escape_ascii().to_string());
    }
}

/// `getn_regex` est en mode octets : une lettre non ASCII du motif désigne ses
/// octets UTF-8, et `\xFF` un octet brut.
#[test]
fn regex_non_ascii_literal() {
    let mut art = OxidArt::new();
    set(&mut art, "é1".as_bytes(), b"1");
    set(&mut art, b"\xFF1", b"2");
    let n = |art: &OxidArt, re: &str| art.getn_regex(re).map(|v| v.len());
    assert_eq!(n(&art, "^é.$").ok(), Some(1), "{:?}", n(&art, "^é.$"));
    assert_eq!(n(&art, r"^\xFF.$").ok(), Some(1));
    assert_eq!(n(&art, r"^..1$").ok(), Some(1)); // « é » = 2 octets
}

#[test]
fn glob_kind_classifies() {
    assert_eq!(glob_kind(b"user:1"), GlobKind::Exact(b"user:1"));
    assert_eq!(glob_kind(b""), GlobKind::Exact(b""));
    assert_eq!(glob_kind(b"user:*"), GlobKind::Prefix(b"user:"));
    assert_eq!(glob_kind(b"*"), GlobKind::Prefix(b""));
    assert_eq!(glob_kind(b"\x00\xFF*"), GlobKind::Prefix(b"\x00\xFF"));
    assert_eq!(glob_kind(b"user\\*"), GlobKind::Pattern); // `*` échappé : pas un préfixe
    assert_eq!(glob_kind(b"user:*:x"), GlobKind::Pattern);
    assert_eq!(glob_kind(b"user:**"), GlobKind::Pattern);
    assert_eq!(glob_kind(b"u?er"), GlobKind::Pattern);
}
}
