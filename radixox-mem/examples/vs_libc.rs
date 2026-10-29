//! ns par appel, nos fonctions contre celles de la libc (glibc) :
//! `taskset -c 2 cargo run --release -p radixox-mem --example vs_libc`
//! Tailles fixes, puis tailles tirées dans 1..=128 (le prédicteur ne devine pas la branche).

use std::hint::black_box;
use std::time::Instant;

unsafe extern "C" {
    #[link_name = "memcpy"]
    fn c_memcpy(d: *mut u8, s: *const u8, n: usize) -> *mut u8;
    #[link_name = "memset"]
    fn c_memset(d: *mut u8, c: i32, n: usize) -> *mut u8;
    #[link_name = "memcmp"]
    fn c_memcmp(a: *const u8, b: *const u8, n: usize) -> i32;
    #[link_name = "bcmp"]
    fn c_bcmp(a: *const u8, b: *const u8, n: usize) -> i32;
}

type Copy = unsafe extern "C" fn(*mut u8, *const u8, usize) -> *mut u8;
type Set = unsafe extern "C" fn(*mut u8, i32, usize) -> *mut u8;
type Cmp = unsafe extern "C" fn(*const u8, *const u8, usize) -> i32;

const ITERS: usize = 4_000_000;

fn time(f: impl Fn(usize)) -> f64 {
    for i in 0..ITERS / 10 {
        f(i);
    }
    let t = Instant::now();
    for i in 0..ITERS {
        f(i);
    }
    t.elapsed().as_nanos() as f64 / ITERS as f64
}

fn main() {
    let src = vec![7u8; 1 << 20];
    let mut dst = vec![0u8; 1 << 20];
    let other = src.clone();
    let rnd: Vec<usize> = (0..4096u64).map(|i| (i.wrapping_mul(0x9E3779B97F4A7C15) >> 57) as usize + 1).collect();
    let (s, d, o) = (src.as_ptr(), dst.as_mut_ptr(), other.as_ptr());

    let copy = |f: Copy, n: Option<usize>| {
        time(|i| unsafe {
            let n = n.unwrap_or(rnd[i & 4095]);
            black_box(f(black_box(d.add(i & 63)), black_box(s.add(i & 31)), black_box(n)));
        })
    };
    let set = |f: Set, n: Option<usize>| {
        time(|i| unsafe {
            let n = n.unwrap_or(rnd[i & 4095]);
            black_box(f(black_box(d.add(i & 63)), black_box(0x5A), black_box(n)));
        })
    };
    let cmp = |f: Cmp, n: Option<usize>| {
        time(|i| unsafe {
            let n = n.unwrap_or(rnd[i & 4095]);
            black_box(f(black_box(s.add(i & 31)), black_box(o.add(i & 31)), black_box(n)));
        })
    };

    println!("{:>9} | {:>15} | {:>15} | {:>15} | {:>15}", "taille", "memcpy rx/libc", "memset rx/libc", "memcmp rx/libc", "bcmp rx/libc");
    let mut sizes: Vec<Option<usize>> = [8, 16, 24, 32, 48, 64, 100, 128, 256, 512, 1024, 4096, 65536].map(Some).to_vec();
    sizes.push(None);
    for n in sizes {
        let name = n.map_or("1..=128".to_string(), |n| n.to_string());
        println!(
            "{name:>9} | {:>6.2} / {:>6.2} | {:>6.2} / {:>6.2} | {:>6.2} / {:>6.2} | {:>6.2} / {:>6.2}",
            copy(radixox_mem::memcpy, n),
            copy(c_memcpy, n),
            set(radixox_mem::memset, n),
            set(c_memset, n),
            cmp(radixox_mem::memcmp, n),
            cmp(c_memcmp, n),
            cmp(radixox_mem::bcmp, n),
            cmp(c_bcmp, n),
        );
    }
}
