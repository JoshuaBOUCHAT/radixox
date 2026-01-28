use std::fmt::Display;

use bytes::{Bytes, BytesMut};
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
    pub command_id: u64,
}
impl Command {
    pub fn new(action: CommandAction, command_id: u64) -> Self {
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
pub struct GetN {
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
    //println!("  Message recieve");
    // 1. On récupère le buffer (ownership)
    let mut tmp = std::mem::take(buf);

    if tmp.capacity() < 4 {
        tmp.reserve(4);
    }

    //println!("  Reading message len");
    // 2. Première lecture (on peut lire le header + une partie du payload)
    let header_buf = vec![0u8; 4];
    let (res, header_buf) = stream.read_exact(header_buf).await;
    let n = res?;

    if n == 0 {
        *buf = tmp;
        return Err(std::io::ErrorKind::ConnectionReset.into());
    }

    let msg_len = u32::from_be_bytes(core::array::from_fn(|i| header_buf[i])) as usize;
    //println!("  Message len:{}", msg_len);

    // 4. On vérifie s'il nous en manque

    tmp.reserve(msg_len);

    //println!("  Reading the message");
    let (res, slice_mut) = stream.read_exact(tmp.slice_mut(0..msg_len)).await;
    tmp = slice_mut.into_inner();
    res.expect("faut check");

    //println!("  Message read");

    //println!("  Decoding message");
    // 5. Décodage (Zero-copy via slice)
    let cmd = decode_message::<T, V>(tmp.as_slice())?;
    //println!("  Message decoded");

    // 6. On rend le buffer pour la prochaine itération
    // Optionnel : on pourrait vider le buffer ici ou gérer le "surplus" lu
    *buf = tmp;
    buf.clear();

    Ok(cmd)
}
pub async fn read_message_batch<T, V>(
    stream: &mut OwnedReadHalf<TcpStream>,
    datas: &mut BytesMut,
) -> IOResult<Vec<V>>
where
    T: Message + NetValidate<V> + Default,
{
    // 1. Monoio prend le buffer (Transfert d'ownership pour io_uring)
    let buffer = std::mem::take(datas);
    let len = buffer.len();
    let (n, buf) = stream.read(buffer.slice_mut(len..)).await;
    let mut buf = buf.into_inner();

    if !n.as_ref().is_ok_and(|nb| *nb != 0) {
        println!("{}", std::io::Error::last_os_error());
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("{:?}", n),
        ));
    }

    let mut res = Vec::new();
    let mut cursor = 0;

    // 2. Boucle de parsing sur le buffer actuel
    // On utilise un curseur pour ne pas modifier le buffer pendant qu'on lit
    while buf.len() - cursor >= 4 {
        let start = cursor;
        let msg_len =
            u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]])
                as usize;

        let total_len = 4 + msg_len;

        // Si le message est incomplet dans le buffer actuel
        if buf.len() - cursor < total_len {
            break;
        }

        // Extraction et décodage (Zero-copy via slicing)
        let data = &buf[start + 4..start + total_len];
        if let Ok(net_message) = T::decode(data) {
            if let Ok(validated) = net_message.validate() {
                res.push(validated);
            }
        }

        cursor += total_len;
    }

    // 3. LE SHIFT : Maximiser l'espace pour le prochain syscall
    if cursor > 0 {
        if cursor < buf.len() {
            // On déplace le "reste" du message au tout début du buffer
            buf.copy_within(cursor.., 0);
            buf.truncate(buf.len() - cursor);
        } else {
            // Tout a été lu, on reset simplement le buffer à zéro (O(1))
            buf.clear();
        }
    }

    // On rend le buffer propre à la boucle infinie
    *datas = buf;
    Ok(res)
}
