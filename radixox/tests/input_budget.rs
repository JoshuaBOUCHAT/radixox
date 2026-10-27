//! Budget global d'entrée (RUNTIME_ARCH.md §5.3) : une connexion parquée qui reçoit sans
//! fin voit sa réception suspendue (backpressure TCP, pas de fermeture) ; tout passe une
//! fois qu'elle repart.

mod common;

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

const PORT: u16 = 16413;

fn info_field(c: &mut redis::Connection, key: &str) -> i64 {
    let s: String = redis::cmd("INFO").query(c).unwrap();
    s.lines().find_map(|l| l.strip_prefix(&format!("{key}:"))).and_then(|v| v.trim().parse().ok()).unwrap()
}

#[test]
fn parked_flood_is_throttled_then_drains() {
    common::start_server_env(PORT, &[("RADIXOX_INPUT_BUDGET_MB", "1")]);
    let mut c = common::conn(PORT);

    // BLPOP parque la connexion ; tout ce qui suit attend dans `rx`
    let n = 16_000;
    let val = "x".repeat(1000);
    let mut flood = b"*3\r\n$5\r\nBLPOP\r\n$1\r\nq\r\n$1\r\n0\r\n".to_vec();
    for i in 0..n {
        let k = format!("k:{i}");
        flood.extend_from_slice(format!("*3\r\n$3\r\nSET\r\n${}\r\n{k}\r\n${}\r\n{val}\r\n", k.len(), val.len()).as_bytes());
    }
    let total = flood.len();
    let mut raw = TcpStream::connect(("127.0.0.1", PORT)).unwrap();
    let mut reader = raw.try_clone().unwrap();
    let writer = std::thread::spawn(move || raw.write_all(&flood).unwrap());
    // les réponses sont lues pour ne pas mêler la sortie à l'essai
    std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));

    let mut throttled = 0;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        throttled = info_field(&mut c, "input_budget_throttled_clients");
        if throttled >= 1 {
            break;
        }
    }
    assert_eq!(throttled, 1, "réception suspendue attendue");
    std::thread::sleep(Duration::from_millis(300));
    let held = info_field(&mut c, "clients_input_bytes");
    // budget + ce que l'anneau (64 × 32 Kio) a livré avant l'annulation
    assert!(held < 4 << 20, "entrée tenue : {held} o pour {total} o envoyés");

    // le BLPOP servi, la connexion repart : ce qui attendait s'exécute, la réception
    // reprend sous 90 % du budget, tout le flot passe
    let _: () = redis::cmd("LPUSH").arg("q").arg("go").query(&mut c).unwrap();
    writer.join().unwrap();
    let last = format!("k:{}", n - 1);
    let mut done = false;
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(50));
        let e: bool = redis::cmd("EXISTS").arg(&last).query(&mut c).unwrap();
        if e {
            done = true;
            break;
        }
    }
    assert!(done, "le flot entier est passé");
    let size: i64 = redis::cmd("DBSIZE").query(&mut c).unwrap();
    assert_eq!(size, n as i64);
    assert!(info_field(&mut c, "input_budget_throttle_events") >= 1);
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
}
