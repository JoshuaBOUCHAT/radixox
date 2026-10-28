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
    let n: i64 = redis::cmd("DEL").arg("k1").arg("k2").query(&mut c).unwrap();
    assert_eq!(n, 2);
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
