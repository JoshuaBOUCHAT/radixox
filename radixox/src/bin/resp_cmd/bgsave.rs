//! BGSAVE : DFS par tranches (SAVE_DESIGN.md §4) et écriture du fichier en
//! double buffer (§5, étape 7).
//!
//! `fill` est rempli par `save_step` ; l'écriture en cours possède l'autre
//! buffer. Quand `fill` dépasse `CHUNK` et qu'aucune écriture n'est en vol,
//! la tâche de save échange les buffers et lance une tâche d'écriture. Celle-ci,
//! son écriture finie, réécrit ce qui s'est accumulé dans `fill` tant qu'il y en
//! a ; sinon elle s'arrête, ou finalise le fichier si le DFS est terminé
//! (modèle proactif : la dernière tâche active fait l'action finale).
//!
//! La barrière se ferme dès la fin du DFS, sans attendre le disque ; un nouveau
//! BGSAVE reste refusé jusqu'à la finalisation (`save_running`).

use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use monoio::fs::File;
use oxidart::async_command::yield_now;
use oxidart::save::format;

use crate::SharedART;

/// Budget d'une tranche de DFS (§4) : à régler sur le p99.99.
const SAVE_TRANCHE_BUDGET: u32 = 512;
/// Taille à partir de laquelle la tâche de save lance une écriture.
const CHUNK: usize = 2 << 20;

const FILE_NAME: &str = "dump.rxd";
const TMP_NAME: &str = "dump.rxd.tmp";

thread_local! {
    /// Vrai du lancement de BGSAVE jusqu'à la fin de la finalisation du fichier.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn save_running() -> bool {
    RUNNING.with(Cell::get)
}

/// Lance un BGSAVE ; `false` si une save (DFS ou écriture) est déjà en cours.
pub(crate) fn start_bgsave(art: &SharedART) -> bool {
    if save_running() || art.borrow().is_saving() {
        return false;
    }
    RUNNING.with(|r| r.set(true));
    let dir = PathBuf::from(std::env::var("RADIXOX_DIR").unwrap_or_else(|_| ".".into()));
    monoio::spawn(bgsave_task(art.clone(), dir));
    true
}

struct SaveFile {
    file: Rc<File>,
    dir: PathBuf,
    /// Prochain offset d'écriture.
    pos: u64,
    /// Rempli par `save_step`.
    fill: Vec<u8>,
    /// Buffer vide prêt à l'échange ; vide sans capacité pendant une
    /// écriture (c'est elle qui le possède).
    spare: Vec<u8>,
    /// crc de tout ce qui a été pris pour écriture, dans l'ordre du flux.
    crc: crc32fast::Hasher,
    in_flight: bool,
    walk_done: bool,
    err: Option<io::Error>,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    start: Option<Instant>,
    walk: Duration,
    tranches: u64,
    max_tranche: Duration,
    in_tranches: Duration,
    max_yield: Duration,
    writes: u64,
    max_write: Duration,
}

type Shared = Rc<RefCell<SaveFile>>;

impl SaveFile {
    /// Échange `fill` et `spare`, renvoie le buffer à écrire.
    fn take_chunk(&mut self) -> Vec<u8> {
        let buf = std::mem::replace(&mut self.fill, std::mem::take(&mut self.spare));
        self.crc.update(&buf);
        self.in_flight = true;
        buf
    }
}

async fn bgsave_task(art: SharedART, dir: PathBuf) {
    let start = Instant::now();
    let file = match File::create(dir.join(TMP_NAME)).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[bgsave] création de {TMP_NAME} impossible : {e}");
            RUNNING.with(|r| r.set(false));
            return;
        }
    };
    let sf: Shared = Rc::new(RefCell::new(SaveFile {
        file: Rc::new(file),
        dir,
        pos: 0,
        fill: Vec::with_capacity(CHUNK + CHUNK / 4),
        spare: Vec::with_capacity(CHUNK + CHUNK / 4),
        crc: crc32fast::Hasher::new(),
        in_flight: false,
        walk_done: false,
        err: None,
        stats: Stats { start: Some(start), ..Default::default() },
    }));

    let mut cur = art.borrow_mut().save_start(&mut sf.borrow_mut().fill);
    let aborted = loop {
        if sf.borrow().err.is_some() {
            break true;
        }
        // Backpressure : disque en retard, on ne produit plus (on cède seulement).
        let blocked = {
            let s = sf.borrow();
            s.in_flight && s.fill.len() >= CHUNK
        };
        if !blocked {
            let t = Instant::now();
            let done = art.borrow_mut().save_step(&mut cur, &mut sf.borrow_mut().fill, SAVE_TRANCHE_BUDGET);
            let el = t.elapsed();
            let mut s = sf.borrow_mut();
            s.stats.tranches += 1;
            s.stats.in_tranches += el;
            s.stats.max_tranche = s.stats.max_tranche.max(el);
            if done {
                break false;
            }
            if !s.in_flight && s.fill.len() >= CHUNK {
                let buf = s.take_chunk();
                drop(s);
                monoio::spawn(write_loop(sf.clone(), buf));
            }
        }
        let y = Instant::now();
        yield_now().await;
        let el = y.elapsed();
        let mut s = sf.borrow_mut();
        s.stats.max_yield = s.stats.max_yield.max(el);
    };
    // Le curseur ne contient que des idx et des valeurs possédées : on le
    // libère avant de fermer la barrière, même en plein DFS.
    drop(cur);
    art.borrow_mut().finish_save();

    let mut s = sf.borrow_mut();
    s.stats.walk = start.elapsed();
    s.walk_done = true;
    if aborted {
        s.fill.clear();
    } else {
        // Pied : crc de tout le flux, y compris la fin encore dans `fill`.
        let mut h = s.crc.clone();
        h.update(&s.fill);
        let foot = format::footer(h.finalize());
        s.fill.extend_from_slice(&foot);
    }
    if s.in_flight {
        // L'écriture en vol prendra la suite (reste de `fill`, finalisation).
        return;
    }
    if aborted {
        drop(s);
        return end(sf).await;
    }
    let buf = s.take_chunk();
    drop(s);
    write_loop(sf, buf).await;
}

/// Écrit `buf`, puis tout ce qui s'est accumulé dans `fill` entre-temps.
/// Sort quand `fill` est vide ; finalise si le DFS est terminé.
async fn write_loop(sf: Shared, mut buf: Vec<u8>) {
    loop {
        let (file, pos) = {
            let s = sf.borrow();
            (s.file.clone(), s.pos)
        };
        let len = buf.len() as u64;
        let t = Instant::now();
        let (res, b) = file.write_all_at(buf, pos).await;
        let el = t.elapsed();
        buf = b;
        buf.clear();
        let mut s = sf.borrow_mut();
        s.stats.writes += 1;
        s.stats.max_write = s.stats.max_write.max(el);
        match res {
            Ok(()) => s.pos += len,
            Err(e) => {
                s.err = Some(e);
                s.fill.clear();
            }
        }
        s.spare = buf;
        s.in_flight = false;
        if s.err.is_none() && !s.fill.is_empty() {
            buf = s.take_chunk();
            continue;
        }
        let walk_done = s.walk_done;
        drop(s);
        if walk_done {
            end(sf).await;
        }
        return;
    }
}

/// DFS terminé et plus rien en vol : finalise le fichier, ou nettoie après
/// une erreur.
async fn end(sf: Shared) {
    let (file, dir, err) = {
        let mut s = sf.borrow_mut();
        (s.file.clone(), s.dir.clone(), s.err.take())
    };
    let tmp = dir.join(TMP_NAME);
    let t = Instant::now();
    let res = match err {
        Some(e) => Err(e),
        None => finalize(&file, &dir).await,
    };
    drop(file);
    let s = sf.borrow();
    match res {
        Ok(()) => {
            let st = &s.stats;
            eprintln!(
                "[bgsave] {:.1} MB écrits en {:.2?} (DFS {:.2?}, fsync+rename {:.2?}) ; \
                 {} tranches, tranche max {:.1?}, dans les tranches {:.2?}, yield max {:.2?} ; \
                 {} écritures, écriture max {:.2?}",
                s.pos as f64 / 1e6,
                st.start.map(|t| t.elapsed()).unwrap_or_default(),
                st.walk,
                t.elapsed(),
                st.tranches,
                st.max_tranche,
                st.in_tranches,
                st.max_yield,
                st.writes,
                st.max_write,
            );
        }
        Err(e) => {
            eprintln!("[bgsave] échec : {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }
    RUNNING.with(|r| r.set(false));
}

/// fsync du fichier, rename atomique sur `dump.rxd`, fsync du dossier.
async fn finalize(file: &File, dir: &std::path::Path) -> io::Result<()> {
    file.sync_data().await?;
    std::fs::rename(dir.join(TMP_NAME), dir.join(FILE_NAME))?;
    File::open(dir).await?.sync_all().await
}
