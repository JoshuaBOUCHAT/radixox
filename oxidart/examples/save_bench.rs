//! Débit de la save (DFS + sérialisation binaire), sans I/O.
//!
//! `cargo run --release -p oxidart --example save_bench -- [N] [string|hash] [budget]`

use std::time::Instant;

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(0)
        / 1024
}

use oxidart::{OxidArt, Value, parse_dump};
use radixox_lib::shared_byte::SharedByte;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5_000_000);
    let kind = args.get(2).map(String::as_str).unwrap_or("string");
    let budget: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1024);

    let mut art = OxidArt::new();
    // Ordre d'insertion mélangé : les idx du slab ne suivent pas l'ordre DFS.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let t = Instant::now();
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let key = format!("user{}", x % (n * 4));
        // Une valeur distincte par clé (une SharedByte partagée par toutes
        // les clés n'est pas un cas réaliste).
        let val = SharedByte::from_slice([b'x'; 100]);
        match kind {
            "hash" => {
                let _ = art.cmd_hset(key.as_bytes(), &[(SharedByte::from_str("field0"), val)], None);
            }
            _ => art.set(SharedByte::from_str(&key), Value::String(val)),
        }
    }
    eprintln!("load : {n} insertions {kind} en {:.2?}, RSS {} MB", t.elapsed(), rss_mb());

    let mut out = Vec::new();
    let keys = art.node_count(); // approximation pour le ns/clé, sans getn
    let t = Instant::now();
    let mut cur = art.save_start(&mut out);
    let mut steps = 0u64;
    while !art.save_step(&mut cur, &mut out, budget) {
        steps += 1;
        if steps % 100_000 == 0 {
            eprintln!("  {steps} tranches, dump {} MB, RSS {} MB", out.len() >> 20, rss_mb());
        }
    }
    let dt = t.elapsed();
    art.finish_save();
    println!(
        "save : {keys} clés, {:.1} MB, {:.2?} → {:.1} ns/clé, {:.0} MB/s, {steps} tranches (budget {budget})",
        out.len() as f64 / 1e6,
        dt,
        dt.as_nanos() as f64 / keys as f64,
        out.len() as f64 / 1e6 / dt.as_secs_f64()
    );
    eprintln!("RSS après save {} MB", rss_mb());
    // Vérification complète seulement sur les petits dumps.
    if n <= 200_000 {
        let dump = parse_dump(&out).expect("dump illisible");
        assert_eq!(dump.entries.len(), art.getn(SharedByte::from_str("")).len());
    }
}
