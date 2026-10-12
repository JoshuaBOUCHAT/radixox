use std::pin::Pin;

use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

use crate::SharedART;
use oxidart::async_command::OxidArtAsync;

use oxidart::regex::{GlobKind, glob_kind};

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
    match glob_kind(&pattern) {
        // `KEYS user:1` : la clé exacte seulement (pas user:10, user:100…)
        GlobKind::Exact(key) => {
            let mut expired = Vec::new();
            let found = art.borrow().exists_count(&[key], &mut expired) == 1;
            if !expired.is_empty() {
                art.borrow_mut().purge_expired(&[key], &expired);
            }
            let keys = if found { vec![Frame::BulkString(SharedByte::from_slice(key))] } else { Vec::new() };
            return Frame::Array(keys);
        }
        GlobKind::Prefix(prefix) => {
            let keys = art.getn_async(SharedByte::from_slice(prefix)).await;
            return Frame::Array(keys.into_iter().map(Frame::BulkString).collect());
        }
        GlobKind::Pattern => {}
    }

    let borrowed = art.borrow();
    match borrowed.getn_glob(&pattern) {
        Ok(pairs) => Frame::Array(
            pairs
                .into_iter()
                .map(|(k, _)| Frame::BulkString(k))
                .collect(),
        ),
        Err(_) => Frame::Error("ERR invalid pattern".into()),
    }
}
