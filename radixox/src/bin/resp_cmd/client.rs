//! État par connexion et commandes de compatibilité client : HELLO, CLIENT,
//! COMMAND. RESP2 seulement : `HELLO 3` répond `-NOPROTO`, ce qui fait
//! retomber go-redis, Lettuce et redis-py en RESP2.

use std::sync::atomic::{AtomicU64, Ordering};

use radixox_lib::shared_byte::SharedByte;

use crate::Frame;
use crate::resp_cmd::auth;

/// État d'une connexion.
pub(crate) struct Client {
    pub(crate) authed: bool,
    /// CLIENT ID : unique sur la vie du processus, croissant.
    pub(crate) id: u64,
    pub(crate) name: Option<SharedByte>,
}

impl Client {
    pub(crate) fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            authed: auth::password().is_none(),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name: None,
        }
    }
}

fn ok() -> Frame {
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}

fn bulk(s: &[u8]) -> Frame {
    Frame::BulkString(SharedByte::from_slice(s))
}

/// Nom accepté par CLIENT SETNAME : octets visibles, sans espace (comme Redis).
fn valid_name(name: &[u8]) -> bool {
    name.iter().all(|&b| (b'!'..=b'~').contains(&b))
}

const BAD_NAME: &str = "ERR Client names cannot contain spaces, newlines or special characters.";

/// HELLO [protover [AUTH user pass] [SETNAME name]], dans l'ordre de Redis :
/// version d'abord (rien n'est appliqué si elle est refusée), puis AUTH,
/// puis exigence d'authentification, puis SETNAME.
pub(crate) fn cmd_hello(args: &[&[u8]], client: &mut Client) -> Frame {
    if let Some(v) = args.first() {
        match std::str::from_utf8(v).ok().and_then(|s| s.parse::<i64>().ok()) {
            Some(2) => {}
            Some(_) => return Frame::Error("NOPROTO unsupported protocol version".into()),
            None => return Frame::Error("ERR Protocol version is not an integer or out of range".into()),
        }
    }
    let (mut creds, mut setname) = (None, None);
    let mut i = 1;
    while i < args.len() {
        let opt = args[i];
        if opt.eq_ignore_ascii_case(b"auth") && i + 2 < args.len() {
            creds = Some((args[i + 1], args[i + 2]));
            i += 3;
        } else if opt.eq_ignore_ascii_case(b"setname") && i + 1 < args.len() {
            setname = Some(args[i + 1]);
            i += 2;
        } else {
            return Frame::Error(format!("ERR Syntax error in HELLO option '{}'", String::from_utf8_lossy(opt)));
        }
    }
    if let Some((user, pass)) = creds {
        let r = auth::cmd_auth(Some(user), pass, &mut client.authed);
        if matches!(r, Frame::Error(_)) {
            return r;
        }
    }
    if !client.authed {
        return Frame::Error(
            "NOAUTH HELLO must be called with the client already authenticated, otherwise the \
             HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and \
             select the RESP protocol version at the same time"
                .into(),
        );
    }
    if let Some(name) = setname {
        if !valid_name(name) {
            return Frame::Error(BAD_NAME.into());
        }
        client.name = (!name.is_empty()).then(|| SharedByte::from_slice(name));
    }
    Frame::Array(vec![
        bulk(b"server"),
        bulk(b"redis"),
        bulk(b"version"),
        bulk(b"7.0.0"),
        bulk(b"proto"),
        Frame::Integer(2),
        bulk(b"id"),
        Frame::Integer(client.id as i64),
        bulk(b"mode"),
        bulk(b"standalone"),
        bulk(b"role"),
        bulk(b"master"),
        bulk(b"modules"),
        Frame::Array(Vec::new()),
    ])
}

/// CLIENT SETNAME | GETNAME | ID | SETINFO | NO-EVICT | NO-TOUCH.
pub(crate) fn cmd_client(args: &[&[u8]], client: &mut Client) -> Frame {
    let Some((sub, rest)) = args.split_first() else {
        return Frame::Error("ERR wrong number of arguments for 'client' command".into());
    };
    let is = |name: &[u8]| sub.eq_ignore_ascii_case(name);
    let arity_err = || {
        Frame::Error(format!(
            "ERR wrong number of arguments for 'client|{}' command",
            String::from_utf8_lossy(sub).to_lowercase()
        ))
    };
    if is(b"setname") {
        let [name] = rest else { return arity_err() };
        if !valid_name(name) {
            return Frame::Error(BAD_NAME.into());
        }
        client.name = (!name.is_empty()).then(|| SharedByte::from_slice(name));
        ok()
    } else if is(b"getname") {
        if !rest.is_empty() {
            return arity_err();
        }
        client.name.clone().map_or(Frame::Null, Frame::BulkString)
    } else if is(b"id") {
        if !rest.is_empty() {
            return arity_err();
        }
        Frame::Integer(client.id as i64)
    } else if is(b"setinfo") {
        // Acceptée et ignorée (redis-py, Lettuce l'envoient à la connexion).
        let [attr, _] = rest else { return arity_err() };
        if attr.eq_ignore_ascii_case(b"lib-name") || attr.eq_ignore_ascii_case(b"lib-ver") {
            ok()
        } else {
            Frame::Error(format!("ERR Unrecognized option '{}'", String::from_utf8_lossy(attr)))
        }
    } else if is(b"no-evict") || is(b"no-touch") {
        let [flag] = rest else { return arity_err() };
        if flag.eq_ignore_ascii_case(b"on") || flag.eq_ignore_ascii_case(b"off") {
            ok()
        } else {
            Frame::Error("ERR syntax error".into())
        }
    } else {
        Frame::Error(format!(
            "ERR unknown subcommand '{}'. Try CLIENT HELP.",
            String::from_utf8_lossy(sub)
        ))
    }
}

/// COMMAND [...] : pas de table de commandes publiée. redis-cli demande
/// `COMMAND DOCS` au démarrage pour ses indications et accepte une liste vide.
pub(crate) fn cmd_command(args: &[&[u8]]) -> Frame {
    match args.first() {
        Some(sub) if sub.eq_ignore_ascii_case(b"count") => Frame::Integer(0),
        _ => Frame::Array(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn client_names() {
        assert!(valid_name(b"worker-1:api"));
        assert!(valid_name(b""));
        for bad in [&b"a b"[..], b"a\nb", b"a\x7f", b"\xc3\xa9"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }
}
