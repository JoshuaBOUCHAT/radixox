//! Moteur AMAC (RUNTIME_ARCH.md §8bis) contre le chemin normal : les mêmes
//! pipelines aléatoires vont à un serveur avec AMAC (et AOF `always`) et à un
//! serveur sans ; les réponses doivent être identiques octet pour octet, puis
//! le contenu (`DEBUG DIGEST`), y compris après une reprise sur l'AOF.
//!
//! Chaque connexion a ses propres clés (suffixe `#i`) : l'ordre entre
//! connexions dans une fenêtre ne change aucune réponse. Les clés partagent
//! leurs préfixes : les mêmes nœuds portent les clés de plusieurs connexions.
//! `AMAC_ROUNDS=2000` pour une passe longue.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common;

const PORT_AMAC: u16 = 6450;
const PORT_PLAIN: u16 = 6451;
const CONNS: usize = 4;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(port: u16, dir: &Path, env: &[(&str, &str)]) -> Server {
    let _ = Command::new("fuser").args(["-k", &format!("{port}/tcp")]).stderr(Stdio::null()).status();
    std::thread::sleep(Duration::from_millis(150));
    let server = Command::new(common::server_bin())
        .envs(env.iter().copied())
        .env("RADIXOX_PORT", port.to_string())
        .env("RADIXOX_DIR", dir)
        .env("RADIXOX_SAVE", "")
        .env("RADIXOX_PREFETCH", "64")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Server)
        .expect("spawn radixox");
    for _ in 0..1200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return server;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("radixox n'a pas démarré sur {port}");
}

fn test_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("radixox-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// xorshift64* : pas de dépendance, suite reproductible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick(&mut self, s: &[u8]) -> u8 {
        s[self.below(s.len() as u64) as usize]
    }
}

fn resp(args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Préfixes partagés courts (splits fréquents) et nœuds larges.
fn key(rng: &mut Rng, conn: usize) -> Vec<u8> {
    let mut k = if rng.below(2) == 0 {
        let mut k = b"user:".to_vec();
        for _ in 0..rng.below(5) {
            k.push(rng.pick(b"abc"));
        }
        k
    } else {
        let mut k = b"k:".to_vec();
        for i in 0..1 + rng.below(6) {
            k.push(if i < 2 { rng.pick(b"abcdefghijklmnopqrst") } else { rng.pick(b"abc") });
        }
        k
    };
    k.extend_from_slice(format!("#{conn}").as_bytes());
    k
}

fn value(rng: &mut Rng) -> Vec<u8> {
    match rng.below(10) {
        0..=2 => rng.below(100).to_string().into_bytes(),
        3..=4 => vec![b'x'; 40 + rng.below(80) as usize],
        _ => format!("v{}", rng.next() % 100_000).into_bytes(),
    }
}

/// Une commande ; `true` : réponse de TTL / PTTL (la valeur dépend de l'horloge).
fn command(rng: &mut Rng, conn: usize) -> (Vec<u8>, bool) {
    let k = key(rng, conn);
    let v = value(rng);
    let k2 = key(rng, conn);
    let k3 = key(rng, conn);
    let c = match rng.below(100) {
        0..=19 => resp(&[b"GET", &k]),
        20..=31 => resp(&[b"SET", &k, &v]),
        32..=35 => resp(&[b"SET", &k, &v, b"EX", b"100"]),
        36..=38 => resp(&[b"SET", &k, &v, b"NX"]),
        39..=41 => resp(&[b"SET", &k, &v, b"XX"]),
        42..=44 => resp(&[b"SETNX", &k, &v]),
        45..=54 => resp(&[b"DEL", &k]),
        55..=56 => resp(&[b"DEL", &k, &k2]),
        57..=60 => resp(&[b"EXISTS", &k]),
        61 => resp(&[b"EXISTS", &k, &k2]),
        62..=64 => resp(&[b"TYPE", &k]),
        65..=67 => return (resp(&[b"TTL", &k]), true),
        68 => return (resp(&[b"PTTL", &k]), true),
        69..=71 => resp(&[b"EXPIRE", &k, b"100"]),
        72..=73 => resp(&[b"PERSIST", &k]),
        74..=77 => resp(&[b"INCR", &k]),
        78 => resp(&[b"DECR", &k]),
        79 => resp(&[b"INCRBY", &k, b"7"]),
        80 => resp(&[b"DECRBY", &k, b"5"]),
        81..=83 => resp(&[b"MGET", &k, &k2, &k3]),
        84..=86 => resp(&[b"HSET", &k, b"f", &v]),
        87 => resp(&[b"HGET", &k, b"f"]),
        88 => resp(&[b"PING"]),
        89..=91 => resp(&[b"MSET", &k, &v, &k2, &v]),
        // plusieurs clés, dont des doublons
        92..=93 => {
            let ks: Vec<Vec<u8>> = (0..8).map(|_| key(rng, conn)).collect();
            let mut args: Vec<&[u8]> = vec![b"MGET"];
            args.extend(ks.iter().map(|k| k.as_slice()));
            args.push(&k);
            resp(&args)
        }
        94 => resp(&[b"DEL", &k, &k2, &k, &k3, &k2]),
        95 => resp(&[b"MSET", &k, &v, &k2, b"1", &k, b"2"]),
        96 => resp(&[b"EXISTS", &k, &k, &k2]),
        _ => resp(&[b"GET", &k]),
    };
    (c, false)
}

/// Fin de la réponse qui commence à `pos`, si elle est complète.
fn reply_end(buf: &[u8], pos: usize) -> Option<usize> {
    let line_end = pos + buf.get(pos..)?.windows(2).position(|w| w == b"\r\n")?;
    let head = std::str::from_utf8(&buf[pos + 1..line_end]).unwrap();
    let after = line_end + 2;
    match buf[pos] {
        b'+' | b'-' | b':' => Some(after),
        b'$' => {
            let n: i64 = head.parse().unwrap();
            if n < 0 {
                return Some(after);
            }
            let end = after + n as usize + 2;
            (buf.len() >= end).then_some(end)
        }
        b'*' => {
            let n: i64 = head.parse().unwrap();
            let mut p = after;
            for _ in 0..n.max(0) {
                p = reply_end(buf, p)?;
            }
            Some(p)
        }
        c => panic!("réponse RESP inattendue : {:?}", c as char),
    }
}

/// Lit `n` réponses ; celles des TTL (`ttl[i]`) positives deviennent `:T`.
fn read_replies(s: &mut TcpStream, buf: &mut Vec<u8>, ttl: &[bool]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(ttl.len());
    let mut pos = 0;
    let mut chunk = [0u8; 64 << 10];
    while out.len() < ttl.len() {
        match reply_end(buf, pos) {
            Some(end) => {
                let r = &buf[pos..end];
                let ttl_positive = ttl[out.len()] && r.starts_with(b":") && !r.starts_with(b":-");
                out.push(if ttl_positive { b":T".to_vec() } else { r.to_vec() });
                pos = end;
            }
            None => {
                let n = s.read(&mut chunk).expect("lecture");
                assert!(n > 0, "connexion fermée");
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
    buf.drain(..pos);
    out
}

fn simple(port: u16, args: &[&[u8]]) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(&resp(args)).unwrap();
    let mut buf = Vec::new();
    read_replies(&mut s, &mut buf, &[false]).remove(0)
}

fn info_field(port: u16, field: &str) -> u64 {
    let r = simple(port, &[b"INFO"]);
    let text = String::from_utf8_lossy(&r);
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{field}:")))
        .unwrap_or_else(|| panic!("{field} absent de INFO"))
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn amac_answers_like_plain_path() {
    let rounds: usize = std::env::var("AMAC_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let dir_a = test_dir("amac-a");
    let dir_b = test_dir("amac-b");
    let amac_env = [("RADIXOX_AMAC", "8"), ("RADIXOX_AMAC_PF", "2"), ("RADIXOX_APPENDONLY", "yes"), ("RADIXOX_APPENDFSYNC", "always")];
    let a = start(PORT_AMAC, &dir_a, &amac_env);
    let _b = start(PORT_PLAIN, &dir_b, &[("RADIXOX_AMAC", "0")]);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let open = |port| -> Vec<(TcpStream, Vec<u8>)> {
        (0..CONNS).map(|_| (TcpStream::connect(("127.0.0.1", port)).unwrap(), Vec::new())).collect()
    };
    let (mut ca, mut cb) = (open(PORT_AMAC), open(PORT_PLAIN));
    for round in 0..rounds {
        // toutes les connexions envoient, puis on lit : les fenêtres mêlent
        // les connexions
        let mut kinds: Vec<Vec<bool>> = Vec::new();
        for c in 0..CONNS {
            let n = 1 + rng.below(80) as usize;
            let mut bytes = Vec::new();
            let mut ttl = Vec::with_capacity(n);
            for _ in 0..n {
                let (cmd, is_ttl) = command(&mut rng, c);
                bytes.extend_from_slice(&cmd);
                ttl.push(is_ttl);
            }
            ca[c].0.write_all(&bytes).unwrap();
            cb[c].0.write_all(&bytes).unwrap();
            kinds.push(ttl);
        }
        for c in 0..CONNS {
            let (sa, ba) = &mut ca[c];
            let got = read_replies(sa, ba, &kinds[c]);
            let (sb, bb) = &mut cb[c];
            let want = read_replies(sb, bb, &kinds[c]);
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                assert_eq!(
                    g.escape_ascii().to_string(),
                    w.escape_ascii().to_string(),
                    "round {round} connexion {c} commande {i}"
                );
            }
        }
    }
    assert_eq!(simple(PORT_AMAC, &[b"DBSIZE"]), simple(PORT_PLAIN, &[b"DBSIZE"]));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs().to_string();
    let digest = |port| simple(port, &[b"DEBUG", b"DIGEST", at.as_bytes()]);
    let want = digest(PORT_PLAIN);
    assert_eq!(digest(PORT_AMAC), want, "contenu");
    let served = info_field(PORT_AMAC, "amac_cmds");
    assert!(served > (rounds * CONNS) as u64, "le moteur AMAC n'a presque pas servi : {served}");
    // `always` : tout ce qui a été répondu est sur le disque
    drop(a);
    let _a = start(PORT_AMAC, &dir_a, &amac_env);
    assert_eq!(digest(PORT_AMAC), want, "contenu après reprise sur l'AOF");
}
