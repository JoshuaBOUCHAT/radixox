//! `maxmemory` (MEMORY_ARCH.md §10.5) : borne du total, `-OOM` sur les écritures qui
//! grossissent, lectures et suppressions toujours servies, refus de démarrer sur des
//! données trop grosses.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn info(c: &mut redis::Connection) -> std::collections::HashMap<String, String> {
    let s: String = redis::cmd("INFO").query(c).unwrap();
    s.lines().filter_map(|l| l.split_once(':')).map(|(k, v)| (k.to_string(), v.trim().to_string())).collect()
}

const MB: u64 = 1 << 20;

#[test]
fn growth_is_refused_at_the_limit_reads_and_deletes_are_not() {
    const PORT: u16 = 16431;
    common::start_server_env(PORT, &[("RADIXOX_MAXMEMORY", "48mb")]);
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
    let used: u64 = m["used_memory"].parse().unwrap();
    let rss: u64 = m["mem_rss_anon_bytes"].parse().unwrap();
    assert!(used <= 48 * MB && rss <= 48 * MB, "registre {used} o, RSS {rss} o pour 48 Mo");
    assert!(m["oom_rejected_commands"].parse::<u64>().unwrap() >= 1);
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
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
}

#[test]
fn startup_is_refused_when_the_data_does_not_fit() {
    const PORT: u16 = 16432;
    let dir = std::env::temp_dir().join(format!("radixox-maxmem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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
/// mémoire reste sous `maxmemory`. `volatile-ttl` sans aucune clé avec TTL : rien
/// d'évinçable, `-OOM`.
#[test]
fn eviction_keeps_writes_going_under_the_limit() {
    const PORT: u16 = 16433;
    common::start_server_env(PORT, &[("RADIXOX_MAXMEMORY", "48mb"), ("RADIXOX_MAXMEMORY_POLICY", "allkeys-random")]);
    let mut c = common::conn(PORT);
    let val = vec![b'v'; 1000];
    for batch in 0..150 {
        let mut p = redis::pipe();
        for i in 0..1000 {
            p.cmd("SET").arg(format!("k{}", batch * 1000 + i)).arg(&val).ignore();
        }
        // une réponse -OOM fait échouer le pipeline côté client : comptés par INFO
        let _ = p.query::<()>(&mut c);
    }
    let m = info(&mut c);
    let oom: u64 = m["oom_rejected_commands"].parse().unwrap();
    let ok = 150_000 - oom;
    let evicted: u64 = m["evicted_keys"].parse().unwrap();
    let rss: u64 = m["mem_rss_anon_bytes"].parse().unwrap();
    assert!(evicted > 50_000, "{evicted} clés évincées");
    assert!(ok > 140_000, "{ok} acceptés, {oom} refusés");
    assert!(rss <= 48 * MB, "RSS {rss} o pour 48 Mo");
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
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
}
