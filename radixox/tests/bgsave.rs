//! BGSAVE de bout en bout : le serveur écrit `dump.rxd` dans `RADIXOX_DIR`,
//! le fichier se relit avec `check_footer` + `parse_dump`.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oxidart::{Value, check_footer, parse_dump};

const PORT: u16 = 6412;
const PORT_ERR: u16 = 6413;

struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(dir: &PathBuf) -> Server {
    start_on(PORT, dir)
}

fn start_on(port: u16, dir: &PathBuf) -> Server {
    let _ = Command::new("fuser")
        .args(["-k", &format!("{port}/tcp")])
        .stderr(Stdio::null())
        .status();
    std::thread::sleep(Duration::from_millis(150));
    let child = Command::new(env!("CARGO_BIN_EXE_radixox"))
        .env("RADIXOX_PORT", port.to_string())
        .env("RADIXOX_DIR", dir)
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

    let reply: String = redis::cmd("BGSAVE").query(&mut c).unwrap();
    assert_eq!(reply, "Background saving started");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match redis::cmd("BGSAVE").query::<String>(&mut c) {
            Ok(r) => {
                assert_eq!(r, "Background saving started");
                break;
            }
            Err(e) => assert!(e.to_string().contains("in progress"), "{e}"),
        }
        assert!(Instant::now() < deadline, "save jamais libérée après l'échec");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!dir.exists());
}
