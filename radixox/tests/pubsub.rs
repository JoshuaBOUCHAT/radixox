//! Pub/Sub sur le fil, comportement commun aux deux serveurs : abonnement,
//! publication, réception, désabonnement puis retour au mode normal.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::OnceLock;
use std::time::Duration;

const PORT: u16 = 16396;

static INIT: OnceLock<()> = OnceLock::new();

fn conn() -> TcpStream {
    INIT.get_or_init(|| common::start_server(PORT));
    let s = TcpStream::connect(("127.0.0.1", PORT)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn expect(s: &mut TcpStream, want: &str) {
    let mut got = vec![0; want.len()];
    s.read_exact(&mut got).unwrap();
    assert_eq!(String::from_utf8_lossy(&got), want);
}

#[test]
fn subscribe_publish_unsubscribe() {
    let mut sub = conn();
    let mut publ = conn();
    sub.write_all(b"SUBSCRIBE ps:news\r\n").unwrap();
    expect(&mut sub, "*3\r\n$9\r\nsubscribe\r\n$7\r\nps:news\r\n:1\r\n");
    publ.write_all(b"PUBLISH ps:news hello\r\nPUBLISH ps:none x\r\n").unwrap();
    expect(&mut publ, ":1\r\n:0\r\n");
    expect(&mut sub, "*3\r\n$7\r\nmessage\r\n$7\r\nps:news\r\n$5\r\nhello\r\n");
    sub.write_all(b"UNSUBSCRIBE ps:news\r\n").unwrap();
    expect(&mut sub, "*3\r\n$11\r\nunsubscribe\r\n$7\r\nps:news\r\n:0\r\n");
    // Retour au mode normal.
    sub.write_all(b"SET ps:k v\r\nGET ps:k\r\n").unwrap();
    expect(&mut sub, "+OK\r\n$1\r\nv\r\n");
    publ.write_all(b"PUBLISH ps:news again\r\n").unwrap();
    expect(&mut publ, ":0\r\n");
}
