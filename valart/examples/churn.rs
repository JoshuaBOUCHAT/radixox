//! Churn : insert/remove frénétiques à taille constante, puis réduction.
//! Mesure la pression sur l'allocateur, la latence de queue et la
//! fragmentation (RSS réel vs octets vivants). Un processus par structure,
//! sinon le RSS d'une structure pollue la suivante :
//!
//! ```text
//! for s in valart btree hash; do cargo run --release -p valart --example churn -- $s; done
//! ```
//!
//! Phases :
//! 1. load   : N clés `user:{i}` tirées d'un univers de 2N, ordre aléatoire
//! 2. churn  : ROUNDS × OPS fois (retire une clé présente au hasard, ajoute une
//!    absente au hasard) → la taille reste N, la structure est réécrite
//!    plusieurs fois ; snapshot mémoire après chaque tour (dérive ?)
//! 3. shrink : on retire la moitié des clés → est-ce que la mémoire revient ?
//!
//! Allocateur global : allocateur système instrumenté (les overflows de valart et
//! les `SharedByte` vont au pool, hors compte).
//! « vivant » = octets demandés à l'allocateur + slab mmap et overflows de
//! valart. « RSS » = mémoire résidente réelle du processus
//! (`/proc/self/statm`), delta par rapport à juste avant la création de la
//! structure : inclut l'arrondi aux classes de taille, les trous, et les pages
//! jamais rendues.

use std::alloc::{GlobalAlloc, Layout, System};

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use radixox_lib::shared_byte::SharedByte;
use valart::ValTrees;

const N: usize = 1_000_000;
const OPS: usize = 5_000_000;
const ROUNDS: usize = 4;

struct CountingAlloc;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn rss_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

/// Histogramme 1 ns jusqu'à 64 µs, au-delà dans un seul bucket (+ max exact).
/// Alloué avant la mesure de base : n'apparaît pas dans les deltas.
struct Hist {
    buckets: Vec<u32>,
    max: u64,
    count: u64,
}

impl Hist {
    const LIMIT: usize = 1 << 16;

    fn new() -> Self {
        Self {
            buckets: vec![0; Self::LIMIT + 1],
            max: 0,
            count: 0,
        }
    }

    #[inline]
    fn record(&mut self, ns: u64) {
        self.buckets[(ns as usize).min(Self::LIMIT)] += 1;
        self.max = self.max.max(ns);
        self.count += 1;
    }

    fn percentile(&self, p: f64) -> String {
        let target = ((self.count as f64) * p).ceil() as u64;
        let mut seen = 0u64;
        for (ns, &c) in self.buckets.iter().enumerate() {
            seen += c as u64;
            if seen >= target {
                return if ns == Self::LIMIT {
                    format!(">{}", Self::LIMIT)
                } else {
                    ns.to_string()
                };
            }
        }
        unreachable!()
    }

    fn line(&self) -> String {
        format!(
            "p50 {:>5}  p99 {:>5}  p99.9 {:>6}  p99.99 {:>6}  max {:>8} ns",
            self.percentile(0.5),
            self.percentile(0.99),
            self.percentile(0.999),
            self.percentile(0.9999),
            self.max
        )
    }
}

trait SetLike {
    /// Mémoire hors allocateur global (valart : slab mmap + overflows du pool).
    fn extra_bytes(&self) -> usize {
        0
    }
    fn add(&mut self, k: SharedByte) -> bool;
    fn rem(&mut self, k: &SharedByte) -> bool;
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
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.trees.set_remove(self.root, k)
    }
}

impl<S: std::hash::BuildHasher> SetLike for HashSet<SharedByte, S> {
    fn add(&mut self, k: SharedByte) -> bool {
        self.insert(k)
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.remove(k)
    }
}

impl SetLike for BTreeSet<SharedByte> {
    fn add(&mut self, k: SharedByte) -> bool {
        self.insert(k)
    }
    fn rem(&mut self, k: &SharedByte) -> bool {
        self.remove(k)
    }
}

struct Baseline {
    heap: usize,
    rss: usize,
}

fn snapshot<S: SetLike>(phase: &str, set: &S, base: &Baseline, len: usize) {
    let live = ALLOCATED.load(Ordering::Relaxed) - base.heap + set.extra_bytes();
    let rss = rss_bytes().saturating_sub(base.rss);
    let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
    println!(
        "  {phase:<7} clés {len:>8}  vivant {:>6.1} MiB ({:>5.1} B/clé)  RSS {:>6.1} MiB  RSS/vivant {:.2}",
        mib(live),
        live as f64 / len.max(1) as f64,
        mib(rss),
        rss as f64 / live.max(1) as f64,
    );
}

fn run<S: SetLike>(name: &str, make: impl FnOnce() -> S) {
    let mut rng = fastrand::Rng::with_seed(0xC4_0123);
    let universe: Vec<SharedByte> = (0..2 * N)
        .map(|i| SharedByte::from_slice(format!("user:{i}")))
        .collect();
    let mut ids: Vec<u32> = (0..2 * N as u32).collect();
    rng.shuffle(&mut ids);
    let mut present = Vec::with_capacity(2 * N);
    present.extend_from_slice(&ids[..N]);
    let mut absent = Vec::with_capacity(2 * N);
    absent.extend_from_slice(&ids[N..]);
    drop(ids);
    let mut hist_add = Hist::new();
    let mut hist_rem = Hist::new();

    let base = Baseline {
        heap: ALLOCATED.load(Ordering::Relaxed),
        rss: rss_bytes(),
    };
    let mut set = make();

    println!("\n== {name} ==");
    let start = Instant::now();
    for &id in &present {
        assert!(set.add(universe[id as usize].clone()));
    }
    println!("  load    {:.0} ns/insert", start.elapsed().as_nanos() as f64 / N as f64);
    snapshot("load", &set, &base, N);

    let calls_before = ALLOC_CALLS.load(Ordering::Relaxed);
    for op in 0..ROUNDS * OPS {
        if op > 0 && op % OPS == 0 {
            snapshot(&format!("tour {}", op / OPS), &set, &base, N);
        }
        let id = present.swap_remove(rng.usize(..present.len()));
        let t = Instant::now();
        assert!(set.rem(&universe[id as usize]));
        hist_rem.record(t.elapsed().as_nanos() as u64);
        absent.push(id);

        let id = absent.swap_remove(rng.usize(..absent.len()));
        let key = universe[id as usize].clone();
        let t = Instant::now();
        assert!(set.add(key));
        hist_add.record(t.elapsed().as_nanos() as u64);
        present.push(id);
    }
    let calls = ALLOC_CALLS.load(Ordering::Relaxed) - calls_before;
    println!(
        "  churn   {} × (remove + insert), {:.2} alloc/op",
        ROUNDS * OPS,
        calls as f64 / (2 * ROUNDS * OPS) as f64
    );
    println!("    insert {}", hist_add.line());
    println!("    remove {}", hist_rem.line());
    snapshot(&format!("tour {ROUNDS}"), &set, &base, N);

    for _ in 0..N / 2 {
        let id = present.pop().unwrap();
        assert!(set.rem(&universe[id as usize]));
    }
    snapshot("shrink", &set, &base, N - N / 2);
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_default();
    match which.as_str() {
        "valart" => run("valart", || {
            let trees = ValTrees::new();
            let root = trees.set_create();
            Valart { trees, root }
        }),
        "btree" => run("BTreeSet", BTreeSet::<SharedByte>::new),
        "hash" => run(
            "HashSet (foldhash)",
            HashSet::<SharedByte, foldhash::quality::RandomState>::default,
        ),
        _ => eprintln!("usage: churn valart|btree|hash"),
    }
}
