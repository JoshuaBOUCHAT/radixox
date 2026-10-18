mod common;

use std::sync::OnceLock;

const PORT: u16 = 16390;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| {
        common::start_server_env(PORT, &[("RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT", "2")]);
        let mut c = common::conn(PORT);
        let mut p = redis::pipe();
        for i in 0..1500 {
            p.cmd("SET").arg(format!("scan:{i:05}")).arg("v").ignore();
        }
        p.query::<()>(&mut c).unwrap();
    });
    common::conn(PORT)
}

fn scan(c: &mut redis::Connection, cursor: u64, extra: &[&str]) -> redis::RedisResult<(u64, Vec<String>)> {
    redis::cmd("SCAN").arg(cursor).arg("MATCH").arg("scan:*").arg(extra).query(c)
}

/// Sans COUNT : pages de 1000 (Redis prend 10, une simple indication).
#[test]
fn default_page_is_1000_keys() {
    let mut c = server();
    let (cursor, keys) = scan(&mut c, 0, &[]).unwrap();
    assert_eq!(keys.len(), 1000);
    assert_ne!(cursor, 0);
    let (cursor, rest) = scan(&mut c, cursor, &[]).unwrap();
    assert_eq!((cursor, rest.len()), (0, 500));
    // COUNT explicite : exact
    let (_, keys) = scan(&mut c, 0, &["COUNT", "7"]).unwrap();
    assert_eq!(keys.len(), 7);
}

/// Au plus 2 curseurs actifs par connexion (variable d'environnement du
/// serveur de test) ; les autres connexions ne sont pas touchées, et un scan
/// terminé rend sa place.
#[test]
fn per_connection_cursor_limit() {
    let mut a = server();
    let (c1, _) = scan(&mut a, 0, &["COUNT", "1"]).unwrap();
    let (c2, _) = scan(&mut a, 0, &["COUNT", "1"]).unwrap();
    assert!(c1 != 0 && c2 != 0);
    let err = scan(&mut a, 0, &["COUNT", "1"]).unwrap_err();
    assert!(err.to_string().contains("max number of SCAN cursors for this connection"), "{err}");
    // un scan qui finit en une page n'a pas besoin de curseur
    let (done, keys) = scan(&mut a, 0, &["COUNT", "5000"]).unwrap();
    assert_eq!((done, keys.len()), (0, 1500));

    let mut b = server();
    assert!(scan(&mut b, 0, &["COUNT", "1"]).unwrap().0 != 0, "autre connexion libre");
    // les curseurs de `a` se reprennent depuis `b`, et y restent comptés à `a`
    let (next, _) = scan(&mut b, c1, &["COUNT", "5000"]).unwrap();
    assert_eq!(next, 0);
    assert!(scan(&mut a, 0, &["COUNT", "1"]).unwrap().0 != 0, "la place libérée est rendue à `a`");
    let _ = c2;
}
