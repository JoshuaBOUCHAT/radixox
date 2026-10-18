//! BGSAVE : DFS par tranches (SAVE_DESIGN.md §4) et écriture du fichier en
//! double buffer (§5, étape 7).
//!
//! `fill` est rempli par `save_step` ; l'écriture en cours possède l'autre
//! buffer. Quand `fill` dépasse `CHUNK` et qu'aucune écriture n'est en vol,
//! la tâche de save échange les buffers et lance une tâche d'écriture. Celle-ci,
//! son écriture finie, enchaîne tant qu'un morceau plein attend dans `fill`
//! (ou le reste, une fois la production finie) ; sinon elle s'arrête, ou
//! finalise le fichier si la production est terminée
//! (modèle proactif : la dernière tâche active fait l'action finale).
//!
//! La barrière se ferme dès la fin du DFS, sans attendre le disque ; la liste
//! différée valart est ensuite vidée par tranches. Un nouveau BGSAVE reste
//! refusé jusqu'à la fin du vidage et de la finalisation (`save_running`).

use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use monoio::fs::File;
use oxidart::async_command::yield_now;
use oxidart::compress::ZstdStream;
use oxidart::save::format;

use crate::SharedART;

/// Budget d'une tranche de DFS (§4) : à régler sur le p99.99.
const SAVE_TRANCHE_BUDGET: u32 = 512;
/// Durée visée d'une tranche de compression (au moins un bloc zstd, dont la
/// taille dépend du niveau : `oxidart::compress::block_size`).
const COMPRESS_SLICE: Duration = Duration::from_micros(150);
/// Taille à partir de laquelle la tâche de save lance une écriture.
const CHUNK: usize = 2 << 20;

/// Part du thread serveur que le BGSAVE s'accorde (`RADIXOX_SAVE_CPU_SHARE`,
/// entre 0 et 1). Sans plafond, la save prenait ~60 % du cœur pendant qu'elle
/// tournait : les clients faisaient la queue et le p99.9 doublait. Au-delà
/// de sa part (temps de tranches + compression rapporté au temps écoulé), la
/// tâche cède sans travailler — sauf si aucune lecture réseau n'a abouti
/// depuis `IDLE_AFTER` : pas de client actif, le thread est libre.
///
/// (Un `yield` qui revient tout de suite ne suffit pas comme signal : des
/// requêtes déjà arrivées dans le noyau attendent que la boucle les relève, et
/// la save qui comblait ces trous remettait le p99.9 au double.)
fn save_cpu_share() -> f64 {
    static SHARE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SHARE.get_or_init(|| match std::env::var("RADIXOX_SAVE_CPU_SHARE") {
        Err(_) => DEFAULT_SAVE_CPU_SHARE,
        Ok(v) => match v.parse::<f64>() {
            Ok(x) if x > 0.0 && x <= 1.0 => x,
            _ => {
                eprintln!("[bgsave] RADIXOX_SAVE_CPU_SHARE={v} invalide, {DEFAULT_SAVE_CPU_SHARE}");
                DEFAULT_SAVE_CPU_SHARE
            }
        },
    })
}
const DEFAULT_SAVE_CPU_SHARE: f64 = 0.1;
/// Sans lecture réseau depuis ce délai, la save n'est plus plafonnée.
const IDLE_AFTER: Duration = Duration::from_millis(1);

thread_local! {
    /// Lectures réseau abouties (toutes connexions) : signal d'activité.
    static READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// À appeler à chaque lecture réseau non vide.
#[inline]
pub(crate) fn note_read() {
    READS.with(|r| r.set(r.get().wrapping_add(1)));
}

const FILE_NAME: &str = "dump.rxd";
const TMP_NAME: &str = "dump.rxd.tmp";

thread_local! {
    /// Vrai du lancement de BGSAVE jusqu'à la fin de la finalisation du fichier.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn save_running() -> bool {
    RUNNING.with(Cell::get)
}

fn save_dir() -> PathBuf {
    PathBuf::from(std::env::var("RADIXOX_DIR").unwrap_or_else(|_| ".".into()))
}

/// Recharge `RADIXOX_DIR/dump.rxd` dans l'arbre (vide) avant d'accepter des
/// connexions (SAVE_DESIGN.md §5bis). Fichier absent : démarrage à vide.
/// Fichier illisible, tronqué ou corrompu : erreur, le serveur ne démarre
/// pas (on n'écrase pas un dump qu'on n'a pas su lire).
pub(crate) fn load_at_startup(art: &SharedART) -> io::Result<()> {
    let path = save_dir().join(FILE_NAME);
    let start = Instant::now();
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut art = art.borrow_mut();
    art.tick();
    // Lecture par fragments (fenêtre de 1 MB), crc vérifié en fin de flux.
    let stats = art.load_file(file).map_err(|e| {
        let kind = match &e {
            oxidart::LoadError::Io(e) => e.kind(),
            oxidart::LoadError::Dump(_) => io::ErrorKind::InvalidData,
        };
        io::Error::new(kind, format!("{}: {e}", path.display()))
    })?;
    println!(
        "[load] {} : {} clés ({} expirées sautées), {:.1} MB en {:?}",
        path.display(),
        stats.keys,
        stats.expired,
        size as f64 / 1e6,
        start.elapsed()
    );
    Ok(())
}

/// Taille des écritures de `SAVE` : le DFS remplit un buffer, `write`
/// le copie dans le cache du noyau, qui écrit sur disque en parallèle.
const SYNC_CHUNK: usize = 8 << 20;
/// Budget d'un pas de DFS pour `SAVE` (seulement pour borner le buffer).
const SYNC_STEP_BUDGET: u32 = 1 << 16;

/// Bilan d'une `SAVE`.
pub(crate) struct SyncStats {
    pub(crate) raw: u64,
    pub(crate) written: u64,
    pub(crate) walk: Duration,
    pub(crate) total: Duration,
}

/// `SAVE` : save complète **bloquante** (ni tranche, ni `yield`) : le DFS
/// tourne d'une traite, le fichier est écrit par des `write` bloquants (copie
/// dans le cache du noyau, qui écrit en parallèle), puis fsync + rename.
/// `level` : niveau zstd (`None` : brut, le plus rapide en CPU). Erreur si un
/// BGSAVE est en cours.
pub(crate) fn save_sync(art: &SharedART, level: Option<i32>) -> io::Result<SyncStats> {
    use std::io::Write;
    if save_running() || art.borrow().is_saving() {
        return Err(io::Error::other("Background save already in progress"));
    }
    let start = Instant::now();
    let dir = save_dir();
    let tmp = dir.join(TMP_NAME);
    let res = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        let mut art = art.borrow_mut();
        let mut z = level.map(ZstdStream::new);
        let mut buf = Vec::with_capacity(SYNC_CHUNK + SYNC_CHUNK / 4);
        let mut packed = Vec::new();
        let mut crc = crc32fast::Hasher::new();
        let (mut raw, mut written) = (0u64, 0u64);
        let mut flush = |buf: &mut Vec<u8>, last: bool, file: &mut std::fs::File| -> io::Result<()> {
            crc.update(buf);
            raw += buf.len() as u64;
            if last {
                let foot = format::footer(crc.clone().finalize());
                buf.extend_from_slice(&foot);
                raw += foot.len() as u64;
            }
            match &mut z {
                None => {
                    file.write_all(buf)?;
                    written += buf.len() as u64;
                }
                Some(z) => {
                    z.compress(buf, &mut packed, Duration::MAX);
                    if last {
                        while !z.finish(&mut packed, Duration::MAX) {}
                    }
                    file.write_all(&packed)?;
                    written += packed.len() as u64;
                    packed.clear();
                }
            }
            buf.clear();
            Ok(())
        };
        let mut cur = art.save_start(&mut buf);
        let walk_start = Instant::now();
        let mut walk = Duration::ZERO;
        loop {
            let done = art.save_step(&mut cur, &mut buf, SYNC_STEP_BUDGET);
            if done {
                walk = walk_start.elapsed();
                // Rien n'a muté pendant la save : la liste différée est vide.
                drop(cur);
                art.finish_save();
                flush(&mut buf, true, &mut file)?;
                break;
            }
            if buf.len() >= SYNC_CHUNK {
                flush(&mut buf, false, &mut file)?;
            }
        }
        drop(flush);
        drop(art);
        file.sync_data()?;
        drop(file);
        std::fs::rename(&tmp, dir.join(FILE_NAME))?;
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(SyncStats { raw, written, walk, total: start.elapsed() })
    })();
    if res.is_err() {
        if art.borrow().is_saving() {
            art.borrow_mut().finish_save();
        }
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Niveau zstd de `SAVE` et de la save de sortie : `RADIXOX_SAVE_SYNC_ZSTD`
/// (défaut, absent ou 0 : brut, le plus rapide).
pub(crate) fn sync_level() -> Option<i32> {
    match std::env::var("RADIXOX_SAVE_SYNC_ZSTD").ok().and_then(|v| v.parse::<i32>().ok()) {
        Some(0) | None => None,
        Some(l) => Some(l.clamp(zstd_safe::min_c_level(), zstd_safe::max_c_level())),
    }
}

/// Lance un BGSAVE ; `false` si une save (DFS ou écriture) est déjà en cours.
pub(crate) fn start_bgsave(art: &SharedART) -> bool {
    if save_running() || art.borrow().is_saving() {
        return false;
    }
    RUNNING.with(|r| r.set(true));
    monoio::spawn(bgsave_task(art.clone(), save_dir(), zstd_conf()));
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
    /// crc de tout ce qui a été pris pour écriture, dans l'ordre du flux
    /// (sert au pied du dump brut ; ignoré en mode compressé).
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

/// Compression du dump (paramètres `RADIXOX_SAVE_ZSTD`, `RADIXOX_SAVE_BACKLOG_MB`).
#[derive(Clone, Copy)]
pub(crate) struct ZstdConf {
    level: i32,
    /// Brut en attente de compression, au plus (octets). Plein : le DFS
    /// attend le compresseur (la barrière reste ouverte plus longtemps).
    backlog: usize,
}

/// Niveau par défaut : dump compact, tranches courtes (max ~0,2 ms mesuré).
const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// `RADIXOX_SAVE_ZSTD=<niveau>` (défaut 3 ; 0 : dump brut) ;
/// `RADIXOX_SAVE_BACKLOG_MB` (défaut 64).
fn zstd_conf() -> Option<ZstdConf> {
    let level: i32 = match std::env::var("RADIXOX_SAVE_ZSTD") {
        Ok(v) => match v.parse() {
            Ok(level) => level,
            Err(_) => {
                eprintln!("[bgsave] RADIXOX_SAVE_ZSTD={v} invalide, niveau {DEFAULT_ZSTD_LEVEL}");
                DEFAULT_ZSTD_LEVEL
            }
        },
        Err(_) => DEFAULT_ZSTD_LEVEL,
    };
    if level == 0 {
        return None;
    }
    let level = level.clamp(zstd_safe::min_c_level(), zstd_safe::max_c_level());
    let mb: usize = std::env::var("RADIXOX_SAVE_BACKLOG_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    Some(ZstdConf { level, backlog: mb.max(1) << 20 })
}

/// État de la compression pendant une save.
struct Compress {
    z: ZstdStream,
    /// Flux brut produit par le DFS ; `raw[pos..]` reste à compresser.
    raw: Vec<u8>,
    pos: usize,
    backlog: usize,
    /// crc du fichier brut (le pied est dans la frame).
    crc: crc32fast::Hasher,
    finished: bool,
    raw_total: u64,
    in_compress: Duration,
    max_slice: Duration,
}

impl Compress {
    fn pending(&self) -> usize {
        self.raw.len() - self.pos
    }
}

/// Ferme la barrière et vide la liste différée par tranches (§2.B).
async fn close_save(art: &SharedART) {
    art.borrow_mut().end_save();
    let t = Instant::now();
    let mut drain_tranches = 0u32;
    while !art.borrow_mut().drain_deferred(SAVE_TRANCHE_BUDGET) {
        drain_tranches += 1;
        yield_now().await;
    }
    if drain_tranches > 0 {
        eprintln!("[bgsave] liste différée vidée en {:.2?}, {drain_tranches} tranches", t.elapsed());
    }
}

async fn bgsave_task(art: SharedART, dir: PathBuf, zstd: Option<ZstdConf>) {
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
    let mut comp = zstd.map(|c| Compress {
        z: ZstdStream::new(c.level),
        raw: Vec::with_capacity(CHUNK + CHUNK / 4),
        pos: 0,
        backlog: c.backlog,
        crc: crc32fast::Hasher::new(),
        finished: false,
        raw_total: 0,
        in_compress: Duration::ZERO,
        max_slice: Duration::ZERO,
    });

    // Le DFS écrit dans `comp.raw` (compression) ou directement dans `fill`.
    let mut cur = Some(match &mut comp {
        Some(c) => {
            let cur = art.borrow_mut().save_start(&mut c.raw);
            c.crc.update(&c.raw);
            cur
        }
        None => art.borrow_mut().save_start(&mut sf.borrow_mut().fill),
    });
    let share = save_cpu_share();
    let (mut reads_seen, mut last_read) = (READS.with(|r| r.get()), Instant::now());
    let aborted = loop {
        if sf.borrow().err.is_some() {
            break true;
        }
        // Backpressure : disque en retard, on ne produit plus (on cède seulement).
        let blocked = {
            let s = sf.borrow();
            s.in_flight && s.fill.len() >= CHUNK
        };
        // Plafond de CPU : en avance sur sa part, la save cède sans travailler.
        let reads = READS.with(|r| r.get());
        if reads != reads_seen {
            (reads_seen, last_read) = (reads, Instant::now());
        }
        let paced = share < 1.0 && last_read.elapsed() < IDLE_AFTER && {
            let work = sf.borrow().stats.in_tranches + comp.as_ref().map_or(Duration::ZERO, |c| c.in_compress);
            work.as_secs_f64() > share * start.elapsed().as_secs_f64()
        };
        if !blocked && !paced {
            // 1. Une tranche de DFS, si le brut en attente a de la place.
            if let Some(c_cur) = cur.as_mut()
                && comp.as_ref().is_none_or(|c| c.pending() < c.backlog)
            {
                let t = Instant::now();
                let done = match &mut comp {
                    Some(c) => {
                        let before = c.raw.len();
                        let done = art.borrow_mut().save_step(c_cur, &mut c.raw, SAVE_TRANCHE_BUDGET);
                        c.crc.update(&c.raw[before..]);
                        done
                    }
                    None => art.borrow_mut().save_step(c_cur, &mut sf.borrow_mut().fill, SAVE_TRANCHE_BUDGET),
                };
                let el = t.elapsed();
                {
                    let mut s = sf.borrow_mut();
                    s.stats.tranches += 1;
                    s.stats.in_tranches += el;
                    s.stats.max_tranche = s.stats.max_tranche.max(el);
                }
                if done {
                    // Le curseur ne contient que des idx et des valeurs
                    // possédées : libéré avant de fermer la barrière.
                    cur = None;
                    close_save(&art).await;
                    sf.borrow_mut().stats.walk = start.elapsed();
                    if let Some(c) = &mut comp {
                        let foot = format::footer(c.crc.clone().finalize());
                        c.raw.extend_from_slice(&foot);
                    }
                }
            }
            // 2. Une tranche de compression.
            if let Some(c) = &mut comp {
                let mut s = sf.borrow_mut();
                let t = Instant::now();
                if c.pending() > 0 {
                    let n = c.z.compress(&c.raw[c.pos..], &mut s.fill, COMPRESS_SLICE);
                    c.pos += n;
                    c.raw_total += n as u64;
                    if c.pos == c.raw.len() {
                        c.raw.clear();
                        c.pos = 0;
                    } else if c.pos > CHUNK && c.pos > c.raw.len() / 2 {
                        c.raw.drain(..c.pos);
                        c.pos = 0;
                    }
                } else if cur.is_none() && !c.finished {
                    c.finished = c.z.finish(&mut s.fill, COMPRESS_SLICE);
                }
                let el = t.elapsed();
                c.in_compress += el;
                c.max_slice = c.max_slice.max(el);
            }
            // 3. Écriture d'un morceau plein.
            let mut s = sf.borrow_mut();
            if !s.in_flight && s.fill.len() >= CHUNK {
                let buf = s.take_chunk();
                drop(s);
                monoio::spawn(write_loop(sf.clone(), buf));
            }
        }
        if cur.is_none() && comp.as_ref().is_none_or(|c| c.finished) {
            break false;
        }
        let y = Instant::now();
        yield_now().await;
        let el = y.elapsed();
        let mut s = sf.borrow_mut();
        s.stats.max_yield = s.stats.max_yield.max(el);
    };
    if cur.take().is_some() {
        // Abandon en plein DFS.
        close_save(&art).await;
        sf.borrow_mut().stats.walk = start.elapsed();
    }
    if let Some(c) = &comp {
        eprintln!(
            "[bgsave] zstd : {:.1} MB bruts, compression {:.2?} en tout, tranche de compression max {:.2?}",
            c.raw_total as f64 / 1e6,
            c.in_compress,
            c.max_slice
        );
    }

    let mut s = sf.borrow_mut();
    s.walk_done = true;
    if aborted {
        s.fill.clear();
    } else if comp.is_none() {
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
        // Un morceau plein seulement, sauf en fin de production : avec la
        // compression, `fill` grossit lentement et des écritures de quelques
        // KB s'enchaîneraient (10 000 écritures de 18 KB pour 185 MB).
        if s.err.is_none() && (s.fill.len() >= CHUNK || (s.walk_done && !s.fill.is_empty())) {
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
