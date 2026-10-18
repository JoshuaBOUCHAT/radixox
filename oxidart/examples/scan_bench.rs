//! SCAN (`scan_keys`) : scan complet de 1M clés par pages de `count`, clés
//! courtes (suffixes inline) puis longues (suffixes de feuille sur le tas).
//!
//! `cargo run --release -p oxidart --example scan_bench -- [count]`

use std::hint::black_box;
use std::time::Instant;

use oxidart::{OxidArt, Value};
use radixox_lib::shared_byte::SharedByte;

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn main() {
    let count: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let n = 1_000_000;
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let shapes: [(&str, Box<dyn Fn(u64) -> String>); 2] = [
        ("courtes  msg:N", Box::new(|r| format!("msg:{}", r % 100_000_000))),
        ("longues  session:<24 hex>", Box::new(|r| format!("session:{:016x}{:08x}", r, r >> 7 & 0xFFFF_FFFF))),
    ];
    for (name, key) in shapes {
        // insertion dans un ordre aléatoire : allocations dispersées
        let mut art = OxidArt::new();
        for _ in 0..n {
            art.set(SharedByte::from_slice(key(xorshift(&mut x)).as_bytes()), Value::Int(1));
        }
        for (label, pattern) in [("tout", None), ("MATCH *7", Some(&b"*7"[..]))] {
            let mut best = f64::MAX;
            let mut total = 0;
            for _ in 0..3 {
                let t = Instant::now();
                let mut from = Vec::new();
                total = 0;
                loop {
                    let page = art.scan_keys(b"", &from, count, count.max(1000), |k, _| {
                        pattern.is_none_or(|p| oxidart::regex::glob_match(p, k))
                    });
                    for k in page.iter() {
                        black_box(k.len());
                    }
                    total += page.len();
                    match page.next {
                        Some(next) => from = next,
                        None => break,
                    }
                }
                best = best.min(t.elapsed().as_nanos() as f64);
            }
            println!(
                "{name:28} {label:9} count={count}: {total:7} clés, {:6.1} ms, {:5.1} ns/clé de l'arbre",
                best / 1e6,
                best / art.len() as f64
            );
        }
    }
}
