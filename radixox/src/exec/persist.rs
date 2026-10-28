//! Fichiers de persistance indépendants du runtime : emplacement du dump,
//! rechargement au démarrage (SAVE_DESIGN.md §5bis), `SAVE` bloquante.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use oxidart::OxidArt;
use oxidart::compress::ZstdStream;
use oxidart::save::format;

/// Nom du dump dans `RADIXOX_DIR`.
pub const FILE_NAME: &str = "dump.rxd";
/// Fichier en cours d'écriture, renommé en `FILE_NAME` une fois complet.
pub const TMP_NAME: &str = "dump.rxd.tmp";

/// `RADIXOX_DIR` (défaut : dossier courant).
pub fn save_dir() -> PathBuf {
    PathBuf::from(std::env::var("RADIXOX_DIR").unwrap_or_else(|_| ".".into()))
}

/// Où écrire un dump : `dir/tmp`, renommé en `dir/name` une fois complet ;
/// `pos` : position du flux de commandes qu'il contient (en-tête v2).
#[derive(Debug, Clone)]
pub struct Target {
    pub dir: PathBuf,
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
pub fn load_at_startup(art: &mut OxidArt) -> io::Result<()> {
    let path = save_dir().join(FILE_NAME);
    let start = Instant::now();
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
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
        "[load] {} : {} clés ({} expirées sautées), {:.1} MB en {:?}{}",
        path.display(),
        stats.keys,
        stats.expired,
        size as f64 / 1e6,
        start.elapsed(),
        if stats.digest_checked { ", condensé vérifié" } else { "" }
    );
    Ok(())
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
pub fn save_sync(art: &mut OxidArt, level: Option<i32>) -> io::Result<SyncStats> {
    save_sync_to(art, level, &Target::dump())
}

/// [`save_sync`] vers `to`.
pub fn save_sync_to(art: &mut OxidArt, level: Option<i32>, to: &Target) -> io::Result<SyncStats> {
    use std::io::Write;
    if art.is_saving() {
        return Err(io::Error::other("Background save already in progress"));
    }
    let start = Instant::now();
    let dir = &to.dir;
    let tmp = dir.join(&to.tmp);
    let res = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        let mut z = level.map(ZstdStream::new);
        let mut buf = Vec::with_capacity(SYNC_CHUNK + SYNC_CHUNK / 4);
        let mut packed = Vec::new();
        let mut crc = crc32fast::Hasher::new();
        let (mut raw, mut written) = (0u64, 0u64);
        // `digest` : à la dernière écriture, le condensé du pied.
        let mut flush = |buf: &mut Vec<u8>, digest: Option<u32>, file: &mut std::fs::File| -> io::Result<()> {
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
        file.sync_data()?;
        drop(file);
        std::fs::rename(&tmp, dir.join(&to.name))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(SyncStats { raw, written, walk, total: start.elapsed() })
    })();
    if res.is_err() {
        if art.is_saving() {
            art.finish_save();
        }
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Niveau zstd de `SAVE` et de la save de sortie : `RADIXOX_SAVE_SYNC_ZSTD`
/// (défaut, absent ou 0 : brut, le plus rapide).
pub fn sync_level() -> Option<i32> {
    match std::env::var("RADIXOX_SAVE_SYNC_ZSTD").ok().and_then(|v| v.parse::<i32>().ok()) {
        Some(0) | None => None,
        Some(l) => Some(l.clamp(zstd_safe::min_c_level(), zstd_safe::max_c_level())),
    }
}

