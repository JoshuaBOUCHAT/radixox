use std::fmt::Display;

use bytes::Bytes;

pub struct Command {
    pub action: CommandAction,
    pub command_id: Option<u32>,
}
impl Command {
    pub fn new(action: CommandAction, command_id: Option<u32>) -> Self {
        Command { action, command_id }
    }
}

pub enum CommandAction {
    Set(SetAction),
    Get(GetAction),
    Del(DelAction),
}
pub struct SetAction {
    key: Bytes,
    val: Bytes,
}
pub struct GetAction {
    key: Bytes,
}
pub struct DelAction {
    key: Bytes,
}

impl CommandAction {
    pub fn get(key: Bytes) -> Result<Self, &'static str> {
        Ok(Self::Get(GetAction::new(key)?))
    }
    pub fn set(key: Bytes, val: Bytes) -> Result<Self, &'static str> {
        Ok(Self::Set(SetAction::new(key, val)?))
    }
    pub fn del(key: Bytes) -> Result<Self, &'static str> {
        Ok(Self::Del(DelAction::new(key)?))
    }
}

impl SetAction {
    pub fn new(key: Bytes, val: Bytes) -> Result<Self, &'static str> {
        if key.is_ascii() {
            Ok(Self { key, val })
        } else {
            Err("Key contain non ASCII characters")
        }
    }
    ///return (key,val)
    pub fn into_parts(self) -> (Bytes, Bytes) {
        (self.key, self.val)
    }
}

impl GetAction {
    pub fn into_parts(self) -> Bytes {
        self.key
    }
    pub fn new(key: Bytes) -> Result<Self, &'static str> {
        if key.is_ascii() {
            Ok(Self { key })
        } else {
            Err("Key contain non ASCII characters")
        }
    }
}

impl DelAction {
    pub fn into_parts(self) -> Bytes {
        self.key
    }
    pub fn new(key: Bytes) -> Result<Self, &'static str> {
        if key.is_ascii() {
            Ok(Self { key })
        } else {
            Err("Key contain non ASCII characters")
        }
    }
}

impl Display for GetAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Key is ASCII so from_utf8 is safe
        let key = std::str::from_utf8(&self.key).unwrap();
        write!(f, "key: {key}")
    }
}

impl Display for DelAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = std::str::from_utf8(&self.key).unwrap();
        write!(f, "key: {key}")
    }
}

impl Display for SetAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = std::str::from_utf8(&self.key).unwrap();
        let val = String::from_utf8_lossy(&self.val);
        write!(f, "key: {key}\n    value: {val}")
    }
}

impl Display for CommandAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandAction::Get(action) => write!(f, "GET:\n    {action}"),
            CommandAction::Del(action) => write!(f, "DEL:\n    {action}"),
            CommandAction::Set(action) => write!(f, "SET:\n    {action}"),
        }
    }
}
impl Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = self
            .command_id
            .map(|n| n.to_string())
            .unwrap_or(String::from("None"));
        write!(f, "{}: ID: {id}", self.action)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn it_works() {}
}
