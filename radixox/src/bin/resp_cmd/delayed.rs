//! Pilotage monoio des commandes longues (`radixox::exec::jobs`) : une tranche,
//! puis on rend la main aux autres connexions, jusqu'à la fin du job.

use oxidart::async_command::yield_now;
use radixox::exec::jobs::{FREE_BUDGET, FlushJob, KEYS_BUDGET, KeysJob, Step, UnlinkJob};
use radixox_lib::shared_byte::SharedByte;

use crate::SharedART;

/// KEYS : réponse RESP complète, encodée par le job.
pub(crate) async fn cmd_keys(pattern: SharedByte, art: &SharedART) -> Vec<u8> {
    let mut job = KeysJob::new(pattern);
    while job.step(&mut art.borrow_mut(), KEYS_BUDGET) == Step::More {
        yield_now().await;
    }
    job.response().to_vec()
}

/// UNLINK : nombre de clés supprimées, une fois tout libéré.
pub(crate) async fn cmd_unlink(keys: &[SharedByte], art: &SharedART) -> i64 {
    let mut job = UnlinkJob::start(&mut art.borrow_mut(), keys);
    while job.step(&mut art.borrow_mut(), FREE_BUDGET) == Step::More {
        yield_now().await;
    }
    job.count() as i64
}

/// FLUSHDB : base détachée tout de suite (la réponse part sans attendre),
/// mémoire rendue par une tâche de fond.
pub(crate) fn cmd_flushdb(art: &SharedART) {
    let mut job = FlushJob::start(&mut art.borrow_mut());
    if job.is_done() {
        return;
    }
    let art = art.clone();
    monoio::spawn(async move {
        while job.step(&mut art.borrow_mut(), FREE_BUDGET) == Step::More {
            yield_now().await;
        }
    });
}
