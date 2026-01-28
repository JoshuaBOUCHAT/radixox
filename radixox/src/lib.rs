pub mod monoio_client;
pub mod tokio_client;
pub struct OxidART {}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use monoio::{net::TcpStream, time::Instant};
    use radixox_common::network::{NetGetRequest, NetSetRequest, net_command::NetAction};

    use crate::monoio_client::monoio_art::SharedMonoIOClient;

    #[monoio::test]
    async fn it_works() {
        let addr_v4 = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8379);
    }

    #[monoio::test(enable_timer = true)]
    async fn test_alphabet_hardcore() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 24), 8379));

        let client = SharedMonoIOClient::new(addr)
            .await
            .expect("client init error");

        let words: Vec<String> = include_str!("../../list.txt")
            .lines()
            .map(|s| s.to_string())
            .collect();

        let total_words = words.len();
        let now = Instant::now();

        // --- PHASE 1 : SET ---
        let mut handles = Vec::new();
        for chunk in words.chunks(50) {
            let chunk_data = chunk.to_vec();
            let c = client.clone();

            // monoio::spawn renvoie un JoinHandle
            let handle = monoio::spawn(async move {
                for word in chunk_data {
                    let action = NetAction::Set(NetSetRequest {
                        key: word.clone().into(),
                        value: word.into(),
                    });

                    c.send(action).expect("enc").await.expect("resp");
                }
            });
            handles.push(handle);
        }

        // On attend toutes les tâches de SET
        for h in handles {
            h.await;
        }
        println!("Time for SET: {}s", now.elapsed().as_secs_f32());

        // --- PHASE 2 : GET ---
        let mut handles = Vec::new();
        let get_start = Instant::now();
        for chunk in words.chunks(50) {
            let chunk_data = chunk.to_vec();
            let c = client.clone();

            let handle = monoio::spawn(async move {
                for word in chunk_data {
                    let action = NetAction::Get(NetGetRequest { key: word.into() });
                    c.send(action).expect("enc").await.expect("resp");
                }
            });
            handles.push(handle);
        }

        for h in handles {
            h.await;
        }

        let elapsed = now.elapsed();
        println!("---------------------------------------");
        println!("Total Time: {}s", elapsed.as_secs_f32());
        println!(
            "Throughput: {} req/s",
            (total_words * 2) as f32 / elapsed.as_secs_f32()
        );
        println!("---------------------------------------");
    }
}
