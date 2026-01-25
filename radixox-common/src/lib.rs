use monoio::{
    io::{OwnedReadHalf, stream::Stream},
    net::TcpStream,
};

use crate::network::NetError;

pub mod network {
    use bytes::Bytes;

    use crate::{
        NetValidate,
        network::net_command::NetAction,
        protocol::{Command, CommandAction},
    };

    include!(concat!(env!("OUT_DIR"), "/radixox.rs"));

    pub enum NetError {
        NetError(String),
        CommandEmpty,
        GetEmpty,
        SetEmpty,
        KeyNotAscii,
        ResponseBodyEmpty,
    }
    pub enum Response {
        Empty,
        Err(),
        Data(Bytes),
        Datas(Vec<Bytes>),
    }
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
                NetAction::Getn(_getn) => {
                    todo!()
                }
                NetAction::Set(set) => {
                    CommandAction::set(set.key, set.value).map_err(|_| NetError::KeyNotAscii)
                }
                NetAction::Del(del) => {
                    CommandAction::del(del.key).map_err(|_| NetError::KeyNotAscii)
                }
            }
        }
    }
    impl NetValidate<Response> for NetResponse {
        fn validate(self) -> Result<Response, NetError> {
            let Some(result) = self.result else {
                return Ok(Response::Empty);
            };

            let success_val = match result {
                net_response::Result::Error(err) => return Err(NetError::NetError(err.message)),
                net_response::Result::Success(success_val) => success_val,
            };

            let body = success_val.body.ok_or(NetError::ResponseBodyEmpty)?;
            match body {
                net_success_response::Body::GetVal(val) => Ok(Response::Data(val)),
                net_success_response::Body::KeysVal(vals) => Ok(Response::Datas(vals.keys)),
            }
        }
    }
}

pub mod protocol;
pub trait NetValidate<T>
where
    Self: Sized,
{
    fn validate(self) -> Result<T, NetError>;
}
pub trait FromStream
where
    Self: Sized,
{
    fn from_stream(
        stream: &mut OwnedReadHalf<TcpStream>,
        buffer: &mut Vec<u8>,
    ) -> std::io::Result<Self>;
}
