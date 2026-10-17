//! Ancien arbre (oxidart-legacy) contre le nouveau, un arbre par processus.
//!
//! ```text
//! cargo run --release -p oxidart --example tree_bench -- <old|new> <mem|speed> <charge> [clés] [ttl] [str100]
//! ```
//! Charges : memtier-seq, memtier-rand, ycsb, session, user-email, fields4,
//! uuid, uuid-bin. `mem` : RSS ajouté par l'arbre (o/clé). `speed` : ns/op
//! pour insertion, mise à jour, GET présent, GET absent, DEL (ordre aléatoire).

use std::hint::black_box;
use std::io::Write;
use std::time::{Duration, Instant};

use radixox_lib::shared_byte::SharedByte;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

fn rss_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

fn workload_key(work: &str, i: u64, rng: &mut StdRng, buf: &mut Vec<u8>) {
    buf.clear();
    match work {
        "memtier-seq" => write!(buf, "memtier-{i}").unwrap(),
        "readme" => write!(buf, "key:{}", i + 1).unwrap(),
        "memtier-rand" => write!(buf, "memtier-{}", rng.gen_range(0..10_000_000u64)).unwrap(),
        "ycsb" => write!(buf, "user{}", rng.r#gen::<u64>() >> 1).unwrap(),
        "session" => {
            let u: u128 = rng.r#gen();
            let h = format!("{u:032x}");
            write!(buf, "session:{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]).unwrap()
        }
        "user-email" => {
            if i % 2 == 0 {
                write!(buf, "user:{}", i / 2).unwrap()
            } else {
                write!(buf, "user:{}:email", i / 2).unwrap()
            }
        }
        "fields4" => {
            let f = ["email", "uuid", "name", "age"][(i % 4) as usize];
            write!(buf, "user:{}:{f}", i / 4).unwrap()
        }
        "uuid" | "uuid-bin" => {
            let mut u: u128 = rng.r#gen();
            u = (u & !(0xF << 76)) | (0x4 << 76);
            u = (u & !(0b11 << 62)) | (0b10 << 62);
            if work == "uuid-bin" {
                buf.extend_from_slice(&u.to_be_bytes());
            } else {
                let h = format!("{u:032x}");
                write!(buf, "{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]).unwrap()
            }
        }
        _ => panic!("charge inconnue {work}"),
    }
}

enum Tree {
    Old(oxidart_legacy::OxidArt),
    New(oxidart::OxidArt),
}

impl Tree {
    fn set(&mut self, k: &SharedByte, v: &SharedByte, i: i64, str_vals: bool, ttl: bool) {
        let d = Duration::from_secs(10_000_000);
        match self {
            Tree::Old(t) => {
                let v = if str_vals { oxidart_legacy::Value::String(v.clone()) } else { oxidart_legacy::Value::Int(i) };
                if ttl { t.set_ttl(k.clone(), d, v) } else { t.set(k.clone(), v) }
            }
            Tree::New(t) => {
                let v = if str_vals { oxidart::Value::String(v.clone()) } else { oxidart::Value::Int(i) };
                if ttl { t.set_ttl(k.clone(), d, v) } else { t.set(k.clone(), v) }
            }
        }
    }
    fn get(&mut self, k: &[u8]) -> bool {
        match self {
            Tree::Old(t) => black_box(t.get(k)).is_some(),
            Tree::New(t) => black_box(t.get(k)).is_some(),
        }
    }
    fn del(&mut self, k: &[u8]) -> bool {
        match self {
            Tree::Old(t) => t.del(k),
            Tree::New(t) => t.del(k),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tree = args.get(1).map_or("new", String::as_str);
    let mode = args.get(2).map_or("mem", String::as_str);
    let work = args.get(3).map_or("ycsb", String::as_str).to_string();
    let n: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let ttl = args.get(5).is_some_and(|s| s == "1");
    let str_vals = args.get(6).is_some_and(|s| s == "str100");

    let mut rng = StdRng::seed_from_u64(7);
    let mut buf = Vec::new();
    // clés distinctes générées d'avance (mémoire mesurée à part : avant l'arbre)
    let mut seen = std::collections::HashSet::new();
    let mut keys: Vec<SharedByte> = Vec::with_capacity(n as usize);
    for i in 0..n {
        workload_key(&work, i, &mut rng, &mut buf);
        if seen.insert(buf.clone()) {
            keys.push(SharedByte::from_slice(&buf));
        }
    }
    drop(seen);
    let len = keys.len();
    // valeurs : chaînes distinctes de 100 o (str100) ou un pool partagé
    // (refcount u16 : jamais plus de quelques centaines de parts par valeur)
    let vals: Vec<SharedByte> = if str_vals {
        (0..len).map(|i| {
            let mut v = [b'x'; 100];
            v[..8].copy_from_slice(&(i as u64).to_le_bytes());
            SharedByte::from_slice(v)
        }).collect()
    } else {
        (0..4096).map(|i| SharedByte::from_slice(format!("value-{i:010}"))).collect()
    };
    let val = |i: usize| &vals[i % vals.len()];

    let mut t = match tree {
        "old" => Tree::Old(oxidart_legacy::OxidArt::new()),
        _ => Tree::New(oxidart::OxidArt::new()),
    };

    if mode == "mem" {
        let before = rss_bytes();
        for (i, k) in keys.iter().enumerate() {
            t.set(k, val(i), i as i64, str_vals, ttl);
        }
        let rss = rss_bytes().saturating_sub(before);
        let nodes = match &t {
            Tree::Old(o) => o.node_count(),
            Tree::New(a) => a.node_count(),
        };
        println!(
            "MEM tree={tree:3} work={work:12} ttl={ttl:5} vals={} keys={len} nodes={nodes} RSS={:.1} o/clé",
            if str_vals { "str100" } else { "int" },
            rss as f64 / len as f64
        );
        return;
    }

    if mode == "pipe" {
        // schéma du serveur en pipeline : fenêtres de 8 commandes, clés
        // préchargées ensemble, puis exécution ; 1:4 SET:GET, valeurs 100 o
        let window: usize = std::env::var("WINDOW").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        let only = std::env::var("ONLY").unwrap_or_default();
        let big: Vec<SharedByte> = (0..4096).map(|i| SharedByte::from_slice(format!("{i:0100}"))).collect();
        for (i, k) in keys.iter().enumerate() {
            t.set(k, &big[i % big.len()], 0, true, false);
        }
        if std::env::var("SMAPS").is_ok() {
            let r = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
            for l in r.lines().filter(|l| l.starts_with("Rss") || l.starts_with("AnonHugePages")) {
                println!("SMAPS {tree} {l}");
            }
        }
        let ops: usize = std::env::var("OPS").ok().and_then(|v| v.parse().ok()).unwrap_or(4_000_000);
        let picks: Vec<(usize, bool)> = (0..ops).map(|_| (rng.gen_range(0..len), rng.gen_range(0..5) == 0)).collect();
        let s = Instant::now();
        for (w, chunk) in picks.chunks(window).enumerate() {
            let refs: Vec<&[u8]> = chunk.iter().map(|&(i, _)| keys[i].as_slice()).collect();
            if only != "noprefetch" {
                match &t {
                    Tree::Old(o) => o.prefetch_keys(&refs),
                    Tree::New(a) => a.prefetch_keys(&refs),
                }
            }
            if only == "prefetch" {
                continue;
            }
            for (j, &(i, is_set)) in chunk.iter().enumerate() {
                if is_set {
                    let v = &big[(w + j) % big.len()];
                    match &mut t {
                        Tree::Old(o) => { o.set_cond(&keys[i], None, oxidart_legacy::Value::String(v.clone()), None); }
                        Tree::New(a) => { a.set_cond(&keys[i], None, oxidart::Value::String(v.clone()), None); }
                    }
                } else {
                    t.get(&keys[i]);
                }
            }
        }
        let el = s.elapsed();
        println!("PIPE tree={tree:3} work={work} window={window} only={only:10} {:.0} ns/op {:.2} Mops/s", el.as_nanos() as f64 / ops as f64, ops as f64 / el.as_secs_f64() / 1e6);
        return;
    }

    let ns = |start: Instant| start.elapsed().as_nanos() as f64 / len as f64;
    let s = Instant::now();
    for (i, k) in keys.iter().enumerate() {
        t.set(k, val(i), i as i64, true, ttl);
    }
    let load = ns(s);

    let mut order: Vec<usize> = (0..len).collect();
    order.shuffle(&mut rng);
    let s = Instant::now();
    for &i in &order {
        t.set(&keys[i], val(i + 1), i as i64, true, ttl);
    }
    let update = ns(s);

    order.shuffle(&mut rng);
    let s = Instant::now();
    let mut hits = 0;
    for &i in &order {
        hits += t.get(&keys[i]) as usize;
    }
    let get_hit = ns(s);
    assert_eq!(hits, len);

    // absentes qui descendent jusqu'en bas : la clé + un octet 0xFF
    let misses: Vec<Vec<u8>> = keys.iter().map(|k| {
        let mut m = k.to_vec();
        m.push(0xFF);
        m
    }).collect();
    let s = Instant::now();
    for &i in &order {
        black_box(t.get(&misses[i]));
    }
    let get_miss = ns(s);

    order.shuffle(&mut rng);
    let s = Instant::now();
    for &i in &order {
        black_box(t.del(&keys[i]));
    }
    let del = ns(s);
    println!(
        "SPEED tree={tree:3} work={work:12} keys={len} load={load:5.0} update={update:5.0} get_hit={get_hit:5.0} get_miss={get_miss:5.0} del={del:5.0} ns/op"
    );
}
