//! AOF de bout en bout (DURABILITY_DESIGN.md §12, test 5) : des clients
//! écrivent des clés numérotées, le serveur est tué (`kill -9`) à un instant
//! quelconque, redémarré sur les mêmes fichiers ; l'état doit être un préfixe
//! exact de l'histoire de chaque client (ni écriture inventée, ni trou, `INCR`
//! rejoué une seule fois) et contenir ce qui a été acquitté assez tôt.
//!
//! `AOF_CRASH_ITERS=200` pour une passe longue.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod common;

const PORT_EVERYSEC: u16 = 6430;
const PORT_NO: u16 = 6431;
const PORT_CLEAN: u16 = 6432;
const PORT_REWRITE: u16 = 6433;
const PORT_ALWAYS: u16 = 6434;
const PORT_CONTINUOUS: u16 = 6435;
const CLIENTS: usize = 3;
/// Commandes par pipeline : `INCR c{t}` puis `SET c{t}:{n} n`, par paires.
const PAIRS: usize = 20;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(port: u16, dir: &Path, fsync: &str) -> Server {
    start_env(port, dir, fsync, &[])
}

fn start_env(port: u16, dir: &Path, fsync: &str, env: &[(&str, &str)]) -> Server {
    let _ = Command::new("fuser").args(["-k", &format!("{port}/tcp")]).stderr(Stdio::null()).status();
    let child = Command::new(common::server_bin())
        .envs(env.iter().copied())
        .env("RADIXOX_PORT", port.to_string())
        .env("RADIXOX_DIR", dir)
        .env("RADIXOX_SAVE", "")
        .env("RADIXOX_APPENDONLY", "yes")
        .env("RADIXOX_APPENDFSYNC", fsync)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn radixox");
    // La reprise rejoue le journal avant d'écouter : jusqu'à 30 s.
    for _ in 0..1200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server(child);
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

/// Un client : paires (INCR, SET) numérotées à partir de `from`, jusqu'à la
/// coupure. Rend les acquittements : (numéro suivant le dernier acquitté,
/// instant).
fn writer(port: u16, t: usize, from: u64, stop: Arc<AtomicBool>) -> Vec<(u64, Instant)> {
    let mut acks = Vec::new();
    let Ok(client) = redis::Client::open(format!("redis://127.0.0.1:{port}")) else { return acks };
    let Ok(mut con) = client.get_connection() else { return acks };
    let mut n = from;
    while !stop.load(Ordering::Relaxed) {
        let mut pipe = redis::pipe();
        for i in 0..PAIRS as u64 {
            pipe.cmd("INCR").arg(format!("c{t}")).ignore();
            pipe.cmd("SET").arg(format!("c{t}:{}", n + i)).arg(n + i).ignore();
        }
        if pipe.query::<()>(&mut con).is_err() {
            break;
        }
        n += PAIRS as u64;
        acks.push((n, Instant::now()));
    }
    acks
}

/// Préfixe présent pour le client `t` : nombre `m` de clés `c{t}:0..m`, en
/// vérifiant qu'il n'y a ni trou ni clé au-delà, et que le compteur vaut `m`
/// ou `m + 1` (INCR précède son SET dans chaque paire).
fn prefix(con: &mut redis::Connection, t: usize, upper: u64) -> u64 {
    let end = upper + 2 * PAIRS as u64;
    let mut m = None;
    for chunk in (0..end).collect::<Vec<_>>().chunks(1000) {
        let keys: Vec<String> = chunk.iter().map(|k| format!("c{t}:{k}")).collect();
        let vals: Vec<Option<u64>> = redis::cmd("MGET").arg(&keys).query(con).unwrap();
        for (&k, v) in chunk.iter().zip(vals) {
            match (v, m) {
                (Some(x), None) => assert_eq!(x, k, "client {t} : valeur de c{t}:{k}"),
                (None, None) => m = Some(k),
                (Some(_), Some(p)) => panic!("client {t} : trou avant c{t}:{k} (préfixe {p})"),
                (None, Some(_)) => {}
            }
        }
    }
    let m = m.expect("borne trop basse");
    let ctr: Option<u64> = redis::cmd("GET").arg(format!("c{t}")).query(con).unwrap();
    let ctr = ctr.unwrap_or(0);
    assert!(ctr == m || ctr == m + 1, "client {t} : INCR {ctr} pour {m} SET");
    m
}

/// Écritures, `kill -9` au hasard, redémarrage, vérification ; `iters` fois.
/// `rewrites` : un client lance `BGREWRITEAOF` toutes les 100 ms, et le seuil
/// de réécriture automatique est bas (coupures pendant les rotations).
fn crash_loop(port: u16, fsync: &str) {
    crash_loop_with(port, fsync, false)
}

fn rewriter(port: u16, stop: Arc<AtomicBool>) {
    let Ok(client) = redis::Client::open(format!("redis://127.0.0.1:{port}")) else { return };
    let Ok(mut con) = client.get_connection() else { return };
    while !stop.load(Ordering::Relaxed) {
        // « already in progress » est une réponse normale ; une coupure arrête.
        if let Err(e) = redis::cmd("BGREWRITEAOF").query::<String>(&mut con)
            && !e.to_string().contains("in progress")
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn crash_loop_with(port: u16, fsync: &str, rewrites: bool) {
    let env: &[(&str, &str)] = if rewrites {
        &[("RADIXOX_AUTO_AOF_REWRITE_MIN_SIZE", "256kb"), ("RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE", "50")]
    } else {
        &[]
    };
    let iters: u64 = std::env::var("AOF_CRASH_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let dir = test_dir(&format!("aof-{fsync}-{rewrites}"));
    let mut next = [0u64; CLIENTS];
    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ port as u64;
    for it in 0..iters {
        let mut server = start_env(port, &dir, fsync, env);
        // Reprise : le préfixe de l'itération précédente est là.
        let mut con = common::conn(port);
        for (t, n) in next.iter_mut().enumerate() {
            *n = prefix(&mut con, t, *n + 4 * PAIRS as u64);
        }
        drop(con);
        let stop = Arc::new(AtomicBool::new(false));
        let handles: Vec<_> = (0..CLIENTS)
            .map(|t| {
                let (stop, from) = (stop.clone(), next[t]);
                std::thread::spawn(move || writer(port, t, from, stop))
            })
            .collect();
        let rw = rewrites.then(|| {
            let stop = stop.clone();
            std::thread::spawn(move || rewriter(port, stop))
        });
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(150 + rng % 700));
        // SAFETY: pid de notre enfant.
        unsafe { libc::kill(server.0.id() as i32, libc::SIGKILL) };
        let killed = Instant::now();
        let _ = server.0.wait();
        stop.store(true, Ordering::Relaxed);
        let acks: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        if let Some(h) = rw {
            h.join().unwrap();
        }
        drop(server);

        let server = start_env(port, &dir, fsync, env);
        let mut con = common::conn(port);
        for (t, acks) in acks.iter().enumerate() {
            let sent = acks.last().map_or(next[t], |a| a.0) + PAIRS as u64;
            let m = prefix(&mut con, t, sent);
            // `kill -9` laisse le cache de pages : seul le dernier tour peut
            // manquer. Tout ce qui est acquitté 200 ms avant la coupure est là.
            // `always` : rien d'acquitté ne se perd.
            let margin = if fsync == "always" { Duration::ZERO } else { Duration::from_millis(200) };
            let old = acks.iter().rev().find(|a| killed.duration_since(a.1) >= margin);
            if let Some(&(n, _)) = old {
                assert!(m >= n, "{fsync}, itération {it}, client {t} : {m} présents, {n} acquittés avant la coupure");
            }
            next[t] = m;
        }
        drop(con);
        drop(server);
    }
    assert!(next.iter().all(|&n| n > 0), "aucune écriture : {next:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill9_everysec_keeps_an_exact_prefix() {
    crash_loop(PORT_EVERYSEC, "everysec");
}

#[test]
fn kill9_always_loses_nothing_acknowledged() {
    crash_loop_with(PORT_ALWAYS, "always", true);
}

#[test]
fn kill9_during_rewrites_keeps_an_exact_prefix() {
    crash_loop_with(PORT_REWRITE, "everysec", true);
}

/// `continuous` avec réécritures : cycles écriture + fsync liés, rotations.
#[test]
fn kill9_continuous_keeps_an_exact_prefix() {
    crash_loop_with(PORT_CONTINUOUS, "continuous", true);
}

#[test]
fn kill9_no_fsync_keeps_an_exact_prefix() {
    crash_loop(PORT_NO, "no");
}

/// Arrêt propre (SIGTERM) : tout ce qui est acquitté est relu.
#[test]
fn sigterm_keeps_every_acknowledged_write() {
    let dir = test_dir("aof-clean");
    let server = start(PORT_CLEAN, &dir, "everysec");
    let stop = Arc::new(AtomicBool::new(false));
    let h = {
        let stop = stop.clone();
        std::thread::spawn(move || writer(PORT_CLEAN, 0, 0, stop))
    };
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let acks = h.join().unwrap();
    let acked = acks.last().map_or(0, |a| a.0);
    assert!(acked > 0);
    // SAFETY: pid de notre enfant.
    unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) };
    let mut server = server;
    let status = server.0.wait().unwrap();
    assert!(status.success(), "arrêt : {status:?}");
    drop(server);
    let _server = start(PORT_CLEAN, &dir, "everysec");
    let mut con = common::conn(PORT_CLEAN);
    assert_eq!(prefix(&mut con, 0, acked + PAIRS as u64), acked);
    let info: String = redis::cmd("INFO").query(&mut con).unwrap();
    assert!(info.contains("aof_enabled:1") && info.contains("aof_last_write_status:ok"), "{info}");
    let _ = std::fs::remove_dir_all(&dir);
}
