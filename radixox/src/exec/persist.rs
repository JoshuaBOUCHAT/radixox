//! Fichiers de persistance indépendants du runtime : emplacement du dump,
//! rechargement au démarrage (SAVE_DESIGN.md §5bis), `SAVE` bloquante.

#[allow(unused_imports)]
use crate::prelude::*;

use core::time::Duration;
use radixox_lib::sys::Instant;

use oxidart::OxidArt;
use oxidart::compress::ZstdStream;
use oxidart::save::format;
use rustix::fd::OwnedFd;

use crate::error::{Error, Result};
use crate::fs::{self, FdSource};

/// Nom du dump dans `RADIXOX_DIR`.
pub const FILE_NAME: &str = "dump.rxd";
/// Fichier en cours d'écriture, renommé en `FILE_NAME` une fois complet.
pub const TMP_NAME: &str = "dump.rxd.tmp";

/// Dossier imposé par [`set_save_dir`] (sinon `RADIXOX_DIR`). Mono-thread : seul le
/// thread de la boucle (ou celui d'un test) y touche.
static mut SAVE_DIR: Option<String> = None;

/// Remplace `RADIXOX_DIR` (lu au démarrage) pour la suite du processus : les tests
/// changent de dossier sans `setenv`, que [`radixox_lib::sys::env`] ne voit pas.
pub fn set_save_dir(dir: Option<String>) {
    // SAFETY: mono-thread (voir `SAVE_DIR`).
    unsafe { *&raw mut SAVE_DIR = dir };
}

/// `RADIXOX_DIR` (défaut : dossier courant), ou le dossier de [`set_save_dir`].
pub fn save_dir() -> String {
    // SAFETY: mono-thread (voir `SAVE_DIR`).
    let set = unsafe { (*&raw const SAVE_DIR).clone() };
    set.unwrap_or_else(|| radixox_lib::sys::env("RADIXOX_DIR").unwrap_or(".").into())
}

/// Où écrire un dump : `dir/tmp`, renommé en `dir/name` une fois complet ;
/// `pos` : position du flux de commandes qu'il contient (en-tête v2).
#[derive(Debug, Clone)]
pub struct Target {
    pub dir: String,
    pub tmp: String,
    pub name: String,
    pub pos: oxidart::JournalPos,
}

impl Target {
    /// `RADIXOX_DIR/dump.rxd`, sans flux.
    pub fn dump() -> Self {
        Self { dir: save_dir(), tmp: TMP_NAME.into(), name: FILE_NAME.into(), pos: Default::default() }
    }
}

/// Recharge `RADIXOX_DIR/dump.rxd` dans l'arbre (vide) avant d'accepter des
/// connexions (SAVE_DESIGN.md §5bis). Fichier absent : démarrage à vide.
/// Fichier illisible, tronqué ou corrompu : erreur, le serveur ne démarre
/// pas (on n'écrase pas un dump qu'on n'a pas su lire).
pub fn load_at_startup(art: &mut OxidArt) -> Result<()> {
    let path = fs::join(&save_dir(), FILE_NAME);
    let start = Instant::now();
    let file = match fs::open_read(&path) {
        Ok(file) => file,
        Err(e) if e.is_not_found() => return Ok(()),
        Err(e) => return Err(e),
    };
    let size = fs::len(&file).unwrap_or(0);
    art.tick();
    // Lecture par fragments (fenêtre de 1 MB), crc vérifié en fin de flux.
    let stats = art.load_file(FdSource(file)).map_err(|e| load_error(e, &path))?;
    radixox_lib::rx_println!(
        "[load] {path} : {} clés ({} expirées sautées), {:.1} MB en {:?}{}",
        stats.keys,
        stats.expired,
        size as f64 / 1e6,
        start.elapsed(),
        if stats.digest_checked { ", condensé vérifié" } else { "" }
    );
    Ok(())
}

/// Erreur de rechargement de `path` (le code du noyau reste, s'il y en a un).
pub fn load_error(e: oxidart::LoadError, path: &str) -> Error {
    // `FdSource` ne rend que des `Os` (pas de `Io` de `std`).
    let os = match &e {
        oxidart::LoadError::Os(os) => Some(*os),
        _ => None,
    };
    Error::new(os, format!("{path}: {e}"))
}


/// Taille des écritures de `SAVE` : le DFS remplit un buffer, `write`
/// le copie dans le cache du noyau, qui écrit sur disque en parallèle.
const SYNC_CHUNK: usize = 2 << 20;
/// Budget d'un pas de DFS pour `SAVE` (seulement pour borner le buffer).
const SYNC_STEP_BUDGET: u32 = 1 << 16;

/// Bilan d'une `SAVE`.
pub struct SyncStats {
    pub raw: u64,
    pub written: u64,
    pub walk: Duration,
    pub total: Duration,
}

/// `SAVE` : save complète **bloquante** (ni tranche, ni `yield`) : le DFS
/// tourne d'une traite, le fichier est écrit par des `write` bloquants (copie
/// dans le cache du noyau, qui écrit en parallèle), puis fsync + rename.
/// `level` : niveau zstd (`None` : brut, le plus rapide en CPU). Erreur si un
/// BGSAVE est en cours.
pub fn save_sync(art: &mut OxidArt, level: Option<i32>) -> Result<SyncStats> {
    save_sync_to(art, level, &Target::dump())
}

/// [`save_sync`] vers `to`.
pub fn save_sync_to(art: &mut OxidArt, level: Option<i32>, to: &Target) -> Result<SyncStats> {
    if art.save_busy() {
        return Err(Error::msg("Background save already in progress"));
    }
    let start = Instant::now();
    let dir = &to.dir;
    let tmp = fs::join(dir, &to.tmp);
    let res = (|| -> Result<SyncStats> {
        let mut file = fs::create(&tmp)?;
        let mut z = level.map(ZstdStream::new);
        let mut buf = Vec::with_capacity(SYNC_CHUNK + SYNC_CHUNK / 4);
        let mut packed = Vec::new();
        let mut crc = crc32fast::Hasher::new();
        let (mut raw, mut written) = (0u64, 0u64);
        // `digest` : à la dernière écriture, le condensé du pied.
        let mut flush = |buf: &mut Vec<u8>, digest: Option<u32>, file: &mut OwnedFd| -> Result<()> {
            crc.update(buf);
            raw += buf.len() as u64;
            let last = digest.is_some();
            if let Some(digest) = digest {
                let foot = format::footer(crc.clone().finalize(), digest);
                buf.extend_from_slice(&foot);
                raw += foot.len() as u64;
            }
            match &mut z {
                None => {
                    fs::write_all(&*file, buf)?;
                    written += buf.len() as u64;
                }
                Some(z) => {
                    z.compress(buf, &mut packed, Duration::MAX);
                    if last {
                        while !z.finish(&mut packed, Duration::MAX) {}
                    }
                    fs::write_all(&*file, &packed)?;
                    written += packed.len() as u64;
                    packed.clear();
                }
            }
            buf.clear();
            Ok(())
        };
        let mut cur = art.save_start_at(&mut buf, &to.pos);
        let walk_start = Instant::now();
        let mut walk = Duration::ZERO;
        loop {
            let done = art.save_step(&mut cur, &mut buf, SYNC_STEP_BUDGET);
            if done {
                walk = walk_start.elapsed();
                // Rien n'a muté pendant la save : la liste différée est vide.
                let digest = cur.digest().value();
                drop(cur);
                art.finish_save();
                flush(&mut buf, Some(digest), &mut file)?;
                break;
            }
            if buf.len() >= SYNC_CHUNK {
                flush(&mut buf, None, &mut file)?;
            }
        }
        drop(flush);
        rustix::fs::fdatasync(&file)?;
        drop(file);
        rustix::fs::rename(&tmp, &fs::join(dir, &to.name))?;
        fs::sync_dir(dir)?;
        Ok(SyncStats { raw, written, walk, total: start.elapsed() })
    })();
    if res.is_err() {
        if art.is_saving() {
            art.finish_save();
        }
        let _ = rustix::fs::unlink(&tmp);
    }
    res
}

/// Niveau zstd de `SAVE` et de la save de sortie : `RADIXOX_SAVE_SYNC_ZSTD`
/// (défaut, absent ou 0 : brut, le plus rapide).
pub fn sync_level() -> Option<i32> {
    match radixox_lib::sys::env("RADIXOX_SAVE_SYNC_ZSTD").and_then(|v| v.parse::<i32>().ok()) {
        Some(0) | None => None,
        Some(l) => Some(l.clamp(zstd_safe::min_c_level(), zstd_safe::max_c_level())),
    }
}

