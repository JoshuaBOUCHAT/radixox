//! Réplication de bout en bout (DURABILITY_DESIGN.md §12, test 7) : deux
//! processus, le primaire pousse vers le réplica. Comparaison par
//! `DEBUG DIGEST <at>` (condensé du contenu à une même date) une fois que le
//! réplica a appliqué le dernier bloc du primaire.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod common;

const PRIMARY: u16 = 6440;
const REPLICA: u16 = 6441;
const PRIMARY_AOF: u16 = 6442;
const REPLICA_2: u16 = 6443;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(port: u16, dir: &Path, env: &[(&str, &str)]) -> Server {
    let _ = Command::new("fuser").args(["-k", &format!("{port}/tcp")]).stderr(Stdio::null()).status();
    let child = Command::new(common::server_bin())
        .env("RADIXOX_PORT", port.to_string())
        .env("RADIXOX_DIR", dir)
        .env("RADIXOX_SAVE", "")
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn radixox");
    for _ in 0..1200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
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

fn info(con: &mut redis::Connection) -> String {
    redis::cmd("INFO").query(con).unwrap()
}

fn field(info: &str, name: &str) -> Option<String> {
    info.lines().find_map(|l| l.strip_prefix(&format!("{name}:")).map(|v| v.trim().to_string()))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn digest(port: u16, at: u64) -> String {
    let mut con = common::conn(port);
    redis::cmd("DEBUG").arg("DIGEST").arg(at).query(&mut con).unwrap()
}

/// Attend que le réplica ait appliqué le dernier bloc du primaire et que le
/// lien soit en ligne ; rend ce `seq`.
fn wait_caught_up(primary: u16, replica: u16, within: Duration) -> u64 {
    let end = Instant::now() + within;
    let (mut p, mut r) = (common::conn(primary), common::conn(replica));
    loop {
        let (pi, ri) = (info(&mut p), info(&mut r));
        let pseq = field(&pi, "repl_seq");
        if field(&pi, "connected_replicas").as_deref() == Some("1")
            && field(&ri, "master_link_status").as_deref() == Some("up")
            && pseq.is_some()
            && pseq == field(&ri, "repl_seq")
        {
            return pseq.unwrap().parse().unwrap();
        }
        assert!(Instant::now() < end, "réplica pas à jour :\n{pi}\n---\n{ri}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Écrivain : SET, INCR, HSET, SADD, DEL, EXPIRE sur un jeu de clés partagé,
/// en pipelines, jusqu'à `stop`.
fn writer(port: u16, t: u64, stop: Arc<AtomicBool>) -> u64 {
    let mut con = common::conn(port);
    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ t;
    let mut n = 0;
    while !stop.load(Ordering::Relaxed) {
        let mut pipe = redis::pipe();
        for _ in 0..50 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let k = format!("k{}", rng % 2000);
            match rng % 7 {
                0 | 1 => pipe.cmd("SET").arg(&k).arg(rng % 1000).ignore(),
                2 => pipe.cmd("INCR").arg(format!("ctr{}", rng % 10)).ignore(),
                3 => pipe.cmd("HSET").arg(format!("h{}", rng % 50)).arg(&k).arg(rng % 7).ignore(),
                4 => pipe.cmd("SADD").arg(format!("s{}", rng % 50)).arg(&k).ignore(),
                5 => pipe.cmd("DEL").arg(&k).ignore(),
                _ => pipe.cmd("SET").arg(&k).arg("ttl").arg("EX").arg(1 + rng % 3).ignore(),
            };
        }
        if pipe.query::<()>(&mut con).is_err() {
            break;
        }
        n += 50;
    }
    n
}

fn load_keys(port: u16, n: u64) {
    let mut con = common::conn(port);
    for chunk in (0..n).collect::<Vec<_>>().chunks(5000) {
        let mut pipe = redis::pipe();
        for i in chunk {
            pipe.cmd("SET").arg(format!("big:{i}")).arg(format!("{i:0>64}")).ignore();
        }
        pipe.query::<()>(&mut con).unwrap();
    }
}

/// Primaire sous charge, réplica tué (`kill -9`) et relancé plusieurs fois :
/// il repart d'une synchronisation complète (dump de plusieurs Mo, envoyé en
/// morceaux) et rattrape ; à la fin, même condensé que le primaire.
#[test]
fn replica_survives_kill9_and_converges() {
    let (pdir, rdir) = (test_dir("repl-p"), test_dir("repl-r"));
    let mut replica = start(REPLICA, &rdir, &[]);
    let target = format!("127.0.0.1:{REPLICA}");
    let _primary = start(PRIMARY, &pdir, &[("RADIXOX_REPLICATE", &target)]);
    // ~13 Mo de dump : plusieurs RXLOAD DATA.
    load_keys(PRIMARY, 150_000);
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..3)
        .map(|t| {
            let stop = stop.clone();
            std::thread::spawn(move || writer(PRIMARY, t, stop))
        })
        .collect();
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..3 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(300 + rng % 700));
        // SAFETY: pid de notre enfant.
        unsafe { libc::kill(replica.0.id() as i32, libc::SIGKILL) };
        let _ = replica.0.wait();
        drop(replica);
        replica = start(REPLICA, &rdir, &[]);
    }
    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Relaxed);
    let written: u64 = writers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(written > 0);
    wait_caught_up(PRIMARY, REPLICA, Duration::from_secs(20));
    let at = unix_now();
    assert_eq!(digest(PRIMARY, at), digest(REPLICA, at));
    // WAIT : la dernière écriture de la connexion est acquittée par le réplica.
    let mut p = common::conn(PRIMARY);
    redis::cmd("SET").arg("waited").arg(1).query::<()>(&mut p).unwrap();
    let n: u64 = redis::cmd("WAIT").arg(1).arg(2000).query(&mut p).unwrap();
    assert_eq!(n, 1);
    // Le réplica refuse les écritures, sert les lectures.
    let mut r = common::conn(REPLICA);
    let err = redis::cmd("SET").arg("x").arg(1).query::<()>(&mut r).unwrap_err();
    assert!(err.to_string().contains("read only replica"), "{err}");
    let v: String = redis::cmd("GET").arg("big:42").query(&mut r).unwrap();
    assert_eq!(v, format!("{:0>64}", 42));
    let pi = info(&mut common::conn(PRIMARY));
    // Une synchronisation au départ, au moins une après les coupures (un
    // réplica tué avant que le lien ne revienne n'en demande pas).
    assert!(field(&pi, "sync_full").unwrap().parse::<u64>().unwrap() >= 2, "{pi}");
    assert_eq!(field(&pi, "replica0").map(|l| l.ends_with("lag_bytes=0")), Some(true), "{pi}");
    drop(replica);
    let _ = std::fs::remove_dir_all(&pdir);
    let _ = std::fs::remove_dir_all(&rdir);
}

/// Coupure courte du lien (le réplica reste vivant) : reprise partielle dans
/// le backlog, pas de nouveau dump.
#[test]
fn link_cut_resumes_from_backlog() {
    let (pdir, rdir) = (test_dir("repl-cut-p"), test_dir("repl-cut-r"));
    let _replica = start(REPLICA_2, &rdir, &[]);
    let primary = start(PRIMARY, &pdir, &[]);
    let mut p = common::conn(PRIMARY);
    redis::cmd("SET").arg("a").arg(1).query::<()>(&mut p).unwrap();
    redis::cmd("REPLICATE").arg("127.0.0.1").arg(REPLICA_2).query::<()>(&mut p).unwrap();
    wait_caught_up(PRIMARY, REPLICA_2, Duration::from_secs(10));
    // Lien coupé (la cible retirée puis remise), écritures entre-temps.
    redis::cmd("REPLICATE").arg("STOP").arg("127.0.0.1").arg(REPLICA_2).query::<()>(&mut p).unwrap();
    for i in 0..100 {
        redis::cmd("SET").arg(format!("k{i}")).arg(i).query::<()>(&mut p).unwrap();
    }
    redis::cmd("REPLICATE").arg("127.0.0.1").arg(REPLICA_2).query::<()>(&mut p).unwrap();
    wait_caught_up(PRIMARY, REPLICA_2, Duration::from_secs(10));
    let pi = info(&mut p);
    assert_eq!(field(&pi, "sync_full").as_deref(), Some("1"), "{pi}");
    assert_eq!(field(&pi, "sync_partial_ok").as_deref(), Some("1"), "{pi}");
    let at = unix_now();
    assert_eq!(digest(PRIMARY, at), digest(REPLICA_2, at));
    drop(primary);
    let _ = std::fs::remove_dir_all(&pdir);
    let _ = std::fs::remove_dir_all(&rdir);
}

/// Primaire avec AOF tué et relancé : nouvelle histoire, le réplica est
/// resynchronisé sur l'état relu des fichiers. Puis promotion du réplica :
/// écritures acceptées, l'ancien primaire est refusé.
#[test]
fn primary_restart_then_promotion() {
    let (pdir, rdir) = (test_dir("repl-aof-p"), test_dir("repl-aof-r"));
    let _replica = start(REPLICA, &rdir, &[]);
    let target = format!("127.0.0.1:{REPLICA}");
    let env = [("RADIXOX_APPENDONLY", "yes"), ("RADIXOX_REPLICATE", target.as_str())];
    let mut primary = start(PRIMARY_AOF, &pdir, &env);
    load_keys(PRIMARY_AOF, 20_000);
    wait_caught_up(PRIMARY_AOF, REPLICA, Duration::from_secs(10));
    let before = field(&info(&mut common::conn(REPLICA)), "master_replid").unwrap();
    // SAFETY: pid de notre enfant.
    unsafe { libc::kill(primary.0.id() as i32, libc::SIGKILL) };
    let _ = primary.0.wait();
    drop(primary);
    let _primary = start(PRIMARY_AOF, &pdir, &env);
    let mut p = common::conn(PRIMARY_AOF);
    redis::cmd("SET").arg("after").arg("restart").query::<()>(&mut p).unwrap();
    wait_caught_up(PRIMARY_AOF, REPLICA, Duration::from_secs(10));
    let mut r = common::conn(REPLICA);
    assert_ne!(field(&info(&mut r), "master_replid").unwrap(), before, "nouvelle histoire après redémarrage");
    let at = unix_now();
    assert_eq!(digest(PRIMARY_AOF, at), digest(REPLICA, at));
    // Promotion.
    redis::cmd("REPLICAOF").arg("NO").arg("ONE").query::<()>(&mut r).unwrap();
    redis::cmd("SET").arg("promoted").arg(1).query::<()>(&mut r).unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    let pi = info(&mut p);
    assert_eq!(field(&pi, "connected_replicas").as_deref(), Some("0"), "{pi}");
    let v: String = redis::cmd("GET").arg("promoted").query(&mut r).unwrap();
    assert_eq!(v, "1", "le promu n'a pas été écrasé par l'ancien primaire");
    let _ = std::fs::remove_dir_all(&pdir);
    let _ = std::fs::remove_dir_all(&rdir);
}
