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

// ─── BGSAVE ───────────────────────────────────────────────────────────────────

/// Budget d'une tranche de save (SAVE_DESIGN.md §4) : à régler sur le p99.99.
const SAVE_TRANCHE_BUDGET: u32 = 1024;

struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.0 {
            return std::task::Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}

/// Save en arrière-plan, par tranches avec `yield` entre deux tranches.
/// v1 (mesure) : le dump est sérialisé en mémoire puis jeté, pas encore
/// écrit sur disque (étape 7).
pub(crate) async fn bgsave_task(art: SharedART) {
    let start = std::time::Instant::now();
    let mut out = Vec::with_capacity(4 << 20);
    let (mut total, mut tranches) = (0usize, 0u64);
    let mut max_tranche = std::time::Duration::ZERO;
    let mut cur = art.borrow_mut().save_start(&mut out);
    loop {
        let t = std::time::Instant::now();
        let done = art.borrow_mut().save_step(&mut cur, &mut out, SAVE_TRANCHE_BUDGET);
        max_tranche = max_tranche.max(t.elapsed());
        tranches += 1;
        if done || out.len() >= 4 << 20 {
            total += out.len();
            out.clear();
        }
        if done {
            break;
        }
        YieldNow(false).await;
    }
    art.borrow_mut().finish_save();
    eprintln!(
        "[bgsave] {:.1} MB en {:.2?}, {tranches} tranches, tranche max {:.1?}",
        total as f64 / 1e6,
        start.elapsed(),
        max_tranche
    );
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
