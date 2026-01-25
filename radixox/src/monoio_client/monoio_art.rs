use bytes::{Bytes, BytesMut};
use local_sync::mpsc;
use local_sync::oneshot::Sender;
// Utilise local_sync pour le zéro-overhead atomique
use monoio::io::{AsyncReadRent, AsyncWriteRentExt, OwnedWriteHalf};
use monoio::io::{OwnedReadHalf, Splitable};
use monoio::net::TcpStream;

use prost::Message;
use radixox_common::network::net_command::NetAction;
use radixox_common::network::{NetResponse, Response};
use radixox_common::protocol::read_message;
use slotmap::{DefaultKey, SlotMap};

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
// La commande que tes tâches envoient à la boucle IO
pub struct Command {
    pub key: Bytes,
    pub value: Option<Bytes>,
    pub responder: local_sync::oneshot::Sender<Response>,
}

type IOResult<T> = std::io::Result<T>;

struct MonoIOClient {
    write: OwnedWriteHalf<TcpStream>,
    map: SlotMap<DefaultKey, Sender<Response>>,
}

pub struct SharedMonoIOClient {
    client: Rc<RefCell<MonoIOClient>>,
}
impl Clone for SharedMonoIOClient {
    fn clone(&self) -> Self {
        Self {
            client: Rc::clone(&self.client),
        }
    }
}

impl SharedMonoIOClient {
    pub async fn send(&self, command: NetAction) -> Response {
        let (tx, rx) = local_sync::oneshot::channel::<Response>();
        let mut buffer = BytesMut::new();
        command.encode(&mut buffer);
        self.client.borrow_mut().write.write_all(buffer).await;

        todo!()
    }
    pub async fn new(addr: SocketAddr) -> IOResult<Self> {
        let stream = TcpStream::connect(addr).await.expect("Failed to connect");
        let (read_stream, mut write_stream) = stream.into_split();
        let client = MonoIOClient {
            map: SlotMap::new(),
            write: write_stream,
        };
        let ret = Self {
            client: Rc::new(RefCell::new(client)),
        };

        monoio::spawn(async move {
            let mut read_stream = read_stream;
            let mut buffer = Vec::with_capacity(1 << 16);
            let shared_client = ret.clone();
            loop {
                let response = match read_message::<NetResponse, Response>(
                    &mut read_stream,
                    &mut buffer,
                )
                .await
                {
                    Err(err) => {
                        eprintln!("Error: {}", err);
                        continue;
                    }
                    Ok(response) => response,
                };
                shared_client
                    .client
                    .borrow_mut()
                    .map
                    .remove(response.command_id);
            }
        });

        todo!()
    }
}

fn encode_request(buf: &mut BytesMut, id: u32, key: &Bytes) {
    // Ton code Prost ou manuel pour packager la requête
}
