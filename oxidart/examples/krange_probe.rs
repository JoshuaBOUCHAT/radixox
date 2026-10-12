//! Sonde pour `perf stat` : charge 1M clés puis exécute UNE seule opération
//! en boucle, pour comparer les compteurs par opération.
//!
//! `krange_probe <none|krange|get> [ops]`  (none = charge seule, à soustraire)

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
    let mode = std::env::args().nth(1).unwrap_or_else(|| "none".into());
    let ops: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(2_000_000);
    let n = 1_000_000;
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let keys: Vec<Vec<u8>> = (0..n).map(|_| format!("msg:{}", xorshift(&mut x) % 100_000_000).into_bytes()).collect();
    let mut art = OxidArt::new();
    for k in &keys {
        art.set(SharedByte::from_slice(k), Value::String(SharedByte::from_slice([b'v'; 100])));
    }
    let picks: Vec<u32> = (0..ops).map(|_| (xorshift(&mut x) % n as u64) as u32).collect();

    let t = Instant::now();
    match mode.as_str() {
        "krange" => {
            for &i in &picks {
                black_box(art.krange(b"msg:", &keys[i as usize], 1, false).len());
            }
        }
        "get" => {
            for &i in &picks {
                black_box(art.get(&keys[i as usize]).is_some());
            }
        }
        _ => {}
    }
    eprintln!("{mode}: {:.0} ns/op", t.elapsed().as_nanos() as f64 / ops as f64);
}
