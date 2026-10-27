//! SHUTDOWN [NOSAVE|SAVE] [NOW] [FORCE] [ABORT], comme Redis : sortie après la save
//! demandée ; save impossible sans FORCE : erreur, le serveur continue.

mod common;

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn(port: u16, dir: &std::path::Path) -> Child {
    let _ = Command::new("fuser").args(["-k", &format!("{port}/tcp")]).stderr(Stdio::null()).status();
    std::thread::sleep(Duration::from_millis(150));
    let child = Command::new(common::server_bin())
        .env("RADIXOX_PORT", port.to_string())
        .env("RADIXOX_SAVE", "")
        .env("RADIXOX_DIR", dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t.elapsed() < Duration::from_secs(5), "le serveur ne démarre pas");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

/// Attend la sortie du processus ; son code.
fn exits(child: &mut Child) -> i32 {
    let t = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st.code().unwrap_or(-1);
        }
        assert!(t.elapsed() < Duration::from_secs(5), "le serveur ne s'arrête pas");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("radixox-shutdown-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn shutdown_nosave_exits_without_dump() {
    let dir = tmpdir("nosave");
    let mut child = spawn(16398, &dir);
    let mut c = common::conn(16398);
    let err = redis::cmd("SHUTDOWN").arg("ABORT").query::<()>(&mut c).unwrap_err();
    assert!(err.to_string().contains("No shutdown in progress"), "{err}");
    let _: () = redis::cmd("SET").arg("k").arg("v").query(&mut c).unwrap();
    // pas de réponse : la connexion se ferme avec le serveur
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
    assert_eq!(exits(&mut child), 0);
    assert!(!dir.join("dump.rxd").exists());
}

#[test]
fn shutdown_save_writes_the_dump() {
    let dir = tmpdir("save");
    let mut child = spawn(16399, &dir);
    let mut c = common::conn(16399);
    let _: i64 = redis::cmd("RPUSH").arg("l").arg(&["a", "b", "c"][..]).query(&mut c).unwrap();
    let _ = redis::cmd("SHUTDOWN").arg("SAVE").query::<()>(&mut c);
    assert_eq!(exits(&mut child), 0);
    assert!(dir.join("dump.rxd").exists());
    // rechargé au redémarrage
    let mut child = spawn(16399, &dir);
    let mut c = common::conn(16399);
    let l: Vec<String> = redis::cmd("LRANGE").arg("l").arg(0).arg(-1).query(&mut c).unwrap();
    assert_eq!(l, vec!["a", "b", "c"]);
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
    assert_eq!(exits(&mut child), 0);
}
