//! MGET : ancien chemin (get clé par clé + clone) vs `mget_ref` / `mget_owned`
//! (descentes entrelacées, valeurs par référence), sur 1M clés aléatoires.
//!
//! `cargo run --release -p oxidart --example mget_bench -- [n_keys_par_mget]`

use std::hint::black_box;
use std::time::Instant;

use oxidart::krange::{OwnedVal, RangeVal};
use oxidart::{OxidArt, Value};
use radixox_lib::shared_byte::SharedByte;

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn main() {
    let batch: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(64);
    let n = 1_000_000;
    let calls = 2_000_000 / batch.max(1);

    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let keys: Vec<Vec<u8>> = (0..n).map(|_| format!("msg:{}", xorshift(&mut x) % 100_000_000).into_bytes()).collect();
    let mut art = OxidArt::new();
    for k in &keys {
        art.set(SharedByte::from_slice(k), Value::String(SharedByte::from_slice([b'v'; 100])));
    }
    let picks: Vec<Vec<&[u8]>> = (0..calls)
        .map(|_| (0..batch).map(|_| keys[xorshift(&mut x) as usize % n].as_slice()).collect())
        .collect();

    // Ancien MGET : get (clone de Value) puis as_bytes (re-clone), une clé après l'autre.
    let t = Instant::now();
    for ks in &picks {
        let v: Vec<Option<SharedByte>> = ks.iter().map(|k| art.get(k).and_then(|v| v.as_bytes())).collect();
        black_box(v.iter().map(|v| v.as_ref().map_or(0, |b| b.as_slice()[0] as usize)).sum::<usize>());
    }
    let old = t.elapsed();

    let t = Instant::now();
    for ks in &picks {
        let mut expired = Vec::new();
        let v = art.mget_ref(ks, &mut expired);
        black_box(v.iter().map(|v| match v {
            Some(RangeVal::Bytes(b)) => b.as_slice()[0] as usize,
            _ => 0,
        }).sum::<usize>());
    }
    let new_ref = t.elapsed();

    let t = Instant::now();
    for ks in &picks {
        let v = art.mget_owned(ks);
        black_box(v.iter().map(|v| match v {
            Some(OwnedVal::Bytes(b)) => b.as_slice()[0] as usize,
            _ => 0,
        }).sum::<usize>());
    }
    let new_owned = t.elapsed();

    let per = |d: std::time::Duration| d.as_nanos() as f64 / (calls * batch) as f64;
    println!(
        "MGET {batch:3} clés : ancien {:6.1} ns/clé | mget_ref {:6.1} ns/clé ({:.2}×) | mget_owned {:6.1} ns/clé ({:.2}×)",
        per(old),
        per(new_ref),
        per(old) / per(new_ref),
        per(new_owned),
        per(old) / per(new_owned),
    );
}
