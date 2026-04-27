use std::hash::{Hash, Hasher};

use redis_protocol::{
    digits_in_usize,
    resp2::types::{FrameKind, Resp2Frame},
    types::{PATTERN_PUBSUB_PREFIX, PUBSUB_PREFIX, SHARD_PUBSUB_PREFIX},
};

use crate::shared_byte::SharedByte;
use redis_protocol::resp2::types::NULL;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SharedFrame {
    /// A RESP2 simple string.
    SimpleString(SharedByte),
    /// A short string representing an error.
    Error(String),
    /// A signed 64-bit integer.
    Integer(i64),
    /// A byte array.
    BulkString(SharedByte),
    /// An array of frames.
    Array(Vec<SharedFrame>),
    /// A null value.
    Null,
}

impl Hash for SharedFrame {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind().hash_prefix().hash(state);

        match self {
            SharedFrame::SimpleString(b) | SharedFrame::BulkString(b) => b.hash(state),
            SharedFrame::Error(s) => s.hash(state),
            SharedFrame::Integer(i) => i.hash(state),
            SharedFrame::Array(f) => f.iter().for_each(|f| f.hash(state)),
            SharedFrame::Null => NULL.hash(state),
        }
    }
}

impl Resp2Frame for SharedFrame {
    fn encode_len(&self, int_as_bulkstring: bool) -> usize {
        match self {
            SharedFrame::BulkString(b) => bulkstring_encode_len(b),
            SharedFrame::Array(frames) => frames
                .iter()
                .fold(1 + digits_in_usize(frames.len()) + 2, |m, f| {
                    m + f.encode_len(int_as_bulkstring)
                }),
            SharedFrame::Null => NULL.len(),
            SharedFrame::SimpleString(s) => simplestring_encode_len(s),
            SharedFrame::Error(s) => error_encode_len(s),
            SharedFrame::Integer(i) => integer_encode_len(*i, int_as_bulkstring),
        }
    }

    fn take(&mut self) -> SharedFrame {
        std::mem::replace(self, SharedFrame::Null)
    }

    fn kind(&self) -> FrameKind {
        match self {
            SharedFrame::SimpleString(_) => FrameKind::SimpleString,
            SharedFrame::Error(_) => FrameKind::Error,
            SharedFrame::Integer(_) => FrameKind::Integer,
            SharedFrame::BulkString(_) => FrameKind::BulkString,
            SharedFrame::Array(_) => FrameKind::Array,
            SharedFrame::Null => FrameKind::Null,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            SharedFrame::BulkString(b) => str::from_utf8(b).ok(),
            SharedFrame::SimpleString(s) => str::from_utf8(s).ok(),
            SharedFrame::Error(s) => Some(s),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            SharedFrame::BulkString(b) | SharedFrame::SimpleString(b) => bytes_to_bool(b),
            SharedFrame::Integer(0) => Some(false),
            SharedFrame::Integer(1) => Some(true),
            SharedFrame::Null => Some(false),
            _ => None,
        }
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        Some(match self {
            SharedFrame::BulkString(b) => b,
            SharedFrame::SimpleString(s) => s,
            SharedFrame::Error(s) => s.as_bytes(),
            _ => return None,
        })
    }

    fn to_string(&self) -> Option<String> {
        match self {
            SharedFrame::BulkString(b) | SharedFrame::SimpleString(b) => {
                String::from_utf8(b.to_vec()).ok()
            }
            SharedFrame::Error(b) => Some(b.to_string()),
            SharedFrame::Integer(i) => Some(i.to_string()),
            _ => None,
        }
    }
    fn is_normal_pubsub_message(&self) -> bool {
        // format is ["message", <channel>, <message>]
        match self {
            SharedFrame::Array(data) => {
                data.len() == 3
                    && data[0].kind() == FrameKind::BulkString
                    && data[0]
                        .as_str()
                        .map(|s| s == PUBSUB_PREFIX)
                        .unwrap_or(false)
            }
            _ => false,
        }
    }

    fn is_pattern_pubsub_message(&self) -> bool {
        // format is ["pmessage", <pattern>, <channel>, <message>]
        match self {
            SharedFrame::Array(data) => {
                data.len() == 4
                    && data[0].kind() == FrameKind::BulkString
                    && data[0]
                        .as_str()
                        .map(|s| s == PATTERN_PUBSUB_PREFIX)
                        .unwrap_or(false)
            }
            _ => false,
        }
    }

    fn is_shard_pubsub_message(&self) -> bool {
        // format is ["smessage", <channel>, <message>]
        match self {
            SharedFrame::Array(data) => {
                data.len() == 3
                    && data[0].kind() == FrameKind::BulkString
                    && data[0]
                        .as_str()
                        .map(|s| s == SHARD_PUBSUB_PREFIX)
                        .unwrap_or(false)
            }
            _ => false,
        }
    }
}
pub fn bulkstring_encode_len(b: &[u8]) -> usize {
    1 + digits_in_usize(b.len()) + 2 + b.len() + 2
}

pub fn simplestring_encode_len(s: &[u8]) -> usize {
    1 + s.len() + 2
}

pub fn error_encode_len(s: &str) -> usize {
    1 + s.len() + 2
}

pub fn integer_encode_len(i: i64, int_as_bulkstring: bool) -> usize {
    let prefix = if i < 0 { 1 } else { 0 };
    let digits = digits_in_usize(i.unsigned_abs() as usize);

    if int_as_bulkstring {
        1 + digits_in_usize(digits + prefix) + 2 + prefix + digits + 2
    } else {
        1 + digits + 2 + prefix
    }
}
pub(crate) fn bytes_to_bool(b: &[u8]) -> Option<bool> {
    match b {
        b"true" | b"TRUE" | b"t" | b"T" | b"1" => Some(true),
        b"false" | b"FALSE" | b"f" | b"F" | b"0" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// RESP2 encoder for SharedFrame — zero-alloc, no external deps
// ---------------------------------------------------------------------------

/// Stack buffer for integer-to-decimal conversion, no heap allocation.
#[inline]
fn write_usize(dst: &mut Vec<u8>, mut n: usize) {
    if n == 0 {
        dst.extend_from_slice(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20usize;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    dst.extend_from_slice(&buf[i..]);
}

#[inline]
fn write_i64(dst: &mut Vec<u8>, n: i64) {
    if n < 0 {
        dst.extend_from_slice(b"-");
        write_usize(dst, n.unsigned_abs() as usize);
    } else {
        write_usize(dst, n as usize);
    }
}

fn encode_frame(dst: &mut Vec<u8>, frame: &SharedFrame) {
    match frame {
        SharedFrame::SimpleString(s) => {
            dst.extend_from_slice(b"+");
            dst.extend_from_slice(s.as_slice());
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::Error(s) => {
            dst.extend_from_slice(b"-");
            dst.extend_from_slice(s.as_bytes());
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::Integer(i) => {
            dst.extend_from_slice(b":");
            write_i64(dst, *i);
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::BulkString(b) => {
            dst.extend_from_slice(b"$");
            write_usize(dst, b.len());
            dst.extend_from_slice(b"\r\n");
            dst.extend_from_slice(b.as_slice());
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::Array(frames) => {
            dst.extend_from_slice(b"*");
            write_usize(dst, frames.len());
            dst.extend_from_slice(b"\r\n");
            for f in frames {
                encode_frame(dst, f);
            }
        }
        SharedFrame::Null => {
            dst.extend_from_slice(b"$-1\r\n");
        }
    }
}

/// Encode `frame` into `dst`, extending it as needed.
/// Equivalent to `redis_protocol::resp2::encode::extend_encode` but for `SharedFrame`.
pub fn extend_encode(dst: &mut Vec<u8>, frame: &SharedFrame) {
    let needed = frame.encode_len(false);
    dst.reserve(needed);
    encode_frame(dst, frame);
}
