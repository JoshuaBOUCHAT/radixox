mod common;

use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use redis::Commands;

const PORT: u16 = 16386;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

/// Insère 99 999 clés `user:0`..`user:99998`, puis lance en parallèle :
///
/// - Thread 1 : `UNLINK user:*` — suppression chunked (~390 yields de 256 nœuds)
/// - Thread 2 : 10 ms après, `SET admin:1 alive` + `GET admin:1`
///
/// Le but : vérifier que `deln_async` relâche le borrow entre chaque chunk
/// (via `yield_now()`) et permet à d'autres connexions d'être servies pendant
/// l'opération longue. Si la boucle d'événements était bloquée, le SET+GET
/// du thread 2 stagnerait jusqu'à la fin de UNLINK.
#[test]
fn unlink_yields_to_concurrent_clients() {
    const N: usize = 99_999;

    // ── 1. Insertion de N clés ────────────────────────────────────────────
    let mut setup = server();
    let _: () = redis::cmd("FLUSHDB").query(&mut setup).unwrap();

    // Pipeline : tous les SETs en un seul aller-retour TCP
    let mut pipe = redis::pipe();
    for i in 0..N {
        pipe.set(format!("user:{i}"), i);
    }
    pipe.query::<()>(&mut setup).unwrap();

    let size: i64 = redis::cmd("DBSIZE").query(&mut setup).unwrap();
    assert_eq!(size, N as i64, "setup: attendu {N} clés, trouvé {size}");
    drop(setup);

    // ── 2. Thread 1 : UNLINK user:* (opération longue) ───────────────────
    let t1 = thread::spawn(|| {
        let mut c = common::conn(PORT);
        redis::cmd("UNLINK")
            .arg("user:*")
            .query::<i64>(&mut c)
            .unwrap()
    });

    // ── 3. Thread 2 : SET + GET admin:1 pendant que UNLINK tourne ────────
    // Petit délai pour laisser UNLINK démarrer sa phase de libération.
    thread::sleep(Duration::from_millis(10));

    let t2 = thread::spawn(|| {
        let mut c = common::conn(PORT);
        let t0 = Instant::now();
        let _: () = c.set("admin:1", "alive").unwrap();
        let val: String = c.get("admin:1").unwrap();
        (val, t0.elapsed())
    });

    let deleted = t1.join().expect("thread UNLINK paniqué");
    let (admin_val, admin_latency) = t2.join().expect("thread admin paniqué");

    println!("  → UNLINK a supprimé {deleted} clés");
    println!("  → SET+GET admin:1 latence : {admin_latency:?}");

    // ── 4. Assertions ─────────────────────────────────────────────────────

    // UNLINK doit avoir supprimé exactement N clés
    assert_eq!(
        deleted, N as i64,
        "UNLINK a supprimé {deleted} clés, attendu {N}"
    );

    // admin:1 doit être lisible même pendant UNLINK
    assert_eq!(admin_val, "alive", "admin:1 illisible pendant UNLINK");

    // SET+GET ne doit pas avoir stagné — si la boucle était bloquée on
    // attendrait plusieurs centaines de ms, voire un timeout
    assert!(
        admin_latency < Duration::from_millis(500),
        "SET+GET admin:1 a pris {:?} — la boucle d'événements semble bloquée pendant UNLINK",
        admin_latency
    );

    // ── 5. État final ──────────────────────────────────────────────────────
    let mut verify = server();

    let user_keys: Vec<String> = redis::cmd("KEYS").arg("user:*").query(&mut verify).unwrap();
    assert!(
        user_keys.is_empty(),
        "des clés user: subsistent après UNLINK : {:?}",
        &user_keys[..user_keys.len().min(5)]
    );

    let admin: String = verify.get("admin:1").unwrap();
    assert_eq!(admin, "alive", "admin:1 a disparu après UNLINK");

    let remaining: i64 = redis::cmd("DBSIZE").query(&mut verify).unwrap();
    assert_eq!(
        remaining, 1,
        "DBSIZE final attendu 1 (admin:1), trouvé {remaining}"
    );
}
