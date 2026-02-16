use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use monoio::io::{AsyncWriteRentExt, Splitable};
use monoio::net::{TcpListener, TcpStream};

use oxidart::OxidArt;
use oxidart::monoio::{spawn_evictor, spawn_ticker};
use oxidart::value::Value;
use radixox_common::NetEncode;
use radixox_common::network::net_response::NetResponseResult;
use radixox_common::network::{NetCommand, NetMultiValueResponse, NetResponse, NetSuccessResponse};
use radixox_common::protocol::{Command, CommandAction, read_message_batch};

type IOResult<T> = std::io::Result<T>;

const MAX_MSG_SIZE: usize = 1024 * 1024 * 100;
const HEADER_SIZE: usize = 10;

type SharedART = Rc<RefCell<OxidArt>>;

#[monoio::main(enable_timer = true)]
async fn main() -> IOResult<()> {
    let listener = TcpListener::bind("0.0.0.0:8379")?;
    println!("RadixOx Legacy Server listening on 0.0.0.0:8379");

    let shared_art = SharedART::new(RefCell::new(OxidArt::new()));
    spawn_ticker(shared_art.clone(), Duration::from_millis(100));
    spawn_evictor(shared_art.clone(), Duration::from_secs(1));
    shared_art.borrow_mut().tick();

    loop {
        let (stream, _addr) = listener.accept().await?;
        monoio::spawn(handle_connection(stream, shared_art.clone()));
    }
}

async fn handle_connection(stream: TcpStream, arena: SharedART) -> IOResult<()> {
    let mut buf = BytesMut::with_capacity(MAX_MSG_SIZE + HEADER_SIZE);
    let (mut read, mut write) = stream.into_split();
    let mut response_buffer = BytesMut::with_capacity(MAX_MSG_SIZE + HEADER_SIZE);

    loop {
        let cmd_res = read_message_batch::<NetCommand, Command>(&mut read, &mut buf).await;
        let cmd = match cmd_res {
            Ok(cmd) => cmd,
            Err(conn_close) if conn_close.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(err) => {
                eprintln!("Err: {err}");
                return Err(err);
            }
        };

        let net_responses = execute_command_batch(cmd, &mut arena.borrow_mut());
        for net_response in net_responses {
            net_response
                .net_encode(&mut response_buffer)
                .expect("encoding should not fail");
        }

        let nb_res;
        (nb_res, response_buffer) = write.write_all(response_buffer).await;
        if !nb_res.as_ref().is_ok_and(|nb| *nb == response_buffer.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("Error while responding: {:?}", &nb_res),
            ));
        }
        response_buffer.clear();
    }
}

fn execute_command(cmd: Command, arena: &mut OxidArt) -> NetResponse {
    match cmd.action {
        CommandAction::Set(action) => {
            let (key, val) = action.into_parts();
            arena.set(key, Value::String(val));
            success_response(None, cmd.command_id)
        }
        CommandAction::Get(action) => {
            let key: &[u8] = &(action.into_parts());
            let val = arena.get(key).and_then(|v| v.as_bytes());
            success_response(val, cmd.command_id)
        }
        CommandAction::Del(action) => {
            let val = arena
                .del(action.into_parts().as_ref())
                .and_then(|v| v.as_bytes());
            success_response(val, cmd.command_id)
        }
        CommandAction::GetN(action) => {
            let pairs = arena.getn(action.into_parts());
            let values: Vec<Bytes> = pairs
                .into_iter()
                .filter_map(|(_, v)| v.as_bytes())
                .collect();
            multi_response(values, cmd.command_id)
        }
        CommandAction::DelN(action) => {
            let _count = arena.deln(action.into_parts());
            success_response(None, cmd.command_id)
        }
    }
}

fn execute_command_batch(cmds: Vec<Command>, arena: &mut OxidArt) -> Vec<NetResponse> {
    cmds.into_iter()
        .map(|cmd| execute_command(cmd, arena))
        .collect()
}

fn success_response(maybe_val: Option<Bytes>, request_id: u64) -> NetResponse {
    use radixox_common::network::net_success_response::Body;

    if let Some(val) = maybe_val {
        NetResponse {
            net_response_result: Some(NetResponseResult::Success(NetSuccessResponse {
                body: Some(Body::SingleValue(val)),
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

fn multi_response(values: Vec<Bytes>, request_id: u64) -> NetResponse {
    use radixox_common::network::net_success_response::Body;

    NetResponse {
        net_response_result: Some(NetResponseResult::Success(NetSuccessResponse {
            body: Some(Body::MultiValue(NetMultiValueResponse { values })),
        })),
        request_id,
    }
}
