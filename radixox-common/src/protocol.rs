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

pub struct GetAction {
    key: Bytes,
}
impl GetAction {
    pub fn into_byte(self) -> Bytes {
        self.key
    }
}

impl GetAction {
    fn new(key: Bytes) -> Result<Self, &'static str> {
        if key.is_ascii() {
            Ok(Self { key })
        } else {
            Err("Key contain non ASCII characters")
        }
    }
}

impl Command {
    pub fn get(key: Bytes) -> Result<Self, &'static str> {
        Ok(Self::Get(GetAction::new(key)?))
    }
    pub fn set(key: Bytes, val: Bytes) -> Result<Self, &'static str> {
        Ok(Self::Set(SetAction::new(key, val)?))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn it_works() {}
}
