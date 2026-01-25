pub mod monoio_client;
pub mod tokio_client;
pub struct OxidART {}
impl OxidART {
    pub async fn new(addr: core::net::SocketAddr) -> std::io::Result<Self> {
        let stream = TcpStream::connect_addr(addr).await?;
        let (read, write) = stream.into_split();

        Ok(Self {})
    }
    pub async fn get(
        &mut self,
        get_action: GetAction,
        chanels: monoio::io::stream,
    ) -> Option<Bytes> {
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
