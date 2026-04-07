use std::pin::Pin;

use bytes::Bytes;
use redis_protocol::resp2::types::BytesFrame as Frame;

use crate::SharedART;
use oxidart::async_command::OxidArtAsync;

use super::{glob_to_regex, is_simple_prefix};

pub(crate) type AsyncFrame = Pin<Box<dyn Future<Output = Frame>>>;

// ─── UNLINK ───────────────────────────────────────────────────────────────────

pub(crate) fn cmd_unlink(args: &[Bytes], art: SharedART) -> AsyncFrame {
    Box::pin(handle_unlink(args.to_vec(), art))
}

async fn handle_unlink(args: Vec<Bytes>, art: SharedART) -> Frame {
    let mut count = 0i64;
    for arg in args {
        // Support simple "prefix*" glob — strip trailing * for prefix match,
        // same convention as KEYS uses. "user:*" → prefix "user:".
        let prefix = if arg.ends_with(b"*") {
            arg.slice(..arg.len() - 1)
        } else {
            arg
        };
        count += art.deln_async(prefix).await as i64;
    }
    Frame::Integer(count)
}

// ─── KEYS ─────────────────────────────────────────────────────────────────────

pub(crate) fn cmd_keys(args: &[Bytes], art: SharedART) -> AsyncFrame {
    Box::pin(handle_keys(args.to_vec(), art))
}

async fn handle_keys(args: Vec<Bytes>, art: SharedART) -> Frame {
    if args.is_empty() {
        let keys = art.getn_async(Bytes::new()).await;
        return Frame::Array(keys.into_iter().map(Frame::BulkString).collect());
    }

    let pattern = args[0].clone();

    // Fast path: simple prefix* or exact prefix → async getn
    if is_simple_prefix(&pattern) {
        let prefix = if pattern.ends_with(b"*") {
            pattern.slice(..pattern.len() - 1)
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
            pairs.into_iter().map(|(k, _)| Frame::BulkString(k)).collect(),
        ),
        Err(_) => Frame::Error("ERR invalid pattern".into()),
    }
}
