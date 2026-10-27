//! Bench comparatif Hash : valart vs HashMap (SipHash et foldhash) vs BTreeMap.
//!
//! ```text
//! cargo test --release -p valart --test bench_hash -- --ignored --nocapture
//! ```
//!
//! Champs `field:{0..N}` et valeurs pré-générés en `SharedByte` : chaque structure reçoit un
//! `clone()` (bump de refcount, pas d'allocation), donc le coût d'allocation
//! des clés est hors mesure pour tout le monde.
//!
//! Mémoire : allocateur global instrumenté au-dessus de l'allocateur système (octets
//! demandés, sans l'arrondi aux classes de taille), mesuré après la phase
//! d'insertion. Les clés sont partagées et allouées via l'allocateur système en direct :
//! hors compte. Pour valart on ajoute le slab (mmap) et les overflows (allocateur système
//! en direct), tous deux invisibles pour l'allocateur global.

use std::alloc::{GlobalAlloc, Layout, System};

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::hint::black_box;
use std::time::{Duration, Instant};

use radixox_lib::shared_byte::SharedByte;
use valart::ValTrees;

const N: usize = 1_000_000;

struct CountingAlloc;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn heap_bytes() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

struct Keys {
    /// `field:{i}` dans l'ordre d'insertion (mélangé).
    insert: Vec<SharedByte>,
    /// Valeur de chaque champ de `insert` (même index).
    vals: Vec<SharedByte>,
    /// Mêmes clés, autre mélange : ordre des lookups et des suppressions.
    lookup: Vec<SharedByte>,
    /// `field:{N..2N}` : jamais insérés.
    miss: Vec<SharedByte>,
}

fn keys() -> Keys {
    let mut rng = fastrand::Rng::with_seed(0x5EED);
    let base: Vec<SharedByte> = (0..N)
        .map(|i| SharedByte::from_slice(format!("field:{i}")))
        .collect();
    let mut insert = base.clone();
    rng.shuffle(&mut insert);
    let vals = (0..N)
        .map(|i| SharedByte::from_slice(format!("value:{i}")))
        .collect();
    let mut lookup = base;
    rng.shuffle(&mut lookup);
    let mut miss: Vec<SharedByte> = (N..2 * N)
        .map(|i| SharedByte::from_slice(format!("field:{i}")))
        .collect();
    rng.shuffle(&mut miss);
    Keys {
        insert,
        vals,
        lookup,
        miss,
    }
}

#[derive(Default)]
struct Timings {
    /// Octets de la structure une fois les N clés insérées.
    bytes: usize,
    /// Part de `bytes` hors allocateur global (slab valart).
    extra: usize,
    insert: Duration,
    hit: Duration,
    miss: Duration,
    remove: Duration,
}

fn time(f: impl FnOnce() -> usize, expected: usize) -> Duration {
    let start = Instant::now();
    let found = black_box(f());
    let elapsed = start.elapsed();
    assert_eq!(found, expected);
    elapsed
}

/// Les quatre phases, communes à toutes les structures.
trait MapLike {
    /// Mémoire hors allocateur global (valart : slab mmap + overflows du pool).
    fn extra_bytes(&self) -> usize {
        0
    }
    /// `true` si le champ est nouveau.
    fn add(&mut self, k: SharedByte, v: SharedByte) -> bool;
    fn has(&self, k: &SharedByte) -> bool;
    fn rem(&mut self, k: &SharedByte) -> bool;
}

fn run<S: MapLike>(set: &mut S, keys: &Keys) -> Timings {
    let before = heap_bytes() + set.extra_bytes();
    let insert = time(
        || {
            keys.insert
                .iter()
                .zip(&keys.vals)
                .filter(|(k, v)| set.add((*k).clone(), (*v).clone()))
                .count()
        },
        N,
    );
    let bytes = heap_bytes() + set.extra_bytes() - before;
    let extra = set.extra_bytes();
    Timings {
        bytes,
        extra,
        insert,
        hit: time(|| keys.lookup.iter().filter(|k| set.has(k)).count(), N),
        miss: time(|| keys.miss.iter().filter(|k| set.has(k)).count(), 0),
        remove: time(|| keys.lookup.iter().filter(|k| set.rem(k)).count(), N),
    }
}

struct Valart {
    trees: ValTrees,
    root: u32,
}

impl MapLike for Valart {
    fn extra_bytes(&self) -> usize {
        self.trees.slab_bytes() + self.trees.hash_overflow_bytes(self.root)
    }
    fn add(&mut self, k: SharedByte, v: SharedByte) -> bool {
        self.trees.hash_set(self.root, k, v)
    }
    fn has(&self, k: &SharedByte) -> bool {
        self.trees.hash_get(self.root, k).is_some()
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.trees.hash_remove(self.root, k)
    }
}

impl<S: std::hash::BuildHasher> MapLike for HashMap<SharedByte, SharedByte, S> {
    fn add(&mut self, k: SharedByte, v: SharedByte) -> bool {
        self.insert(k, v).is_none()
    }
    fn has(&self, k: &SharedByte) -> bool {
        self.get(k).is_some()
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.remove(k).is_some()
    }
}

impl MapLike for BTreeMap<SharedByte, SharedByte> {
    fn add(&mut self, k: SharedByte, v: SharedByte) -> bool {
        self.insert(k, v).is_none()
    }
    fn has(&self, k: &SharedByte) -> bool {
        self.get(k).is_some()
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.remove(k).is_some()
    }
}

fn report(name: &str, t: &Timings) {
    let ns = |d: Duration| d.as_nanos() as f64 / N as f64;
    println!(
        "{name:<18} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>10.1} {:>8.1}",
        ns(t.insert),
        ns(t.hit),
        ns(t.miss),
        ns(t.remove),
        t.bytes as f64 / (1024.0 * 1024.0),
        t.bytes as f64 / N as f64,
    );
    if t.extra > 0 {
        println!(
            "{:<18} dont slab + overflow {:.1} MiB, allocateur global {:.1} MiB",
            "",
            t.extra as f64 / (1024.0 * 1024.0),
            (t.bytes - t.extra) as f64 / (1024.0 * 1024.0),
        );
    }
}

#[test]
#[ignore = "bench : lancer en --release avec --ignored --nocapture"]
fn bench_hash_fields_1m() {
    if cfg!(debug_assertions) {
        eprintln!("⚠ build debug : chiffres non représentatifs, utiliser --release");
    }
    let keys = keys();

    println!("\n{N} champs field:{{i}}, ordre aléatoire — ns/op, mémoire hors champs et valeurs");
    println!(
        "{:<18} {:>9} {:>9} {:>9} {:>9} {:>10} {:>8}",
        "", "insert", "hit", "miss", "remove", "MiB", "B/champ"
    );

    let mut valart = Valart {
        trees: ValTrees::new(),
        root: 0,
    };
    valart.root = valart.trees.hash_create();
    let t = run(&mut valart, &keys);
    assert_eq!(valart.trees.hash_len(valart.root), 0);
    report("valart", &t);

    let t = run(&mut HashMap::<SharedByte, SharedByte>::new(), &keys);
    report("HashMap (SipHash)", &t);

    let t = run(
        &mut HashMap::<SharedByte, SharedByte, foldhash::quality::RandomState>::default(),
        &keys,
    );
    report("HashMap (foldhash)", &t);

    let t = run(&mut BTreeMap::<SharedByte, SharedByte>::new(), &keys);
    report("BTreeMap", &t);
}
