use bytes::BytesMut;
use monoio::{buf::IoBufMut, io::OwnedReadHalf, net::TcpStream};
use prost::{EncodeError, Message};

use crate::network::{
    NetError, ResponseResult, net_response::NetResponseResult, net_success_response::Body,
};

pub mod protocol;

// ============================================================================
// NETWORK MODULE - Protobuf types and validation
// ============================================================================

pub mod network {
    use bytes::Bytes;

    use crate::{
        NetValidate,
        network::net_command::NetAction,
        parse_response_result,
        protocol::{Command, CommandAction},
    };

    // Include generated protobuf code
    include!(concat!(env!("OUT_DIR"), "/radixox.rs"));

    /// Network-level errors
    #[derive(Debug)]
    pub enum NetError {
        NetError(String),
        CommandEmpty,
        GetEmpty,
        SetEmpty,
        PrefixNotAscii,
        KeyNotAscii,
        ResponseBodyEmpty,
    }

    /// Validated response from server
    #[derive(Debug)]
    pub struct Response {
        pub command_id: u64,
        pub result: ResponseResult,
    }

    /// Response result variants
    #[derive(Debug)]
    pub enum ResponseResult {
        Empty,
        Err(),
        Data(Bytes),       // Single value (GET response)
        Datas(Vec<Bytes>), // Multiple values (GETN response)
    }

    // ========================================================================
    // VALIDATION IMPLEMENTATIONS
    // ========================================================================

    impl NetValidate<Command> for NetCommand {
        fn validate(self) -> Result<Command, NetError> {
            let Some(command_action) = self.net_action else {
                return Err(NetError::CommandEmpty);
            };
            Ok(Command::new(command_action.validate()?, self.request_id))
        }
    }

    impl NetValidate<CommandAction> for NetAction {
        fn validate(self) -> Result<CommandAction, NetError> {
            match self {
                NetAction::Get(get) => {
                    CommandAction::get(get.key).map_err(|_| NetError::KeyNotAscii)
                }
                NetAction::Set(set) => {
                    CommandAction::set(set.key, set.value).map_err(|_| NetError::KeyNotAscii)
                }
                NetAction::Del(del) => {
                    CommandAction::del(del.key).map_err(|_| NetError::KeyNotAscii)
                }
                NetAction::Getn(getn) => {
                    CommandAction::getn(getn.prefix).map_err(|_| NetError::PrefixNotAscii)
                }
                NetAction::Deln(deln) => {
                    CommandAction::deln(deln.prefix).map_err(|_| NetError::PrefixNotAscii)
                }
            }
        }
    }

    impl NetValidate<Response> for NetResponse {
        fn validate(self) -> Result<Response, NetError> {
            Ok(Response {
                result: parse_response_result(self.net_response_result)?,
                command_id: self.request_id,
            })
        }
    }
}

// ============================================================================
// RESPONSE PARSING
// ============================================================================

fn parse_response_result(net_res: Option<NetResponseResult>) -> Result<ResponseResult, NetError> {
    let Some(result) = net_res else {
        return Ok(ResponseResult::Empty);
    };

    let success_val = match result {
        NetResponseResult::Error(err) => return Err(NetError::NetError(err.message)),
        NetResponseResult::Success(success_val) => success_val,
    };

    let body = success_val.body.ok_or(NetError::ResponseBodyEmpty)?;
    match body {
        Body::SingleValue(val) => Ok(ResponseResult::Data(val)),
        Body::MultiValue(vals) => Ok(ResponseResult::Datas(vals.values)),
    }
}

// ============================================================================
// TRAITS
// ============================================================================

/// Validate network messages into typed commands
pub trait NetValidate<T>
where
    Self: Sized,
{
    fn validate(self) -> Result<T, NetError>;
}

/// Read messages from TCP stream
pub trait FromStream
where
    Self: Sized,
{
    fn from_stream(
        stream: &mut OwnedReadHalf<TcpStream>,
        buffer: &mut Vec<u8>,
    ) -> std::io::Result<Self>;
}

/// Encode messages for network transmission
pub trait NetEncode<T: IoBufMut> {
    fn net_encode(&self, buffer: &mut T) -> Result<(), EncodeError>;
}

impl<T> NetEncode<BytesMut> for T
where
    T: Message,
{
    fn net_encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        let start_idx = buffer.len();
        // Write 4-byte placeholder for message size
        buffer.extend_from_slice(0u32.to_be_bytes().as_slice());
        self.encode(buffer)?;
        // Update size field with actual message length
        let msg_len_bytes = ((buffer.len() - start_idx - size_of::<u32>()) as u32).to_be_bytes();
        for i in 0..4 {
            buffer[i + start_idx] = msg_len_bytes[i];
        }
        Ok(())
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod test {
    use bytes::BytesMut;
    use prost::Message;

    use crate::{
        NetEncode,
        network::{NetCommand, net_command::NetAction},
    };

    #[test]
    fn test_encoding_get() {
        let command = NetCommand {
            request_id: 0,
            net_action: Some(NetAction::Get(crate::network::NetGetRequest {
                key: "user:1".into(),
            })),
        };
        let mut buffer = BytesMut::new();
        command.net_encode(&mut buffer).expect("encoding error");
        let decoded = NetCommand::decode(&buffer[4..]).expect("decoding error");
        assert_eq!(command, decoded);
    }

    #[test]
    fn test_encoding_getn() {
        let command = NetCommand {
            request_id: 1,
            net_action: Some(NetAction::Getn(crate::network::NetGetNRequest {
                prefix: "user".into(),
            })),
        };
        let mut buffer = BytesMut::new();
        command.net_encode(&mut buffer).expect("encoding error");
        let decoded = NetCommand::decode(&buffer[4..]).expect("decoding error");
        assert_eq!(command, decoded);
    }

    #[test]
    fn test_encoding_deln() {
        let command = NetCommand {
            request_id: 2,
            net_action: Some(NetAction::Deln(crate::network::NetDelNRequest {
                prefix: "session".into(),
            })),
        };
        let mut buffer = BytesMut::new();
        command.net_encode(&mut buffer).expect("encoding error");
        let decoded = NetCommand::decode(&buffer[4..]).expect("decoding error");
        assert_eq!(command, decoded);
    }
}
