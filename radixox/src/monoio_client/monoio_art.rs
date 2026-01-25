use bytes::{Bytes, BytesMut};
use local_sync::mpsc;
use local_sync::oneshot::Sender;
// Utilise local_sync pour le zéro-overhead atomique
use monoio::io::{AsyncReadRent, AsyncWriteRentExt, OwnedWriteHalf};
use monoio::io::{OwnedReadHalf, Splitable};
use monoio::net::TcpStream;

use prost::Message;
use radixox_common::network::Response;
use radixox_common::network::net_command::NetAction;
use slotmap::{DefaultKey, SlotMap};

use std::cell::RefCell;
use std::net::SocketAddr;
// La commande que tes tâches envoient à la boucle IO
pub struct Command {
    pub key: Bytes,
    pub value: Option<Bytes>,
    pub responder: local_sync::oneshot::Sender<Response>,
}

type IOResult<T> = std::io::Result<T>;

struct RadixClient {
    write: OwnedWriteHalf<TcpStream>,
    map: SlotMap<DefaultKey, Sender<Response>>,
}

pub struct RadixClientCell {
    client: RefCell<RadixClient>,
}

impl RadixClientCell {
    pub async fn send(&self, command: NetAction) -> Response {
        let (tx, rx) = local_sync::oneshot::channel::<Response>();
        let mut buffer = BytesMut::new();
        command.encode(&mut buffer);
        self.client.borrow_mut().write.write_all(buffer).await;

        todo!()
    }
    pub async fn new(addr: SocketAddr) -> IOResult<Self> {
        let stream = TcpStream::connect(addr).await.expect("Failed to connect");
        let (mut read_stream, mut write_stream) = stream.into_split();

        monoio::spawn(async move { loop {} });

        todo!()
    }
}

fn encode_request(buf: &mut BytesMut, id: u32, key: &Bytes) {
    // Ton code Prost ou manuel pour packager la requête
}
