//! BGSAVE indépendant du runtime (SAVE_DESIGN.md §4, §5) : une machine à états
//! synchrone, avancée d'une tranche à la fois par la boucle, qui demande ses
//! écritures de fichier au lieu de les faire.
//!
//! Déroulé : DFS par tranches (barrière COW ouverte) → fermeture de la barrière
//! et vidage par tranches de la liste différée valart → fin de la compression
//! zstd → dernières écritures → fsync → `rename` sur `dump.rxd` → fsync du
//! dossier. La création du fichier et le renommage final sont des suites
//! [`FsTask`] exécutées hors de la boucle (DURABILITY_DESIGN.md §8.6) : le DFS
//! commence sans attendre le fichier. Le DFS remplit `fill` (ou le flux brut à compresser) ; quand `fill`
//! dépasse `CHUNK` et qu'aucune écriture n'est en vol, il devient l'écriture
//! en vol (double tampon : `fill` / `spare` / `writing`).

#[allow(unused_imports)]
use crate::prelude::*;

use core::time::Duration;
use radixox_lib::rx_eprintln;
use radixox_lib::sys::{self, Instant};

use oxidart::compress::ZstdStream;
use oxidart::save::format;
use oxidart::{OxidArt, SaveCursor};
use rustix::fd::{AsRawFd, OwnedFd, RawFd};

use crate::error::{Error, Result};
use crate::fs;

use super::fs_task::FsTask;
use super::persist::Target;

/// Budget d'une tranche de DFS ou de vidage (nœuds).
const TRANCHE_BUDGET: u32 = 512;
/// Durée visée d'une tranche de compression.
const COMPRESS_SLICE: Duration = Duration::from_micros(150);
/// Taille à partir de laquelle une écriture part.
const CHUNK: usize = 2 << 20;
/// Niveau zstd par défaut (`RADIXOX_SAVE_ZSTD`, 0 = brut).
const DEFAULT_ZSTD_LEVEL: i32 = 3;
/// Part du thread que le BGSAVE s'accorde sous charge (`RADIXOX_SAVE_CPU_SHARE`).
pub const DEFAULT_CPU_SHARE: f64 = 0.1;

/// Paramètres de compression (`RADIXOX_SAVE_ZSTD`, `RADIXOX_SAVE_BACKLOG_MB`).
#[derive(Clone, Copy)]
pub struct ZstdConf {
    level: i32,
    /// Brut en attente de compression, au plus : plein, le DFS attend.
    backlog: usize,
}

pub fn zstd_conf() -> Option<ZstdConf> {
    let level = match sys::env("RADIXOX_SAVE_ZSTD") {
        Some(v) => v.parse().unwrap_or_else(|_| {
            rx_eprintln!("[bgsave] RADIXOX_SAVE_ZSTD={v} invalide, niveau {DEFAULT_ZSTD_LEVEL}");
            DEFAULT_ZSTD_LEVEL
        }),
        None => DEFAULT_ZSTD_LEVEL,
    };
    if level == 0 {
        return None;
    }
    let level = level.clamp(zstd_safe::min_c_level(), zstd_safe::max_c_level());
    let mb: usize = sys::env_parse("RADIXOX_SAVE_BACKLOG_MB", 64);
    Some(ZstdConf { level, backlog: mb.max(1) << 20 })
}

/// Part du thread accordée au BGSAVE sous charge (entre 0 et 1).
pub fn cpu_share() -> f64 {
    match sys::env("RADIXOX_SAVE_CPU_SHARE") {
        None => DEFAULT_CPU_SHARE,
        Some(v) => match v.parse::<f64>() {
            Ok(x) if x > 0.0 && x <= 1.0 => x,
            _ => {
                rx_eprintln!("[bgsave] RADIXOX_SAVE_CPU_SHARE={v} invalide, {DEFAULT_CPU_SHARE}");
                DEFAULT_CPU_SHARE
            }
        },
    }
}

struct Compress {
    z: ZstdStream,
    /// Flux brut produit par le DFS ; `raw[pos..]` reste à compresser.
    raw: Vec<u8>,
    pos: usize,
    backlog: usize,
    /// crc du fichier brut (le pied est dans la frame zstd).
    crc: crc32fast::Hasher,
    finished: bool,
}

impl Compress {
    fn pending(&self) -> usize {
        self.raw.len() - self.pos
    }
}

/// Opération de fichier demandée par le BGSAVE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    /// Écrire [`BgSave::writing`] à l'offset donné.
    Write { fd: RawFd, off: u64 },
    /// fsync (données) du fichier.
    Sync { fd: RawFd },
}

/// Où en est le BGSAVE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// DFS (barrière ouverte), compression au fil de l'eau.
    Walk,
    /// Barrière fermée, liste différée valart vidée par tranches.
    Drain,
    /// Fin de la compression et des écritures.
    Flush,
    /// fsync en vol.
    Sync,
    /// `rename` et fsync du dossier en vol.
    Finalize,
    Done,
}

pub struct BgSave {
    /// Numéro donné par le cœur : reconnaît les CQE d'un BGSAVE abandonné.
    pub id: u32,
    /// Fichier visé (`dump.rxd`, ou une base de l'AOF) et position du flux.
    target: Target,
    /// Fichier temporaire, une fois créé (suite demandée au démarrage).
    file: Option<OwnedFd>,
    /// Suite de fichiers à lancer (création, puis renommage final).
    fs_req: Option<FsTask>,
    /// Une suite de fichiers est en vol.
    fs_in_flight: bool,
    pos: u64,
    cur: Option<SaveCursor>,
    comp: Option<Compress>,
    fill: Vec<u8>,
    spare: Vec<u8>,
    /// Tampon en cours d'écriture (vide : pas d'écriture en vol). Le noyau le
    /// lit jusqu'au CQE : jamais modifié pendant ce temps.
    writing: Vec<u8>,
    /// crc de tout ce qui a été pris pour écriture (pied du dump brut).
    crc: crc32fast::Hasher,
    /// Pied du dump brut ajouté.
    footer: bool,
    /// Condensé du contenu, connu à la fin du DFS (pied, DURABILITY_DESIGN.md §6.4).
    digest: u32,
    phase: Phase,
    err: Option<Error>,
    /// Temps passé dans les tranches, au repos compris (plancher de CPU).
    work: Duration,
    start: Instant,
    walk: Duration,
    writes: u64,
    /// Durées notables (diagnostic : pics de latence pendant une save).
    max_slice: Duration,
    start_took: Duration,
    finalize_took: Duration,
}

impl BgSave {
    /// Ouvre la barrière de save ; le fichier temporaire de `target` est créé
    /// hors de la boucle ([`take_fs`](Self::take_fs)).
    pub fn start(art: &mut OxidArt, zstd: Option<ZstdConf>, target: Target) -> Result<Self> {
        if art.save_busy() {
            return Err(Error::msg("Background save already in progress"));
        }
        let t0 = Instant::now();
        let mut create = FsTask::new();
        create.open(
            &fs::join(&target.dir, &target.tmp),
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::TRUNC,
            rustix::fs::Mode::from_raw_mode(0o644),
        );
        let mut comp = zstd.map(|c| Compress {
            z: ZstdStream::new(c.level),
            raw: Vec::with_capacity(CHUNK + CHUNK / 4),
            pos: 0,
            backlog: c.backlog,
            crc: crc32fast::Hasher::new(),
            finished: false,
        });
        let mut fill = Vec::with_capacity(CHUNK + CHUNK / 4);
        let cur = match &mut comp {
            Some(c) => {
                let cur = art.save_start_at(&mut c.raw, &target.pos);
                c.crc.update(&c.raw);
                cur
            }
            None => art.save_start_at(&mut fill, &target.pos),
        };
        let start_took = t0.elapsed();
        Ok(Self {
            id: 0,
            target,
            file: None,
            fs_req: Some(create),
            fs_in_flight: false,
            pos: 0,
            cur: Some(cur),
            comp,
            fill,
            spare: Vec::with_capacity(CHUNK + CHUNK / 4),
            writing: Vec::new(),
            crc: crc32fast::Hasher::new(),
            footer: false,
            digest: 0,
            phase: Phase::Walk,
            err: None,
            work: Duration::ZERO,
            start: Instant::now(),
            walk: Duration::ZERO,
            writes: 0,
            max_slice: Duration::ZERO,
            start_took,
            finalize_took: Duration::ZERO,
        })
    }

    /// Temps de CPU pris par les tranches depuis le début.
    pub fn work(&self) -> Duration {
        self.work
    }

    pub fn started(&self) -> Instant {
        self.start
    }

    /// Une tranche ferait avancer la save (sinon on attend seulement le disque).
    pub fn wants_slice(&self) -> bool {
        match self.phase {
            // Morceau plein en attente du disque : le DFS ne produit plus, seule
            // la compression peut avancer.
            Phase::Walk if self.blocked() => self.comp.as_ref().is_some_and(|c| c.pending() > 0),
            Phase::Walk | Phase::Drain => true,
            Phase::Flush => self.comp.as_ref().is_some_and(|c| c.pending() > 0 || !c.finished),
            Phase::Sync | Phase::Finalize | Phase::Done => false,
        }
    }

    /// Morceau plein qui attend le disque (écriture en vol, ou fichier pas
    /// encore créé).
    fn blocked(&self) -> bool {
        (!self.writing.is_empty() || self.file.is_none()) && self.fill.len() >= CHUNK
    }

    /// Une tranche de calcul : DFS, vidage ou compression.
    pub fn slice(&mut self, art: &mut OxidArt) {
        if self.err.is_some() {
            return;
        }
        let t = Instant::now();
        match self.phase {
            Phase::Walk => {
                // Disque en retard (morceau plein en attente) : on ne produit plus.
                let room = self.comp.as_ref().is_none_or(|c| c.pending() < c.backlog);
                if !self.blocked() && room {
                    let cur = self.cur.as_mut().expect("curseur pendant le DFS");
                    let done = match &mut self.comp {
                        Some(c) => {
                            let before = c.raw.len();
                            let done = art.save_step(cur, &mut c.raw, TRANCHE_BUDGET);
                            c.crc.update(&c.raw[before..]);
                            done
                        }
                        None => art.save_step(cur, &mut self.fill, TRANCHE_BUDGET),
                    };
                    if done {
                        // Le curseur ne contient que des idx et des valeurs
                        // possédées : libéré avant de fermer la barrière.
                        self.digest = cur.digest().value();
                        self.cur = None;
                        art.end_save();
                        self.walk = self.start.elapsed();
                        if let Some(c) = &mut self.comp {
                            let foot = format::footer(c.crc.clone().finalize(), self.digest);
                            c.raw.extend_from_slice(&foot);
                        }
                        self.phase = Phase::Drain;
                    }
                }
                self.compress_slice();
            }
            Phase::Drain => {
                if art.drain_deferred(TRANCHE_BUDGET) {
                    self.phase = Phase::Flush;
                }
                self.compress_slice();
            }
            Phase::Flush => self.compress_slice(),
            Phase::Sync | Phase::Finalize | Phase::Done => {}
        }
        let el = t.elapsed();
        self.work += el;
        self.max_slice = self.max_slice.max(el);
    }

    fn compress_slice(&mut self) {
        let Some(c) = &mut self.comp else { return };
        if c.pending() > 0 {
            let n = c.z.compress(&c.raw[c.pos..], &mut self.fill, COMPRESS_SLICE);
            c.pos += n;
            if c.pos == c.raw.len() {
                c.raw.clear();
                c.pos = 0;
            } else if c.pos > CHUNK && c.pos > c.raw.len() / 2 {
                c.raw.drain(..c.pos);
                c.pos = 0;
            }
        } else if self.phase == Phase::Flush && !c.finished {
            c.finished = c.z.finish(&mut self.fill, COMPRESS_SLICE);
        }
    }

    /// Production terminée (DFS, vidage et compression) : tout est dans `fill`.
    fn produced(&self) -> bool {
        self.phase == Phase::Flush && self.comp.as_ref().is_none_or(|c| c.pending() == 0 && c.finished)
    }

    /// Prochaine opération de fichier à soumettre, s'il y en a une (au plus une
    /// en vol). `None` aussi quand le BGSAVE est fini ou en erreur.
    pub fn next_file_op(&mut self) -> Option<FileOp> {
        if !self.writing.is_empty() || matches!(self.phase, Phase::Sync | Phase::Finalize | Phase::Done) {
            return None;
        }
        let fd = self.file.as_ref()?.as_raw_fd();
        if self.err.is_some() {
            return None;
        }
        let last = self.produced();
        if last && self.comp.is_none() && !self.footer {
            // Dump brut : le pied (crc de tout le flux, fin de `fill` comprise)
            // ferme le fichier. En mode zstd, il est dans la frame compressée.
            let mut h = self.crc.clone();
            h.update(&self.fill);
            let foot = format::footer(h.finalize(), self.digest);
            self.fill.extend_from_slice(&foot);
            self.footer = true;
        }
        if self.fill.len() >= CHUNK || (last && !self.fill.is_empty()) {
            let buf = core::mem::replace(&mut self.fill, core::mem::take(&mut self.spare));
            self.crc.update(&buf);
            self.writing = buf;
            return Some(FileOp::Write { fd, off: self.pos });
        }
        if last {
            self.phase = Phase::Sync;
            return Some(FileOp::Sync { fd });
        }
        None
    }

    /// Octets de l'écriture en vol.
    pub fn writing(&self) -> &[u8] {
        &self.writing
    }

    /// Fin d'une écriture (`res` : octets écrits ou `-errno`).
    pub fn write_done(&mut self, res: i32) {
        let len = self.writing.len();
        if res < 0 {
            self.err = Some(Error::from(radixox_lib::sys::OsError(-res)));
        } else if (res as usize) < len {
            // Écriture courte (disque plein…) : le reste repart.
            self.writing.drain(..res as usize);
            self.pos += res as u64;
            return;
        } else {
            self.pos += len as u64;
        }
        self.writes += 1;
        let mut buf = core::mem::take(&mut self.writing);
        buf.clear();
        self.spare = buf;
    }

    /// Écriture partielle à reprendre : même tampon, offset avancé.
    pub fn resume_write(&self) -> Option<FileOp> {
        let fd = self.file.as_ref()?.as_raw_fd();
        (!self.writing.is_empty()).then_some(FileOp::Write { fd, off: self.pos })
    }

    /// Fin du fsync : reste le `rename` et le fsync du dossier, hors de la
    /// boucle ([`take_fs`](Self::take_fs)).
    pub fn sync_done(&mut self, res: i32) {
        if res < 0 {
            self.phase = Phase::Done;
            self.err = Some(Error::from(radixox_lib::sys::OsError(-res)));
            return;
        }
        self.phase = Phase::Finalize;
        let t = &self.target;
        let mut fin = FsTask::new();
        fin.rename(&fs::join(&t.dir, &t.tmp), &fs::join(&t.dir, &t.name));
        fin.sync_dir(&t.dir);
        self.fs_req = Some(fin);
        self.finalize_took = self.start.elapsed();
    }

    /// Suite de fichiers à lancer : création du fichier, puis renommage final.
    pub fn take_fs(&mut self) -> Option<FsTask> {
        let t = self.fs_req.take()?;
        self.fs_in_flight = true;
        Some(t)
    }

    /// Retour de la suite lancée par [`take_fs`](Self::take_fs).
    pub fn fs_done(&mut self, mut t: FsTask) {
        self.fs_in_flight = false;
        let err = t.error();
        if self.phase == Phase::Finalize {
            self.finalize_took = self.start.elapsed() - self.finalize_took;
            self.phase = Phase::Done;
            self.err = err;
            return;
        }
        match err {
            Some(e) => self.err = Some(e),
            None => self.file = t.take_file(0),
        }
    }

    /// Fichier visé.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Fini (ou en erreur et plus rien en vol) : rend le résultat.
    pub fn finished(&mut self, art: &mut OxidArt) -> Option<Result<()>> {
        if self.err.is_some() && !self.in_flight() {
            self.abort(art);
            return Some(Err(self.err.take().unwrap()));
        }
        (self.phase == Phase::Done).then(|| {
            rx_eprintln!(
                "[bgsave] {:.1} MB écrits en {:.2?} (DFS {:.2?}) ; {} écritures, CPU {:.2?} ; \
                 démarrage {:.2?}, tranche max {:.2?}, rename+fsync dossier (hors boucle) {:.2?}",
                self.pos as f64 / 1e6,
                self.start.elapsed(),
                self.walk,
                self.writes,
                self.work,
                self.start_took,
                self.max_slice,
                self.finalize_took
            );
            Ok(())
        })
    }

    /// Abandon (erreur, arrêt du serveur) : barrière fermée si besoin, fichier
    /// temporaire supprimé. Rien ne doit être en vol.
    pub fn abort(&mut self, art: &mut OxidArt) {
        if self.cur.take().is_some() || art.is_saving() {
            art.finish_save();
        }
        self.phase = Phase::Done;
        let _ = rustix::fs::unlink(&fs::join(&self.target.dir, &self.target.tmp));
    }

    /// Le fsync final est en vol.
    pub fn syncing(&self) -> bool {
        self.phase == Phase::Sync
    }

    /// Une écriture, un fsync ou une suite de fichiers est en vol.
    pub fn in_flight(&self) -> bool {
        !self.writing.is_empty() || self.phase == Phase::Sync || self.fs_in_flight
    }
}
