use std::pin::Pin;

use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

use crate::SharedART;
use oxidart::async_command::OxidArtAsync;

use super::{glob_to_regex, is_simple_prefix};

pub(crate) type AsyncFrame = Pin<Box<dyn Future<Output = Frame>>>;

// ─── UNLINK ───────────────────────────────────────────────────────────────────

pub(crate) fn cmd_unlink(keys: Vec<SharedByte>, art: SharedART) -> AsyncFrame {
    Box::pin(handle_unlink(keys, art))
}

async fn handle_unlink(keys: Vec<SharedByte>, art: SharedART) -> Frame {
    let mut count = 0i64;
    for key in keys {
        let prefix = if key.ends_with(b"*") {
            SharedByte::from_slice(&key[..key.len() - 1])
        } else {
            key
        };
        count += art.deln_async(prefix).await as i64;
    }
    Frame::Integer(count)
}

// ─── KEYS ─────────────────────────────────────────────────────────────────────

pub(crate) fn cmd_keys(pattern: SharedByte, art: SharedART) -> AsyncFrame {
    Box::pin(handle_keys(pattern, art))
}

async fn handle_keys(pattern: SharedByte, art: SharedART) -> Frame {
    if pattern.is_empty() || pattern.as_ref() == b"*" {
        let keys = art.getn_async(SharedByte::from_slice(b"")).await;
        return Frame::Array(keys.into_iter().map(Frame::BulkString).collect());
    }

    if is_simple_prefix(&pattern) {
        let prefix = if pattern.ends_with(b"*") {
            SharedByte::from_slice(&pattern[..pattern.len() - 1])
        } else {
            pattern
        };
        let keys = art.getn_async(prefix).await;
        return Frame::Array(keys.into_iter().map(Frame::BulkString).collect());
    }

    let regex = glob_to_regex(&pattern);
    let borrowed = art.borrow();
    match borrowed.getn_regex(&regex) {
        Ok(pairs) => Frame::Array(
            pairs
                .into_iter()
                .map(|(k, _)| Frame::BulkString(k))
                .collect(),
        ),
        Err(_) => Frame::Error("ERR invalid pattern".into()),
    }
}
