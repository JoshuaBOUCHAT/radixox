use crate::network::NetError;

pub mod network {
    use crate::{NetValidate, network::net_command::NetAction, protocol::Command};

    include!(concat!(env!("OUT_DIR"), "/radixox.rs"));

    pub enum NetError {
        CommandEmpty,
        GetEmpty,
        SetEmpty,
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
                NetAction::Get(get) => {
                    if get.key.len() == 0 {
                        return Err(NetError::GetEmpty);
                    }
                    Ok(Command::get(get.key))
                }
                NetAction::Getn(_getn) => {
                    todo!()
                }
                NetAction::Set(set) => {
                    if set.key.len() == 0 {
                        return Err(NetError::SetEmpty);
                    }
                    Ok(Command::set(set.key, set.value))
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
