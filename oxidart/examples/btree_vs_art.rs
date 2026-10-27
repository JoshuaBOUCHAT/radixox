//! OxidArt contre BTreeMap<String, String> (et HashMap en repère), clé →
//! valeur string, en process : insertion, lecture, page triée, suppression,
//! mémoire. Un process par (structure, forme de clés) pour que la RSS ne
//! garde rien du run précédent.
//!
//! `cargo run --release -p oxidart --example btree_vs_art -- <btree|hash|art> <seq|uuid> [n]`

use std::collections::{BTreeMap, HashMap};
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

fn rss_mb() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: f64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096.0 / 1e6
}

/// Ce qu'un encodage de réponse lit d'une entrée. `TOUCH=0` : longueurs
/// seulement (une `String` les a dans son en-tête, un `SharedByte` sur le tas).
fn touch(k: &[u8], v: &[u8]) -> usize {
    static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *FULL.get_or_init(|| std::env::var("TOUCH").map_or(true, |v| v != "0")) {
        k.len() + v.len() + k[k.len() - 1] as usize + v[v.len() - 1] as usize
    } else {
        k.len() + v.len()
    }
}

/// Opérations communes aux trois structures.
trait Map {
    fn insert(&mut self, k: &str, v: &str);
    fn get_len(&mut self, k: &str) -> usize;
    /// `count` entrées strictement après `from`, en ordre ; somme des longueurs.
    fn range_len(&mut self, from: &str, count: usize) -> usize;
    fn remove(&mut self, k: &str) -> bool;
}

impl Map for BTreeMap<String, String> {
    fn insert(&mut self, k: &str, v: &str) {
        BTreeMap::insert(self, k.to_owned(), v.to_owned());
    }
    fn get_len(&mut self, k: &str) -> usize {
        self.get(k).map_or(0, String::len)
    }
    fn range_len(&mut self, from: &str, count: usize) -> usize {
        use std::ops::Bound::{Excluded, Unbounded};
        self.range::<str, _>((Excluded(from), Unbounded)).take(count).map(|(k, v)| touch(k.as_bytes(), v.as_bytes())).sum()
    }
    fn remove(&mut self, k: &str) -> bool {
        BTreeMap::remove(self, k).is_some()
    }
}

impl Map for HashMap<String, String> {
    fn insert(&mut self, k: &str, v: &str) {
        HashMap::insert(self, k.to_owned(), v.to_owned());
    }
    fn get_len(&mut self, k: &str) -> usize {
        self.get(k).map_or(0, String::len)
    }
    fn range_len(&mut self, _: &str, _: usize) -> usize {
        0 // pas d'ordre
    }
    fn remove(&mut self, k: &str) -> bool {
        HashMap::remove(self, k).is_some()
    }
}

impl Map for OxidArt {
    fn insert(&mut self, k: &str, v: &str) {
        self.set(SharedByte::from_slice(k.as_bytes()), Value::String(SharedByte::from_slice(v.as_bytes())));
    }
    fn get_len(&mut self, k: &str) -> usize {
        self.get(k.as_bytes()).and_then(|v| v.as_bytes().map(|b| b.len())).unwrap_or(0)
    }
    fn range_len(&mut self, from: &str, count: usize) -> usize {
        self.krange(b"", from.as_bytes(), count, false)
            .iter()
            .map(|(k, v)| match v {
                oxidart::krange::RangeVal::Bytes(b) => touch(k, b.as_slice()),
                oxidart::krange::RangeVal::Int(_) => touch(k, b"12345678"),
            })
            .sum()
    }
    fn remove(&mut self, k: &str) -> bool {
        self.del(k.as_bytes())
    }
}

fn run(name: &str, mut m: impl Map, keys: &[String], ranged: bool) {
    let n = keys.len();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let vals: Vec<String> = (0..n).map(|i| format!("value-{i:014}")).collect(); // 20 o
    let lookups: Vec<usize> = (0..n).map(|_| xorshift(&mut x) as usize % n).collect();
    let misses: Vec<String> = (0..n / 4).map(|i| format!("{}#miss", keys[i])).collect();

    let rss0 = rss_mb();
    let t = Instant::now();
    for (k, v) in keys.iter().zip(&vals) {
        m.insert(k, v);
    }
    let insert = t.elapsed().as_nanos() as f64 / n as f64;
    let rss = rss_mb() - rss0;

    let t = Instant::now();
    let mut acc = 0;
    for &i in &lookups {
        acc += m.get_len(&keys[i]);
    }
    let get = t.elapsed().as_nanos() as f64 / n as f64;
    assert_eq!(acc, 20 * n);

    let t = Instant::now();
    for k in &misses {
        acc += m.get_len(k);
    }
    let miss = t.elapsed().as_nanos() as f64 / misses.len() as f64;

    let range = if ranged {
        let pages = 100_000;
        let t = Instant::now();
        for &i in &lookups[..pages] {
            acc += m.range_len(&keys[i], 64);
        }
        format!("{:7.0}", t.elapsed().as_nanos() as f64 / pages as f64)
    } else {
        format!("{:>7}", "—")
    };

    let t = Instant::now();
    for k in keys {
        assert!(m.remove(k));
    }
    let del = t.elapsed().as_nanos() as f64 / n as f64;
    black_box(acc);

    println!(
        "{name:8} {:6.0} {:6.0} {:6.0} {range} {:6.0} {:7.0} MB",
        insert, get, miss, del, rss
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let which = args.get(1).map(String::as_str).unwrap_or("art");
    let shape = args.get(2).map(String::as_str).unwrap_or("seq");
    let n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);

    // Insertion dans un ordre aléatoire dans les deux cas.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut keys: Vec<String> = match shape {
        "seq" => (0..n).map(|i| format!("user:{i}")).collect(),
        _ => (0..n)
            .map(|_| format!("{:016x}{:016x}", xorshift(&mut x), xorshift(&mut x)))
            .collect(),
    };
    for i in (1..n).rev() {
        keys.swap(i, xorshift(&mut x) as usize % (i + 1));
    }

    if args.iter().any(|a| a == "--header") {
        println!(
            "{:8} {:>6} {:>6} {:>6} {:>7} {:>6} {:>10}",
            "", "insert", "get", "miss", "page64", "del", "mémoire"
        );
        return;
    }
    // `range-only` : pages seules (1M pages après remplissage), pour le profilage.
    if args.iter().any(|a| a == "--range-only") {
        fn pages(m: &mut impl Map, keys: &[String]) -> (f64, usize) {
            for (i, k) in keys.iter().enumerate() {
                m.insert(k, &format!("value-{i:014}"));
            }
            let mut acc = 0;
            let t = Instant::now();
            let page: usize = std::env::var("PAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
            let n_pages: usize = std::env::var("PAGES").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
            for i in 0..n_pages {
                acc += m.range_len(&keys[(i * 7919) % keys.len()], page);
            }
            (t.elapsed().as_nanos() as f64 / n_pages as f64, acc)
        }
        let (ns, acc) = match which {
            "btree" => pages(&mut BTreeMap::<String, String>::new(), &keys),
            _ => pages(&mut OxidArt::new(), &keys),
        };
        println!("range-only {which} {shape} n={n} : {ns:.0} ns/page ({acc})");
        return;
    }
    match which {
        "btree" => run("BTreeMap", BTreeMap::<String, String>::new(), &keys, true),
        "hash" => run("HashMap", HashMap::<String, String>::new(), &keys, false),
        _ => run("OxidArt", OxidArt::new(), &keys, true),
    }
}
