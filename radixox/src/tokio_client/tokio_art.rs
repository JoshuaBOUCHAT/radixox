use std::net::IpAddr;

use monoio::net::TcpListener;
#[cfg(feature = "tokio")]
use monoio::net::unix::SocketAddr;
use radixox_common::protocol::Command;

type IOResult<T> = std::io::Result<T>;

#[cfg(feature = "tokio")]
pub struct TokioClient {
    queue: tokio::sync::mpsc::Sender<Command>,
}
#[cfg(feature = "tokio")]
impl OxidArtTokioClient {
    pub async fn new(addr: SocketAddr) -> Self {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<Command>(1024);

        // On lance le thread IO dédié à Monoio
        std::thread::spawn(move || {
            let mut rt = monoio::RuntimeBuilder::new_uring().build().unwrap();
            rt.block_on(async {
                // Ici, on initialise ton client Monoio natif
                let mut native_client = OxidArtNative::connect(addr).await.unwrap();

                // On écoute les commandes venant du monde Tokio
                while let Some(cmd) = command_rx.recv().await {
                    // On exécute sur le client Monoio et on répond via oneshot
                    let res = native_client.execute(cmd.data).await;
                    let _ = cmd.responder.send(res);
                }
            });
        });

        Self { command_tx }
    }
}
