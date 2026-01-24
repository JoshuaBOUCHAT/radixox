use crate::network::NetError;

pub mod network {
    use crate::{NetValidate, network::net_command::NetAction, protocol::Command};

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
            command_action.validate()
        }
    }
    impl NetValidate<Command> for NetAction {
        fn validate(self) -> Result<Command, NetError> {
            match self {
                NetAction::Get(get) => Command::get(get.key).map_err(|_| NetError::KeyNotAscii),
                NetAction::Getn(_getn) => {
                    todo!()
                }
                NetAction::Set(set) => {
                    Command::set(set.key, set.value).map_err(|_| NetError::KeyNotAscii)
                }
                NetAction::Del(del) => Command::del(del.key).map_err(|_| NetError::KeyNotAscii),
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
