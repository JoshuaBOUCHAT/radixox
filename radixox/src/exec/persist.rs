//! Fichiers de persistance indépendants du runtime : emplacement du dump et
//! rechargement au démarrage (SAVE_DESIGN.md §5bis).

use std::io;
use std::path::PathBuf;
use std::time::Instant;

use oxidart::OxidArt;

/// Nom du dump dans `RADIXOX_DIR`.
pub const FILE_NAME: &str = "dump.rxd";

/// `RADIXOX_DIR` (défaut : dossier courant).
pub fn save_dir() -> PathBuf {
    PathBuf::from(std::env::var("RADIXOX_DIR").unwrap_or_else(|_| ".".into()))
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
        "[load] {} : {} clés ({} expirées sautées), {:.1} MB en {:?}",
        path.display(),
        stats.keys,
        stats.expired,
        size as f64 / 1e6,
        start.elapsed()
    );
    Ok(())
}

