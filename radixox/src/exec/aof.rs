//! Fichiers de l'AOF (DURABILITY_DESIGN.md §6.2–§6.5, §9) : manifeste,
//! segments de blocs, base, reprise au démarrage. Sans io_uring : la reprise
//! et la création tournent avant la boucle ; l'écriture des blocs pendant le
//! service passe par le cœur (`Op::FileWrite`).
//!
//! ```text
//! RADIXOX_DIR/appendonlydir/
//!   manifest          quelle base et quels segments forment l'état
//!   base.<n>.rxdb     dump v2 : contient exactement les blocs ≤ base_seq
//!   journal.<n>.rxj   blocs consécutifs, préalloués par paliers de 64 Mio
//! ```

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use oxidart::{JournalPos, OxidArt};

use super::journal::{BadBlock, Cursor, HEADER, MAX_PAYLOAD, apply, read_block};
use super::persist::{Target, save_dir, save_sync_to};

pub const DIR_NAME: &str = "appendonlydir";
pub const MANIFEST: &str = "manifest";
const MANIFEST_TMP: &str = "manifest.tmp";
/// Palier de préallocation d'un segment.
pub const PREALLOC: u64 = 64 << 20;

/// `RADIXOX_DIR/appendonlydir`.
pub fn aof_dir() -> PathBuf {
    save_dir().join(DIR_NAME)
}

pub fn base_name(n: u64) -> String {
    format!("base.{n}.rxdb")
}

pub fn segment_name(n: u64) -> String {
    format!("journal.{n}.rxj")
}

/// Numéro d'un fichier `base.<n>.rxdb` ou `journal.<n>.rxj`.
pub fn file_index(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("base.").or_else(|| name.strip_prefix("journal."))?;
    let (n, ext) = rest.split_once('.')?;
    matches!(ext, "rxdb" | "rxj").then_some(())?;
    n.parse().ok()
}

// ── Identifiant d'histoire ──────────────────────────────────────────────────

pub type ReplId = [u8; 20];

/// 20 octets aléatoires (`getrandom`).
pub fn new_replid() -> ReplId {
    let mut id = [0u8; 20];
    let mut got = 0;
    while got < id.len() {
        // SAFETY: tampon valide de `len - got` octets.
        let r = unsafe { libc::getrandom(id[got..].as_mut_ptr().cast(), id.len() - got, 0) };
        if r > 0 {
            got += r as usize;
        } else if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            panic!("getrandom : {}", io::Error::last_os_error());
        }
    }
    id
}

pub fn hex(id: &ReplId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<ReplId> {
    if s.len() != 40 || !s.is_ascii() {
        return None;
    }
    let mut id = [0u8; 20];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(id)
}

// ── Manifeste (§6.5) ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub replid: ReplId,
    pub base: String,
    pub base_seq: u64,
    pub base_offset: u64,
    /// Segments, dans l'ordre du flux.
    pub incrs: Vec<String>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut version, mut replid, mut base, mut incrs) = (None, None, None, Vec::new());
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = || format!("ligne {} illisible : {line:?}", n + 1);
            let words: Vec<&str> = line.split_ascii_whitespace().collect();
            match words.as_slice() {
                ["version", v] => version = Some(v.parse::<u32>().map_err(|_| bad())?),
                ["replid", id] => replid = Some(unhex(id).ok_or_else(bad)?),
                ["base", name, "seq", seq, "offset", off] => {
                    let seq = seq.parse().map_err(|_| bad())?;
                    let off = off.parse().map_err(|_| bad())?;
                    file_index(name).ok_or_else(bad)?;
                    base = Some((name.to_string(), seq, off));
                }
                ["incr", name] => {
                    file_index(name).ok_or_else(bad)?;
                    incrs.push(name.to_string());
                }
                _ => return Err(bad()),
            }
        }
        match version {
            Some(1) => {}
            Some(v) => return Err(format!("version de manifeste inconnue : {v}")),
            None => return Err("version absente".into()),
        }
        let (base, base_seq, base_offset) = base.ok_or("base absente")?;
        Ok(Self { replid: replid.ok_or("replid absent")?, base, base_seq, base_offset, incrs })
    }

    pub fn render(&self) -> String {
        let mut s = format!(
            "# radixox aof manifest\nversion 1\nreplid {}\nbase {} seq {} offset {}\n",
            hex(&self.replid),
            self.base,
            self.base_seq,
            self.base_offset
        );
        for name in &self.incrs {
            s += &format!("incr {name}\n");
        }
        s
    }

    /// Écriture atomique : `manifest.tmp`, fdatasync, rename, fsync du dossier.
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        use std::io::Write;
        let tmp = dir.join(MANIFEST_TMP);
        let mut f = File::create(&tmp)?;
        f.write_all(self.render().as_bytes())?;
        f.sync_data()?;
        drop(f);
        std::fs::rename(&tmp, dir.join(MANIFEST))?;
        File::open(dir)?.sync_all()
    }

    pub fn read(dir: &Path) -> io::Result<Option<Self>> {
        match std::fs::read_to_string(dir.join(MANIFEST)) {
            Ok(text) => Self::parse(&text)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", dir.join(MANIFEST).display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Numéro du dernier segment (le segment courant).
    pub fn last_index(&self) -> u64 {
        self.incrs.last().and_then(|n| file_index(n)).or_else(|| file_index(&self.base)).unwrap_or(0)
    }
}

// ── Segments ────────────────────────────────────────────────────────────────

/// Crée un segment vide, préalloué (§6.2) et synchronisé avec son dossier.
pub fn create_segment(dir: &Path, name: &str) -> io::Result<File> {
    let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(dir.join(name))?;
    preallocate(&f, 0)?;
    f.sync_all()?;
    File::open(dir)?.sync_all()?;
    Ok(f)
}

/// Réserve le palier qui commence à `from` (mode 0 : la taille du fichier
/// augmente, l'espace se lit comme des zéros).
pub fn preallocate(f: &File, from: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fd ouvert en écriture.
    let r = unsafe { libc::fallocate(f.as_raw_fd(), 0, from as i64, PREALLOC as i64) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Lit autant d'octets que possible (moins seulement à la fin du fichier).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Comment s'arrête la lecture d'un segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegEnd {
    /// Fin du fichier, ou en-tête nul (préallocation) : fin normale.
    Clean,
    /// Bloc invalide (écriture coupée, octets corrompus).
    Bad(BadBlock),
}

/// Rejoue les blocs d'un segment ; rend la fin logique (octets valides) et
/// la façon dont la lecture s'est arrêtée. Un bloc dont la charge ne se
/// rejoue pas est une erreur (flux valide d'une autre sémantique ou bogue).
pub fn replay_segment(art: &mut OxidArt, path: &Path, cur: &mut Cursor) -> io::Result<(u64, SegEnd, u64)> {
    let mut rd = io::BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut buf = vec![0u8; HEADER];
    let (mut end, mut blocks) = (0u64, 0u64);
    loop {
        buf.resize(HEADER, 0);
        let got = read_full(&mut rd, &mut buf)?;
        if got == 0 {
            return Ok((end, SegEnd::Clean, blocks));
        }
        if got < HEADER {
            let zero = buf[..got].iter().all(|&b| b == 0);
            return Ok((end, if zero { SegEnd::Clean } else { SegEnd::Bad(BadBlock::Truncated) }, blocks));
        }
        let len = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        if len <= MAX_PAYLOAD && !buf.iter().all(|&b| b == 0) {
            buf.resize(HEADER + len, 0);
            let got = read_full(&mut rd, &mut buf[HEADER..])?;
            buf.truncate(HEADER + got);
        }
        let (b, payload) = match read_block(&buf, cur) {
            Ok(x) => x,
            Err(BadBlock::Zero) => return Ok((end, SegEnd::Clean, blocks)),
            Err(e) => return Ok((end, SegEnd::Bad(e), blocks)),
        };
        art.set_now(b.now);
        apply(art, payload).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: bloc {} (octet {end}) : {}", path.display(), b.seq, e.0),
            )
        })?;
        end += b.size();
        blocks += 1;
    }
}

// ── Reprise (§9) ────────────────────────────────────────────────────────────

/// État repris : de quoi continuer le flux.
#[derive(Debug)]
pub struct Resume {
    pub manifest: Manifest,
    /// Après le dernier bloc rejoué (ou la base).
    pub cursor: Cursor,
    /// Fin logique du segment courant (dernier du manifeste) : on y ajoute.
    pub tail_end: u64,
    /// Octets du flux avant le segment courant.
    pub tail_start_offset: u64,
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Charge la base et rejoue les segments du manifeste. `None` : pas de
/// manifeste (premier démarrage avec l'AOF). Un bloc invalide en fin du
/// dernier segment (écriture coupée) est tronqué si `load_truncated` ;
/// ailleurs, refus de démarrer.
pub fn recover(art: &mut OxidArt, dir: &Path, load_truncated: bool) -> io::Result<Option<Resume>> {
    let Some(m) = Manifest::read(dir)? else { return Ok(None) };
    let base_path = dir.join(&m.base);
    // Toutes les clés de la base, même expirées depuis : une commande du
    // journal les a peut-être vues vivantes (DURABILITY_DESIGN.md §6.3).
    art.set_now(0);
    let file = File::open(&base_path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", base_path.display())))?;
    let stats = art.load_file(file).map_err(|e| invalid(format!("{}: {e}", base_path.display())))?;
    if !stats.digest_checked {
        return Err(invalid(format!("{} : base sans condensé (dump v1)", base_path.display())));
    }
    if stats.pos != (JournalPos { seq: m.base_seq, offset: m.base_offset, replid: m.replid }) {
        return Err(invalid(format!(
            "{} : position {:?} différente de celle du manifeste (seq {}, offset {}, replid {})",
            base_path.display(),
            stats.pos,
            m.base_seq,
            m.base_offset,
            hex(&m.replid)
        )));
    }
    let mut cur = Cursor { seq: m.base_seq, offset: m.base_offset, now: stats.t0 };
    art.set_now(stats.t0);
    let (mut tail_end, mut tail_start_offset) = (0, cur.offset);
    for (i, name) in m.incrs.iter().enumerate() {
        let last = i + 1 == m.incrs.len();
        let path = dir.join(name);
        let start_offset = cur.offset;
        let (end, how, blocks) = replay_segment(art, &path, &mut cur)?;
        match how {
            SegEnd::Clean => {}
            SegEnd::Bad(BadBlock::Version(v)) => {
                return Err(invalid(format!(
                    "{} : bloc de version {v}, ce binaire lit la version {} ; faire BGREWRITEAOF avec l'ancienne version avant la montée",
                    path.display(),
                    super::journal::VERSION
                )));
            }
            SegEnd::Bad(e) if last && load_truncated && e.is_torn() => {
                eprintln!(
                    "[aof] {} : fin coupée après {blocks} blocs ({e:?}), tronqué à {end} octets (RADIXOX_AOF_LOAD_TRUNCATED=yes)",
                    path.display()
                );
            }
            SegEnd::Bad(e) => {
                return Err(invalid(format!(
                    "{} : bloc invalide à l'octet {end} ({e:?}){}",
                    path.display(),
                    if !e.is_torn() {
                        " : bloc intact mais incohérent (segment manquant ou d'une autre histoire ?)"
                    } else if last {
                        " ; RADIXOX_AOF_LOAD_TRUNCATED=yes pour tronquer"
                    } else {
                        " au milieu du journal"
                    }
                )));
            }
        }
        if last {
            // La fin logique devient la fin du fichier : rien de l'ancienne
            // queue (préallocation, bloc coupé) ne peut suivre les nouveaux blocs.
            let f = std::fs::OpenOptions::new().write(true).open(&path)?;
            f.set_len(end)?;
            f.sync_all()?;
            tail_end = end;
            tail_start_offset = start_offset;
        }
    }
    Ok(Some(Resume { manifest: m, cursor: cur, tail_end, tail_start_offset }))
}

/// Premier démarrage avec l'AOF (§9.1) : l'état chargé (dump.rxd ou vide)
/// devient `base.1.rxdb`, suivi du segment vide `journal.1.rxj`, nouvelle
/// histoire.
pub fn create(art: &mut OxidArt, dir: &Path) -> io::Result<Resume> {
    std::fs::create_dir_all(dir)?;
    let replid = new_replid();
    let pos = JournalPos { seq: 0, offset: 0, replid };
    let base = base_name(1);
    let target = Target { dir: dir.to_path_buf(), tmp: format!("{base}.tmp"), name: base.clone(), pos };
    save_sync_to(art, super::persist::sync_level(), &target)?;
    let seg = segment_name(1);
    // Un reste d'une tentative précédente (crash avant le manifeste).
    let _ = std::fs::remove_file(dir.join(&seg));
    create_segment(dir, &seg)?;
    let manifest = Manifest { replid, base, base_seq: 0, base_offset: 0, incrs: vec![seg] };
    manifest.write(dir)?;
    Ok(Resume {
        manifest,
        cursor: Cursor { seq: 0, offset: 0, now: art.now },
        tail_end: 0,
        tail_start_offset: 0,
    })
}

/// Supprime les fichiers de l'AOF que le manifeste ne cite pas (restes d'une
/// rotation interrompue), après un chargement réussi (§9.7).
pub fn remove_unlisted(dir: &Path, m: &Manifest) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        let listed = name == MANIFEST || name == m.base || m.incrs.iter().any(|n| n == name);
        let ours = file_index(name).is_some() || name.ends_with(".tmp");
        if ours && !listed {
            eprintln!("[aof] {name} : non cité par le manifeste, supprimé");
            std::fs::remove_file(dir.join(name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_and_errors() {
        let m = Manifest {
            replid: [0xab; 20],
            base: base_name(17),
            base_seq: 120_345,
            base_offset: 9_876_543,
            incrs: vec![segment_name(18), segment_name(19)],
        };
        assert_eq!(Manifest::parse(&m.render()).unwrap(), m);
        assert_eq!(m.last_index(), 19);
        assert!(Manifest::parse("version 2\n").is_err());
        assert!(Manifest::parse("version 1\nreplid zz\n").is_err());
        assert!(Manifest::parse("version 1\nincr x.txt\n").is_err());
        assert!(Manifest::parse(&m.render().replace("base ", "# base ")).is_err());
    }

    #[test]
    fn names_and_replid() {
        assert_eq!(file_index("journal.12.rxj"), Some(12));
        assert_eq!(file_index("base.3.rxdb"), Some(3));
        assert_eq!(file_index("journal.x.rxj"), None);
        assert_eq!(file_index("dump.rxd"), None);
        let id = new_replid();
        assert_eq!(unhex(&hex(&id)), Some(id));
        assert_ne!(new_replid(), id);
    }
}
