//! Bench comparatif Set : valart vs HashSet (SipHash et foldhash) vs BTreeSet.
//!
//! ```text
//! cargo test --release -p valart --test bench_set -- --ignored --nocapture
//! ```
//!
//! Clés `user:{0..N}` pré-générées en `SharedByte` : chaque structure reçoit un
//! `clone()` (bump de refcount, pas d'allocation), donc le coût d'allocation
//! des clés est hors mesure pour tout le monde.
//!
//! Mémoire : allocateur global instrumenté au-dessus de MiMalloc (octets
//! demandés, sans l'arrondi aux classes de taille), mesuré après la phase
//! d'insertion. Les clés sont partagées et allouées via MiMalloc en direct :
//! hors compte. Pour valart on ajoute le slab (mmap) et les overflows (MiMalloc
//! en direct), tous deux invisibles pour l'allocateur global.

use std::alloc::{GlobalAlloc, Layout};

use mimalloc::MiMalloc;
use std::collections::{BTreeSet, HashSet};
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
        unsafe { MiMalloc.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { MiMalloc.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { MiMalloc.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn heap_bytes() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

struct Keys {
    /// `user:{i}` dans l'ordre d'insertion (mélangé).
    insert: Vec<SharedByte>,
    /// Mêmes clés, autre mélange : ordre des lookups et des suppressions.
    lookup: Vec<SharedByte>,
    /// `user:{N..2N}` : jamais insérées.
    miss: Vec<SharedByte>,
}

fn keys() -> Keys {
    let mut rng = fastrand::Rng::with_seed(0x5EED);
    let base: Vec<SharedByte> = (0..N)
        .map(|i| SharedByte::from_slice(format!("user:{i}")))
        .collect();
    let mut insert = base.clone();
    rng.shuffle(&mut insert);
    let mut lookup = base;
    rng.shuffle(&mut lookup);
    let mut miss: Vec<SharedByte> = (N..2 * N)
        .map(|i| SharedByte::from_slice(format!("user:{i}")))
        .collect();
    rng.shuffle(&mut miss);
    Keys { insert, lookup, miss }
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
trait SetLike {
    /// Mémoire hors allocateur global (valart : slab mmap + overflows MiMalloc).
    fn extra_bytes(&self) -> usize {
        0
    }
    fn add(&mut self, k: SharedByte) -> bool;
    fn has(&self, k: &SharedByte) -> bool;
    fn rem(&mut self, k: &SharedByte) -> bool;
}

fn run<S: SetLike>(set: &mut S, keys: &Keys) -> Timings {
    let before = heap_bytes() + set.extra_bytes();
    let insert = time(|| keys.insert.iter().filter(|k| set.add((*k).clone())).count(), N);
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

impl SetLike for Valart {
    fn extra_bytes(&self) -> usize {
        self.trees.slab_bytes() + self.trees.set_overflow_bytes(self.root)
    }
    fn add(&mut self, k: SharedByte) -> bool {
        self.trees.set_add(self.root, k)
    }
    fn has(&self, k: &SharedByte) -> bool {
        self.trees.set_contains(self.root, k)
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.trees.set_remove(self.root, k)
    }
}

impl<S: std::hash::BuildHasher> SetLike for HashSet<SharedByte, S> {
    fn add(&mut self, k: SharedByte) -> bool {
        self.insert(k)
    }
    fn has(&self, k: &SharedByte) -> bool {
        self.contains(k)
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.remove(k)
    }
}

impl SetLike for BTreeSet<SharedByte> {
    fn add(&mut self, k: SharedByte) -> bool {
        self.insert(k)
    }
    fn has(&self, k: &SharedByte) -> bool {
        self.contains(k)
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.remove(k)
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
fn bench_set_user_keys_1m() {
    if cfg!(debug_assertions) {
        eprintln!("⚠ build debug : chiffres non représentatifs, utiliser --release");
    }
    let keys = keys();

    println!("\n{N} clés user:{{i}}, ordre aléatoire — ns/op, mémoire hors clés");
    println!(
        "{:<18} {:>9} {:>9} {:>9} {:>9} {:>10} {:>8}",
        "", "insert", "hit", "miss", "remove", "MiB", "B/clé"
    );

    let mut valart = Valart {
        trees: ValTrees::new(),
        root: 0,
    };
    valart.root = valart.trees.set_create();
    let t = run(&mut valart, &keys);
    assert_eq!(valart.trees.set_card(valart.root), 0);
    report("valart", &t);

    let t = run(&mut HashSet::<SharedByte>::new(), &keys);
    report("HashSet (SipHash)", &t);

    let t = run(
        &mut HashSet::<SharedByte, foldhash::quality::RandomState>::default(),
        &keys,
    );
    report("HashSet (foldhash)", &t);

    let t = run(&mut BTreeSet::<SharedByte>::new(), &keys);
    report("BTreeSet", &t);
}
