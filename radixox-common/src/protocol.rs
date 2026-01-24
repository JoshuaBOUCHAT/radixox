use bytes::Bytes;

pub enum Command {
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

impl Command {
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

#[cfg(test)]
mod tests {

    #[test]
    fn it_works() {}
}
