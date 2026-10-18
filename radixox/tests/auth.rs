mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::OnceLock;

const PORT: u16 = 16387;
const PASS: &str = "s3cr3t pass";

static INIT: OnceLock<()> = OnceLock::new();

/// Connexion brute (sans AUTH automatique du client redis).
fn raw() -> (TcpStream, BufReader<TcpStream>) {
    INIT.get_or_init(|| common::start_server_env(PORT, &[("RADIXOX_PASSWORD", PASS)]));
    let s = TcpStream::connect(("127.0.0.1", PORT)).unwrap();
    let r = BufReader::new(s.try_clone().unwrap());
    (s, r)
}

fn encode(args: &[&str]) -> String {
    let mut out = format!("*{}\r\n", args.len());
    for a in args {
        out += &format!("${}\r\n{a}\r\n", a.len());
    }
    out
}

fn line(r: &mut BufReader<TcpStream>) -> String {
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    line.trim_end().to_string()
}

/// Envoie une commande et rend la première ligne de réponse.
fn send(s: &mut TcpStream, r: &mut BufReader<TcpStream>, args: &[&str]) -> String {
    s.write_all(encode(args).as_bytes()).unwrap();
    line(r)
}

/// Plusieurs commandes dans une seule écriture (un seul paquet) ; rend la
/// première ligne de chaque réponse.
fn pipeline(s: &mut TcpStream, r: &mut BufReader<TcpStream>, cmds: &[&[&str]]) -> Vec<String> {
    let all: String = cmds.iter().map(|c| encode(c)).collect();
    s.write_all(all.as_bytes()).unwrap();
    cmds.iter().map(|_| line(r)).collect()
}

/// Connexion déjà authentifiée.
fn authed() -> (TcpStream, BufReader<TcpStream>) {
    let (mut s, mut r) = raw();
    assert_eq!(send(&mut s, &mut r, &["AUTH", PASS]), "+OK");
    (s, r)
}

const NOAUTH: &str = "-NOAUTH Authentication required.";
const WRONGPASS: &str = "-WRONGPASS invalid username-password pair or user is disabled.";

#[test]
fn commands_are_refused_before_auth() {
    let (mut s, mut r) = raw();
    let cmds: [&[&str]; 18] = [
        &["PING"], &["ECHO", "x"], &["GET", "k"], &["SET", "k", "v"], &["DEL", "k"],
        &["FLUSHDB"], &["DBSIZE"], &["INFO"], &["KEYS", "*"], &["SCAN", "0"],
        &["KRANGE", "", "", "10"], &["MGET", "a", "b"], &["UNLINK", "k"], &["SAVE"],
        &["BGSAVE"], &["SUBSCRIBE", "c"], &["PUBLISH", "c", "m"], &["NOSUCHCMD"],
    ];
    for cmd in cmds {
        assert_eq!(send(&mut s, &mut r, cmd), NOAUTH, "{cmd:?}");
    }
    // AUTH à mauvais nombre d'arguments : refusé, et n'authentifie pas
    assert_eq!(send(&mut s, &mut r, &["AUTH", "default", PASS, "extra"]), NOAUTH);
    assert_eq!(send(&mut s, &mut r, &["AUTH"]), NOAUTH);
    assert_eq!(send(&mut s, &mut r, &["PING"]), NOAUTH);
}

/// Les commandes refusées n'ont aucun effet : vérifié depuis une connexion
/// authentifiée.
#[test]
fn refused_commands_have_no_side_effects() {
    let (mut a, mut ar) = authed();
    assert_eq!(send(&mut a, &mut ar, &["SET", "auth:sentinel", "keep"]), "+OK");
    let (mut u, mut ur) = raw();
    let refused = pipeline(
        &mut u,
        &mut ur,
        &[&["DEL", "auth:sentinel"], &["SET", "auth:intruder", "x"], &["FLUSHDB"], &["UNLINK", "auth:*"]],
    );
    assert!(refused.iter().all(|l| l == NOAUTH), "{refused:?}");
    assert_eq!(send(&mut a, &mut ar, &["GET", "auth:sentinel"]), "$4");
    assert_eq!(line(&mut ar), "keep");
    assert_eq!(send(&mut a, &mut ar, &["EXISTS", "auth:intruder"]), ":0");
}

/// Dans un même paquet : ce qui précède AUTH est refusé, ce qui suit passe ;
/// un AUTH raté au milieu ne laisse rien passer.
#[test]
fn auth_inside_a_pipeline_applies_in_order() {
    let (mut s, mut r) = raw();
    let got = pipeline(
        &mut s,
        &mut r,
        &[
            &["SET", "auth:pipe", "before"],
            &["AUTH", "wrong"],
            &["SET", "auth:pipe", "after-wrong"],
            &["AUTH", PASS],
            &["SET", "auth:pipe", "after-right"],
            &["PING"],
        ],
    );
    assert_eq!(got, [NOAUTH, WRONGPASS, NOAUTH, "+OK", "+OK", "+PONG"]);
    assert_eq!(send(&mut s, &mut r, &["GET", "auth:pipe"]), "$11");
    assert_eq!(line(&mut r), "after-right");
}

/// L'authentification appartient à la connexion.
#[test]
fn auth_is_per_connection() {
    let (_a, _ar) = authed();
    let (mut b, mut br) = raw();
    assert_eq!(send(&mut b, &mut br, &["PING"]), NOAUTH);
    // une nouvelle connexion repart non authentifiée, même après une autre fermée
    drop(b);
    let (mut c, mut cr) = raw();
    assert_eq!(send(&mut c, &mut cr, &["PING"]), NOAUTH);
}

/// Mots de passe proches du vrai : préfixe, suffixe, casse, vide, octet nul.
#[test]
fn near_miss_passwords_are_rejected() {
    let (mut s, mut r) = raw();
    let upper = PASS.to_uppercase();
    let longer = format!("{PASS}x");
    let nul = format!("{PASS}\0");
    let lead = format!(" {PASS}");
    for p in ["", &PASS[..PASS.len() - 1], &longer, &upper, &nul, &lead, "s3cr3t"] {
        assert_eq!(send(&mut s, &mut r, &["AUTH", p]), WRONGPASS, "{p:?}");
        assert_eq!(send(&mut s, &mut r, &["AUTH", "default", p]), WRONGPASS, "{p:?}");
    }
    // utilisateur autre que `default`, même avec le bon mot de passe
    for u in ["Default", "", "root", "default "] {
        assert_eq!(send(&mut s, &mut r, &["AUTH", u, PASS]), WRONGPASS, "{u:?}");
    }
    assert_eq!(send(&mut s, &mut r, &["PING"]), NOAUTH);
}

/// Beaucoup de commandes non authentifiées d'un coup : toutes refusées, la
/// connexion reste utilisable.
#[test]
fn large_unauthenticated_pipeline() {
    let (mut s, mut r) = raw();
    let cmds: Vec<&[&str]> = (0..5000).map(|_| &["SET", "auth:flood", "x"][..]).collect();
    let got = pipeline(&mut s, &mut r, &cmds);
    assert!(got.iter().all(|l| l == NOAUTH));
    assert_eq!(send(&mut s, &mut r, &["AUTH", PASS]), "+OK");
    assert_eq!(send(&mut s, &mut r, &["EXISTS", "auth:flood"]), ":0");
}

/// INFO ne laisse pas fuiter le mot de passe.
#[test]
fn info_does_not_leak_the_password() {
    let (mut s, mut r) = authed();
    let head = send(&mut s, &mut r, &["INFO"]);
    let len: usize = head[1..].parse().unwrap();
    let mut body = vec![0u8; len + 2];
    std::io::Read::read_exact(&mut r, &mut body).unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("s3cr3t"));
}

/// Sans RADIXOX_PASSWORD : pas d'AUTH exigé, et AUTH répond comme Redis.
#[test]
fn server_without_password() {
    const OPEN_PORT: u16 = 16388;
    static OPEN: OnceLock<()> = OnceLock::new();
    OPEN.get_or_init(|| common::start_server(OPEN_PORT));
    let mut s = TcpStream::connect(("127.0.0.1", OPEN_PORT)).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    assert_eq!(send(&mut s, &mut r, &["PING"]), "+PONG");
    let reply = send(&mut s, &mut r, &["AUTH", "anything"]);
    assert!(reply.starts_with("-ERR AUTH <password> called without any password configured"), "{reply}");
    assert_eq!(send(&mut s, &mut r, &["PING"]), "+PONG");
}

#[test]
fn wrong_password_then_right_one() {
    let (mut s, mut r) = raw();
    let wrong = WRONGPASS;
    assert_eq!(send(&mut s, &mut r, &["AUTH", "nope"]), wrong);
    assert_eq!(send(&mut s, &mut r, &["AUTH", "alice", PASS]), wrong);
    assert_eq!(send(&mut s, &mut r, &["PING"]), NOAUTH);
    assert_eq!(send(&mut s, &mut r, &["AUTH", PASS]), "+OK");
    assert_eq!(send(&mut s, &mut r, &["PING"]), "+PONG");
    // un échec après coup ne retire pas l'authentification
    assert_eq!(send(&mut s, &mut r, &["AUTH", "nope"]), wrong);
    assert_eq!(send(&mut s, &mut r, &["SET", "auth:k", "v"]), "+OK");
}

#[test]
fn auth_with_default_user() {
    let (mut s, mut r) = raw();
    assert_eq!(send(&mut s, &mut r, &["AUTH", "default", PASS]), "+OK");
    assert_eq!(send(&mut s, &mut r, &["PING"]), "+PONG");
}

#[test]
fn redis_client_with_password_in_url() {
    raw();
    let mut c = redis::Client::open(format!("redis://:{}@127.0.0.1:{PORT}", "s3cr3t%20pass"))
        .unwrap()
        .get_connection()
        .unwrap();
    let _: () = redis::cmd("SET").arg("auth:url").arg("1").query(&mut c).unwrap();
    let v: String = redis::cmd("GET").arg("auth:url").query(&mut c).unwrap();
    assert_eq!(v, "1");
}

#[test]
fn quit_is_allowed_before_auth() {
    let (mut s, mut r) = raw();
    assert_eq!(send(&mut s, &mut r, &["QUIT"]), "+OK");
}

const HELLO_NOAUTH: &str = "-NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time";

/// HELLO avant AUTH : seul HELLO … AUTH bon_mot_de_passe authentifie ; une
/// version refusée n'authentifie pas, même avec le bon mot de passe.
#[test]
fn hello_and_auth() {
    let (mut s, mut r) = raw();
    assert_eq!(send(&mut s, &mut r, &["HELLO"]), HELLO_NOAUTH);
    assert_eq!(send(&mut s, &mut r, &["HELLO", "2"]), HELLO_NOAUTH);
    assert_eq!(send(&mut s, &mut r, &["HELLO", "3", "AUTH", "default", PASS]), "-NOPROTO unsupported protocol version");
    assert_eq!(send(&mut s, &mut r, &["HELLO", "2", "AUTH", "default", "nope"]), WRONGPASS);
    assert_eq!(send(&mut s, &mut r, &["HELLO", "2", "AUTH", "alice", PASS]), WRONGPASS);
    assert_eq!(send(&mut s, &mut r, &["HELLO", "2", "SETNAME", "x"]), HELLO_NOAUTH);
    assert_eq!(send(&mut s, &mut r, &["PING"]), NOAUTH);
    assert_eq!(send(&mut s, &mut r, &["CLIENT", "GETNAME"]), NOAUTH);
    // tête de tableau, puis 14 éléments : 11 bulk (2 lignes), 2 entiers, `*0`
    assert_eq!(send(&mut s, &mut r, &["HELLO", "2", "AUTH", "default", PASS, "SETNAME", "svc"]), "*14");
    for _ in 0..11 * 2 + 2 {
        line(&mut r);
    }
    assert_eq!(line(&mut r), "*0", "modules");
    assert_eq!(send(&mut s, &mut r, &["PING"]), "+PONG");
    assert_eq!(send(&mut s, &mut r, &["CLIENT", "GETNAME"]), "$3");
    assert_eq!(line(&mut r), "svc");
}

