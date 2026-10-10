//! Arrêt sur SIGINT / SIGTERM avec save (`RADIXOX_SAVE_ON_EXIT=1`) : `SAVE`
//! bloquante, puis sortie. Un second signal quitte tout de suite, sans save.
//!
//! Le gestionnaire de signal ne fait que lever un drapeau (rien d'autre n'est
//! sûr dans un gestionnaire) ; une tâche de la boucle le surveille.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::bgsave::{save_running, save_sync, sync_level};
use crate::SharedART;

static STOP: AtomicBool = AtomicBool::new(false);
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
/// Délai de réaction au signal.
const POLL: Duration = Duration::from_millis(20);

unsafe extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
    fn _exit(code: i32) -> !;
}

extern "C" fn on_signal(_sig: i32) {
    if STOP.swap(true, Ordering::SeqCst) {
        // Second signal : sortie immédiate (`_exit` est sûr ici).
        unsafe { _exit(130) }
    }
}

/// Installe les gestionnaires si `RADIXOX_SAVE_ON_EXIT` vaut 1 / true / yes.
pub(crate) fn install(art: SharedART) {
    let on = std::env::var("RADIXOX_SAVE_ON_EXIT")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    if !on {
        return;
    }
    // SAFETY: `on_signal` ne touche qu'un atomique et `_exit`.
    unsafe {
        signal(SIGINT, on_signal as usize);
        signal(SIGTERM, on_signal as usize);
    }
    monoio::spawn(watch(art));
}

async fn watch(art: SharedART) {
    while !STOP.load(Ordering::SeqCst) {
        monoio::time::sleep(POLL).await;
    }
    eprintln!("[shutdown] signal reçu : save puis sortie (second signal : sortie immédiate)");
    // BGSAVE en cours : la barrière doit être fermée avant une SAVE.
    while save_running() {
        monoio::time::sleep(Duration::from_millis(5)).await;
    }
    match save_sync(&art, sync_level()) {
        Ok(st) => {
            eprintln!(
                "[shutdown] {:.1} MB ({:.1} MB bruts) en {:.2?} (DFS {:.2?})",
                st.written as f64 / 1e6,
                st.raw as f64 / 1e6,
                st.total,
                st.walk
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[shutdown] échec de la save : {e}");
            std::process::exit(1);
        }
    }
}
