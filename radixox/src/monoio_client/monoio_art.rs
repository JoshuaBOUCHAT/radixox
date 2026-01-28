use bytes::{Bytes, BytesMut, buf};
use local_sync::mpsc;
use local_sync::oneshot::{Receiver, Sender};
// Utilise local_sync pour le zéro-overhead atomique
use monoio::io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt, OwnedWriteHalf};
use monoio::io::{OwnedReadHalf, Splitable};
use monoio::net::TcpStream;

use monoio::time::{Interval, interval, sleep};
use prost::{EncodeError, Message};
use radixox_common::NetEncode;
use radixox_common::network::net_command::NetAction;
use radixox_common::network::{NetCommand, NetResponse, Response};
use radixox_common::protocol::{read_message, read_message_batch};
use slotmap::{DefaultKey, Key, SlotMap};

use slotmap::KeyData;
use std::cell::{RefCell, UnsafeCell};
use std::net::SocketAddr;
use std::rc::Rc;
use std::str::from_utf8;
// La commande que tes tâches envoient à la boucle IO
type IOResult<T> = std::io::Result<T>;

struct MonoIOClient {
    map: RefCell<SlotMap<DefaultKey, Sender<Response>>>,
    buffer: RefCell<BytesMut>,
}
pub struct SharedMonoIOClient {
    client: Rc<MonoIOClient>,
}

use std::borrow::BorrowMut;
use std::time::Duration;
impl Clone for SharedMonoIOClient {
    fn clone(&self) -> Self {
        Self {
            client: Rc::clone(&self.client),
        }
    }
}

impl SharedMonoIOClient {
    pub(crate) fn send(&self, action: NetAction) -> Result<Receiver<Response>, EncodeError> {
        let (tx, rx) = local_sync::oneshot::channel::<Response>();
        let key = self.client.map.borrow_mut().insert(tx);
        let request_id = key.data().as_ffi();
        let command = NetCommand {
            net_action: Some(action),
            request_id,
        };
        command.net_encode(&mut self.client.buffer.borrow_mut())?;
        Ok(rx)
    }

    pub async fn new(addr: SocketAddr) -> IOResult<Self> {
        let stream = TcpStream::connect(addr).await.expect("Failed to connect");
        let (read_stream, write_stream) = stream.into_split();
        let client = MonoIOClient {
            map: RefCell::new(SlotMap::new()),
            buffer: RefCell::new(BytesMut::with_capacity(1 << 16)),
        };
        let ret = Self {
            client: Rc::new(client),
        };
        let read_client = ret.clone();
        let write_client = ret.clone();

        monoio::spawn(read_client.read_loop(read_stream));
        monoio::spawn(write_client.write_loop(write_stream));

        Ok(ret)
    }
    async fn read_loop(self, mut read: OwnedReadHalf<TcpStream>) {
        let mut buffer = BytesMut::with_capacity(1 << 16); // 64 KiB

        loop {
            let response_result =
                read_message_batch::<NetResponse, Response>(&mut read, &mut buffer).await;

            let responses = match response_result {
                Err(err) => {
                    eprintln!("Error with the response: {}", err);
                    break;
                }
                Ok(responses) => responses,
            };
            let mut borrow = self.client.map.borrow_mut();
            for response in responses {
                let key = response.command_id;
                let key = DefaultKey::from(KeyData::from_ffi(key));
                let tx = borrow.remove(key).expect("none key");
                tx.send(response).expect("can't be sent");
            }
        }
    }
    async fn write_loop(self, mut write: OwnedWriteHalf<TcpStream>) {
        let mut interval = interval(Duration::from_millis(1));
        loop {
            interval.tick().await;

            // On détache les données du buffer principal
            let data = {
                let mut buf = self.client.buffer.borrow_mut();
                if buf.is_empty() {
                    continue;
                }
                // split() extrait les octets et laisse le buffer principal VIDE
                // Les nouveaux 'send' écriront dans une zone mémoire fraîche.
                buf.split().freeze()
            };

            // On envoie un buffer IMmuable que personne ne peut plus modifier
            let (res, _) = write.write_all(data).await;

            if res.is_err() {
                eprintln!("TCP Connection Closed during write");
                break;
            }
        }
    }
}
