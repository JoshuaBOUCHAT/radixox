//! Nœuds à fort fan-out (clés base64) : load, GET, RSS.
//!
//! `cargo run --release -p oxidart --example b64_bench -- [random|dense]`
//! - random : 1M ids aléatoires de 16 caractères base64 (`s:<id>`)
//! - dense  : espace complet de 3 caractères base64 sous 4 préfixes (1 048 576 clés)

use std::hint::black_box;
use std::time::Instant;

use oxidart::{OxidArt, Value};
use radixox_lib::shared_byte::SharedByte;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn keys(kind: &str) -> Vec<Vec<u8>> {
    match kind {
        "dense" => {
            let mut v = Vec::with_capacity(1 << 20);
            for p in [b"a:", b"b:", b"c:", b"d:"] {
                for i in 0..64 * 64 * 64usize {
                    let mut k = p.to_vec();
                    k.extend([B64[i >> 12], B64[(i >> 6) & 63], B64[i & 63]]);
                    v.push(k);
                }
            }
            // ordre d'insertion mélangé
            let mut x = 0x1234_5678_9ABC_DEF1u64;
            for i in (1..v.len()).rev() {
                v.swap(i, xorshift(&mut x) as usize % (i + 1));
            }
            v
        }
        _ => {
            let mut x = 0x9E37_79B9_7F4A_7C15u64;
            (0..1_000_000)
                .map(|_| {
                    let mut k = b"s:".to_vec();
                    let r = xorshift(&mut x) as u128 | (xorshift(&mut x) as u128) << 64;
                    k.extend((0..16).map(|j| B64[(r >> (6 * j)) as usize & 63]));
                    k
                })
                .collect()
        }
    }
}

fn main() {
    let kind = std::env::args().nth(1).unwrap_or_else(|| "random".into());
    let keys = keys(&kind);
    let n = keys.len();

    let mut art = OxidArt::new();
    let rss0 = rss_kb();
    let t = Instant::now();
    for k in &keys {
        art.set(SharedByte::from_slice(k), Value::String(SharedByte::from_slice(&[b'v'; 16])));
    }
    let load = t.elapsed();
    let rss1 = rss_kb();

    // 4M GET aléatoires sur des clés existantes + 1M GET manqués
    let mut x = 0xDEAD_BEEF_CAFE_F00Du64;
    let t = Instant::now();
    let mut hits = 0u32;
    for _ in 0..4_000_000 {
        let k = &keys[xorshift(&mut x) as usize % n];
        hits += black_box(art.get(k)).is_some() as u32;
    }
    let get_hit = t.elapsed();
    let t = Instant::now();
    let mut miss_key = keys[0].clone();
    let last = miss_key.len() - 1;
    for _ in 0..1_000_000 {
        let r = xorshift(&mut x);
        miss_key[last - 1] = B64[r as usize & 63];
        miss_key[last] = b'!'; // jamais dans l'alphabet : manqué au dernier niveau
        black_box(art.get(&miss_key));
    }
    let get_miss = t.elapsed();
    assert_eq!(hits, 4_000_000);

    println!(
        "{kind:6} n={n} load {:6.1} ns/clé | GET hit {:5.1} ns | GET miss {:5.1} ns | RSS +{:.1} MB ({:.1} o/clé)",
        load.as_nanos() as f64 / n as f64,
        get_hit.as_nanos() as f64 / 4e6,
        get_miss.as_nanos() as f64 / 1e6,
        (rss1 - rss0) as f64 / 1024.0,
        (rss1 - rss0) as f64 * 1024.0 / n as f64,
    );
}
