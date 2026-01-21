use std::collections::HashMap;
mod oxidart;

use monoio::{
    io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt},
    net::{TcpListener, TcpStream},
};
use serde::{Deserialize, Serialize};

type IOResult<T> = std::io::Result<T>;

#[monoio::main]
async fn main() -> IOResult<()> {
    let listener = TcpListener::bind("127.0.0.1:8379")?;
    let mut key_store = HashMap::new();
    println!("Listening !");
    loop {
        let (mut stream, _addr) = listener.accept().await?;
        handle_request(&mut stream, &mut key_store);
    }

    Ok(())
}

async fn handle_request(
    stream: &mut TcpStream,
    key_store: &mut HashMap<&[u8], Vec<u8>>,
) -> IOResult<()> {
    let (res, buffer) = stream.read(vec![]).await;
    if res? == 0 {
        return Ok(());
    }
    if buffer.starts_with(b"GET:") {
        return handle_get(&buffer[4..], stream, key_store).await;
    }
    Ok(())
    /*  if buffer.starts_with(b"SET:") {
        handle_set(&buffer[4..]);
    }*/
}

#[derive(Serialize)]
pub enum GetResponse {
    Found(Vec<u8>),
    NotFound,
    Invalid,
}

async fn handle_get(
    key: &[u8],
    stream: &mut TcpStream,
    key_store: &mut HashMap<&[u8], Vec<u8>>,
) -> IOResult<()> {
    let result = match key_store.get(key) {
        Some(val) => GetResponse::Found(val.clone()),
        None => GetResponse::NotFound,
    };
    let reponse = serde_json::to_vec(&result).unwrap_or(Vec::new());
    stream.write_all(reponse).await;
    Ok(())
}

struct SetRequest {
    key: Vec<u8>,
}
async fn handle_set(
    key_val: &[u8],
    stream: &mut TcpStream,
    key_store: &mut HashMap<&[u8], Vec<u8>>,
) -> IOResult<()> {
    Ok(())
}
