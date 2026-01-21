use bytes::Bytes;

pub enum Command {
    Set(SetAction),
    Get(GetAction),
}
pub struct SetAction {
    key: Bytes,
    val: Bytes,
}
impl SetAction {
    pub fn new(key: Bytes, val: Bytes) -> Self {
        assert!(key.len() != 0);
        Self { key, val }
    }
}

pub struct GetAction {
    key: Bytes,
}
impl GetAction {
    fn new(key: Bytes) -> Self {
        assert!(key.len() != 0);
        Self { key }
    }
}

impl Command {
    pub fn get(key: Bytes) -> Self {
        Self::Get(GetAction::new(key))
    }
    pub fn set(key: Bytes, val: Bytes) -> Self {
        Self::Set(SetAction::new(key, val))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn it_works() {}
}
