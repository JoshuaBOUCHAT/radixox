use std::hash::{Hash, Hasher};

use crate::shared_byte::SharedByte;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SharedFrame {
    SimpleString(SharedByte),
    Error(String),
    Integer(i64),
    BulkString(SharedByte),
    Array(Vec<SharedFrame>),
    Null,
}

impl Hash for SharedFrame {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Use RESP prefix bytes as type discriminants (same role as FrameKind::hash_prefix)
        let prefix: u8 = match self {
            SharedFrame::SimpleString(_) => b'+',
            SharedFrame::Error(_) => b'-',
            SharedFrame::Integer(_) => b':',
            SharedFrame::BulkString(_) => b'$',
            SharedFrame::Array(_) => b'*',
            SharedFrame::Null => b'_',
        };
        prefix.hash(state);
        match self {
            SharedFrame::SimpleString(b) | SharedFrame::BulkString(b) => b.hash(state),
            SharedFrame::Error(s) => s.hash(state),
            SharedFrame::Integer(i) => i.hash(state),
            SharedFrame::Array(f) => f.iter().for_each(|f| f.hash(state)),
            SharedFrame::Null => {}
        }
    }
}

impl SharedFrame {
    fn encode_len(&self) -> usize {
        match self {
            SharedFrame::BulkString(b) => 1 + digits_in(b.len()) + 2 + b.len() + 2,
            SharedFrame::Array(frames) => frames
                .iter()
                .fold(1 + digits_in(frames.len()) + 2, |m, f| m + f.encode_len()),
            SharedFrame::Null => 5, // b"$-1\r\n"
            SharedFrame::SimpleString(s) => 1 + s.len() + 2,
            SharedFrame::Error(s) => 1 + s.len() + 2,
            SharedFrame::Integer(i) => {
                let prefix = usize::from(*i < 0);
                let digits = digits_in(i.unsigned_abs() as usize);
                1 + digits + 2 + prefix
            }
        }
    }
}

#[inline]
fn digits_in(mut n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut d = 0;
    while n > 0 {
        d += 1;
        n /= 10;
    }
    d
}

// ---------------------------------------------------------------------------
// RESP2 encoder — zero-alloc, no external deps
// ---------------------------------------------------------------------------

#[inline]
fn write_usize(dst: &mut Vec<u8>, mut n: usize) {
    if n == 0 {
        dst.push(b'0');
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
        dst.push(b'-');
        write_usize(dst, n.unsigned_abs() as usize);
    } else {
        write_usize(dst, n as usize);
    }
}

fn encode_frame(dst: &mut Vec<u8>, frame: &SharedFrame) {
    match frame {
        SharedFrame::SimpleString(s) => {
            dst.push(b'+');
            dst.extend_from_slice(s.as_slice());
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::Error(s) => {
            dst.push(b'-');
            dst.extend_from_slice(s.as_bytes());
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::Integer(i) => {
            dst.push(b':');
            write_i64(dst, *i);
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::BulkString(bytes) => {
            dst.push(b'$');
            write_usize(dst, bytes.len());
            dst.extend_from_slice(b"\r\n");
            dst.extend_from_slice(bytes.as_slice());
            dst.extend_from_slice(b"\r\n");
        }
        SharedFrame::Array(frames) => {
            dst.push(b'*');
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

pub fn extend_encode(dst: &mut Vec<u8>, frame: &SharedFrame) {
    dst.reserve(frame.encode_len());
    encode_frame(dst, frame);
}
