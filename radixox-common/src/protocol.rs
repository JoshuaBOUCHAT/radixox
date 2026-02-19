use std::fmt::Display;

use bytes::{Bytes, BytesMut};
use monoio::{
    buf::IoBufMut,
    io::{AsyncReadRent, AsyncReadRentExt, OwnedReadHalf},
    net::TcpStream,
};
use prost::Message;

use crate::NetValidate;

type IOResult<T> = std::io::Result<T>;

// ============================================================================
// COMMAND STRUCTURES
// ============================================================================

/// A validated command with its action and unique identifier
pub struct Command {
    pub action: CommandAction,
    pub command_id: u64,
}

impl Command {
    pub fn new(action: CommandAction, command_id: u64) -> Self {
        Command { action, command_id }
    }
}

/// All possible command actions
pub enum CommandAction {
    Set(SetAction),
    Get(GetAction),
    GetN(GetNAction),
    Del(DelAction),
    DelN(DelNAction),
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

    pub fn getn(prefix: Bytes) -> Result<Self, &'static str> {
        Ok(Self::GetN(GetNAction::new(prefix)?))
    }

    pub fn deln(prefix: Bytes) -> Result<Self, &'static str> {
        Ok(Self::DelN(DelNAction::new(prefix)?))
    }
}

// ============================================================================
// SINGLE KEY ACTIONS
// ============================================================================

/// SET action: store a value at a key
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

    /// Returns (key, value)
    pub fn into_parts(self) -> (Bytes, Bytes) {
        (self.key, self.val)
    }
}

/// GET action: retrieve a value by key
pub struct GetAction {
    key: Bytes,
}

impl GetAction {
    pub fn new(key: Bytes) -> Result<Self, &'static str> {
        if key.is_ascii() {
            Ok(Self { key })
        } else {
            Err("Key contain non ASCII characters")
        }
    }

    pub fn into_parts(self) -> Bytes {
        self.key
    }
}

/// DEL action: delete a value by key
pub struct DelAction {
    key: Bytes,
}

impl DelAction {
    pub fn new(key: Bytes) -> Result<Self, &'static str> {
        if key.is_ascii() {
            Ok(Self { key })
        } else {
            Err("Key contain non ASCII characters")
        }
    }

    pub fn into_parts(self) -> Bytes {
        self.key
    }
}

// ============================================================================
// PREFIX-BASED ACTIONS (implicit wildcard suffix)
// ============================================================================

/// GETN action: retrieve all values with keys starting with the given prefix
/// Example: prefix "user" matches "user:1", "user:2", "user:admin", etc.
pub struct GetNAction {
    prefix: Bytes,
}

impl GetNAction {
    pub fn new(prefix: Bytes) -> Result<Self, &'static str> {
        if prefix.is_ascii() {
            Ok(Self { prefix })
        } else {
            Err("Prefix contain non ASCII characters")
        }
    }

    pub fn into_parts(self) -> Bytes {
        self.prefix
    }
}

/// DELN action: delete all keys starting with the given prefix
/// Example: prefix "session" deletes "session:1", "session:abc", etc.
pub struct DelNAction {
    prefix: Bytes,
}

impl DelNAction {
    pub fn new(prefix: Bytes) -> Result<Self, &'static str> {
        if prefix.is_ascii() {
            Ok(Self { prefix })
        } else {
            Err("Prefix contain non ASCII characters")
        }
    }

    pub fn into_parts(self) -> Bytes {
        self.prefix
    }
}

// ============================================================================
// DISPLAY IMPLEMENTATIONS
// ============================================================================

impl Display for SetAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = std::str::from_utf8(&self.key).unwrap();
        let val = String::from_utf8_lossy(&self.val);
        write!(f, "key: {key}\n    value: {val}")
    }
}

impl Display for GetAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

impl Display for GetNAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = std::str::from_utf8(&self.prefix).unwrap();
        write!(f, "prefix: {prefix}")
    }
}

impl Display for DelNAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = std::str::from_utf8(&self.prefix).unwrap();
        write!(f, "prefix: {prefix}")
    }
}

impl Display for CommandAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandAction::Set(action) => write!(f, "SET:\n    {action}"),
            CommandAction::Get(action) => write!(f, "GET:\n    {action}"),
            CommandAction::GetN(action) => write!(f, "GETN:\n    {action}"),
            CommandAction::Del(action) => write!(f, "DEL:\n    {action}"),
            CommandAction::DelN(action) => write!(f, "DELN:\n    {action}"),
        }
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ID: {}", self.action, self.command_id)
    }
}

// ============================================================================
// NETWORK I/O
// ============================================================================

/// Decode buffer into a validated message
fn decode_message<T, V>(buf: &[u8]) -> IOResult<V>
where
    T: Message + NetValidate<V> + Default,
{
    let net_cmd = T::decode(buf).map_err(|e| std::io::Error::other(format!("Proto: {e}")))?;

    net_cmd
        .validate()
        .map_err(|_| std::io::Error::other("Invalid command"))
}

/// Read a single message from the stream
pub async fn read_message<T, V>(
    stream: &mut OwnedReadHalf<TcpStream>,
    buf: &mut Vec<u8>,
) -> IOResult<V>
where
    T: Message + NetValidate<V> + Default,
{
    let mut tmp = std::mem::take(buf);

    if tmp.capacity() < 4 {
        tmp.reserve(4);
    }

    // Read 4-byte header (message length)
    let header_buf = vec![0u8; 4];
    let (res, header_buf) = stream.read_exact(header_buf).await;
    let n = res?;

    if n == 0 {
        *buf = tmp;
        return Err(std::io::ErrorKind::ConnectionReset.into());
    }

    let msg_len = u32::from_be_bytes(core::array::from_fn(|i| header_buf[i])) as usize;

    // Read message payload
    tmp.reserve(msg_len);

    let (res, slice_mut) = stream.read_exact(tmp.slice_mut(0..msg_len)).await;
    tmp = slice_mut.into_inner();
    res.expect("faut check");

    // Decode and validate
    let cmd = decode_message::<T, V>(tmp.as_slice())?;

    // Return buffer for reuse
    *buf = tmp;
    buf.clear();

    Ok(cmd)
}

/// Read a batch of messages from the stream (for high throughput)
pub async fn read_message_batch<T, V>(
    stream: &mut OwnedReadHalf<TcpStream>,
    datas: &mut BytesMut,
) -> IOResult<Vec<V>>
where
    T: Message + NetValidate<V> + Default,
{
    // Monoio takes buffer ownership for io_uring
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

    // Parse messages from buffer using cursor
    while buf.len() - cursor >= 4 {
        let start = cursor;
        let msg_len =
            u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]])
                as usize;

        let total_len = 4 + msg_len;

        // Incomplete message, wait for more data
        if buf.len() - cursor < total_len {
            break;
        }

        // Zero-copy decode via slicing
        let data = &buf[start + 4..start + total_len];
        if let Ok(net_message) = T::decode(data)
            && let Ok(validated) = net_message.validate()
        {
            res.push(validated);
        }

        cursor += total_len;
    }

    // Shift remaining bytes to start of buffer
    if cursor > 0 {
        if cursor < buf.len() {
            buf.copy_within(cursor.., 0);
            buf.truncate(buf.len() - cursor);
        } else {
            buf.clear();
        }
    }

    *datas = buf;
    Ok(res)
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
