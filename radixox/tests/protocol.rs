//! Le découpage du flux sur le fil : commandes inline, noms inconnus, erreurs
//! de protocole (réponse puis fermeture, comme Redis).

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::OnceLock;
use std::time::Duration;

const PORT: u16 = 16395;

static INIT: OnceLock<()> = OnceLock::new();

fn conn() -> TcpStream {
    INIT.get_or_init(|| common::start_server(PORT));
    let s = TcpStream::connect(("127.0.0.1", PORT)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

/// Envoie `req`, lit jusqu'à la fermeture par le serveur.
fn until_close(req: &[u8]) -> String {
    let mut s = conn();
    s.write_all(req).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).expect("le serveur doit fermer la connexion");
    String::from_utf8_lossy(&out).into()
}

/// Envoie `req`, lit exactement `n` octets de réponse.
fn reply(req: &[u8], n: usize) -> String {
    let mut s = conn();
    s.write_all(req).unwrap();
    let mut out = vec![0; n];
    s.read_exact(&mut out).unwrap();
    String::from_utf8_lossy(&out).into()
}

#[test]
fn inline_commands() {
    let r = "+OK\r\n$3\r\na b\r\n+PONG\r\n";
    assert_eq!(reply(b"SET inline:k \"a b\"\r\nGET inline:k\r\n\r\nping\n", r.len()), r);
}

#[test]
fn unknown_and_bad_arguments_keep_the_connection() {
    let r = "-ERR unknown command 'dump'\r\n-ERR wrong number of arguments for 'get' command\r\n+PONG\r\n";
    assert_eq!(reply(b"DUMP k\r\nGET\r\nPING\r\n", r.len()), r);
}

#[test]
fn protocol_error_replies_then_closes() {
    // Les commandes valides d'avant répondent, puis l'erreur, puis la fermeture.
    let out = until_close(b"PING\r\n*1\r\n$-5\r\n");
    assert_eq!(out, "+PONG\r\n-ERR Protocol error: invalid bulk length\r\n");
    let out = until_close(b"*1\r\n$18446744073709551615\r\nGET\r\n");
    assert_eq!(out, "-ERR Protocol error: invalid bulk length\r\n");
    let out = until_close(b"ECHO \"abc\r\n");
    assert_eq!(out, "-ERR Protocol error: unbalanced quotes in request\r\n");
}
