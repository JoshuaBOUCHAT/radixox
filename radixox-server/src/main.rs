mod oxidart;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use bytes::Bytes;
use monoio::buf::IoBufMut;
use monoio::io::{AsyncReadRent, AsyncReadRentExt, AsyncWriteRentExt, OwnedWriteHalf, Splitable};
use monoio::net::{TcpListener, TcpStream};
use prost::Message;
use radixox_common::NetValidate;
use radixox_common::network::{NetCommand, NetErrorResponse, NetResponse, NetSuccessResponse};
use radixox_common::protocol::{
    Command, CommandAction, DelAction, GetAction, SetAction, read_message,
};
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

async fn handle_connection(stream: TcpStream, arena: SharedART) -> IOResult<()> {
    let mut buf = vec![0u8; MAX_MSG_SIZE + HEADER_SIZE];
    let (mut read, mut write) = stream.into_split();

    loop {
        let cmd = read_message::<NetCommand, Command>(&mut read, &mut buf).await?;
        println!("Received: {cmd}");

        let response = execute_command(cmd, arena.clone());
        write_response(&mut write, &response).await?;
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

fn success_response(val: Option<Bytes>, request_id: u32) -> NetResponse {
    use radixox_common::network::net_response::Result as NetResult;
    use radixox_common::network::net_success_response::Body;

    let body = val.map(Body::GetVal);
    NetResponse {
        result: Some(NetResult::Success(NetSuccessResponse { body })),
        request_id,
    }
}

fn error_response(message: String, request_id: u32) -> NetResponse {
    use radixox_common::network::net_response::Result as NetResult;
    NetResponse {
        result: Some(NetResult::Error(NetErrorResponse { message })),
        request_id,
    }
}

async fn write_response(
    stream: &mut OwnedWriteHalf<TcpStream>,
    response: &NetResponse,
) -> IOResult<()> {
    let encoded = response.encode_length_delimited_to_vec();
    let (res, _) = stream.write_all(encoded).await;
    res.map(|_| ())
}
