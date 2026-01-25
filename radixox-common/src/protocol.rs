use std::fmt::Display;

use bytes::Bytes;
use monoio::{
    buf::IoBufMut,
    io::{AsyncReadRent, AsyncReadRentExt, OwnedReadHalf},
    net::TcpStream,
};
use prost::Message;

use crate::{FromStream, NetValidate};

type IOResult<T> = std::io::Result<T>;

pub struct Command {
    pub action: CommandAction,
    pub command_id: u32,
}
impl Command {
    pub fn new(action: CommandAction, command_id: u32) -> Self {
        Command { action, command_id }
    }
}
/*impl FromStream for Command {
    async fn from_stream(
        stream: &mut monoio::io::OwnedReadHalf<TcpStream>,
        buffer: &mut Vec<u8>,
    ) -> std::io::Result<Self> {
        read_message(stream, buffer).await
    }
}*/

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
        write!(f, "{}: ID: {}", self.action, self.command_id)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn it_works() {}
}

fn parse_varint_header(buf: &[u8], n: usize) -> IOResult<(usize, usize)> {
    let mut cursor = std::io::Cursor::new(&buf[..n]);
    let msg_len = prost::encoding::decode_varint(&mut cursor)
        .map_err(|e| std::io::Error::other(format!("Varint: {e}")))? as usize;

    Ok((msg_len, cursor.position() as usize))
}

/// Decode le buffer en Command
fn decode_message<T, V>(buf: &[u8]) -> IOResult<V>
where
    T: Message + NetValidate<V> + Default,
{
    let net_cmd = T::decode(buf).map_err(|e| std::io::Error::other(format!("Proto: {e}")))?;

    net_cmd
        .validate()
        .map_err(|_| std::io::Error::other("Invalid command"))
}

pub async fn read_message<T, V>(
    stream: &mut OwnedReadHalf<TcpStream>,
    buf: &mut Vec<u8>,
) -> IOResult<V>
where
    T: Message + NetValidate<V> + Default,
{
    // 1. On récupère le buffer (ownership)
    let tmp = std::mem::take(buf);

    // 2. Première lecture (on peut lire le header + une partie du payload)
    let (res, tmp_slice) = stream.read_exact(tmp.slice_mut(0..4)).await;
    let tmp = tmp_slice.into_inner();
    let n = res?;

    if n == 0 {
        *buf = tmp;
        return Err(std::io::ErrorKind::ConnectionReset.into());
    }

    // 3. Analyse du header
    let (msg_len, varint_len) = parse_varint_header(&tmp, n)?;
    let total_expected = varint_len + msg_len;

    // 4. On vérifie s'il nous en manque
    let mut tmp = tmp;
    if n < total_expected {
        let slice = tmp.slice_mut(n..total_expected);

        let (res, slice) = stream.read_exact(slice).await;
        res?;

        tmp = slice.into_inner();
    }

    // 5. Décodage (Zero-copy via slice)
    let cmd = decode_message::<T, V>(&tmp[varint_len..total_expected])?;

    // 6. On rend le buffer pour la prochaine itération
    // Optionnel : on pourrait vider le buffer ici ou gérer le "surplus" lu
    *buf = tmp;
    buf.clear();

    Ok(cmd)
}
