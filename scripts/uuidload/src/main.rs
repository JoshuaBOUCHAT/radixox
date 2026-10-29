//! Client de charge RESP2 pipeliné, sans dépendance : clés UUID (36 o, pire cas
//! du radix) ou `key:NNN`, valeur UUID de 36 o.
//!
//!   uuidload --mode load    --keys 15000000             # SET de chaque clé, une fois
//!   uuidload --mode zipf    --keys 15000000 --time 10   # 1:4 SET:GET, Zipf 0,99
//!   uuidload --mode uniform --keys 15000000 --time 10
//!
//! Options : --host, --port (6379), --threads (6), --conns (14 par thread),
//! --pipeline (64), --shape uuid|b64|key, --zipf (0.99), --scramble (rang Zipf
//! mélangé par bijection : les clés chaudes ne sont plus `key:1`, `key:2`…),
//! --sets N (1 SET pour N commandes, 5 → 1:4), --seed.
//!
//! Débit : le client affiche le sien et le delta de `total_commands_processed`
//! du serveur (INFO) sur la fenêtre de mesure, la référence (le débit client
//! pipeliné varie de ±15 %).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Cfg {
    host: String,
    port: u16,
    threads: usize,
    conns: usize,
    pipeline: usize,
    keys: u64,
    shape_uuid: bool,
    /// UUID en base64url (22 o) au lieu de l'hexadécimal.
    b64: bool,
    mode: Mode,
    zipf: f64,
    scramble: bool,
    time: u64,
    sets: u64,
    seed: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Load,
    Zipf,
    Uniform,
}

fn parse_args() -> Cfg {
    let mut c = Cfg {
        host: "127.0.0.1".into(),
        port: 6379,
        threads: 6,
        conns: 14,
        pipeline: 64,
        keys: 1_000_000,
        shape_uuid: true,
        b64: false,
        mode: Mode::Zipf,
        zipf: 0.99,
        scramble: false,
        time: 10,
        sets: 5,
        seed: 0x5eed,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut v = || it.next().unwrap_or_else(|| panic!("{a} : valeur manquante"));
        match a.as_str() {
            "--host" => c.host = v(),
            "--port" => c.port = v().parse().unwrap(),
            "--threads" => c.threads = v().parse().unwrap(),
            "--conns" => c.conns = v().parse().unwrap(),
            "--pipeline" => c.pipeline = v().parse().unwrap(),
            "--keys" => c.keys = v().parse().unwrap(),
            "--shape" => {
                let s = v();
                c.shape_uuid = s == "uuid" || s == "b64";
                c.b64 = s == "b64";
            }
            "--mode" => {
                c.mode = match v().as_str() {
                    "load" => Mode::Load,
                    "zipf" => Mode::Zipf,
                    "uniform" => Mode::Uniform,
                    m => panic!("mode inconnu : {m}"),
                }
            }
            "--zipf" => c.zipf = v().parse().unwrap(),
            "--scramble" => c.scramble = true,
            "--time" => c.time = v().parse().unwrap(),
            "--sets" => c.sets = v().parse().unwrap(),
            "--seed" => c.seed = v().parse().unwrap(),
            _ => panic!("option inconnue : {a}"),
        }
    }
    c
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        splitmix(self.0)
    }
    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
    fn below(&mut self, n: u64) -> u64 {
        ((self.next() as u128 * n as u128) >> 64) as u64
    }
}

/// 36 o au format UUID, déterministe en `(i, salt)`.
fn uuid(out: &mut Vec<u8>, i: u64, salt: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let a = splitmix(i ^ salt);
    let b = splitmix(a ^ i.rotate_left(32));
    let bits = (a as u128) << 64 | b as u128;
    for n in 0..32 {
        if matches!(n, 8 | 12 | 16 | 20) {
            out.push(b'-');
        }
        out.push(HEX[(bits >> (124 - 4 * n) & 0xf) as usize]);
    }
}

/// Les mêmes 128 bits que [`uuid`], en base64url sans remplissage (22 o).
fn uuid_b64(out: &mut Vec<u8>, i: u64, salt: u64) {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let a = splitmix(i ^ salt);
    let b = splitmix(a ^ i.rotate_left(32));
    let bits = (a as u128) << 64 | b as u128;
    for n in 0..22 {
        let shift = 128i32 - 6 * (n + 1);
        let v = if shift >= 0 { bits >> shift } else { bits << -shift };
        out.push(B64[(v & 0x3f) as usize]);
    }
}

const KEY_SALT: u64 = 0x6b65_7973;
const VAL_SALT: u64 = 0x7661_6c73;

fn key(out: &mut Vec<u8>, cfg: &Cfg, i: u64) {
    if cfg.b64 {
        uuid_b64(out, i, KEY_SALT);
    } else if cfg.shape_uuid {
        uuid(out, i, KEY_SALT);
    } else {
        out.extend_from_slice(b"key:");
        out.extend_from_slice(i.to_string().as_bytes());
    }
}

fn push_cmd(buf: &mut Vec<u8>, cfg: &Cfg, i: u64, set: bool) {
    let mut k = Vec::with_capacity(40);
    key(&mut k, cfg, i);
    if set {
        buf.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$");
    } else {
        buf.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$");
    }
    buf.extend_from_slice(k.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(&k);
    buf.extend_from_slice(b"\r\n");
    if set {
        buf.extend_from_slice(b"$36\r\n");
        uuid(buf, i, VAL_SALT);
        buf.extend_from_slice(b"\r\n");
    }
}

/// Zipf par rejet-inversion (Hörmann et Derflinger), rang dans 1..=n.
struct Zipf {
    s: f64,
    n: f64,
    hx1: f64,
    hn: f64,
    t: f64,
}

fn helper1(x: f64) -> f64 {
    if x.abs() > 1e-8 { x.ln_1p() / x } else { 1.0 - x * (0.5 - x * (1.0 / 3.0 - 0.25 * x)) }
}
fn helper2(x: f64) -> f64 {
    if x.abs() > 1e-8 { x.exp_m1() / x } else { 1.0 + x * 0.5 * (1.0 + x / 3.0 * (1.0 + 0.25 * x)) }
}

impl Zipf {
    fn new(n: u64, s: f64) -> Self {
        let mut z = Zipf { s, n: n as f64, hx1: 0.0, hn: 0.0, t: 0.0 };
        z.hx1 = z.hint(1.5) - 1.0;
        z.hn = z.hint(z.n + 0.5);
        z.t = 2.0 - z.hint_inv(z.hint(2.5) - z.h(2.0));
        z
    }
    fn h(&self, x: f64) -> f64 {
        (-self.s * x.ln()).exp()
    }
    fn hint(&self, x: f64) -> f64 {
        let l = x.ln();
        helper2((1.0 - self.s) * l) * l
    }
    fn hint_inv(&self, x: f64) -> f64 {
        let t = (x * (1.0 - self.s)).max(-1.0);
        (helper1(t) * x).exp()
    }
    fn sample(&self, rng: &mut Rng) -> u64 {
        loop {
            let u = self.hn + rng.f64() * (self.hx1 - self.hn);
            let x = self.hint_inv(u);
            let k = (x + 0.5).clamp(1.0, self.n).floor();
            if k - x <= self.t || u >= self.hint(k + 0.5) - self.h(k) {
                return k as u64;
            }
        }
    }
}

/// Bijection de 0..n : i·a + b mod n, a premier avec n.
fn scrambler(n: u64) -> (u64, u64) {
    let mut a = (n as f64 * 0.618_033_988_7) as u64 | 1;
    while gcd(a, n) != 1 {
        a += 2;
    }
    (a, n / 3)
}
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Compte les réponses complètes de `buf[*pos..]` ; avance `pos`.
fn count_replies(buf: &[u8], pos: &mut usize) -> usize {
    let mut n = 0;
    loop {
        let p = *pos;
        let Some(nl) = buf[p..].iter().position(|&b| b == b'\n') else { return n };
        let line_end = p + nl + 1;
        match buf[p] {
            b'+' | b'-' | b':' => *pos = line_end,
            b'$' => {
                let len: i64 = std::str::from_utf8(&buf[p + 1..line_end - 2]).unwrap().parse().unwrap();
                let end = if len < 0 { line_end } else { line_end + len as usize + 2 };
                if end > buf.len() {
                    return n;
                }
                *pos = end;
            }
            c => panic!("réponse inattendue : {:?}", c as char),
        }
        n += 1;
    }
}

fn worker(cfg: Cfg, t: usize, stop: Arc<AtomicBool>, done: Arc<AtomicU64>, next_load: Arc<AtomicU64>) {
    let mut conns: Vec<TcpStream> = (0..cfg.conns)
        .map(|_| {
            let s = TcpStream::connect((cfg.host.as_str(), cfg.port)).expect("connexion");
            s.set_nodelay(true).unwrap();
            s
        })
        .collect();
    let mut rng = Rng(splitmix(cfg.seed ^ (t as u64 + 1) * 0x1234_5678));
    let zipf = Zipf::new(cfg.keys, cfg.zipf);
    let (sa, sb) = scrambler(cfg.keys);
    let mut out = Vec::with_capacity(cfg.pipeline * 100);
    let mut inb = vec![0u8; 1 << 16];
    let mut active = vec![true; conns.len()];
    while !stop.load(Ordering::Relaxed) {
        let mut sent = vec![0usize; conns.len()];
        for (c, s) in conns.iter_mut().enumerate() {
            if !active[c] {
                continue;
            }
            out.clear();
            if cfg.mode == Mode::Load {
                let from = next_load.fetch_add(cfg.pipeline as u64, Ordering::Relaxed);
                let to = (from + cfg.pipeline as u64).min(cfg.keys);
                if from >= cfg.keys {
                    active[c] = false;
                    continue;
                }
                for i in from..to {
                    push_cmd(&mut out, &cfg, i, true);
                }
                sent[c] = (to - from) as usize;
            } else {
                for _ in 0..cfg.pipeline {
                    let i = if cfg.mode == Mode::Zipf {
                        let r = zipf.sample(&mut rng) - 1;
                        if cfg.scramble { ((r as u128 * sa as u128 + sb as u128) % cfg.keys as u128) as u64 } else { r }
                    } else {
                        rng.below(cfg.keys)
                    };
                    push_cmd(&mut out, &cfg, i, rng.below(cfg.sets) == 0);
                }
                sent[c] = cfg.pipeline;
            }
            s.write_all(&out).expect("écriture");
        }
        if sent.iter().all(|&n| n == 0) {
            break;
        }
        for (c, s) in conns.iter_mut().enumerate() {
            let (mut got, mut fill, mut pos) = (0, 0, 0);
            while got < sent[c] {
                if fill == inb.len() {
                    inb.copy_within(pos..fill, 0);
                    (fill, pos) = (fill - pos, 0);
                }
                let n = s.read(&mut inb[fill..]).expect("lecture");
                assert!(n > 0, "connexion fermée");
                fill += n;
                got += count_replies(&inb[..fill], &mut pos);
            }
            done.fetch_add(sent[c] as u64, Ordering::Relaxed);
        }
    }
}

fn server_cmds(cfg: &Cfg) -> Option<u64> {
    let mut s = TcpStream::connect((cfg.host.as_str(), cfg.port)).ok()?;
    s.write_all(b"*1\r\n$4\r\nINFO\r\n").ok()?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let len: usize = line.trim_start_matches('$').trim().parse().ok()?;
    let mut body = vec![0; len + 2];
    r.read_exact(&mut body).ok()?;
    let body = String::from_utf8_lossy(&body);
    body.lines().find_map(|l| l.strip_prefix("total_commands_processed:")).and_then(|v| v.trim().parse().ok())
}

fn main() {
    let cfg = parse_args();
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let next_load = Arc::new(AtomicU64::new(0));
    let s0 = server_cmds(&cfg);
    let t0 = Instant::now();
    let hs: Vec<_> = (0..cfg.threads)
        .map(|t| {
            let (cfg, stop, done, nl) = (cfg.clone(), stop.clone(), done.clone(), next_load.clone());
            std::thread::spawn(move || worker(cfg, t, stop, done, nl))
        })
        .collect();
    if cfg.mode != Mode::Load {
        // 1 s de chauffe, puis fenêtre de mesure
        std::thread::sleep(Duration::from_secs(1));
        let (c0, s0, t0) = (done.load(Ordering::Relaxed), server_cmds(&cfg), Instant::now());
        std::thread::sleep(Duration::from_secs(cfg.time));
        let (c1, s1, dt) = (done.load(Ordering::Relaxed), server_cmds(&cfg), t0.elapsed().as_secs_f64());
        stop.store(true, Ordering::Relaxed);
        report(c1 - c0, s0.zip(s1).map(|(a, b)| b - a - 1), dt);
    }
    for h in hs {
        h.join().unwrap();
    }
    if cfg.mode == Mode::Load {
        let dt = t0.elapsed().as_secs_f64();
        report(done.load(Ordering::Relaxed), s0.zip(server_cmds(&cfg)).map(|(a, b)| b - a), dt);
    }
}

fn report(client: u64, server: Option<u64>, dt: f64) {
    print!("client {:.3} M/s", client as f64 / dt / 1e6);
    if let Some(s) = server {
        print!("  serveur {:.3} M/s", s as f64 / dt / 1e6);
    }
    println!("  ({dt:.1} s)");
}
