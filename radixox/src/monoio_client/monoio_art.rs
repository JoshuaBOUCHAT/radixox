//! Monoio-based client implementation.
//!
//! This module provides a high-performance async client using the monoio runtime
//! with io_uring support on Linux.
//!
//! # Architecture
//!
//! The client uses a split architecture:
//! - **Write loop**: Batches outgoing requests on a 1ms timer for throughput
//! - **Read loop**: Processes incoming responses and dispatches to waiters
//!
//! # Example
//!
//! ```no_run
//! use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
//! use radixox::{ArtClient, monoio_client::monoio_art::SharedMonoIOClient};
//!
//! #[monoio::main]
//! async fn main() {
//!     let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8379));
//!     let client = SharedMonoIOClient::new(addr).await.unwrap();
//!
//!     client.set("hello", "world").await.unwrap();
//!     println!("{:?}", client.get("hello").await);
//! }
//! ```

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use local_sync::oneshot::{Receiver, Sender};
use monoio::io::{AsyncWriteRentExt, OwnedReadHalf, OwnedWriteHalf, Splitable};
use monoio::net::TcpStream;
use monoio::time::interval;
use prost::EncodeError;
use slotmap::{DefaultKey, Key, KeyData, SlotMap};

use radixox_common::NetEncode;
use radixox_common::network::net_command::NetAction;
use radixox_common::network::{
    NetCommand, NetDelNRequest, NetDelRequest, NetGetNRequest, NetGetRequest, NetResponse,
    NetSetRequest, Response, ResponseResult,
};
use radixox_common::protocol::read_message_batch;

use crate::{ArtClient, ArtError};

type IOResult<T> = std::io::Result<T>;

// ============================================================================
// INTERNAL CLIENT STATE
// ============================================================================

struct MonoIOClient {
    map: RefCell<SlotMap<DefaultKey, Sender<Response>>>,
    buffer: RefCell<BytesMut>,
}

// ============================================================================
// PUBLIC CLIENT
// ============================================================================

/// Shared monoio-based ART client
///
/// This client is designed for single-threaded async usage with monoio runtime.
/// It uses request batching for high throughput.
#[derive(Clone)]
pub struct SharedMonoIOClient {
    client: Rc<MonoIOClient>,
}

impl SharedMonoIOClient {
    /// Create a new client connected to the server at the given address
    pub async fn new(addr: SocketAddr) -> IOResult<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (read_stream, write_stream) = stream.into_split();

        let client = MonoIOClient {
            map: RefCell::new(SlotMap::new()),
            buffer: RefCell::new(BytesMut::with_capacity(1 << 16)),
        };

        let ret = Self {
            client: Rc::new(client),
        };

        monoio::spawn(ret.clone().read_loop(read_stream));
        monoio::spawn(ret.clone().write_loop(write_stream));

        Ok(ret)
    }

    /// Send a raw action and return a receiver for the response
    pub fn send(&self, action: NetAction) -> Result<Receiver<Response>, EncodeError> {
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

    async fn read_loop(self, mut read: OwnedReadHalf<TcpStream>) {
        let mut buffer = BytesMut::with_capacity(1 << 16);

        loop {
            let response_result =
                read_message_batch::<NetResponse, Response>(&mut read, &mut buffer).await;

            let responses = match response_result {
                Err(err) => {
                    eprintln!("Read error: {}", err);
                    break;
                }
                Ok(responses) => responses,
            };

            let mut borrow = self.client.map.borrow_mut();
            for response in responses {
                let key = DefaultKey::from(KeyData::from_ffi(response.command_id));
                if let Some(tx) = borrow.remove(key) {
                    let _ = tx.send(response);
                }
            }
        }
    }

    async fn write_loop(self, mut write: OwnedWriteHalf<TcpStream>) {
        let mut interval = interval(Duration::from_millis(1));

        loop {
            interval.tick().await;

            let data = {
                let mut buf = self.client.buffer.borrow_mut();
                if buf.is_empty() {
                    continue;
                }
                buf.split().freeze()
            };

            let (res, _) = write.write_all(data).await;
            if res.is_err() {
                eprintln!("Write error: connection closed");
                break;
            }
        }
    }
}

// ============================================================================
// TRAIT IMPLEMENTATION
// ============================================================================

impl ArtClient for SharedMonoIOClient {
    async fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Bytes>, ArtError> {
        let action = NetAction::Get(NetGetRequest {
            key: Bytes::copy_from_slice(key.as_ref()),
        });

        let response = self.send(action)?.await.map_err(|_| ArtError::ChannelClosed)?;

        match response.result {
            ResponseResult::Data(data) => Ok(Some(data)),
            ResponseResult::Empty => Ok(None),
            _ => Ok(None),
        }
    }

    async fn set(&self, key: impl AsRef<[u8]>, value: impl Into<Bytes>) -> Result<(), ArtError> {
        let action = NetAction::Set(NetSetRequest {
            key: Bytes::copy_from_slice(key.as_ref()),
            value: value.into(),
        });

        let _response = self.send(action)?.await.map_err(|_| ArtError::ChannelClosed)?;
        Ok(())
    }

    async fn del(&self, key: impl AsRef<[u8]>) -> Result<Option<Bytes>, ArtError> {
        let action = NetAction::Del(NetDelRequest {
            key: Bytes::copy_from_slice(key.as_ref()),
        });

        let response = self.send(action)?.await.map_err(|_| ArtError::ChannelClosed)?;

        match response.result {
            ResponseResult::Data(data) => Ok(Some(data)),
            ResponseResult::Empty => Ok(None),
            _ => Ok(None),
        }
    }

    async fn getn(&self, prefix: impl AsRef<[u8]>) -> Result<Vec<Bytes>, ArtError> {
        let action = NetAction::Getn(NetGetNRequest {
            prefix: Bytes::copy_from_slice(prefix.as_ref()),
        });

        let response = self.send(action)?.await.map_err(|_| ArtError::ChannelClosed)?;

        match response.result {
            ResponseResult::Datas(data) => Ok(data),
            ResponseResult::Empty => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }

    async fn deln(&self, prefix: impl AsRef<[u8]>) -> Result<(), ArtError> {
        let action = NetAction::Deln(NetDelNRequest {
            prefix: Bytes::copy_from_slice(prefix.as_ref()),
        });

        let _response = self.send(action)?.await.map_err(|_| ArtError::ChannelClosed)?;
        Ok(())
    }
}
