//! KRANGE / KREVRANGE : pages de `count` entrées depuis un `from` aléatoire
//! sur 1M clés, comparées à `count` GET sur les mêmes clés.
//!
//! `cargo run --release -p oxidart --example krange_bench -- [count]`

use std::hint::black_box;
use std::time::Instant;

use oxidart::krange::RangeVal;
use oxidart::{OxidArt, Value};
use radixox_lib::shared_byte::SharedByte;

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn main() {
    let count: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(64);
    let n = 1_000_000;
    let pages = 200_000;

    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let keys: Vec<Vec<u8>> = (0..n).map(|_| format!("msg:{}", xorshift(&mut x) % 100_000_000).into_bytes()).collect();
    let mut art = OxidArt::new();
    for k in &keys {
        art.set(SharedByte::from_slice(k), Value::String(SharedByte::from_slice([b'v'; 100])));
    }

    for rev in [false, true] {
        let name = if rev { "KREVRANGE" } else { "KRANGE" };
        let froms: Vec<&[u8]> = (0..pages).map(|_| keys[xorshift(&mut x) as usize % n].as_slice()).collect();

        let mut entries = 0usize;
        let t = Instant::now();
        for from in &froms {
            let r = art.krange(b"msg:", from, count, rev);
            // lecture des valeurs, comme l'encodage RESP
            for (k, v) in r.iter() {
                if let RangeVal::Bytes(b) = v {
                    black_box((k.len(), b.as_slice()[0]));
                }
            }
            entries += r.len();
        }
        let dt = t.elapsed();

        // Version possédée (SharedByte, refcount +1, prefetch en avance).
        let t = Instant::now();
        let mut owned_entries = 0usize;
        for from in &froms {
            let v = art.krange_owned(b"msg:", from, count, rev);
            owned_entries += black_box(&v).len();
        }
        let dt_owned = t.elapsed();
        assert_eq!(owned_entries, entries);

        // Référence : les mêmes clés, une par une depuis la racine.
        let page_keys: Vec<Vec<Vec<u8>>> = froms
            .iter()
            .take(pages / 4)
            .map(|f| art.krange(b"msg:", f, count, rev).iter().map(|(k, _)| k.to_vec()).collect())
            .collect();
        let t = Instant::now();
        let mut got = 0usize;
        for page in &page_keys {
            for k in page {
                if let Some(v) = art.get(k) {
                    got += black_box(v.as_bytes().map(|b| b.as_slice()[0] as usize).unwrap_or(0)).min(1);
                }
            }
        }
        let dt_get = t.elapsed();
        let get_entries: usize = page_keys.iter().map(Vec::len).sum();
        assert_eq!(got, get_entries);

        println!(
            "{name:9} count={count}: réf {:6.0} ns/page ({:5.1} ns/entrée) | owned {:6.0} ns/page ({:5.1} ns/entrée) | {count}×GET mêmes clés : {:6.0} ns/page ({:5.1} ns/entrée)",
            dt.as_nanos() as f64 / pages as f64,
            dt.as_nanos() as f64 / entries as f64,
            dt_owned.as_nanos() as f64 / pages as f64,
            dt_owned.as_nanos() as f64 / entries as f64,
            dt_get.as_nanos() as f64 / (pages / 4) as f64,
            dt_get.as_nanos() as f64 / get_entries as f64,
        );
    }
}
