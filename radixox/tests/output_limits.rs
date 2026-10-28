//! Sortie bornée (RUNTIME_ARCH.md §6.2 bis) : limite par client normal (fermeture), budget
//! global (pause sans fermeture, toutes les réponses arrivent).

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn info_field(c: &mut redis::Connection, key: &str) -> i64 {
    let s: String = redis::cmd("INFO").query(c).unwrap();
    s.lines().find_map(|l| l.strip_prefix(&format!("{key}:"))).and_then(|v| v.trim().parse().ok()).unwrap()
}

#[test]
fn client_over_output_limit_is_closed() {
    const PORT: u16 = 16411;
    common::start_server_env(PORT, &[("RADIXOX_CLIENT_OUTPUT_LIMIT_MB", "1")]);
    let mut c = common::conn(PORT);
    let _: () = redis::cmd("SET").arg("big").arg(vec![b'x'; 3 << 20]).query(&mut c).unwrap();
    let _: () = redis::cmd("SET").arg("small").arg("ok").query(&mut c).unwrap();
    // Une grande valeur seule part depuis l'arbre sans tampon (RUNTIME_ARCH.md §6.2
    // quater) : elle ne coûte pas de mémoire de sortie, la limite ne s'applique pas.
    let v: Vec<u8> = redis::cmd("GET").arg("big").query(&mut c).unwrap();
    assert_eq!(v.len(), 3 << 20);
    for k in ["a", "b", "c"] {
        let _: () = redis::cmd("SET").arg(k).arg(vec![b'y'; 600 << 10]).query(&mut c).unwrap();
    }
    // une réponse construite dans le tampon (1,8 Mo) dépasse la limite : fermée sans réponse
    let r: redis::RedisResult<Vec<Vec<u8>>> = redis::cmd("MGET").arg("a").arg("b").arg("c").query(&mut c);
    assert!(r.is_err(), "connexion fermée attendue");
    // le serveur continue ; le reste passe
    let mut c = common::conn(PORT);
    let v: String = redis::cmd("GET").arg("small").query(&mut c).unwrap();
    assert_eq!(v, "ok");
    assert_eq!(info_field(&mut c, "client_output_limit_closed"), 1);
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
}

#[test]
fn output_budget_pauses_without_closing() {
    const PORT: u16 = 16412;
    common::start_server_env(PORT, &[("RADIXOX_OUTPUT_BUDGET_MB", "1")]);
    let mut c = common::conn(PORT);
    let val = vec![b'v'; 512 << 10];
    let _: () = redis::cmd("SET").arg("v").arg(&val).query(&mut c).unwrap();

    // 20 GET pipelinés, rien de lu : ~10 Mo de réponses voulues pour 1 Mo de budget
    let n = 20;
    let mut raw = TcpStream::connect(("127.0.0.1", PORT)).unwrap();
    raw.write_all(&b"*2\r\n$3\r\nGET\r\n$1\r\nv\r\n".repeat(n)).unwrap();
    let mut exceeded = 0;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(50));
        exceeded = info_field(&mut c, "output_budget_exceeded");
        if exceeded == 1 {
            break;
        }
    }
    assert_eq!(exceeded, 1, "budget dépassé attendu");
    assert!(info_field(&mut c, "output_budget_paused_clients") >= 1);
    // les autres clients sont servis pendant ce temps
    let _: () = redis::cmd("SET").arg("k").arg("1").query(&mut c).unwrap();

    // le client lent lit : toutes ses réponses arrivent, entières et dans l'ordre
    let one = format!("${}\r\n", val.len()).into_bytes();
    let want = n * (one.len() + val.len() + 2);
    let mut got = vec![0u8; want];
    raw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    raw.read_exact(&mut got).unwrap();
    for chunk in got.chunks(one.len() + val.len() + 2) {
        assert!(chunk.starts_with(&one) && chunk[one.len()..one.len() + val.len()] == val[..] && chunk.ends_with(b"\r\n"));
    }
    let mut back = 0;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(50));
        back = info_field(&mut c, "output_budget_exceeded");
        if back == 0 {
            break;
        }
    }
    assert_eq!(back, 0, "budget revenu sous la limite");
    assert_eq!(info_field(&mut c, "client_output_limit_closed"), 0);
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
}

/// KEYS dont la réponse dépasse la limite de sortie : une erreur, pas une fermeture
/// (rien n'est encore parti), et la connexion reste utilisable.
#[test]
fn keys_too_big_is_an_error_not_a_close() {
    const PORT: u16 = 16413;
    common::start_server_env(PORT, &[("RADIXOX_CLIENT_OUTPUT_LIMIT_MB", "1")]);
    let mut c = common::conn(PORT);
    let mut p = redis::pipe();
    for i in 0..40_000 {
        p.cmd("SET").arg(format!("some:long:key:name:{i:08}")).arg("v").ignore();
    }
    p.query::<()>(&mut c).unwrap();
    let r: redis::RedisResult<Vec<String>> = redis::cmd("KEYS").arg("*").query(&mut c);
    let e = r.expect_err("réponse > 1 Mo : erreur attendue");
    assert!(e.to_string().contains("use SCAN"), "{e}");
    let small: Vec<String> = redis::cmd("KEYS").arg("some:long:key:name:0000000*").query(&mut c).unwrap();
    assert_eq!(small.len(), 10);
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
}
