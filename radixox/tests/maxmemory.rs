//! `maxmemory` (MEMORY_ARCH.md §10.5) : borne du total, `-OOM` sur les écritures qui
//! grossissent, lectures et suppressions toujours servies, refus de démarrer sur des
//! données trop grosses ; éviction (politiques, collections, `CONFIG SET` à chaud), vue de
//! l'AOF et des réplicas.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MB: u64 = 1 << 20;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Server {
    /// Pic de mémoire anonyme du processus : `VmHWM` moins le résident fichier et partagé
    /// (le binaire, à peu près constant). Voit un dépassement passager, pas seulement l'état
    /// final.
    fn peak_anon(&self) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.0.id())).unwrap();
        let kb = |name: &str| -> u64 {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
                .unwrap()
        };
        (kb("VmHWM:") - kb("RssFile:") - kb("RssShmem:")) * 1024
    }
}

/// Serveur dans son propre dossier (pas de dump du dossier courant chargé par hasard).
fn spawn(port: u16, dir: &Path, env: &[(&str, &str)]) -> Server {
    let _ = Command::new("fuser").args(["-k", &format!("{port}/tcp")]).stderr(Stdio::null()).status();
    std::thread::sleep(Duration::from_millis(150));
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
    let d = std::env::temp_dir().join(format!("radixox-maxmem-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn info(c: &mut redis::Connection) -> HashMap<String, String> {
    let s: String = redis::cmd("INFO").query(c).unwrap();
    s.lines().filter_map(|l| l.split_once(':')).map(|(k, v)| (k.to_string(), v.trim().to_string())).collect()
}

fn num(m: &HashMap<String, String>, k: &str) -> u64 {
    m[k].parse().unwrap()
}

/// L'éviction de fond a fini : deux relevés à 300 ms d'écart, mêmes évictions et même
/// estimation.
fn settle(c: &mut redis::Connection) -> HashMap<String, String> {
    let end = Instant::now() + Duration::from_secs(10);
    let mut prev = info(c);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let m = info(c);
        if m["evicted_keys"] == prev["evicted_keys"] && m["mem_live_estimate"] == prev["mem_live_estimate"] {
            return m;
        }
        assert!(Instant::now() < end, "l'éviction ne s'arrête pas");
        prev = m;
    }
}

/// Cible de l'éviction de fond : limite des données − 5 %.
fn target(m: &HashMap<String, String>) -> u64 {
    let limit = num(m, "maxmemory_data_limit");
    limit - limit / 20
}

/// L'éviction s'arrête à la cible, pas loin en dessous : elle voit ce qu'elle rend.
fn assert_not_over_evicted(m: &HashMap<String, String>) {
    let (live, target) = (num(m, "mem_live_estimate"), target(m));
    assert!(live * 10 >= target * 9, "sur-éviction : {live} o vivants pour une cible de {target} o");
}

/// `n` lots de 1 000 écritures pipelinées ; les `-OOM` sont comptés par `INFO`.
fn fill(c: &mut redis::Connection, batches: usize, mut write: impl FnMut(&mut redis::Pipeline, usize)) {
    for b in 0..batches {
        let mut p = redis::pipe();
        for i in 0..1000 {
            write(&mut p, b * 1000 + i);
        }
        // une réponse -OOM fait échouer le pipeline côté client
        let _ = p.query::<()>(c);
    }
}

#[test]
fn growth_is_refused_at_the_limit_reads_and_deletes_are_not() {
    const PORT: u16 = 16431;
    let dir = test_dir("growth");
    let server = spawn(PORT, &dir, &[("RADIXOX_MAXMEMORY", "48mb")]);
    let mut c = common::conn(PORT);
    let val = vec![b'v'; 1000];
    let (mut ok, mut oom) = (0, None);
    'fill: for batch in 0..200 {
        let mut p = redis::pipe();
        for i in 0..1000 {
            p.cmd("SET").arg(format!("k{}", batch * 1000 + i)).arg(&val);
        }
        let replies: Vec<redis::RedisResult<redis::Value>> = p
            .query::<Vec<redis::Value>>(&mut c)
            .map(|v| v.into_iter().map(Ok).collect())
            .unwrap_or_else(|e| vec![Err(e)]);
        for r in replies {
            match r {
                Ok(redis::Value::Okay) => ok += 1,
                Ok(redis::Value::ServerError(e)) => {
                    oom = Some(format!("{e:?}"));
                    break 'fill;
                }
                other => {
                    oom = Some(format!("{other:?}"));
                    break 'fill;
                }
            }
        }
    }
    let oom = oom.expect("la limite doit être atteinte avant 200 000 SET de 1 Ko");
    assert!(oom.contains("OOM"), "{oom}");
    assert!(ok > 10_000, "{ok} SET acceptés avant la limite");
    let m = info(&mut c);
    let used = num(&m, "used_memory");
    let rss = num(&m, "mem_rss_anon_bytes");
    assert!(used <= 48 * MB && rss <= 48 * MB, "registre {used} o, RSS {rss} o pour 48 Mo");
    assert!(server.peak_anon() <= 48 * MB, "pic {} o pour 48 Mo", server.peak_anon());
    assert!(num(&m, "oom_rejected_commands") >= 1);
    // Lectures et suppressions passent toujours.
    let v: Vec<u8> = redis::cmd("GET").arg("k1").query(&mut c).unwrap();
    assert_eq!(v, val);
    // Tout supprimer rend la place : un nouveau remplissage passe (le registre ne descend
    // qu'au rendu des pages, l'usage compte le réutilisable).
    for batch in 0..(ok / 1000 + 1) {
        let mut p = redis::pipe();
        for i in 0..1000 {
            p.cmd("DEL").arg(format!("k{}", batch * 1000 + i)).ignore();
        }
        p.query::<()>(&mut c).unwrap();
    }
    let mut again = 0;
    for i in 0..ok / 2 {
        match redis::cmd("SET").arg(format!("n{i}")).arg(&val).query::<()>(&mut c) {
            Ok(()) => again += 1,
            Err(_) => break,
        }
    }
    assert_eq!(again, ok / 2, "après suppression, la moitié du premier remplissage doit repasser");
    // CONFIG GET / SET.
    let got: Vec<String> = redis::cmd("CONFIG").arg("GET").arg("maxmemory").query(&mut c).unwrap();
    assert_eq!(got, vec!["maxmemory".to_string(), (48 * MB).to_string()]);
    let _: () = redis::cmd("CONFIG").arg("SET").arg("maxmemory").arg("0").query(&mut c).unwrap();
    let _: () = redis::cmd("SET").arg("after").arg(&val).query(&mut c).unwrap();
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_is_refused_when_the_data_does_not_fit() {
    const PORT: u16 = 16432;
    let dir = test_dir("startup");
    let d = dir.to_str().unwrap();
    common::start_server_env(PORT, &[("RADIXOX_DIR", d)]);
    let mut c = common::conn(PORT);
    let val = vec![b'x'; 4096];
    for batch in 0..10 {
        let mut p = redis::pipe();
        for i in 0..1000 {
            p.cmd("SET").arg(format!("k{}", batch * 1000 + i)).arg(&val).ignore();
        }
        p.query::<()>(&mut c).unwrap();
    }
    let _ = redis::cmd("SHUTDOWN").arg("SAVE").query::<()>(&mut c);
    std::thread::sleep(Duration::from_millis(300));
    // ~40 Mo de données, limite de 24 Mo : le serveur refuse de démarrer.
    let mut child = Command::new(common::server_bin())
        .env("RADIXOX_PORT", (PORT + 1).to_string())
        .env("RADIXOX_SAVE", "")
        .env("RADIXOX_DIR", d)
        .env("RADIXOX_MAXMEMORY", "24mb")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "le serveur a démarré malgré des données trop grosses");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let mut err = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
    assert!(err.contains("maxmemory"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `allkeys-random` : au-delà de la limite, des clés sont évincées au lieu de refuser ; la
/// mémoire reste sous `maxmemory`, et l'éviction s'arrête à sa cible au lieu de vider la
/// base. `volatile-ttl` sans aucune clé avec TTL : rien d'évinçable, `-OOM`.
#[test]
fn eviction_keeps_writes_going_under_the_limit() {
    const PORT: u16 = 16433;
    let dir = test_dir("evict");
    let server = spawn(PORT, &dir, &[("RADIXOX_MAXMEMORY", "48mb"), ("RADIXOX_MAXMEMORY_POLICY", "allkeys-random")]);
    let mut c = common::conn(PORT);
    let val = vec![b'v'; 1000];
    fill(&mut c, 150, |p, i| {
        p.cmd("SET").arg(format!("k{i}")).arg(&val).ignore();
    });
    // après la fin de l'éviction de fond : sinon DBSIZE et INFO ne voient pas le même état
    let m = settle(&mut c);
    let oom = num(&m, "oom_rejected_commands");
    let ok = 150_000 - oom;
    let evicted = num(&m, "evicted_keys");
    assert!(evicted > 50_000, "{evicted} clés évincées");
    assert!(ok > 140_000, "{ok} acceptés, {oom} refusés");
    assert!(server.peak_anon() <= 48 * MB, "pic {} o pour 48 Mo", server.peak_anon());
    assert_not_over_evicted(&m);
    let dbsize: u64 = redis::cmd("DBSIZE").query(&mut c).unwrap();
    assert_eq!(dbsize, ok - evicted, "clés présentes = acceptées − évincées");
    // volatile-ttl : aucune clé n'a de TTL, rien n'est évinçable
    let _: () = redis::cmd("CONFIG").arg("SET").arg("maxmemory-policy").arg("volatile-ttl").query(&mut c).unwrap();
    let mut refused = false;
    for i in 0..50_000 {
        if redis::cmd("SET").arg(format!("z{i}")).arg(&val).query::<()>(&mut c).is_err() {
            refused = true;
            break;
        }
    }
    assert!(refused, "volatile-ttl sans TTL : -OOM attendu");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Chaque type de collection passe par l'admission (`-OOM` à la limite, pic sous la limite,
/// lectures servies), `ZUNIONSTORE` compris (coût estimé par les cardinalités), puis par
/// l'éviction : une collection entière part, les écritures continuent.
#[test]
fn collections_are_bounded_and_evicted() {
    for (n, ty) in ["hash", "set", "zset", "list"].into_iter().enumerate() {
        let port = 16435 + n as u16;
        let dir = test_dir(ty);
        let server = spawn(port, &dir, &[("RADIXOX_MAXMEMORY", "48mb")]);
        let mut c = common::conn(port);
        // 500 éléments de 200 o par clé
        let write = |p: &mut redis::Pipeline, prefix: &str, i: usize| {
            let (key, member) = (format!("{prefix}{}", i / 500), format!("{i:0>200}"));
            match ty {
                "hash" => p.cmd("HSET").arg(key).arg(member).arg("v"),
                "set" => p.cmd("SADD").arg(key).arg(member),
                "zset" => p.cmd("ZADD").arg(key).arg(i).arg(member),
                _ => p.cmd("LPUSH").arg(key).arg(member),
            }
            .ignore();
        };
        let mut written = 0;
        while num(&info(&mut c), "oom_rejected_commands") == 0 {
            assert!(written < 400_000, "{ty} : pas de -OOM après 80 Mo d'éléments");
            fill(&mut c, 1, |p, i| write(p, "c", written + i));
            written += 1000;
        }
        assert!(written > 20_000, "{ty} : -OOM dès {written} éléments");
        assert!(server.peak_anon() <= 48 * MB, "{ty} : pic {} o pour 48 Mo", server.peak_anon());
        let len_cmd = match ty {
            "hash" => "HLEN",
            "set" => "SCARD",
            "zset" => "ZCARD",
            _ => "LLEN",
        };
        let len: u64 = redis::cmd(len_cmd).arg("c0").query(&mut c).unwrap();
        assert_eq!(len, 500, "{ty} : lecture à la limite");
        if ty == "zset" {
            let mut cmd = redis::cmd("ZUNIONSTORE");
            let keys = written / 500;
            cmd.arg("dst").arg(keys);
            for k in 0..keys {
                cmd.arg(format!("c{k}"));
            }
            let err = cmd.query::<u64>(&mut c).expect_err("ZUNIONSTORE de toute la base à la limite");
            assert!(err.to_string().contains("OOM"), "{err}");
            let exists: u64 = redis::cmd("EXISTS").arg("dst").query(&mut c).unwrap();
            assert_eq!(exists, 0, "refus sans exécution partielle");
        }
        // allkeys-random : de nouvelles collections passent, des anciennes partent entières
        let _: () = redis::cmd("CONFIG").arg("SET").arg("maxmemory-policy").arg("allkeys-random").query(&mut c).unwrap();
        let oom0 = num(&info(&mut c), "oom_rejected_commands");
        fill(&mut c, 100, |p, i| write(p, "d", i));
        let m = settle(&mut c);
        let refused = num(&m, "oom_rejected_commands") - oom0;
        assert!(num(&m, "evicted_keys") > 0, "{ty} : rien d'évincé");
        assert!(refused < 5_000, "{ty} : {refused} refus sur 100 000 avec éviction");
        assert!(server.peak_anon() <= 48 * MB, "{ty} : pic {} o pour 48 Mo", server.peak_anon());
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `SET` d'une valeur de `mb` Mo sur une connexion neuve, réponse lue pendant l'envoi
/// (un client qui n'écrit qu'après ne verrait pas le refus, envoyé pendant qu'il écrit
/// encore). `Err` : connexion fermée sans réponse lisible.
fn set_big(port: u16, mb: usize) -> Result<Vec<u8>, String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut w = s.try_clone().unwrap();
    let size = mb << 20;
    let sender = std::thread::spawn(move || {
        let _ = w.write_all(format!("*3\r\n$3\r\nSET\r\n$3\r\nbig\r\n${size}\r\n").as_bytes());
        let _ = w.write_all(&vec![b'x'; size]);
        let _ = w.write_all(b"\r\n");
    });
    let mut reply = vec![0u8; 256];
    let r = s.read(&mut reply).map_err(|e| e.to_string());
    drop(s);
    sender.join().unwrap();
    let n = r?;
    reply.truncate(n);
    Ok(reply)
}

/// Frame plus grosse que le budget d'entrée (10 % de `maxmemory`) : best effort. Admise si
/// elle tient (2 × sa taille plus le budget d'entrée, sous la limite), sinon `-OOM` et
/// fermeture ; jamais suspendue sans fin (avant : un `SET` de 20 Mo sous 48 Mo restait sans
/// réponse).
#[test]
fn frames_larger_than_the_input_budget() {
    const PORT: u16 = 16444;
    let dir = test_dir("bigframe");
    let server = spawn(PORT, &dir, &[("RADIXOX_MAXMEMORY", "48mb")]);
    // 10 Mo > 4,8 Mo de budget : admise
    assert_eq!(set_big(PORT, 10).unwrap(), b"+OK\r\n");
    let mut c = common::conn(PORT);
    let v: Vec<u8> = redis::cmd("GET").arg("big").query(&mut c).unwrap();
    assert_eq!(v.len(), 10 << 20);
    let _: u64 = redis::cmd("DEL").arg("big").query(&mut c).unwrap();
    // 30 Mo : ne tient pas, refus rapide (ou fermeture avant que la réponse soit lue)
    let t = Instant::now();
    match set_big(PORT, 30) {
        Ok(reply) => assert!(String::from_utf8_lossy(&reply).starts_with("-OOM"), "{reply:?}"),
        Err(e) => assert!(!e.contains("timed out") && !e.contains("temporarily"), "pas de réponse : {e}"),
    }
    assert!(t.elapsed() < Duration::from_secs(5), "refus en {:?}", t.elapsed());
    let _: String = redis::cmd("PING").query(&mut c).unwrap();
    assert!(num(&info(&mut c), "oom_rejected_commands") >= 1);
    assert!(server.peak_anon() <= 48 * MB, "pic {} o pour 48 Mo", server.peak_anon());
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn digest(port: u16, at: u64) -> String {
    redis::cmd("DEBUG").arg("DIGEST").arg(at).query(&mut common::conn(port)).unwrap()
}

/// Le réplica a appliqué le dernier bloc du primaire (voir `tests/replication.rs`).
fn wait_caught_up(primary: u16, replica: u16) {
    let end = Instant::now() + Duration::from_secs(20);
    let (mut p, mut r) = (common::conn(primary), common::conn(replica));
    loop {
        let (pi, ri) = (info(&mut p), info(&mut r));
        if pi.get("connected_replicas").map(String::as_str) == Some("1")
            && ri.get("master_link_status").map(String::as_str) == Some("up")
            && pi.get("repl_seq").is_some()
            && pi.get("repl_seq") == ri.get("repl_seq")
        {
            return;
        }
        assert!(Instant::now() < end, "réplica pas à jour");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// L'éviction n'est pas déterministe : chaque clé évincée part en `DEL` dans le journal.
/// Le réplica (sans limite) finit avec le même contenu que le primaire et n'évince rien
/// lui-même ; le primaire relancé depuis son AOF aussi.
#[test]
fn evictions_reach_the_aof_and_the_replica() {
    const PRIMARY: u16 = 16440;
    const REPLICA: u16 = 16441;
    let (pdir, rdir) = (test_dir("evict-p"), test_dir("evict-r"));
    let replica = spawn(REPLICA, &rdir, &[]);
    let target = format!("127.0.0.1:{REPLICA}");
    let aof = [("RADIXOX_APPENDONLY", "yes"), ("RADIXOX_APPENDFSYNC", "always")];
    let mut env = vec![
        ("RADIXOX_MAXMEMORY", "48mb"),
        ("RADIXOX_MAXMEMORY_POLICY", "allkeys-random"),
        ("RADIXOX_REPLICATE", target.as_str()),
        // le backlog par défaut (64 Mo) prendrait à lui seul toute la limite
        ("RADIXOX_REPL_BACKLOG_MB", "4"),
        ("RADIXOX_REPL_ONLINE_LAG_MB", "2"),
    ];
    env.extend(aof);
    let primary = spawn(PRIMARY, &pdir, &env);
    let mut c = common::conn(PRIMARY);
    let val = vec![b'v'; 1000];
    fill(&mut c, 120, |p, i| {
        match i % 10 {
            0 => p.cmd("HSET").arg(format!("h{}", i / 1000)).arg(i).arg(&val[..200]),
            1 => p.cmd("ZADD").arg(format!("z{}", i / 1000)).arg(i).arg(format!("{i:0>200}")),
            _ => p.cmd("SET").arg(format!("k{i}")).arg(&val),
        }
        .ignore();
    });
    let m = settle(&mut c);
    assert!(num(&m, "evicted_keys") > 10_000, "{} évictions", m["evicted_keys"]);
    wait_caught_up(PRIMARY, REPLICA);
    let at = unix_now();
    let want = digest(PRIMARY, at);
    assert_eq!(digest(REPLICA, at), want, "réplica différent du primaire après éviction");
    assert_eq!(num(&info(&mut common::conn(REPLICA)), "evicted_keys"), 0, "le réplica n'évince pas");
    // le primaire repart de son AOF (fsync always : tout l'acquitté y est)
    drop(primary);
    let _primary = spawn(PRIMARY, &pdir, &aof);
    assert_eq!(digest(PRIMARY, at), want, "AOF rejoué différent du primaire");
    drop(replica);
    let _ = std::fs::remove_dir_all(&pdir);
    let _ = std::fs::remove_dir_all(&rdir);
}

/// `CONFIG SET maxmemory` sous l'usage courant : `noeviction` refuse les écritures et sert
/// le reste ; une politique d'éviction redescend seule à la cible, sans écriture ; rien
/// d'évinçable : le serveur reste réactif (pas de tranche en boucle).
#[test]
fn lowering_maxmemory_at_runtime() {
    const PORT: u16 = 16442;
    let dir = test_dir("lower");
    let _server = spawn(PORT, &dir, &[]);
    let mut c = common::conn(PORT);
    let val = vec![b'v'; 1000];
    fill(&mut c, 30, |p, i| {
        p.cmd("SET").arg(format!("k{i}")).arg(&val).ignore();
    });
    let _: () = redis::cmd("CONFIG").arg("SET").arg("maxmemory").arg("32mb").query(&mut c).unwrap();
    let err = redis::cmd("SET").arg("new").arg(&val).query::<()>(&mut c).unwrap_err();
    assert!(err.to_string().contains("OOM"), "{err}");
    let v: Vec<u8> = redis::cmd("GET").arg("k1").query(&mut c).unwrap();
    assert_eq!(v, val);
    let n: u64 = redis::cmd("DEL").arg("k1").query(&mut c).unwrap();
    assert_eq!(n, 1);
    // éviction de fond, déclenchée par le minuteur seul
    let _: () = redis::cmd("CONFIG").arg("SET").arg("maxmemory-policy").arg("allkeys-random").query(&mut c).unwrap();
    let m = settle(&mut c);
    assert!(num(&m, "evicted_keys") > 0);
    assert!(num(&m, "mem_live_estimate") <= target(&m), "pas redescendu sous la cible");
    assert_not_over_evicted(&m);
    let _: () = redis::cmd("SET").arg("new").arg(&val).query(&mut c).unwrap();
    // rien d'évinçable (aucune clé avec TTL) sous une limite plus basse
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("maxmemory-policy")
        .arg("volatile-random")
        .arg("maxmemory")
        .arg("24mb")
        .query(&mut c)
        .unwrap();
    let evicted = num(&info(&mut c), "evicted_keys");
    std::thread::sleep(Duration::from_millis(500));
    for _ in 0..20 {
        let t = Instant::now();
        let _: String = redis::cmd("PING").query(&mut c).unwrap();
        assert!(t.elapsed() < Duration::from_millis(50), "PING en {:?}", t.elapsed());
    }
    assert_eq!(num(&info(&mut c), "evicted_keys"), evicted);
    assert!(redis::cmd("SET").arg("more").arg(&val).query::<()>(&mut c).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `volatile-random` ne touche jamais une clé sans TTL ; `volatile-ttl` prend surtout les
/// échéances proches.
#[test]
fn volatile_policies_take_only_keys_with_a_ttl() {
    const PORT: u16 = 16443;
    let val = vec![b'v'; 1000];
    let dir = test_dir("volatile-random");
    let server = spawn(PORT, &dir, &[("RADIXOX_MAXMEMORY", "48mb"), ("RADIXOX_MAXMEMORY_POLICY", "volatile-random")]);
    let mut c = common::conn(PORT);
    fill(&mut c, 10, |p, i| {
        p.cmd("SET").arg(format!("plain{i}")).arg(&val).ignore();
    });
    fill(&mut c, 100, |p, i| {
        p.cmd("SET").arg(format!("t{i}")).arg(&val).arg("EX").arg(3600).ignore();
    });
    let m = settle(&mut c);
    assert!(num(&m, "evicted_keys") > 20_000, "{} évictions", m["evicted_keys"]);
    assert_not_over_evicted(&m);
    let mut p = redis::pipe();
    for i in 0..10_000 {
        p.cmd("EXISTS").arg(format!("plain{i}"));
    }
    let alive: Vec<u64> = p.query(&mut c).unwrap();
    assert_eq!(alive.iter().sum::<u64>(), 10_000, "clé sans TTL évincée");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);

    let dir = test_dir("volatile-ttl");
    let server = spawn(PORT, &dir, &[("RADIXOX_MAXMEMORY", "48mb"), ("RADIXOX_MAXMEMORY_POLICY", "volatile-ttl")]);
    let mut c = common::conn(PORT);
    // échéances mélangées : l'ordre d'insertion ne dit rien de l'échéance
    let ttl = |i: usize| 1000 + (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) % 100_000;
    fill(&mut c, 80, |p, i| {
        p.cmd("SET").arg(format!("t{i}")).arg(&val).arg("EX").arg(ttl(i)).ignore();
    });
    settle(&mut c);
    let mut p = redis::pipe();
    for i in 0..80_000 {
        p.cmd("EXISTS").arg(format!("t{i}"));
    }
    let alive: Vec<u64> = p.query(&mut c).unwrap();
    let mean = |keep: u64| {
        let v: Vec<u64> = (0..80_000).filter(|&i| alive[i] == keep).map(ttl).collect();
        assert!(!v.is_empty());
        v.iter().sum::<u64>() / v.len() as u64
    };
    let (gone, kept) = (mean(0), mean(1));
    assert!(gone * 10 < kept * 8, "échéance moyenne : évincées {gone} s, restantes {kept} s");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Feature `eviction`, `allkeys-lru` puis `volatile-lru` : des clés relues pendant le
/// remplissage survivent presque toutes, les autres partent.
#[cfg(feature = "eviction")]
#[test]
fn lru_keeps_hot_keys() {
    const PORT: u16 = 16434;
    for policy in ["allkeys-lru", "volatile-lru"] {
        let dir = test_dir(policy);
        let _server = spawn(PORT, &dir, &[("RADIXOX_MAXMEMORY", "48mb"), ("RADIXOX_MAXMEMORY_POLICY", policy)]);
        let mut c = common::conn(PORT);
        let val = vec![b'v'; 1000];
        let set = |p: &mut redis::Pipeline, key: String| {
            p.cmd("SET").arg(key).arg(&val).arg("EX").arg(3600).ignore();
        };
        let mut p = redis::pipe();
        for i in 0..1000 {
            set(&mut p, format!("hot{i}"));
        }
        p.query::<()>(&mut c).unwrap();
        fill(&mut c, 150, |p, i| {
            set(p, format!("k{i}"));
            if i % 2 == 0 {
                p.cmd("GET").arg(format!("hot{}", (i / 2) % 1000)).ignore();
            }
        });
        let mut alive = 0;
        for i in 0..1000 {
            alive += redis::cmd("EXISTS").arg(format!("hot{i}")).query::<i64>(&mut c).unwrap();
        }
        let evicted = num(&info(&mut c), "evicted_keys");
        assert!(evicted > 50_000, "{policy} : {evicted}");
        assert!(alive > 900, "{policy} : {alive} clés chaudes sur 1000 survivent");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
