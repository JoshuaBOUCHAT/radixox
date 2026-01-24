use std::sync::Mutex;

use bytes::Bytes;
use bytes::BytesMut;
use monoio::io::AsyncWriteRentExt;
use monoio::net::TcpStream;
use monoio::spawn;
use prost::Message;
pub use radixox_common::network;
use radixox_common::network::NetCommand;
use radixox_common::network::NetGetRequest;
use radixox_common::network::net_command::NetAction;
pub use radixox_common::protocol;
use radixox_common::protocol::Command;
use radixox_common::protocol::CommandAction;
use radixox_common::protocol::GetAction;
use slotmap::SlotMap;

pub struct OxidART {
    stream: TcpStream,
}
impl OxidART {
    pub async fn new(addr: core::net::SocketAddr) -> std::io::Result<Self> {
        let stream = TcpStream::connect_addr(addr).await?;
        Ok(Self { stream })
    }
    pub async fn get(&mut self, get_action: GetAction) -> Option<Bytes> {
        let request_id = None;

        let net_command = NetCommand {
            net_action: Some(NetAction::Get(NetGetRequest {
                key: get_action.into_parts(),
            })),
            request_id,
        };
        let mut data = BytesMut::new();
        net_command.encode(&mut data);
        //spawn(self.stream.write_all(data));

        todo!()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
