use std::pin::Pin;

use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

use crate::SharedART;
use oxidart::async_command::OxidArtAsync;

use super::{glob_to_regex, is_simple_prefix};

pub(crate) type AsyncFrame = Pin<Box<dyn Future<Output = Frame>>>;

// ─── UNLINK ───────────────────────────────────────────────────────────────────

pub(crate) fn cmd_unlink(args: &[SharedByte], art: SharedART) -> AsyncFrame {
    Box::pin(handle_unlink(args.to_vec(), art))
}

async fn handle_unlink(args: Vec<SharedByte>, art: SharedART) -> Frame {
    let mut count = 0i64;
    for arg in args {
        let prefix = if arg.ends_with(b"*") {
            SharedByte::from_slice(&arg[..arg.len() - 1])
        } else {
            arg
        };
        count += art.deln_async(prefix).await as i64;
    }
    Frame::Integer(count)
}

// ─── KEYS ─────────────────────────────────────────────────────────────────────

pub(crate) fn cmd_keys(args: &[SharedByte], art: SharedART) -> AsyncFrame {
    Box::pin(handle_keys(args.to_vec(), art))
}

async fn handle_keys(args: Vec<SharedByte>, art: SharedART) -> Frame {
    if args.is_empty() {
        let keys = art.getn_async(SharedByte::from_slice(b"")).await;
        return Frame::Array(keys.into_iter().map(Frame::BulkString).collect());
    }

    let pattern = args[0].clone();

    // Fast path: simple prefix* or exact prefix → async getn
    if is_simple_prefix(&pattern) {
        let prefix = if pattern.ends_with(b"*") {
            SharedByte::from_slice(&pattern[..pattern.len() - 1])
        } else {
            pattern
        };
        let keys = art.getn_async(prefix).await;
        return Frame::Array(keys.into_iter().map(Frame::BulkString).collect());
    }

    // Slow path: complex glob → DFA regex scan (sync, single borrow)
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
