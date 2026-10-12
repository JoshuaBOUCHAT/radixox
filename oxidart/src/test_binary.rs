//! Clés binaires : tous les octets 0x00–0xFF (y compris `\0`, `\n`, octets non
//! UTF-8), à chaque niveau, jusqu'au palier dense (256 enfants d'un nœud).

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
    let mut keys: Vec<Vec<u8>> = keys;
    keys.sort();
    keys.dedup();
    let atoms: [&[u8]; 16] = [
        b"*", b"?", b"u", b":", b"\x00", b"\xFF", b"\n", b"[a-z]", b"[^\x00]", b"[\x00-\x10]",
        b"[\\]]", b"\\*", b"\\[", b"[*?]", b"[\xFE-\x80]", b"\\\\",
    ];
    let mut nonempty = 0;
    for _ in 0..400 {
        let mut pat = Vec::new();
        for _ in 0..1 + rnd() % 5 {
            pat.extend_from_slice(atoms[rnd() as usize % atoms.len()]);
        }
        let mut got: Vec<Vec<u8>> =
            art.getn_glob(&pat).expect("motif valide").into_iter().map(|(k, _)| k.as_slice().to_vec()).collect();
        got.sort();
        let want: Vec<Vec<u8>> = keys.iter().filter(|k| redis_match(&pat, k)).cloned().collect();
        assert_eq!(got, want, "motif {:?} → regex {}", pat.escape_ascii().to_string(), glob_to_regex(&pat));
        nonempty += !want.is_empty() as u32;
    }
    assert!(nonempty >= 60, "trop peu de motifs qui matchent quelque chose : {nonempty}/400");
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
