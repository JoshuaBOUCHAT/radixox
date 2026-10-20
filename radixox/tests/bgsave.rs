//! BGSAVE de bout en bout : le serveur écrit `dump.rxd` dans `RADIXOX_DIR`,
//! le fichier se relit avec `check_footer` + `parse_dump`.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oxidart::{Value, check_footer, parse_dump};

mod common;

const PORT: u16 = 6412;
const PORT_ERR: u16 = 6413;

struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Dump brut (le test relit le format avec `check_footer` + `parse_dump`).
fn start(dir: &PathBuf) -> Server {
    start_with(PORT, dir, &[("RADIXOX_SAVE_ZSTD", "0")])
}

fn start_on(port: u16, dir: &PathBuf) -> Server {
    start_with(port, dir, &[])
}

fn start_with(port: u16, dir: &PathBuf, env: &[(&str, &str)]) -> Server {
    let _ = Command::new("fuser")
        .args(["-k", &format!("{port}/tcp")])
        .stderr(Stdio::null())
        .status();
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
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server(child);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("radixox n'a pas démarré sur {port}");
}

#[test]
fn bgsave_writes_a_readable_dump() {
    let dir = std::env::temp_dir().join(format!("radixox-bgsave-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _server = start(&dir);
    let mut c = redis::Client::open(format!("redis://127.0.0.1:{PORT}"))
        .unwrap()
        .get_connection()
        .unwrap();

    // > 2 Mo de valeurs : plusieurs écritures.
    const N: usize = 30_000;
    let val = "x".repeat(100);
    let mut pipe = redis::pipe();
    for i in 0..N {
        pipe.cmd("SET").arg(format!("k:{i}")).arg(&val).ignore();
    }
    pipe.cmd("INCRBY").arg("n").arg(42).ignore();
    pipe.cmd("HSET").arg("h").arg("f1").arg("a").arg("f2").arg("b").ignore();
    pipe.cmd("SADD").arg("s").arg("m1").arg("m2").arg("m3").ignore();
    pipe.cmd("SET").arg("t").arg("ttl").arg("EX").arg(1000).ignore();
    pipe.query::<()>(&mut c).unwrap();

    let reply: String = redis::cmd("BGSAVE").query(&mut c).unwrap();
    assert_eq!(reply, "Background saving started");

    let path = dir.join("dump.rxd");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "dump.rxd absent après 10 s");
        std::thread::sleep(Duration::from_millis(20));
    }
    // `dump.rxd` n'apparaît qu'au rename, après le fsync du fichier : il est
    // complet dès qu'il existe.
    let file = std::fs::read(&path).unwrap();
    assert!(!dir.join("dump.rxd.tmp").exists());

    let dump = parse_dump(check_footer(&file).expect("pied valide")).expect("dump lisible");
    assert_eq!(dump.entries.len(), N + 4);
    let get = |k: &str| dump.entries.iter().find(|e| e.key == k.as_bytes()).unwrap();
    assert!(matches!(&get("k:123").value, Value::String(v) if v.as_ref() == val.as_bytes()));
    assert!(matches!(get("n").value, Value::Int(42)));
    match &get("h").value {
        Value::Hash(p) => assert_eq!(p.len(), 2),
        v => panic!("h : {v:?}"),
    }
    match &get("s").value {
        Value::Set(m) => assert_eq!(m.len(), 3),
        v => panic!("s : {v:?}"),
    }
    let t = get("t");
    assert!(matches!(t.ttl, Some(s) if s > 990 && s <= 1000), "ttl {:?}", t.ttl);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bgsave_failure_releases_the_save_slot() {
    // Dossier inexistant : la création du fichier échoue.
    let dir = std::env::temp_dir().join(format!("radixox-bgsave-absent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _server = start_on(PORT_ERR, &dir);
    let mut c = redis::Client::open(format!("redis://127.0.0.1:{PORT_ERR}"))
        .unwrap()
        .get_connection()
        .unwrap();
    redis::cmd("SET").arg("a").arg("b").query::<()>(&mut c).unwrap();

    // Le fichier est ouvert tout de suite : l'échec est répondu directement.
    match redis::cmd("BGSAVE").query::<String>(&mut c) {
        Ok(r) => assert_eq!(r, "Background saving started"),
        Err(e) => assert!(e.to_string().contains("No such file"), "{e}"),
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match redis::cmd("BGSAVE").query::<String>(&mut c) {
            Ok(r) => {
                assert_eq!(r, "Background saving started");
                break;
            }
            Err(e) if e.to_string().contains("No such file") => break,
            Err(e) => assert!(e.to_string().contains("in progress"), "{e}"),
        }
        assert!(Instant::now() < deadline, "save jamais libérée après l'échec");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!dir.exists());
}

const PORT_RELOAD: u16 = 6414;

/// Étape 8 : BGSAVE, arrêt, redémarrage sur le même `RADIXOX_DIR` ; tout est
/// relu (String, Int, Hash, Set, TTL).
#[test]
fn restart_reloads_the_dump() {
    let dir = std::env::temp_dir().join(format!("radixox-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("redis://127.0.0.1:{PORT_RELOAD}");
    const N: usize = 20_000;
    {
        let _server = start_on(PORT_RELOAD, &dir);
        let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
        let mut pipe = redis::pipe();
        for i in 0..N {
            pipe.cmd("SET").arg(format!("user:{i}")).arg(format!("valeur-{i}")).ignore();
        }
        pipe.cmd("INCRBY").arg("n").arg(-7).ignore();
        pipe.cmd("HSET").arg("h").arg("f1").arg("a").arg("f2").arg("b").ignore();
        pipe.cmd("SADD").arg("s").arg("m1").arg("m2").ignore();
        pipe.cmd("SET").arg("t").arg("ttl").arg("EX").arg(1000).ignore();
        pipe.query::<()>(&mut c).unwrap();
        let _: String = redis::cmd("BGSAVE").query(&mut c).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.join("dump.rxd").exists() {
            assert!(Instant::now() < deadline, "dump.rxd absent après 10 s");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let _server = start_on(PORT_RELOAD, &dir);
    let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
    let size: usize = redis::cmd("DBSIZE").query(&mut c).unwrap();
    assert_eq!(size, N + 4);
    for i in [0, 1, 10, 123, N - 1] {
        let v: String = redis::cmd("GET").arg(format!("user:{i}")).query(&mut c).unwrap();
        assert_eq!(v, format!("valeur-{i}"));
    }
    let n: i64 = redis::cmd("INCR").arg("n").query(&mut c).unwrap();
    assert_eq!(n, -6);
    let h: Vec<String> = redis::cmd("HMGET").arg("h").arg("f1").arg("f2").query(&mut c).unwrap();
    assert_eq!(h, ["a", "b"]);
    let s: usize = redis::cmd("SCARD").arg("s").query(&mut c).unwrap();
    assert_eq!(s, 2);
    let ttl: i64 = redis::cmd("TTL").arg("t").query(&mut c).unwrap();
    assert!(ttl > 990 && ttl <= 1000, "ttl {ttl}");
    // L'arbre rechargé accepte les écritures (splits sur des nœuds rechargés).
    redis::cmd("SET").arg("user:1x").arg("y").query::<()>(&mut c).unwrap();
    let v: String = redis::cmd("GET").arg("user:1").query(&mut c).unwrap();
    assert_eq!(v, "valeur-1");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Dump corrompu : le serveur refuse de démarrer (et ne l'écrase pas).
#[test]
fn corrupted_dump_prevents_startup() {
    let dir = std::env::temp_dir().join(format!("radixox-corrupt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("dump.rxd"), b"RXDB pas un dump").unwrap();
    let mut child = Command::new(common::server_bin())
        .env("RADIXOX_PORT", "6415")
        .env("RADIXOX_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn radixox");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("le serveur a démarré malgré un dump corrompu");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    assert_eq!(std::fs::read(dir.join("dump.rxd")).unwrap(), b"RXDB pas un dump");
    let _ = std::fs::remove_dir_all(&dir);
}

const PORT_ZSTD: u16 = 6416;

/// BGSAVE compressé (zstd 3, petit tampon : le DFS attend le compresseur),
/// puis redémarrage en mode brut : le format est reconnu au magic.
#[test]
fn compressed_bgsave_reloads() {
    let dir = std::env::temp_dir().join(format!("radixox-zstd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("redis://127.0.0.1:{PORT_ZSTD}");
    const N: usize = 50_000;
    {
        let env = [("RADIXOX_SAVE_ZSTD", "3"), ("RADIXOX_SAVE_BACKLOG_MB", "1")];
        let _server = start_with(PORT_ZSTD, &dir, &env);
        let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
        let mut pipe = redis::pipe();
        for i in 0..N {
            pipe.cmd("SET").arg(format!("user:{i}")).arg(format!("{{\"id\":{i},\"nom\":\"user{i}\"}}")).ignore();
        }
        pipe.cmd("HSET").arg("h").arg("f1").arg("a").ignore();
        pipe.query::<()>(&mut c).unwrap();
        let _: String = redis::cmd("BGSAVE").query(&mut c).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.join("dump.rxd").exists() {
            assert!(Instant::now() < deadline, "dump.rxd absent après 10 s");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let file = std::fs::read(dir.join("dump.rxd")).unwrap();
    assert_eq!(&file[..4], &[0x28, 0xB5, 0x2F, 0xFD], "pas une frame zstd");

    let _server = start_with(PORT_ZSTD, &dir, &[("RADIXOX_SAVE_ZSTD", "0")]);
    let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
    let size: usize = redis::cmd("DBSIZE").query(&mut c).unwrap();
    assert_eq!(size, N + 1);
    let v: String = redis::cmd("GET").arg("user:4242").query(&mut c).unwrap();
    assert_eq!(v, "{\"id\":4242,\"nom\":\"user4242\"}");
    let h: String = redis::cmd("HGET").arg("h").arg("f1").query(&mut c).unwrap();
    assert_eq!(h, "a");
    let _ = std::fs::remove_dir_all(&dir);
}

const PORT_SAVE: u16 = 6417;

/// SAVE bloquante : le fichier est en place quand la réponse arrive.
#[test]
fn save_is_synchronous_and_reloads() {
    let dir = std::env::temp_dir().join(format!("radixox-save-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("redis://127.0.0.1:{PORT_SAVE}");
    {
        let _server = start_on(PORT_SAVE, &dir);
        let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
        let mut pipe = redis::pipe();
        for i in 0..20_000 {
            pipe.cmd("SET").arg(format!("k:{i}")).arg(i).ignore();
        }
        pipe.query::<()>(&mut c).unwrap();
        let reply: String = redis::cmd("SAVE").query(&mut c).unwrap();
        assert_eq!(reply, "OK");
        assert!(dir.join("dump.rxd").exists(), "dump absent après SAVE");
        assert!(!dir.join("dump.rxd.tmp").exists());
    }
    let _server = start_on(PORT_SAVE, &dir);
    let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
    let size: usize = redis::cmd("DBSIZE").query(&mut c).unwrap();
    assert_eq!(size, 20_000);
    let v: String = redis::cmd("GET").arg("k:777").query(&mut c).unwrap();
    assert_eq!(v, "777");
    let _ = std::fs::remove_dir_all(&dir);
}

const PORT_SIG: u16 = 6418;

/// SIGTERM avec `RADIXOX_SAVE_ON_EXIT=1` : save, sortie propre, reload.
#[test]
fn sigterm_saves_before_exit() {
    let dir = std::env::temp_dir().join(format!("radixox-sigterm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("redis://127.0.0.1:{PORT_SIG}");
    let mut server = start_with(PORT_SIG, &dir, &[("RADIXOX_SAVE_ON_EXIT", "1")]);
    {
        let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
        redis::cmd("SET").arg("a").arg("1").query::<()>(&mut c).unwrap();
        redis::cmd("HSET").arg("h").arg("f").arg("v").query::<()>(&mut c).unwrap();
    }
    let pid = server.0.id().to_string();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(st) = server.0.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "le serveur ne s'est pas arrêté");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "{status:?}");
    assert!(dir.join("dump.rxd").exists());

    let _server = start_on(PORT_SIG, &dir);
    let mut c = redis::Client::open(url.as_str()).unwrap().get_connection().unwrap();
    let v: String = redis::cmd("GET").arg("a").query(&mut c).unwrap();
    assert_eq!(v, "1");
    let f: String = redis::cmd("HGET").arg("h").arg("f").query(&mut c).unwrap();
    assert_eq!(f, "v");
    let _ = std::fs::remove_dir_all(&dir);
}

const PORT_AUTO: u16 = 6419;
const PORT_MISCONF: u16 = 6420;

fn info_field(c: &mut redis::Connection, name: &str) -> String {
    let info: String = redis::cmd("INFO").query(c).unwrap();
    info.lines()
        .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(':')))
        .unwrap_or_else(|| panic!("{name} absent d'INFO"))
        .trim()
        .to_string()
}

/// `RADIXOX_SAVE="1 2"` : deux écritures et plus d'une seconde depuis le
/// démarrage déclenchent un BGSAVE ; INFO persistence et LASTSAVE suivent.
#[test]
fn autosave_rule_triggers_bgsave() {
    let dir = std::env::temp_dir().join(format!("radixox-autosave-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _server = start_with(PORT_AUTO, &dir, &[("RADIXOX_SAVE", "1 2")]);
    let mut c = redis::Client::open(format!("redis://127.0.0.1:{PORT_AUTO}")).unwrap().get_connection().unwrap();
    let boot: i64 = redis::cmd("LASTSAVE").query(&mut c).unwrap();
    redis::cmd("SET").arg("a").arg("1").query::<()>(&mut c).unwrap();
    assert_eq!(info_field(&mut c, "rdb_changes_since_last_save"), "1");
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(info_field(&mut c, "rdb_saves"), "0", "une seule écriture : la règle en demande deux");
    redis::cmd("MSET").arg("b").arg("2").arg("c").arg("3").query::<()>(&mut c).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while info_field(&mut c, "rdb_saves") != "1" {
        assert!(Instant::now() < deadline, "pas de save automatique");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(info_field(&mut c, "rdb_last_bgsave_status"), "ok");
    assert_eq!(info_field(&mut c, "rdb_changes_since_last_save"), "0");
    assert_eq!(info_field(&mut c, "rdb_bgsave_in_progress"), "0");
    let last: i64 = redis::cmd("LASTSAVE").query(&mut c).unwrap();
    assert!(last >= boot);
    assert!(dir.join("dump.rxd").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Dossier sans droit d'écriture : le BGSAVE échoue, les écritures sont
/// refusées (`stop-writes-on-bgsave-error`), les lectures passent.
#[test]
fn failed_bgsave_refuses_writes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("radixox-misconf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _server = start_with(PORT_MISCONF, &dir, &[("RADIXOX_SAVE", "3600 1")]);
    let mut c = redis::Client::open(format!("redis://127.0.0.1:{PORT_MISCONF}")).unwrap().get_connection().unwrap();
    redis::cmd("SET").arg("a").arg("1").query::<()>(&mut c).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let r: redis::RedisResult<String> = redis::cmd("BGSAVE").query(&mut c);
    assert!(r.is_err(), "BGSAVE dans un dossier en lecture seule");
    assert_eq!(info_field(&mut c, "rdb_last_bgsave_status"), "err");
    let w: redis::RedisResult<()> = redis::cmd("SET").arg("b").arg("2").query(&mut c);
    assert!(w.unwrap_err().to_string().contains("MISCONF"));
    let v: String = redis::cmd("GET").arg("a").query(&mut c).unwrap();
    assert_eq!(v, "1");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
