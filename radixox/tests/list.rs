//! List et commandes bloquantes (ANALYSE_ART_UNIFIE.md §14). Le détail des réponses est
//! couvert par le différentiel contre Valkey (`scripts/list_vs_valkey.py`) ; ici, l'attente
//! et le service direct.

mod common;

use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use redis::Commands;

const PORT: u16 = 16397;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

fn del(c: &mut redis::Connection, keys: &[&str]) {
    let _: () = redis::cmd("DEL").arg(keys).query(c).unwrap();
}

/// Lance une commande bloquante sur sa propre connexion ; rend sa réponse.
fn blocking<T: redis::FromRedisValue + Send + 'static>(args: Vec<String>) -> thread::JoinHandle<T> {
    let mut c = server();
    let h = thread::spawn(move || {
        let mut cmd = redis::cmd(&args[0]);
        for a in &args[1..] {
            cmd.arg(a);
        }
        cmd.query(&mut c).unwrap()
    });
    // le temps qu'elle s'inscrive
    thread::sleep(Duration::from_millis(100));
    h
}

fn args(s: &str) -> Vec<String> {
    s.split(' ').map(String::from).collect()
}

#[test]
fn push_serves_waiters_directly_rest_goes_in_the_list() {
    let mut c = server();
    del(&mut c, &["l:hand"]);
    let a = blocking::<(String, String)>(args("BLPOP l:hand 5"));
    let b = blocking::<(String, String)>(args("BRPOP l:hand 5"));
    // la liste qu'aurait faite le push : [c, b, a] ; BLPOP prend c, BRPOP prend a
    let n: i64 = redis::cmd("LPUSH").arg("l:hand").arg(&["a", "b", "c"][..]).query(&mut c).unwrap();
    assert_eq!(n, 3, "longueur après le push, avant le service (comme Redis)");
    assert_eq!(a.join().unwrap(), ("l:hand".into(), "c".into()));
    assert_eq!(b.join().unwrap(), ("l:hand".into(), "a".into()));
    let rest: Vec<String> = c.lrange("l:hand", 0, -1).unwrap();
    assert_eq!(rest, vec!["b"]);
}

#[test]
fn everything_served_leaves_no_key() {
    let mut c = server();
    del(&mut c, &["l:solo"]);
    let a = blocking::<(String, String)>(args("BLPOP l:solo 5"));
    let _: i64 = c.rpush("l:solo", "x").unwrap();
    assert_eq!(a.join().unwrap().1, "x");
    let exists: bool = c.exists("l:solo").unwrap();
    assert!(!exists);
}

#[test]
fn waiters_are_served_in_arrival_order() {
    let mut c = server();
    del(&mut c, &["l:fifo"]);
    let w: Vec<_> = (0..3).map(|_| blocking::<Option<(String, String)>>(args("BLPOP l:fifo 1"))).collect();
    let _: i64 = redis::cmd("RPUSH").arg("l:fifo").arg(&["1", "2"][..]).query(&mut c).unwrap();
    let got: Vec<Option<String>> = w.into_iter().map(|h| h.join().unwrap().map(|r| r.1)).collect();
    assert_eq!(got, vec![Some("1".into()), Some("2".into()), None], "le troisième finit par le délai");
}

#[test]
fn timeout_answers_null() {
    let mut c = server();
    del(&mut c, &["l:none"]);
    let t = Instant::now();
    let r: Option<(String, String)> = redis::cmd("BLPOP").arg("l:none").arg(0.3).query(&mut c).unwrap();
    assert!(r.is_none());
    let e = t.elapsed();
    assert!(e >= Duration::from_millis(280) && e < Duration::from_secs(2), "{e:?}");
    // la connexion reprend normalement
    let pong: String = redis::cmd("PING").query(&mut c).unwrap();
    assert_eq!(pong, "PONG");
}

#[test]
fn several_keys_first_producer_wins() {
    let mut c = server();
    del(&mut c, &["l:k1", "l:k2"]);
    let w = blocking::<(String, String)>(args("BLPOP l:k1 l:k2 5"));
    let _: i64 = c.rpush("l:k2", "deux").unwrap();
    assert_eq!(w.join().unwrap(), ("l:k2".into(), "deux".into()));
    // servi : son inscription sur l:k1 ne prend plus rien
    let _: i64 = c.rpush("l:k1", "un").unwrap();
    let k1: Vec<String> = c.lrange("l:k1", 0, -1).unwrap();
    assert_eq!(k1, vec!["un"]);
}

#[test]
fn blmove_chains_to_the_next_waiter() {
    let mut c = server();
    del(&mut c, &["l:src", "l:mid"]);
    let mv = blocking::<String>(args("BLMOVE l:src l:mid LEFT RIGHT 5"));
    let pop = blocking::<(String, String)>(args("BLPOP l:mid 5"));
    let _: i64 = c.rpush("l:src", "x").unwrap();
    assert_eq!(mv.join().unwrap(), "x");
    assert_eq!(pop.join().unwrap(), ("l:mid".into(), "x".into()));
    let (src, mid): (bool, bool) = (c.exists("l:src").unwrap(), c.exists("l:mid").unwrap());
    assert!(!src && !mid);
}

#[test]
fn bzpopmin_served_by_zadd() {
    let mut c = server();
    del(&mut c, &["z:block"]);
    let w = blocking::<(String, String, String)>(args("BZPOPMIN z:block 5"));
    let _: i64 = redis::cmd("ZADD").arg("z:block").arg(&["3", "c", "1", "a", "2", "b"][..]).query(&mut c).unwrap();
    assert_eq!(w.join().unwrap(), ("z:block".into(), "a".into(), "1".into()));
    let rest: Vec<String> = c.zrange("z:block", 0, -1).unwrap();
    assert_eq!(rest, vec!["b", "c"]);
}

#[test]
fn dead_waiter_is_skipped() {
    let mut c = server();
    del(&mut c, &["l:dead"]);
    {
        let mut gone = server();
        let _: () = redis::cmd("CLIENT").arg("SETNAME").arg("gone").query(&mut gone).unwrap();
        // bloque puis la connexion se ferme sans réponse
        let mut raw = std::net::TcpStream::connect(("127.0.0.1", PORT)).unwrap();
        use std::io::Write;
        raw.write_all(b"BLPOP l:dead 0\r\n").unwrap();
        thread::sleep(Duration::from_millis(100));
        drop(raw);
        drop(gone);
        thread::sleep(Duration::from_millis(100));
    }
    let w = blocking::<(String, String)>(args("BLPOP l:dead 5"));
    let _: i64 = c.rpush("l:dead", "v").unwrap();
    assert_eq!(w.join().unwrap().1, "v", "la connexion fermée n'a rien pris");
}

#[test]
fn commands_after_blpop_wait_for_it() {
    let _ = server();
    let mut c = server();
    del(&mut c, &["l:pipe"]);
    let h = thread::spawn(|| {
        let mut raw = std::net::TcpStream::connect(("127.0.0.1", PORT)).unwrap();
        use std::io::{Read, Write};
        raw.write_all(b"BLPOP l:pipe 5\r\nPING\r\n").unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        while !got.ends_with(b"+PONG\r\n") {
            let n = raw.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    thread::sleep(Duration::from_millis(150));
    let _: i64 = c.rpush("l:pipe", "v").unwrap();
    let got = h.join().unwrap();
    assert_eq!(got, b"*2\r\n$6\r\nl:pipe\r\n$1\r\nv\r\n+PONG\r\n");
}
