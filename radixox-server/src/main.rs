mod oxidart;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use bytes::Bytes;
use monoio::buf::IoBufMut;
use monoio::io::{AsyncReadRent, AsyncReadRentExt, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};
use prost::Message;
use radixox_common::NetValidate;
use radixox_common::network::{NetCommand, NetErrorResponse, NetResponse, NetSuccessResponse};
use radixox_common::protocol::{Command, CommandAction, DelAction, GetAction, SetAction};
use rand::seq::SliceRandom;

use crate::oxidart::arena_oxid_art::OxidArtArena;

type IOResult<T> = std::io::Result<T>;

/// 32 KiB max message size
const MAX_MSG_SIZE: usize = 32 * 1024;
/// Varint header max (10 bytes for u64, but 3 bytes enough for 32KB)
const HEADER_SIZE: usize = 10;

type SharedART = Rc<RefCell<OxidArtArena>>;

#[monoio::main]
async fn main() -> IOResult<()> {
    let listener = TcpListener::bind("127.0.0.1:8379")?;
    println!("Listening on 127.0.0.1:8379");

    let shared_art = SharedART::new(RefCell::new(OxidArtArena::new()));

    loop {
        let (stream, addr) = listener.accept().await?;
        println!("Connection from {addr}");
        monoio::spawn(handle_connection(stream, shared_art.clone()));
    }
}

async fn handle_connection(mut stream: TcpStream, arena: SharedART) -> IOResult<()> {
    let mut buf = vec![0u8; MAX_MSG_SIZE + HEADER_SIZE];

    loop {
        let cmd = read_command(&mut stream, &mut buf).await?;
        println!("Received: {cmd}");

        let response = execute_command(cmd, arena.clone());
        write_response(&mut stream, &response).await?;
    }
}

fn execute_command(cmd: Command, arena: SharedART) -> NetResponse {
    match cmd.action {
        CommandAction::Set(action) => {
            arena.borrow_mut().set(action);
            success_response(None, cmd.command_id)
        }
        CommandAction::Get(action) => {
            let val = arena.borrow_mut().get(action);
            success_response(val, cmd.command_id)
        }
        CommandAction::Del(action) => {
            let val = arena.borrow_mut().delete(action);
            success_response(val, cmd.command_id)
        }
    }
}

fn success_response(val: Option<Bytes>, request_id: Option<u32>) -> NetResponse {
    use radixox_common::network::net_response::Result as NetResult;
    use radixox_common::network::net_success_response::Body;

    let body = val.map(Body::GetVal);
    NetResponse {
        result: Some(NetResult::Success(NetSuccessResponse { body })),
        request_id,
    }
}

fn error_response(message: String, request_id: Option<u32>) -> NetResponse {
    use radixox_common::network::net_response::Result as NetResult;
    NetResponse {
        result: Some(NetResult::Error(NetErrorResponse { message })),
        request_id,
    }
}

async fn write_response(stream: &mut TcpStream, response: &NetResponse) -> IOResult<()> {
    let encoded = response.encode_length_delimited_to_vec();
    let (res, _) = stream.write_all(encoded).await;
    res.map(|_| ())
}

/// Lit le header varint et retourne (msg_len, bytes_consumed, total_read)
fn parse_varint_header(buf: &[u8], n: usize) -> IOResult<(usize, usize)> {
    let mut cursor = std::io::Cursor::new(&buf[..n]);
    let msg_len = prost::encoding::decode_varint(&mut cursor)
        .map_err(|e| std::io::Error::other(format!("Varint: {e}")))? as usize;

    if msg_len > MAX_MSG_SIZE {
        return Err(std::io::Error::other(format!(
            "Message too large: {msg_len} > {MAX_MSG_SIZE}"
        )));
    }

    Ok((msg_len, cursor.position() as usize))
}

/// Decode le buffer en Command
fn decode_command(buf: &[u8]) -> IOResult<Command> {
    let net_cmd =
        NetCommand::decode(buf).map_err(|e| std::io::Error::other(format!("Proto: {e}")))?;

    net_cmd
        .validate()
        .map_err(|_| std::io::Error::other("Invalid command"))
}

async fn read_command(stream: &mut TcpStream, buf: &mut Vec<u8>) -> IOResult<Command> {
    // 1. On récupère le buffer (ownership)
    let tmp = std::mem::take(buf);

    // 2. Première lecture (on peut lire le header + une partie du payload)
    let (res, tmp) = stream.read(tmp).await;
    let n = res?;

    if n == 0 {
        *buf = tmp;
        return Err(std::io::ErrorKind::ConnectionReset.into());
    }

    // 3. Analyse du header
    let (msg_len, varint_len) = parse_varint_header(&tmp, n)?;
    let total_expected = varint_len + msg_len;

    // 4. On vérifie s'il nous en manque
    let mut tmp = tmp;
    if n < total_expected {
        // --- LA MAGIE MONOIO ---
        // On crée une "vue" sur le buffer qui commence à l'index 'n'
        // et s'arrête à 'total_expected'
        let slice = tmp.slice_mut(n..total_expected);

        // On passe la slice à read_exact. Monoio va écrire uniquement
        // dans la zone vide à la suite de ce qu'on a déjà lu.
        let (res, slice) = stream.read_exact(slice).await;
        res?;

        // On récupère le Vec d'origine
        tmp = slice.into_inner();
    }

    // 5. Décodage (Zero-copy via slice)
    let cmd = decode_command(&tmp[varint_len..total_expected])?;

    // 6. On rend le buffer pour la prochaine itération
    // Optionnel : on pourrait vider le buffer ici ou gérer le "surplus" lu
    *buf = tmp;
    buf.clear();

    Ok(cmd)
}

#[test]
fn _test_speed() {
    let mut oxid_art = OxidArtArena::new();
    let mut words: Vec<&str> = include_str!("../list.txt").lines().collect();
    words.shuffle(&mut rand::rng());
    let now = Instant::now();
    for &line in &words {
        let key = Bytes::from(line);
        let action = SetAction::new(key.clone(), key).expect("invalid set");
        oxid_art.set(action);
    }
    println!("Le temps total a été de: {}s", now.elapsed().as_secs_f32());
    let now = Instant::now();
    let mut dummy_count = 0;
    for line in words {
        let key = Bytes::from(line);
        let action = GetAction::new(key.clone()).expect("invalid set");
        dummy_count += oxid_art
            .get(action)
            .expect("An item has not been inserted")
            .len();
    }
    println!(
        "Dummy:{dummy_count} time to re-get all the words: {}s",
        now.elapsed().as_secs_f32()
    );
    del(&mut oxid_art, "objet");
    assert!(get(&oxid_art, "objet").is_none());
    assert!(get(&oxid_art, "objets").is_some());
}

// === Test helpers ===

fn set(art: &mut OxidArtArena, key: &str, val: &str) {
    let action = SetAction::new(Bytes::from(key.to_owned()), Bytes::from(val.to_owned()))
        .expect("invalid set");
    art.set(action);
}

fn get(art: &OxidArtArena, key: &str) -> Option<String> {
    let action = GetAction::new(Bytes::from(key.to_owned())).expect("invalid get");
    art.get(action)
        .map(|b| String::from_utf8_lossy(&b).to_string())
}

fn del(art: &mut OxidArtArena, key: &str) -> Option<String> {
    let action = DelAction::new(Bytes::from(key.to_owned())).expect("invalid del");
    art.delete(action)
        .map(|b| String::from_utf8_lossy(&b).to_string())
}
