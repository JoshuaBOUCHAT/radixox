//! Compression zstd par tranches sur le thread de service : durée max d'un
//! appel `compress_stream2` (= durée d'une tranche), ratio et débit selon le
//! niveau et la taille max de bloc (`ZSTD_c_maxBlockSize`).
//!
//! `cargo run --release -p oxidart --example zstd_tranche -- [MB]`

use std::time::{Duration, Instant};

use oxidart::{OxidArt, Value};
use radixox_lib::shared_byte::SharedByte;
use zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer, zstd_sys::ZSTD_EndDirective};


fn main() {
    let mb: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8);
    // Taille d'une frame zstd indépendante, en MB (0 = une seule frame).
    let frame_mb: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let levels: Vec<i32> = std::env::args()
        .nth(3)
        .map(|s| s.split(',').map(|l| l.parse().unwrap()).collect())
        .unwrap_or(vec![1, 3, 6, 9, 12, 15, 19]);
    let mut art = OxidArt::new();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut out = Vec::new();
    let mut i = 0u64;
    while out.len() < mb << 20 {
        for _ in 0..20_000 {
            i += 1;
            let key = format!("user:{}:profile", rnd() % 10_000_000);
            let val = format!(
                "{{\"id\":{i},\"name\":\"user{}\",\"score\":{},\"city\":\"c{}\",\"tags\":[\"t{}\",\"t{}\"]}}",
                rnd() % 1_000_000,
                rnd() % 100_000,
                rnd() % 500,
                rnd() % 50,
                rnd() % 50
            );
            art.set(SharedByte::from_str(&key), Value::String(SharedByte::from_str(&val)));
        }
        out.clear();
        let mut cur = art.save_start(&mut out);
        while !art.save_step(&mut cur, &mut out, 1 << 20) {}
        drop(cur);
        art.finish_save();
    }
    out.truncate(mb << 20);
    println!("dump {:.1} MB", out.len() as f64 / 1e6);
    let frame = if frame_mb == 0 { out.len() } else { frame_mb << 20 };
    println!("frames de {:.1} MB", frame as f64 / 1e6);
    println!("niveau  bloc    ratio   MB/s    1er appel   max (hors 1er appel de frame)   p99");

    for level in levels {
        for block in std::env::var("BLOCKS").ok().map(|v| v.split(',').map(|b| b.parse::<usize>().unwrap() << 10).collect::<Vec<_>>()).unwrap_or(vec![128 << 10, 32 << 10, 8 << 10, 2 << 10]) {
            let mut cctx = CCtx::create();
            cctx.set_parameter(CParameter::CompressionLevel(level)).unwrap();
            cctx.set_parameter(CParameter::MaxBlockSize(block as u32)).unwrap();
            // WLOG=n : plafond de fenêtre (et des tables qui en dépendent).
            if let Some(w) = std::env::var("WLOG").ok().and_then(|v| v.parse::<u32>().ok()) {
                cctx.set_parameter(CParameter::WindowLog(w)).unwrap();
                cctx.set_parameter(CParameter::HashLog(w.min(30))).unwrap();
                cctx.set_parameter(CParameter::ChainLog(w.min(30))).unwrap();
            }
            let mut dst: Vec<u8> = Vec::with_capacity(frame + (frame >> 7) + 1024);
            let mut total_out = 0usize;
            let mut calls: Vec<Duration> = Vec::new();
            let t = Instant::now();
            let mut firsts: Vec<Duration> = Vec::new();
            for frame in out.chunks(frame) {
                let mut first = true;
                // Entrée donnée par morceaux de la taille d'un bloc : chaque
                // appel compresse au plus un bloc.
                for (k, piece) in frame.chunks(block).enumerate() {
                    let last = (k + 1) * block >= frame.len();
                    let mut input = InBuffer::around(piece);
                    loop {
                        let c = Instant::now();
                        let used = dst.len();
                        let mut o = OutBuffer::around_pos(&mut dst, used);
                        let end = if last { ZSTD_EndDirective::ZSTD_e_end } else { ZSTD_EndDirective::ZSTD_e_continue };
                        let rem = cctx.compress_stream2(&mut o, &mut input, end).unwrap();
                        let _ = k;
                        if std::mem::take(&mut first) {
                            firsts.push(c.elapsed());
                        } else {
                            calls.push(c.elapsed());
                        }
                        if input.pos() == piece.len() && (!last || rem == 0) {
                            break;
                        }
                        if dst.capacity() - dst.len() < 1024 {
                            dst.reserve(frame.len());
                        }
                    }
                }
                total_out += dst.len();
                dst.clear();
            }
            let dt = t.elapsed();
            calls.sort();
            let p99 = calls.get(calls.len() * 99 / 100).copied().unwrap_or_default();
            println!(
                "{level:>6}  {:>4}K  {:>6.2}  {:>6.0}  {:>9.2?}  {:>9.2?}  {:>9.2?}",
                block >> 10,
                out.len() as f64 / total_out as f64,
                out.len() as f64 / 1e6 / dt.as_secs_f64(),
                firsts.iter().max().unwrap(),
                calls.last().copied().unwrap_or_default(),
                p99
            );
        }
    }
}
