//! Fichiers de l'AOF (DURABILITY_DESIGN.md §6.2–§6.5, §9) : manifeste,
//! segments de blocs, base, reprise au démarrage. Sans io_uring : la reprise
//! et la création tournent avant la boucle ; pendant le service, l'écriture
//! des blocs passe par le cœur (`Op::JournalWrite`), et la rotation, le
//! manifeste et les suppressions par des suites [`FsTask`] (§8.6).
//!
//! ```text
//! RADIXOX_DIR/appendonlydir/
//!   manifest          quelle base et quels segments forment l'état
//!   base.<n>.rxdb     dump v2 : contient exactement les blocs ≤ base_seq
//!   journal.<n>.rxj   blocs consécutifs, préalloués par paliers de 64 Mio
//! ```

#[allow(unused_imports)]
use crate::prelude::*;

use oxidart::{JournalPos, OxidArt};
use radixox_lib::rx_eprintln;
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::fs::{Mode, OFlags};

use crate::error::{self, Error};
use crate::fs::{self, FdSource};

use super::fs_task::{FdRef, FsTask};
use super::journal::{BadBlock, Cursor, HEADER, MAX_PAYLOAD, apply, read_block};
use super::persist::{Target, save_dir, save_sync_to};

pub const DIR_NAME: &str = "appendonlydir";
pub const MANIFEST: &str = "manifest";
const MANIFEST_TMP: &str = "manifest.tmp";
/// Palier de préallocation d'un segment.
pub const PREALLOC: u64 = 64 << 20;

/// `RADIXOX_DIR/appendonlydir`.
pub fn aof_dir() -> String {
    fs::join(&save_dir(), DIR_NAME)
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
        match rustix::rand::getrandom(&mut id[got..], rustix::rand::GetRandomFlags::empty()) {
            Ok(n) => got += n,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => panic!("getrandom : {e}"),
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
    /// Réplica : l'histoire de la source suivie (DURABILITY_DESIGN.md §10.7) ;
    /// le serveur redémarre en mode suivi.
    pub follow: Option<ReplId>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut version, mut replid, mut base, mut incrs, mut follow) = (None, None, None, Vec::new(), None);
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
                ["follow", id] => follow = Some(unhex(id).ok_or_else(bad)?),
                _ => return Err(bad()),
            }
        }
        match version {
            Some(1) => {}
            Some(v) => return Err(format!("version de manifeste inconnue : {v}")),
            None => return Err("version absente".into()),
        }
        let (base, base_seq, base_offset) = base.ok_or("base absente")?;
        Ok(Self { replid: replid.ok_or("replid absent")?, base, base_seq, base_offset, incrs, follow })
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
        if let Some(id) = &self.follow {
            s += &format!("follow {}\n", hex(id));
        }
        s
    }

    /// Écriture atomique : `manifest.tmp`, fdatasync, rename, fsync du dossier.
    pub fn write(&self, dir: &str) -> error::Result<()> {
        let tmp = fs::join(dir, MANIFEST_TMP);
        let f = fs::create(&tmp)?;
        fs::write_all(&f, self.render().as_bytes())?;
        rustix::fs::fdatasync(&f)?;
        drop(f);
        rustix::fs::rename(&tmp, &fs::join(dir, MANIFEST))?;
        fs::sync_dir(dir)
    }

    pub fn read(dir: &str) -> error::Result<Option<Self>> {
        let path = fs::join(dir, MANIFEST);
        match fs::read(&path) {
            Ok(bytes) => core::str::from_utf8(&bytes)
                .map_err(|_| "manifeste non UTF-8".to_string())
                .and_then(Self::parse)
                .map(Some)
                .map_err(|e| invalid(format!("{path}: {e}"))),
            Err(e) if e.is_not_found() => Ok(None),
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
pub fn create_segment(dir: &str, name: &str) -> error::Result<OwnedFd> {
    let f = fs::open(&fs::join(dir, name), OFlags::RDWR | OFlags::CREATE | OFlags::EXCL, Mode::from_raw_mode(0o644))?;
    preallocate(&f, 0)?;
    rustix::fs::fsync(&f)?;
    fs::sync_dir(dir)?;
    Ok(f)
}

/// Réserve le palier qui commence à `from` (mode 0 : la taille du fichier
/// augmente, l'espace se lit comme des zéros).
pub fn preallocate(f: impl AsFd, from: u64) -> error::Result<()> {
    rustix::fs::fallocate(f, rustix::fs::FallocateFlags::empty(), from, PREALLOC as u64)?;
    Ok(())
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
pub fn replay_segment(art: &mut OxidArt, path: &str, cur: &mut Cursor) -> error::Result<(u64, SegEnd, u64)> {
    let mut rd = fs::BufReader::new(fs::open_read(path)?, 1 << 20);
    let read_err = |e: rustix::io::Errno| Error::from(e).ctx(path);
    let mut buf = vec![0u8; HEADER];
    let (mut end, mut blocks) = (0u64, 0u64);
    loop {
        buf.resize(HEADER, 0);
        let got = rd.read_full(&mut buf).map_err(read_err)?;
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
            let got = rd.read_full(&mut buf[HEADER..]).map_err(read_err)?;
            buf.truncate(HEADER + got);
        }
        let (b, payload) = match read_block(&buf, cur) {
            Ok(x) => x,
            Err(BadBlock::Zero) => return Ok((end, SegEnd::Clean, blocks)),
            Err(e) => return Ok((end, SegEnd::Bad(e), blocks)),
        };
        art.set_now(b.now);
        apply(art, payload).map_err(|e| invalid(format!("{path}: bloc {} (octet {end}) : {}", b.seq, e.0)))?;
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

fn invalid(msg: String) -> Error {
    Error::msg(msg)
}

/// Charge la base et rejoue les segments du manifeste. `None` : pas de
/// manifeste (premier démarrage avec l'AOF). Un bloc invalide en fin du
/// dernier segment (écriture coupée) est tronqué si `load_truncated` ;
/// ailleurs, refus de démarrer.
pub fn recover(art: &mut OxidArt, dir: &str, load_truncated: bool) -> error::Result<Option<Resume>> {
    let Some(m) = Manifest::read(dir)? else { return Ok(None) };
    let base_path = fs::join(dir, &m.base);
    // Toutes les clés de la base, même expirées depuis : une commande du
    // journal les a peut-être vues vivantes (DURABILITY_DESIGN.md §6.3).
    art.set_now(0);
    let file = fs::open_read(&base_path)?;
    let stats = art.load_file(FdSource(file)).map_err(|e| super::persist::load_error(e, &base_path))?;
    if !stats.digest_checked {
        return Err(invalid(format!("{base_path} : base sans condensé (dump v1)")));
    }
    if stats.pos != (JournalPos { seq: m.base_seq, offset: m.base_offset, replid: m.replid }) {
        return Err(invalid(format!(
            "{base_path} : position {:?} différente de celle du manifeste (seq {}, offset {}, replid {})",
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
        let path = fs::join(dir, name);
        let start_offset = cur.offset;
        let (end, how, blocks) = replay_segment(art, &path, &mut cur)?;
        match how {
            SegEnd::Clean => {}
            SegEnd::Bad(BadBlock::Version(v)) => {
                return Err(invalid(format!(
                    "{path} : bloc de version {v}, ce binaire lit la version {} ; faire BGREWRITEAOF avec l'ancienne version avant la montée",
                    super::journal::VERSION
                )));
            }
            SegEnd::Bad(e) if last && load_truncated && e.is_torn() => {
                rx_eprintln!(
                    "[aof] {path} : fin coupée après {blocks} blocs ({e:?}), tronqué à {end} octets (RADIXOX_AOF_LOAD_TRUNCATED=yes)"
                );
            }
            SegEnd::Bad(e) => {
                return Err(invalid(format!(
                    "{path} : bloc invalide à l'octet {end} ({e:?}){}",
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
            let f = fs::open(&path, OFlags::WRONLY, Mode::empty())?;
            rustix::fs::ftruncate(&f, end)?;
            rustix::fs::fsync(&f)?;
            tail_end = end;
            tail_start_offset = start_offset;
        }
    }
    Ok(Some(Resume { manifest: m, cursor: cur, tail_end, tail_start_offset }))
}

/// Premier démarrage avec l'AOF (§9.1) : l'état chargé (dump.rxd ou vide)
/// devient `base.1.rxdb`, suivi du segment vide `journal.1.rxj`, nouvelle
/// histoire.
pub fn create(art: &mut OxidArt, dir: &str) -> error::Result<Resume> {
    fs::create_dir_all(dir)?;
    let replid = new_replid();
    let pos = JournalPos { seq: 0, offset: 0, replid };
    let base = base_name(1);
    let target = Target { dir: dir.into(), tmp: format!("{base}.tmp"), name: base.clone(), pos };
    save_sync_to(art, super::persist::sync_level(), &target)?;
    let seg = segment_name(1);
    // Un reste d'une tentative précédente (crash avant le manifeste).
    let _ = rustix::fs::unlink(&fs::join(dir, &seg));
    create_segment(dir, &seg)?;
    let manifest = Manifest { replid, base, base_seq: 0, base_offset: 0, incrs: vec![seg], follow: None };
    manifest.write(dir)?;
    Ok(Resume {
        manifest,
        cursor: Cursor { seq: 0, offset: 0, now: art.now },
        tail_end: 0,
        tail_start_offset: 0,
    })
}

/// Fichiers de l'AOF que le manifeste ne cite pas (anciennes bases et
/// segments, restes d'une rotation interrompue, temporaires).
pub fn unlisted(dir: &str, m: &Manifest) -> error::Result<Vec<String>> {
    let mut out = Vec::new();
    for name in fs::read_dir(dir)? {
        let name = name.as_str();
        let listed = name == MANIFEST || name == m.base || m.incrs.iter().any(|n| n == name);
        let ours = file_index(name).is_some() || name.ends_with(".tmp");
        if ours && !listed {
            rx_eprintln!("[aof] {name} : non cité par le manifeste, supprimé");
            out.push(fs::join(dir, name));
        }
    }
    Ok(out)
}

/// Supprime, d'une traite, les fichiers que le manifeste ne cite pas, après
/// un chargement réussi (§9.7). Pendant le service : [`unlisted`] et une
/// suite de suppressions hors de la boucle.
pub fn remove_unlisted(dir: &str, m: &Manifest) -> error::Result<()> {
    for path in unlisted(dir, m)? {
        rustix::fs::unlink(&path).map_err(|e| Error::from(e).ctx(&path))?;
    }
    Ok(())
}

// ── Écriture pendant le service (§7.3, §7.4, §7.7) ─────────────────────────

/// `RADIXOX_APPENDFSYNC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fsync {
    /// Jamais (sauf à la rotation d'un segment) : le noyau écrit quand il veut.
    No,
    /// `fdatasync` toutes les secondes, jamais deux en vol.
    EverySec,
    /// Commit groupé : fsync dès qu'il y a du non-durable, un seul en vol ;
    /// les réponses attendent le fsync qui les couvre (§7.5).
    Always,
    /// Cycles « écriture + fsync liés » enchaînés, un seul en vol ; pendant
    /// un cycle, rien ne touche le fichier et les blocs s'accumulent ; les
    /// réponses n'attendent jamais (§7.4). Perte sur coupure ≈ deux cycles.
    Continuous,
}

impl Fsync {
    pub fn parse(v: &str) -> Result<Self, String> {
        match v.to_ascii_lowercase().as_str() {
            "no" => Ok(Fsync::No),
            "everysec" => Ok(Fsync::EverySec),
            "always" => Ok(Fsync::Always),
            "continuous" => Ok(Fsync::Continuous),
            _ => Err(format!("appendfsync inconnu : {v:?} (no, everysec, always, continuous)")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Fsync::No => "no",
            Fsync::EverySec => "everysec",
            Fsync::Always => "always",
            Fsync::Continuous => "continuous",
        }
    }
}

/// Octets écrits par opération au plus (plusieurs blocs fermés à la suite).
const WRITE_MAX: usize = 4 << 20;
/// Seuil par défaut du fsync anticipé d'`everysec` (`AofWriter::sync_bytes`).
pub const SYNC_BYTES: u64 = 32 << 20;
/// Volume minimal d'un cycle `continuous` hors timer : une page.
pub const CYCLE_MIN: u64 = 4 << 10;
/// Volume au-delà duquel un cycle `continuous` se clôt par son fsync même si
/// des blocs attendent encore (un débit soutenu ne repousse pas le fsync).
const CYCLE_MAX: u64 = 64 << 20;

/// Opération de fichier demandée par l'écrivain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AofOp {
    /// Écrire [`AofWriter::writing`] à `off` dans le fichier `fd`. `link` :
    /// le `Sync` rendu juste après lui est lié (`IOSQE_IO_LINK`), annulé
    /// (`-ECANCELED`) si l'écriture échoue ou reste partielle.
    Write { fd: i32, off: u64, link: bool },
    Sync { fd: i32 },
}

/// Passage au segment suivant après un `CUT` (§8.1, §8.3).
#[derive(Debug)]
struct Rotation {
    /// Offset du flux après le bloc `CUT` : fin du segment courant.
    boundary: u64,
    /// Numéro du segment suivant (et de la base de la réécriture).
    index: u64,
    /// fsync couvrant la frontière soumis.
    sync_sent: bool,
    /// Frontière écrite et fsyncée : reste à créer le segment suivant.
    synced: bool,
}

/// Suite de fichiers en vol de l'écrivain (une à la fois, file `Aof` du
/// cœur), avec ce qu'il faut pour l'appliquer à son retour.
#[derive(Debug)]
enum FsWork {
    /// Segment suivant créé, manifeste qui le cite ; `seg` : son fichier.
    Rotate { manifest: Manifest, seg: usize },
    /// Manifeste qui cite la nouvelle base.
    Publish { manifest: Manifest, pos: JournalPos },
    /// Manifeste marqué (ou démarqué) `follow`.
    Follow { manifest: Manifest },
}

/// Écrivain du segment courant : machine à états sans I/O, comme le BGSAVE.
/// Le cœur demande les opérations ([`next_op`](Self::next_op)), les soumet
/// et rend leurs résultats ; une seule écriture et un seul fsync en vol.
pub struct AofWriter {
    pub dir: String,
    pub manifest: Manifest,
    file: OwnedFd,
    /// Fin logique du segment : octets écrits avec succès.
    file_end: u64,
    /// Taille préallouée du fichier.
    reserved: u64,
    /// Octets à écrire (en vol, ou à réessayer après une erreur), à partir de `done`.
    writing: Vec<u8>,
    done: usize,
    in_flight: bool,
    pub fsync: Fsync,
    /// Timer d'une seconde passé (`everysec`) : un fsync est dû.
    sync_due: bool,
    /// fsync en vol : offset du flux qu'il couvrira.
    syncing: Option<u64>,
    /// Offset du flux écrit (cache de pages) / couvert par le dernier fsync réussi.
    pub written: u64,
    pub durable: u64,
    write_err: Option<String>,
    fsync_err: Option<String>,
    /// Après une erreur d'écriture : pas de nouvel essai avant.
    retry_at: Option<radixox_lib::sys::Instant>,
    base_size: u64,
    /// fsync dus alors que le précédent était encore en vol.
    delayed_fsync: u64,
    rotation: Option<Rotation>,
    /// Base écrite par une réécriture finie : publiée dès la rotation finie.
    pending_base: Option<(String, JournalPos)>,
    /// Suite de fichiers en vol (§8.6).
    in_fs: Option<FsWork>,
    /// Marque `follow` du manifeste à écrire.
    follow_pending: Option<Option<ReplId>>,
    /// Fichiers que le manifeste ne cite plus : à supprimer hors de la boucle.
    trash: Vec<String>,
    /// Numéro du prochain manifeste temporaire (noms uniques : une suite d'un
    /// écrivain remplacé ne peut pas écrire dans le temporaire d'une autre).
    tmp_seq: u64,
    /// Génération : un réplica qui recharge un dump remplace son écrivain ;
    /// les opérations de l'ancien encore en vol sont reconnues par ce numéro.
    pub generation: u32,
    /// `everysec` : écrit et pas encore fsyncé au-delà duquel un fsync part
    /// sans attendre le timer (`RADIXOX_AOF_SYNC_MB`, §7.4).
    pub sync_bytes: u64,
    /// `continuous` : un cycle part dès que ce volume attend ; en dessous,
    /// au timer (`RADIXOX_AOF_CYCLE_MIN_KB`, garde-fou d'usure).
    pub cycle_min: u64,
    /// Le `Sync` lié à l'écriture qui vient d'être rendue est à rendre.
    link_next: bool,
    /// Le fsync en vol est lié à une écriture : `-ECANCELED` = pas fait.
    linked_sync: bool,
    /// fsync réussis (`INFO aof_fsyncs`).
    fsyncs: u64,
    /// Octets écrits par le cycle `continuous` en cours (avant son fsync).
    cycle_bytes: u64,
}

impl AofWriter {
    /// Ouvre le segment courant de la reprise, prêt à écrire à sa fin logique.
    pub fn open(dir: &str, res: &Resume, fsync: Fsync) -> error::Result<Self> {
        let name = res.manifest.incrs.last().ok_or_else(|| invalid("manifeste sans segment".into()))?;
        let file = fs::open(&fs::join(dir, name), OFlags::RDWR, Mode::empty())?;
        let base_size = fs::path_len(&fs::join(dir, &res.manifest.base)).unwrap_or(0);
        let reserved = fs::len(&file)?;
        Ok(Self {
            dir: dir.into(),
            manifest: res.manifest.clone(),
            file,
            file_end: res.tail_end,
            reserved,
            writing: Vec::new(),
            done: 0,
            in_flight: false,
            fsync,
            sync_due: false,
            syncing: None,
            written: res.cursor.offset,
            durable: res.cursor.offset,
            write_err: None,
            fsync_err: None,
            retry_at: None,
            base_size,
            delayed_fsync: 0,
            sync_bytes: SYNC_BYTES,
            cycle_min: CYCLE_MIN,
            link_next: false,
            linked_sync: false,
            fsyncs: 0,
            cycle_bytes: 0,
            rotation: None,
            pending_base: None,
            in_fs: None,
            follow_pending: None,
            trash: Vec::new(),
            tmp_seq: 0,
            generation: 0,
        })
    }

    /// Réplica / promotion : marque (ou démarque) le manifeste `follow`. Écrit
    /// hors de la boucle, après les suites déjà demandées.
    pub fn set_follow(&mut self, follow: Option<ReplId>) {
        self.follow_pending = Some(follow);
    }

    /// Une opération est en vol (le noyau lit `writing` ou le fichier).
    pub fn busy(&self) -> bool {
        self.in_flight || self.syncing.is_some() || self.in_fs.is_some()
    }

    /// Prochaine suite de fichiers à lancer (file `Aof` : une à la fois, dans
    /// l'ordre des demandes) : création du segment suivant d'une rotation,
    /// publication d'une base, marque `follow`. Construite au moment de partir,
    /// depuis le manifeste courant.
    pub fn next_fs(&mut self, now: radixox_lib::sys::Instant) -> Option<FsTask> {
        if self.in_fs.is_some() {
            return None;
        }
        let mut t = FsTask::new();
        let work = if let Some(rot) = &self.rotation {
            if !rot.synced || !self.retry_due(now) {
                return None;
            }
            // Segment courant ramené à sa fin logique, segment suivant créé et
            // cité par le manifeste **avant** d'y écrire (§8.3). Un seul fsync
            // du dossier à la fin : il rend durables la création et le renommage.
            let name = segment_name(rot.index);
            t.ftruncate(FdRef::Raw(self.file.as_raw_fd()), self.file_end);
            let path = fs::join(&self.dir, &name);
            t.unlink(&path, true);
            let seg = t.open(&path, OFlags::RDWR | OFlags::CREATE | OFlags::EXCL, Mode::from_raw_mode(0o644));
            t.fallocate(seg, 0, PREALLOC);
            t.fsync(seg, false);
            let mut manifest = self.manifest.clone();
            manifest.incrs.push(name);
            let FdRef::Opened(seg) = seg else { unreachable!() };
            FsWork::Rotate { manifest, seg }
        } else if let Some((base, pos)) = self.pending_base.take() {
            let Some(from) = file_index(&base) else { return None };
            let manifest = Manifest {
                replid: self.manifest.replid,
                base,
                base_seq: pos.seq,
                base_offset: pos.offset,
                incrs: self.manifest.incrs.iter().filter(|n| file_index(n).is_some_and(|i| i >= from)).cloned().collect(),
                follow: self.manifest.follow,
            };
            FsWork::Publish { manifest, pos }
        } else if let Some(follow) = self.follow_pending.take() {
            FsWork::Follow { manifest: Manifest { follow, ..self.manifest.clone() } }
        } else {
            return None;
        };
        let manifest = match &work {
            FsWork::Rotate { manifest, .. } | FsWork::Publish { manifest, .. } | FsWork::Follow { manifest } => manifest,
        };
        self.tmp_seq += 1;
        let tmp = format!("{MANIFEST}.{}.{}.tmp", self.generation, self.tmp_seq);
        t.replace_file(&self.dir, &tmp, MANIFEST, manifest.render().into_bytes());
        self.in_fs = Some(work);
        Some(t)
    }

    /// Retour de la suite lancée par [`next_fs`](Self::next_fs).
    pub fn fs_done(&mut self, mut t: FsTask, now: radixox_lib::sys::Instant) {
        let Some(work) = self.in_fs.take() else { return };
        let err = t.error();
        match work {
            FsWork::Rotate { manifest, seg } => {
                let Some(file) = err.is_none().then(|| t.take_file(seg)).flatten() else {
                    let name = manifest.incrs.last().cloned().unwrap_or_default();
                    let e = err.map_or_else(|| "segment non ouvert".into(), |e| e.to_string());
                    // Nouvel essai après le délai d'erreur (la suite recommence
                    // par supprimer un segment à moitié créé).
                    self.fail_write(format!("rotation vers {name} : {e}"), now);
                    return;
                };
                self.file = file;
                self.file_end = 0;
                self.reserved = PREALLOC;
                self.manifest = manifest;
                let rot = self.rotation.take().expect("rotation en cours");
                debug_assert_eq!(self.written, rot.boundary);
            }
            FsWork::Publish { manifest, pos } => {
                if let Some(e) = err {
                    // L'ancien manifeste reste valide ; la prochaine réécriture réessaiera.
                    rx_eprintln!("[aof] manifeste de la base {} : {e} ; ancienne base gardée", manifest.base);
                    return;
                }
                self.manifest = manifest;
                let base = &self.manifest.base;
                self.base_size = fs::path_len(&fs::join(&self.dir, base)).unwrap_or(0);
                match unlisted(&self.dir, &self.manifest) {
                    Ok(paths) => self.trash.extend(paths),
                    Err(e) => rx_eprintln!("[aof] liste des anciens fichiers : {e}"),
                }
                // Tout ce qui précède la base est dans un fichier neuf, fsyncé : un
                // fsync raté plus tôt ne compte plus que pour les blocs d'après.
                if self.fsync_err.take().is_some() {
                    rx_eprintln!("[aof] nouvelle base publiée : écritures de nouveau acceptées");
                }
                rx_eprintln!("[aof] réécriture publiée : {base} (seq {}), {} segment(s)", pos.seq, self.manifest.incrs.len());
            }
            FsWork::Follow { manifest } => match err {
                None => self.manifest = manifest,
                Some(e) => rx_eprintln!(
                    "[aof] manifeste (follow) : {e} ; au redémarrage, le serveur reprendrait son ancien rôle"
                ),
            },
        }
    }

    /// Fichiers à supprimer hors de la boucle (file `Trash` du cœur).
    pub fn take_trash(&mut self) -> Vec<String> {
        core::mem::take(&mut self.trash)
    }

    fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    /// Prochaine opération à soumettre, s'il y en a une : fsync de rotation,
    /// écriture (reprise d'une écriture partielle ou en erreur, sinon blocs
    /// fermés du flux, jamais au-delà d'une frontière de rotation), fsync
    /// `everysec`. À rappeler tant qu'elle rend quelque chose.
    pub fn next_op(&mut self, stream: &mut super::journal::Stream, now: radixox_lib::sys::Instant) -> Option<AofOp> {
        if core::mem::take(&mut self.link_next) {
            // Couvre l'écriture liée, rendue juste avant.
            self.syncing = Some(self.written + (self.writing.len() - self.done) as u64);
            self.linked_sync = true;
            return Some(AofOp::Sync { fd: self.fd() });
        }
        if let Some(rot) = &mut self.rotation
            && !rot.sync_sent
            && self.written == rot.boundary
            && !self.in_flight
            && self.syncing.is_none()
        {
            rot.sync_sent = true;
            self.syncing = Some(self.written);
            return Some(AofOp::Sync { fd: self.fd() });
        }
        if self.fsync == Fsync::Continuous {
            return self.next_cycle(stream, now);
        }
        if !self.in_flight && !self.retry_at.is_some_and(|t| now < t) && self.prepare_write(stream, now) {
            self.in_flight = true;
            return Some(AofOp::Write { fd: self.fd(), off: self.file_end, link: false });
        }
        // `everysec` : chaque seconde, ou plus tôt si le non-fsyncé dépasse le
        // seuil (perte bornée en temps et en volume, fsync de taille bornée).
        let full = self.fsync == Fsync::EverySec && self.written.saturating_sub(self.durable) >= self.sync_bytes;
        let due = self.sync_due || full || self.fsync == Fsync::Always;
        if due && self.syncing.is_none() && self.fsync_err.is_none() && self.durable < self.written {
            self.sync_due = false;
            self.syncing = Some(self.written);
            return Some(AofOp::Sync { fd: self.fd() });
        }
        None
    }

    /// `continuous` : un cycle à la fois, rien d'autre ne touche le fichier
    /// pendant qu'il tourne (un `write` pendant un fsync attend le commit du
    /// journal ext4 et le rallonge, mesuré). Part dès `cycle_min` en attente,
    /// ou au timer pour un reste plus petit. Un cycle écrit tout ce qui
    /// attend (plusieurs écritures si besoin) ; seule la dernière porte le
    /// fsync lié : sinon le journal plafonnerait à `WRITE_MAX` par fsync.
    fn next_cycle(&mut self, stream: &mut super::journal::Stream, now: radixox_lib::sys::Instant) -> Option<AofOp> {
        if self.in_flight || self.syncing.is_some() || self.retry_at.is_some_and(|t| now < t) {
            return None;
        }
        let pending = self.pending(stream);
        // Reste d'une écriture partielle : le cycle en cours continue.
        let resume = self.done < self.writing.len();
        let started = self.cycle_bytes > 0;
        if pending > 0
            && (resume || started || pending >= self.cycle_min || self.sync_due)
            && self.prepare_write(stream, now)
        {
            self.sync_due = false;
            self.in_flight = true;
            let len = (self.writing.len() - self.done) as u64;
            self.cycle_bytes += len;
            // Dernière écriture du cycle : plus rien d'autre n'attend, ou assez écrit.
            let last = stream.closed_bytes() == 0 || self.cycle_bytes >= CYCLE_MAX;
            self.link_next = last && self.fsync_err.is_none();
            if last {
                self.cycle_bytes = 0;
            }
            return Some(AofOp::Write { fd: self.fd(), off: self.file_end, link: self.link_next });
        }
        // Écrit mais pas fsyncé (fsync lié annulé par une écriture partielle).
        if self.durable < self.written && self.fsync_err.is_none() {
            self.sync_due = false;
            self.cycle_bytes = 0;
            self.syncing = Some(self.written);
            return Some(AofOp::Sync { fd: self.fd() });
        }
        None
    }

    /// Des octets à écrire dans `writing`, la place réservée. `false` : rien
    /// à écrire, ou préallocation en échec.
    fn prepare_write(&mut self, stream: &mut super::journal::Stream, now: radixox_lib::sys::Instant) -> bool {
        if self.done == self.writing.len() {
            self.writing.clear();
            self.done = 0;
            let limit = match &self.rotation {
                // Le segment courant s'arrête à la frontière ; la suite attend
                // le segment suivant.
                Some(r) if r.synced || r.sync_sent => 0,
                Some(r) => r.boundary - self.written,
                None => u64::MAX,
            };
            stream.take_closed(&mut self.writing, WRITE_MAX, limit);
            if self.writing.is_empty() {
                return false;
            }
        }
        let end = self.file_end + (self.writing.len() - self.done) as u64;
        while self.reserved < end {
            if let Err(e) = preallocate(&self.file, self.reserved) {
                self.fail_write(format!("préallocation : {e}"), now);
                return false;
            }
            self.reserved += PREALLOC;
        }
        true
    }

    /// Octets de l'écriture en vol.
    pub fn writing(&self) -> &[u8] {
        &self.writing[self.done..]
    }

    fn fail_write(&mut self, msg: String, now: radixox_lib::sys::Instant) {
        if self.fsync == Fsync::Always {
            // Des réponses retenues ne peuvent plus être garanties (§7.7).
            rx_eprintln!("[aof] erreur d'écriture en appendfsync always : {msg} ; arrêt");
            radixox_lib::sys::exit(1);
        }
        if self.write_err.is_none() {
            rx_eprintln!("[aof] erreur d'écriture : {msg} ; écritures refusées, nouvel essai chaque seconde");
        }
        self.write_err = Some(msg);
        self.retry_at = Some(now + core::time::Duration::from_secs(1));
    }

    /// CQE de l'écriture : octets écrits ou `-errno`.
    pub fn write_done(&mut self, res: i32, now: radixox_lib::sys::Instant) {
        debug_assert!(self.in_flight);
        self.in_flight = false;
        if res < 0 {
            self.fail_write(radixox_lib::sys::OsError(-res).to_string(), now);
            return;
        }
        let n = res as usize;
        if n == 0 && self.done < self.writing.len() {
            self.fail_write("écriture de 0 octet".into(), now);
            return;
        }
        self.done += n;
        self.file_end += n as u64;
        self.written += n as u64;
        if self.write_err.take().is_some() {
            rx_eprintln!("[aof] écriture rétablie");
            self.retry_at = None;
        }
    }

    /// Fin du délai d'attente après une erreur.
    pub fn retry_due(&self, now: radixox_lib::sys::Instant) -> bool {
        self.retry_at.is_none_or(|t| now >= t)
    }

    /// Timer d'une seconde : un fsync `everysec` est dû.
    pub fn tick(&mut self) {
        if self.fsync == Fsync::Continuous {
            self.sync_due = true;
            return;
        }
        if self.fsync != Fsync::EverySec {
            return;
        }
        if self.syncing.is_some() && self.durable < self.written {
            self.delayed_fsync += 1;
        }
        self.sync_due = true;
    }

    /// CQE d'un fsync. Un fsync en échec n'est jamais réessayé (l'état du
    /// cache de pages est inconnu) : écritures refusées jusqu'à la prochaine
    /// base réussie.
    pub fn sync_done(&mut self, res: i32) {
        let Some(upto) = self.syncing.take() else { return };
        if core::mem::take(&mut self.linked_sync) && res == -rustix::io::Errno::CANCELED.raw_os_error() {
            // Écriture liée partielle ou en échec : le cycle suivant reprend.
            return;
        }
        if res < 0 {
            let e = radixox_lib::sys::OsError(-res);
            if self.fsync == Fsync::Always {
                rx_eprintln!("[aof] fsync en échec en appendfsync always : {e} ; arrêt");
                radixox_lib::sys::exit(1);
            }
            rx_eprintln!("[aof] fsync en échec : {e} ; écritures refusées jusqu'à la prochaine réécriture réussie");
            self.fsync_err = Some(format!("fsync : {e}"));
            if let Some(rot) = &mut self.rotation {
                // La rotation réessaiera son fsync au prochain tour.
                rot.sync_sent = false;
            }
            return;
        }
        self.durable = self.durable.max(upto);
        self.fsyncs += 1;
        if let Some(rot) = &mut self.rotation
            && rot.sync_sent
            && upto >= rot.boundary
        {
            // Le segment suivant est créé par `next_fs`, hors de la boucle.
            rot.synced = true;
        }
    }

    // ── Réécriture (§8) ──────────────────────────────────────────────────

    /// Une réécriture ou une rotation est en cours (jusqu'à la publication de
    /// la nouvelle base comprise).
    pub fn rewriting(&self) -> bool {
        self.rotation.is_some() || self.pending_base.is_some() || matches!(self.in_fs, Some(FsWork::Publish { .. }))
    }

    /// Point de coupure d'une réécriture (§8.1) : le bloc en cours est fermé
    /// avec `CUT` (même vide), le segment courant s'arrêtera après lui. Rend
    /// la cible de la nouvelle base, qui contient exactement les blocs
    /// jusqu'au `CUT`. À appeler au T0 du BGSAVE.
    pub fn begin_rewrite(&mut self, stream: &mut super::journal::Stream) -> Target {
        debug_assert!(!self.rewriting());
        let cut = stream.cut();
        let index = self.manifest.last_index() + 1;
        self.rotation = Some(Rotation { boundary: cut.offset, index, sync_sent: false, synced: false });
        let name = base_name(index);
        Target {
            dir: self.dir.clone(),
            tmp: format!("{name}.tmp"),
            name,
            pos: JournalPos { seq: cut.seq, offset: cut.offset, replid: self.manifest.replid },
        }
    }

    /// Fin du BGSAVE de la réécriture. Réussi : la base est publiée dès que
    /// la rotation est finie (manifeste, puis suppression des anciens
    /// fichiers, §8.2). En échec : l'ancien manifeste reste valide (il cite
    /// le nouveau segment).
    pub fn rewrite_done(&mut self, target: &Target, ok: bool) {
        if ok {
            // Publiée par `next_fs` dès la rotation finie.
            self.pending_base = Some((target.name.clone(), target.pos));
        }
    }

    /// Réécriture automatique due (§8.4) : le journal depuis la base dépasse
    /// `min_size` et `percentage` % de la base.
    pub fn rewrite_due(&self, percentage: u64, min_size: u64) -> bool {
        if percentage == 0 || self.rewriting() {
            return false;
        }
        let incr = self.written.saturating_sub(self.manifest.base_offset);
        incr >= min_size && incr.saturating_mul(100) >= self.base_size.saturating_mul(percentage)
    }

    // ── État ─────────────────────────────────────────────────────────────

    /// Offset du flux jusqu'où les réponses peuvent partir (`always` : le
    /// durable) ; `None` : les réponses ne sont pas retenues.
    pub fn release_upto(&self) -> Option<u64> {
        (self.fsync == Fsync::Always).then_some(self.durable)
    }

    /// Octets du flux pas encore écrits (blocs fermés compris) : plafond §7.6.
    pub fn pending(&self, stream: &super::journal::Stream) -> u64 {
        stream.closed_bytes() + (self.writing.len() - self.done) as u64
    }

    /// Erreur qui refuse les écritures des clients (§7.7).
    pub fn error(&self) -> Option<&str> {
        self.write_err.as_deref().or(self.fsync_err.as_deref())
    }

    /// Champs `aof_*` de la section `# Persistence`.
    pub fn info(&self, stream: &super::journal::Stream, out: &mut String) {
        use core::fmt::Write as _;
        let incr = self.written.saturating_sub(self.manifest.base_offset);
        let _ = write!(
            out,
            "aof_enabled:1\r\naof_rewrite_in_progress:{}\r\naof_last_write_status:{}\r\naof_last_fsync_status:{}\r\n\
             aof_fsync:{}\r\naof_current_size:{}\r\naof_base_size:{}\r\naof_pending_bytes:{}\r\naof_delayed_fsync:{}\r\naof_fsyncs:{}\r\n",
            u8::from(self.rewriting()),
            if self.write_err.is_some() { "err" } else { "ok" },
            if self.fsync_err.is_some() { "err" } else { "ok" },
            self.fsync.name(),
            self.base_size + incr,
            self.base_size,
            self.pending(stream),
            self.delayed_fsync,
            self.fsyncs,
        );
    }

    /// Arrêt : ferme le bloc en cours, écrit tout ce qui reste (l'écriture en
    /// vol comprise : mêmes octets au même endroit) et fsync, d'une traite.
    /// Une rotation en cours est menée à son terme d'abord. Aucune suite de
    /// fichiers ne doit être en vol (la boucle les attend avant l'arrêt).
    pub fn flush_sync(&mut self, stream: &mut super::journal::Stream) -> error::Result<()> {
        debug_assert!(self.in_fs.is_none(), "suite de fichiers en vol à l'arrêt");
        stream.end_turn();
        let rest = self.writing[self.done..].to_vec();
        self.writing.clear();
        self.done = 0;
        fs::pwrite_all(&self.file, &rest, self.file_end)?;
        self.file_end += rest.len() as u64;
        self.written += rest.len() as u64;
        if let Some(rot) = &self.rotation {
            let mut before = Vec::new();
            stream.take_closed(&mut before, usize::MAX, rot.boundary - self.written);
            fs::pwrite_all(&self.file, &before, self.file_end)?;
            self.file_end += before.len() as u64;
            self.written += before.len() as u64;
            rustix::fs::fdatasync(&self.file)?;
            self.rotation.as_mut().unwrap().synced = true;
            // Pas de délai d'erreur à l'arrêt : la rotation part tout de suite,
            // puis la publication d'une base déjà écrite, s'il y en a une.
            self.retry_at = None;
            let now = radixox_lib::sys::Instant::now();
            while let Some(mut t) = self.next_fs(now) {
                t.run_blocking();
                self.fs_done(t, now);
            }
            if self.rotation.is_some() {
                return Err(Error::msg(self.write_err.clone().unwrap_or_default()));
            }
        }
        let mut after = Vec::new();
        stream.take_closed(&mut after, usize::MAX, u64::MAX);
        fs::pwrite_all(&self.file, &after, self.file_end)?;
        self.file_end += after.len() as u64;
        self.written += after.len() as u64;
        rustix::fs::fdatasync(&self.file)?;
        self.durable = self.written;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::*;

    #[test]
    fn manifest_roundtrip_and_errors() {
        let m = Manifest {
            replid: [0xab; 20],
            base: base_name(17),
            base_seq: 120_345,
            base_offset: 9_876_543,
            incrs: vec![segment_name(18), segment_name(19)],
            follow: None,
        };
        assert_eq!(Manifest::parse(&m.render()).unwrap(), m);
        let f = Manifest { follow: Some([0x5c; 20]), ..m.clone() };
        assert_eq!(Manifest::parse(&f.render()).unwrap(), f);
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
