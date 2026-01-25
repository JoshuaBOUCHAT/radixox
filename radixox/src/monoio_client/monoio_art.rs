use bytes::{Bytes, BytesMut};
use local_sync::mpsc; // Utilise local_sync pour le zéro-overhead atomique
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::io::{OwnedReadHalf, Splitable};
use monoio::net::TcpStream;
use monoio::net::unix::SocketAddr;
use std::collections::HashMap;

// La commande que tes tâches envoient à la boucle IO
pub struct Command {
    pub key: Bytes,
    pub value: Option<Bytes>,
    pub responder: local_sync::oneshot::Sender<Response>,
}

pub type Response = Result<Option<Bytes>, String>;

type IOResult<T> = std::io::Result<T>;
pub struct RadixClient {
    write_stream: OwnedReadHalf<TcpStream>,
}

impl RadixClient {
    pub async fn send(&self, key: &str) -> Response {
        let (tx, rx) = local_sync::oneshot::channel();
        let cmd = Command {
            key: Bytes::copy_from_slice(key.as_bytes()),
            value: None,
            responder: tx,
        };

        self.tx
            .send(cmd)
            .map_err(|_| "Channel closed".to_string())?;
        rx.await.map_err(|_| "Recv error".to_string())
    }
    pub async fn new(addr: SocketAddr) -> IOResult<Self> {
        let stream = TcpStream::connect(addr).await.expect("Failed to connect");
        let (mut read_stream, mut write_stream) = stream.into_split();

        monoio::spawn(async move { loop {} });
    }
}

/// La boucle de traitement qui tourne dans une tâche Monoio dédiée
pub async fn run_client_loop(addr: SocketAddr, mut rx: mpsc::unbounded::Receiver<Command>) {
    // Pour gérer le multiplexage (ID -> Responder)
    let mut pending_requests = HashMap::with_capacity(1024);
    let mut next_request_id: u32 = 0;

    // Buffer d'écriture pour le batching
    let mut write_buf = BytesMut::with_capacity(8192);

    loop {
        monoio::select! {
            // 1. On reçoit des commandes depuis le reste de l'app
            maybe_cmd = rx.recv() => {
                let Some(cmd) = maybe_cmd else { break; };

                // --- STRATÉGIE BATCHING DYNAMIQUE ---
                let mut batch = Vec::with_capacity(64);
                batch.push(cmd);

                // On vide le channel s'il y a d'autres trucs prêts tout de suite
                while batch.len() < 64 {
                    if let Ok(extra) = rx.try_recv() {
                        batch.push(extra);
                    } else {
                        break;
                    }
                }

                // Encodage du batch dans le buffer
                for cmd in batch {
                    next_request_id += 1;
                    pending_requests.insert(next_request_id, cmd.responder);

                    // Format simple : [ID: u32][Len: u16][Key]
                    // (Adapte à ton protocole Prost/Protobuf ici)
                    encode_request(&mut write_buf, next_request_id, &cmd.key);
                }

                // Envoi sur le réseau via io_uring (zéro-copy du buffer)
                let (res, buf) = writer.write_all(write_buf).await;
                write_buf = buf;
                write_buf.clear();
                res.expect("Network write error");
            }

            // 2. On reçoit des réponses depuis le serveur
            // (Il faudrait lancer une tâche séparée pour la lecture pour être vraiment full-duplex)
            // Ici en version simplifiée dans la même boucle
        }
    }
}

fn encode_request(buf: &mut BytesMut, id: u32, key: &Bytes) {
    // Ton code Prost ou manuel pour packager la requête
}
