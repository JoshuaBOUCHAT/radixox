mod oxidart;

use std::cell::RefCell;
use std::rc::Rc;

use bytes::{Bytes, BytesMut};
use monoio::io::{AsyncWriteRentExt, OwnedWriteHalf, Splitable};
use monoio::net::{TcpListener, TcpStream};

use radixox_common::NetEncode;
use radixox_common::network::net_response::NetResponseResult;
use radixox_common::network::{NetCommand, NetErrorResponse, NetResponse, NetSuccessResponse};
use radixox_common::protocol::{
    Command, CommandAction, SetAction, read_message, read_message_batch,
};

use crate::oxidart::arena_oxid_art::OxidArtArena;

type IOResult<T> = std::io::Result<T>;

/// 32 KiB max message size
const MAX_MSG_SIZE: usize = 32 * 1024;
/// Varint header max (10 bytes for u64, but 3 bytes enough for 32KB)
const HEADER_SIZE: usize = 10;

type SharedART = Rc<RefCell<OxidArtArena>>;

#[monoio::main]
async fn main() -> IOResult<()> {
    let listener = TcpListener::bind("0.0.0.0:8379")?;
    println!("Listening on 0.0.0.0:8379");

    let shared_art = SharedART::new(RefCell::new(OxidArtArena::new()));
    {
        shared_art
            .borrow_mut()
            .set(SetAction::new("user:1".into(), "Joshua".into()).expect("should not fail"));
    }

    loop {
        let (stream, addr) = listener.accept().await?;
        println!("Connection from {addr}");
        monoio::spawn(handle_connection(stream, shared_art.clone()));
    }
}

async fn handle_connection(stream: TcpStream, arena: SharedART) -> IOResult<()> {
    let mut buf = BytesMut::with_capacity(MAX_MSG_SIZE + HEADER_SIZE);
    let (mut read, mut write) = stream.into_split();
    let mut response_buffer = BytesMut::with_capacity(MAX_MSG_SIZE + HEADER_SIZE);
    println!("Handlign conn");

    loop {
        //println!("Now reading");
        let cmd_res = read_message_batch::<NetCommand, Command>(&mut read, &mut buf).await;
        //println!("Message read");
        let cmd = match cmd_res {
            Ok(cmd) => {
                //println!("Message is ok: {}", &cmd);
                cmd
            }
            Err(conn_close) if conn_close.kind() == std::io::ErrorKind::UnexpectedEof => {
                println!("recieve an EOF");
                return Ok(());
            }

            Err(err) => {
                eprintln!("Err: {err}");
                return Err(err);
            }
        };

        let net_responses = { execute_command_batch(cmd, &mut arena.borrow_mut()) };
        println!("Sending {} response(s)", net_responses.len());
        for net_response in net_responses {
            net_response
                .net_encode(&mut response_buffer)
                .expect("should not be impossible to encode");
        }

        //println!("Now responding");
        let nb_res;
        (nb_res, response_buffer) = write.write_all(response_buffer).await;
        if !nb_res.as_ref().is_ok_and(|nb| *nb == response_buffer.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("Error while responding :{:?}", &nb_res),
            ));
        }
        response_buffer.clear();
    }
    println!("Conn closed");
}

fn execute_command(cmd: Command, arena: &mut OxidArtArena) -> NetResponse {
    match cmd.action {
        CommandAction::Set(action) => {
            arena.set(action);
            success_response(None, cmd.command_id)
        }
        CommandAction::Get(action) => {
            let val = arena.get(action);
            success_response(val, cmd.command_id)
        }
        CommandAction::Del(action) => {
            let val = arena.del(action);
            success_response(val, cmd.command_id)
        }
    }
}
fn execute_command_batch(cmds: Vec<Command>, arena: &mut OxidArtArena) -> Vec<NetResponse> {
    cmds.into_iter()
        .map(|cmd| execute_command(cmd, arena))
        .collect()
}

fn success_response(maybe_val: Option<Bytes>, request_id: u64) -> NetResponse {
    use radixox_common::network::net_success_response::Body;

    if let Some(val) = maybe_val {
        NetResponse {
            net_response_result: Some(NetResponseResult::Success(NetSuccessResponse {
                body: Some(Body::GetVal(val)),
            })),
            request_id,
        }
    } else {
        NetResponse {
            request_id,
            net_response_result: None,
        }
    }
}

fn _error_response(message: String, request_id: u64) -> NetResponse {
    let resp = NetResponse {
        net_response_result: Some(NetResponseResult::Error(NetErrorResponse { message })),
        request_id,
    };
    dbg!(&resp);
    resp
}

async fn write_response(
    stream: &mut OwnedWriteHalf<TcpStream>,
    response: &NetResponse,
) -> IOResult<()> {
    let mut buffer = BytesMut::with_capacity(1024);
    response.net_encode(&mut buffer).expect("encode error");
    let (res, _) = stream.write_all(buffer).await;
    res.map(|_| ())
}
