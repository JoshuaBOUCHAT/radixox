mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::OnceLock;

const PORT: u16 = 16389;

static INIT: OnceLock<()> = OnceLock::new();

struct Raw {
    s: TcpStream,
    r: BufReader<TcpStream>,
}

/// Réponse RESP2 décodée en texte : `+OK`, `-ERR …`, `:1`, `$nil`, bulk en
/// clair, tableau en `[a, b, …]`.
fn read(r: &mut BufReader<TcpStream>) -> String {
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let line = line.trim_end();
    match &line[..1] {
        "$" if line == "$-1" => "$nil".into(),
        "$" => {
            let mut b = vec![0; line[1..].parse::<usize>().unwrap() + 2];
            r.read_exact(&mut b).unwrap();
            String::from_utf8_lossy(&b[..b.len() - 2]).into()
        }
        "*" => {
            let n: usize = line[1..].parse().unwrap();
            format!("[{}]", (0..n).map(|_| read(r)).collect::<Vec<_>>().join(", "))
        }
        _ => line.into(),
    }
}

impl Raw {
    fn new() -> Self {
        INIT.get_or_init(|| common::start_server(PORT));
        let s = TcpStream::connect(("127.0.0.1", PORT)).unwrap();
        let r = BufReader::new(s.try_clone().unwrap());
        Raw { s, r }
    }

    fn cmd(&mut self, args: &[&str]) -> String {
        let mut out = format!("*{}\r\n", args.len());
        for a in args {
            out += &format!("${}\r\n{a}\r\n", a.len());
        }
        self.s.write_all(out.as_bytes()).unwrap();
        read(&mut self.r)
    }

    fn id(&mut self) -> i64 {
        self.cmd(&["CLIENT", "ID"])[1..].parse().unwrap()
    }
}

#[test]
fn hello_without_version_and_with_2() {
    let mut c = Raw::new();
    let id = c.id();
    let want = format!(
        "[server, redis, version, 7.0.0, proto, :2, id, :{id}, mode, standalone, role, master, modules, []]"
    );
    assert_eq!(c.cmd(&["HELLO"]), want);
    assert_eq!(c.cmd(&["HELLO", "2"]), want);
}

#[test]
fn hello_3_is_refused_and_changes_nothing() {
    let mut c = Raw::new();
    assert_eq!(c.cmd(&["HELLO", "3", "SETNAME", "x"]), "-NOPROTO unsupported protocol version");
    assert_eq!(c.cmd(&["CLIENT", "GETNAME"]), "$nil", "SETNAME ignoré si la version est refusée");
    assert_eq!(c.cmd(&["HELLO", "1"]), "-NOPROTO unsupported protocol version");
    assert_eq!(c.cmd(&["HELLO", "two"]), "-ERR Protocol version is not an integer or out of range");
    assert_eq!(c.cmd(&["PING"]), "+PONG");
}

#[test]
fn hello_options() {
    let mut c = Raw::new();
    let r = c.cmd(&["HELLO", "2", "SETNAME", "worker-1"]);
    assert!(r.starts_with("[server, redis"), "{r}");
    assert_eq!(c.cmd(&["CLIENT", "GETNAME"]), "worker-1");
    assert_eq!(c.cmd(&["HELLO", "2", "SETNAME"]), "-ERR Syntax error in HELLO option 'SETNAME'");
    assert_eq!(c.cmd(&["HELLO", "2", "FOO", "bar"]), "-ERR Syntax error in HELLO option 'FOO'");
    assert_eq!(
        c.cmd(&["HELLO", "2", "SETNAME", "a b"]),
        "-ERR Client names cannot contain spaces, newlines or special characters."
    );
    // AUTH sans mot de passe configuré : erreur, comme AUTH seul
    let r = c.cmd(&["HELLO", "2", "AUTH", "default", "x"]);
    assert!(r.starts_with("-ERR AUTH <password> called without any password configured"), "{r}");
}

#[test]
fn client_setname_getname() {
    let mut c = Raw::new();
    assert_eq!(c.cmd(&["CLIENT", "GETNAME"]), "$nil");
    assert_eq!(c.cmd(&["client", "setname", "api:42"]), "+OK");
    assert_eq!(c.cmd(&["CLIENT", "GETNAME"]), "api:42");
    for bad in ["a b", "a\nb", "é"] {
        assert_eq!(
            c.cmd(&["CLIENT", "SETNAME", bad]),
            "-ERR Client names cannot contain spaces, newlines or special characters.",
            "{bad:?}"
        );
    }
    assert_eq!(c.cmd(&["CLIENT", "GETNAME"]), "api:42", "nom inchangé après un refus");
    assert_eq!(c.cmd(&["CLIENT", "SETNAME", ""]), "+OK");
    assert_eq!(c.cmd(&["CLIENT", "GETNAME"]), "$nil", "nom vide = effacé");
    assert_eq!(c.cmd(&["CLIENT", "SETNAME"]), "-ERR wrong number of arguments for 'client|setname' command");
}

#[test]
fn client_id_is_unique_and_stable() {
    let mut a = Raw::new();
    let mut b = Raw::new();
    let (ia, ib) = (a.id(), b.id());
    assert!(ia > 0 && ib > 0 && ia != ib);
    assert_eq!(a.id(), ia);
    // le nom appartient à la connexion
    assert_eq!(a.cmd(&["CLIENT", "SETNAME", "only-a"]), "+OK");
    assert_eq!(b.cmd(&["CLIENT", "GETNAME"]), "$nil");
}

#[test]
fn client_misc_subcommands() {
    let mut c = Raw::new();
    assert_eq!(c.cmd(&["CLIENT", "SETINFO", "LIB-NAME", "redis-py"]), "+OK");
    assert_eq!(c.cmd(&["CLIENT", "SETINFO", "lib-ver", "5.0.1"]), "+OK");
    assert_eq!(c.cmd(&["CLIENT", "SETINFO", "color", "x"]), "-ERR Unrecognized option 'color'");
    assert_eq!(c.cmd(&["CLIENT", "NO-EVICT", "on"]), "+OK");
    assert_eq!(c.cmd(&["CLIENT", "NO-TOUCH", "off"]), "+OK");
    assert_eq!(c.cmd(&["CLIENT", "NO-TOUCH", "maybe"]), "-ERR syntax error");
    assert_eq!(c.cmd(&["CLIENT", "KILL", "x"]), "-ERR unknown subcommand 'KILL'. Try CLIENT HELP.");
    assert_eq!(c.cmd(&["CLIENT"]), "-ERR wrong number of arguments for 'client' command");
}

#[test]
fn command_docs_is_empty() {
    let mut c = Raw::new();
    assert_eq!(c.cmd(&["COMMAND", "DOCS"]), "[]");
    assert_eq!(c.cmd(&["COMMAND"]), "[]");
    assert_eq!(c.cmd(&["COMMAND", "COUNT"]), ":0");
}

/// redis-cli (qui envoie COMMAND DOCS au démarrage) fonctionne.
#[test]
fn redis_cli_works() {
    Raw::new();
    let Ok(out) = std::process::Command::new("redis-cli").args(["-p", &PORT.to_string(), "PING"]).output() else {
        eprintln!("redis-cli absent : test sauté");
        return;
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "PONG");
}
