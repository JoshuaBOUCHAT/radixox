use crate::network::NetError;

pub mod network {
    use crate::{
        NetValidate,
        network::net_command::NetAction,
        protocol::{Command, CommandAction},
    };

    include!(concat!(env!("OUT_DIR"), "/radixox.rs"));

    pub enum NetError {
        CommandEmpty,
        GetEmpty,
        SetEmpty,
        KeyNotAscii,
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
}
pub mod protocol;
pub trait NetValidate<T>
where
    Self: Sized,
{
    fn validate(self) -> Result<T, NetError>;
}
