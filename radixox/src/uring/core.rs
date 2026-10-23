//! Cœur de la boucle, sans I/O (RUNTIME_ARCH.md §1.2) : il reçoit des
//! [`Completion`] et émet des [`Op`]. La couche io_uring (ou le faux noyau des
//! tests) traduit les `Op` en SQE et les CQE en `Completion`.
//!
//! Un tour de boucle, vu du cœur :
//! 1. [`Core::complete`] pour chaque CQE : accept, morceaux reçus rangés tels
//!    quels dans la file de leur connexion (aucun parse), fins d'envoi ;
//! 2. [`Core::run_batch`] : par fenêtres de K frames tirées entre connexions
//!    (§2.1) : parse (une seule passe), préchargement, exécution, réponses
//!    écrites dans `out` — les données sont lues juste avant d'être utilisées ;
//! 3. [`Core::run_jobs`] : une tranche de travail de fond ;
//! 4. [`Core::flush`] : un send par connexion sale, fermetures terminées ;
//! 5. [`Core::take_ops`] → SQE, puis `enter`.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use oxidart::OxidArt;
use smallvec::SmallVec;
use radixox_lib::cmd::{Cmd, FrameLen, Parse, frame_len};
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::{SharedFrame as Frame, extend_encode};

use crate::exec::client::Client;
use crate::exec::jobs::{FREE_BUDGET, FlushJob, KEYS_BUDGET, KeysJob, Step, UnlinkJob};
use crate::exec::autosave::{Persistence, Rule};
use crate::exec::aof::{AofOp, AofWriter, ReplId, new_replid};
use crate::exec::journal::{self, Cursor, Stream};
use crate::exec::bgsave::{BgSave, FileOp, cpu_share, zstd_conf};
use crate::exec::fs_task::{FsTask, StepRef};
use crate::exec::persist::{Target, save_dir, save_sync_to, sync_level};
use crate::exec::{Executed, execute_into, resp_ok};
use crate::tsc;

use super::bufs::{Bid, BufPool};
use super::conn::{ConnIn, ConnOut, Mode, Span};

mod repl;

/// Index de connexion = slot dans la table des fichiers enregistrés.
pub type Slot = u32;

pub const ENOBUFS: i32 = 105;
pub const ECANCELED: i32 = 125;

/// Réglages du cœur (valeurs par défaut : RUNTIME_ARCH.md §13.1).
#[derive(Clone, Debug)]
pub struct Config {
    pub max_conn: u32,
    /// Slots gardés pour répondre « max clients » avant de fermer.
    pub reserved: u32,
    pub bufs: Bid,
    pub buf_size: u32,
    /// Frame incomplète d'une connexion, au plus (`client-query-buffer-limit`).
    pub query_limit: usize,
    /// Commandes par tour de boucle, au plus (garde-fou du budget de tour).
    pub batch_cmds: usize,
    /// `out` au-delà duquel on envoie sans attendre la fin du tour (§6.2).
    pub flush_early: usize,
    /// `out` au-delà duquel une connexion qui a déjà un send en vol n'est plus
    /// tirée (backpressure, §6.2).
    pub out_backpressure: usize,
    /// Commandes dont les clés sont préchargées ensemble (`RADIXOX_PREFETCH`,
    /// 1 = pas de préchargement).
    pub prefetch: usize,
    /// Connexion sans rien reçu depuis cette durée : fermée (`timeout` de
    /// Redis ; `RADIXOX_TIMEOUT`, désactivé par défaut).
    pub idle_timeout: Option<Duration>,
    /// Clés examinées ou nœuds libérés par tranche de job (KEYS, UNLINK).
    pub job_budget: usize,
    /// Saves automatiques (`save` de Redis ; `RADIXOX_SAVE`, vide par défaut
    /// ici, les règles de Valkey par défaut dans le binaire).
    pub save_rules: Vec<Rule>,
    /// Écritures refusées après un BGSAVE en échec (`stop-writes-on-bgsave-error`).
    pub stop_writes_on_bgsave_error: bool,
    /// Flux de commandes (AOF, réplication ; DURABILITY_DESIGN.md) tenu à jour.
    pub journal: bool,
    /// Retard du flux sur le disque au-delà duquel les écrivains sont parqués
    /// (`RADIXOX_AOF_MAX_PENDING_MB`, DURABILITY_DESIGN.md §7.6).
    pub aof_max_pending: u64,
    /// `continuous` : délai maximal avant le cycle d'un reste plus petit que
    /// `aof_cycle_min` (`RADIXOX_AOF_CYCLE_MS`), et ce volume
    /// (`RADIXOX_AOF_CYCLE_MIN_KB`).
    pub aof_cycle: Duration,
    pub aof_cycle_min: u64,
    /// `everysec` : fsync anticipé au-delà de ce non-fsyncé (`RADIXOX_AOF_SYNC_MB`).
    pub aof_sync_bytes: u64,
    /// Réécriture automatique (`auto-aof-rewrite-percentage`, `-min-size`, §8.4).
    pub aof_rewrite_percentage: u64,
    pub aof_rewrite_min_size: u64,
    /// Blocs gardés pour la reprise partielle des réplicas
    /// (`RADIXOX_REPL_BACKLOG_MB`, DURABILITY_DESIGN.md §10.5).
    pub repl_backlog: u64,
    /// Retard d'un réplica (octets à lui envoyer) au-delà duquel il est
    /// déconnecté (`RADIXOX_REPL_MAX_LAG_MB`).
    pub repl_max_lag: u64,
    /// Dossier de `dump.rxd` et du dump reçu par un réplica (`None` :
    /// `RADIXOX_DIR`, lu à chaque save).
    pub dir: Option<std::path::PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_conn: 10_000,
            reserved: 4,
            // Petits tampons, rendus vite et réutilisés encore chauds : 64 × 32 Ko
            // contre 32 × 256 Ko = +27 % en pipeline 256, 15 % d'instructions et
            // de défauts L1 en moins (mesuré, RUNTIME_ARCH.md §12.4).
            bufs: 64,
            buf_size: 32 << 10,
            query_limit: 1 << 30,
            // Taille d'une tranche de tour : entre deux tranches, les réponses des
            // connexions qui ont fini partent (la boucle rappelle `run_batch`).
            batch_cmds: 2048,
            flush_early: 64 << 10,
            out_backpressure: 1 << 20,
            prefetch: 8,
            idle_timeout: None,
            job_budget: KEYS_BUDGET.min(FREE_BUDGET),
            save_rules: Vec::new(),
            stop_writes_on_bgsave_error: true,
            journal: false,
            aof_max_pending: 64 << 20,
            aof_cycle: Duration::from_millis(10),
            aof_cycle_min: crate::exec::aof::CYCLE_MIN,
            aof_sync_bytes: crate::exec::aof::SYNC_BYTES,
            aof_rewrite_percentage: 100,
            aof_rewrite_min_size: 64 << 20,
            repl_backlog: 64 << 20,
            repl_max_lag: 256 << 20,
            dir: None,
        }
    }
}

/// Résultat d'une opération noyau, traduit d'un CQE.
#[derive(Debug, Clone, Copy)]
pub enum Completion {
    /// Accept multishot : `fd` d'un socket accepté, ou `-errno`.
    Accept { fd: i32, more: bool },
    /// Recv multishot : `res` octets dans le tampon `bid`, `0` = EOF, ou `-errno`.
    Recv { slot: Slot, res: i32, bid: Option<Bid>, buf_more: bool, more: bool },
    /// Send : octets envoyés ou `-errno`.
    Send { slot: Slot, res: i32 },
    /// Écriture ou fsync du fichier de BGSAVE : octets écrits, 0, ou `-errno` ;
    /// `save` : numéro du BGSAVE qui l'a demandée.
    File { res: i32, save: u32 },
    /// Écriture du segment de l'AOF : octets écrits ou `-errno` ; `writer` :
    /// génération de l'écrivain qui l'a demandée.
    JournalWrite { res: i32, writer: u32 },
    /// fsync du segment de l'AOF : 0 ou `-errno`.
    JournalSync { res: i32, writer: u32 },
    /// Connexion sortante (réplication) : 0 ou `-errno`.
    Connect { slot: Slot, res: i32 },
    /// Étape d'une suite de fichiers (`Op::Fs`) : valeur du CQE.
    Fs { id: u32, res: i32 },
}

/// Opération à soumettre au noyau.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    AcceptMulti,
    /// Socket accepté : réglages (`TCP_NODELAY`…), installation dans `slot`
    /// (`FilesUpdate`) chaînée au premier `RecvMulti`, puis fermeture du fd.
    Install { slot: Slot, fd: i32 },
    /// Plus de slot : `-ERR max number of clients reached`, puis fermeture.
    Reject { fd: i32 },
    RecvMulti { slot: Slot },
    /// Envoyer [`Core::pending_send`] (`MSG_WAITALL`).
    Send { slot: Slot },
    CancelRecv { slot: Slot },
    CancelSend { slot: Slot },
    Close { slot: Slot },
    /// Rendre le tampon à l'anneau.
    ProvideBuf { bid: Bid },
    /// Écrire [`Core::pending_file_write`]`(save)` dans le fichier de BGSAVE à `off`.
    FileWrite { fd: i32, off: u64, save: u32 },
    /// fsync (données) du fichier de BGSAVE.
    FileSync { fd: i32, save: u32 },
    /// Écrire [`Core::pending_journal_write`]`(writer)` dans le segment de l'AOF à `off`.
    /// `link` : l'op suivante (le `JournalSync` du cycle) lui est liée
    /// (`IOSQE_IO_LINK`), soumise dans la même tournée.
    JournalWrite { fd: i32, off: u64, writer: u32, link: bool },
    /// fsync (données) du segment de l'AOF.
    JournalSync { fd: i32, writer: u32 },
    /// Connexion sortante vers un réplica : socket installé dans `slot`
    /// (réglages comme `Install`), puis `connect`.
    Connect { slot: Slot, addr: std::net::SocketAddr },
    /// Étape courante ([`Core::fs_step`]) de la suite de fichiers `id`, à
    /// exécuter hors de la boucle (worker io-wq).
    Fs { id: u32 },
}

/// Files de suites de fichiers : dans une file, une seule étape en vol et les
/// suites dans l'ordre de leur demande ; les files avancent indépendamment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// Segments et manifeste de l'AOF (l'ordre compte).
    Aof,
    /// Fichier d'un BGSAVE (création, renommage final).
    Save,
    /// Suppressions de fichiers que plus rien ne cite.
    Trash,
}

/// À qui rendre une suite finie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsOwner {
    /// Écrivain de l'AOF, par génération.
    Aof(u32),
    /// BGSAVE, par numéro.
    Save(u32),
    Trash,
}

struct FsJob {
    id: u32,
    lane: Lane,
    owner: FsOwner,
    task: FsTask,
    in_flight: bool,
}

/// Une fenêtre de préchargement, sur la pile (pas d'allocation par fenêtre).
type Window<T> = SmallVec<[T; 16]>;

/// Évicteur TTL : une passe d'échantillonnage par seconde, comme l'ancien serveur monoio
/// (l'évicteur à curseur est un chantier d'oxidart à part).
const EVICT_EVERY: Duration = Duration::from_secs(1);
/// Balayage d'inactivité : une tranche de connexions toutes les 100 ms.
const SWEEP_EVERY: Duration = Duration::from_millis(100);
/// Vérification des règles de save automatique (`serverCron` de Redis, hz 10).
const AUTOSAVE_EVERY: Duration = Duration::from_millis(100);
/// fsync `everysec` de l'AOF, nouvel essai après une erreur d'écriture.
const AOF_EVERY: Duration = Duration::from_secs(1);
/// Réplication : reconnexion des cibles perdues, horloge envoyée aux réplicas.
const REPL_EVERY: Duration = Duration::from_secs(1);
const SWEEP_SLOTS: usize = 1024;

/// Échéances du tas de timers (RUNTIME_ARCH.md §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Timer {
    Evict,
    Sweep,
    AutoSave,
    Aof,
    Repl,
    /// Échéance d'un `WAIT`.
    Wait,
}

/// Suite à donner après l'exécution d'une frame.
enum After {
    Continue,
    /// Barrière : plus rien pour cette connexion ce tour-ci.
    Barrier,
    /// QUIT, erreur de protocole : fin propre, file abandonnée.
    Close,
    Background(FlushJob),
    /// Commande longue : la connexion est parquée, le job répondra (§7).
    Park(ConnJob),
    /// BGSAVE / SAVE / LASTSAVE / INFO : dépendent de l'état du serveur (save
    /// en cours, dernière save).
    BgSave,
    BgRewriteAof,
    Save,
    LastSave,
    Info,
    /// Pub/Sub : touchent le registre et l'`out` d'autres connexions (§10).
    Subscribe(SmallVec<[SharedByte; 4]>),
    Unsubscribe(SmallVec<[SharedByte; 4]>),
    Publish { channel: SharedByte, message: SharedByte },
    /// Réplication (DURABILITY_DESIGN.md §10) : état du serveur ; les
    /// octets reçus sont recopiés (la frame est rendue à l'anneau avant).
    Replicate { host: String, port: u16, auth: Option<Vec<u8>> },
    ReplicateStop { host: String, port: u16 },
    ReplicaOfNoOne,
    RxHello(Vec<u8>),
    RxLoadBegin(u64),
    RxLoadData(Vec<u8>),
    RxLoadEnd,
    RxBlock(Vec<u8>),
    RxTick(u64),
    Wait { replicas: u64, timeout_ms: u64 },
}

/// Limites de sortie des abonnés Pub/Sub (`client-output-buffer-limit pubsub`
/// de Redis) : au-delà de la limite dure, ou de la souple pendant 60 s, la
/// connexion est fermée (§10).
const PUBSUB_HARD: usize = 32 << 20;
const PUBSUB_SOFT: usize = 8 << 20;
const PUBSUB_SOFT_FOR: Duration = Duration::from_secs(60);

fn bulk(out: &mut Vec<u8>, b: &[u8]) {
    radixox_lib::shared_frame::encode_bulk(out, b);
}

/// `[kind, channel, count]` (réponses de SUBSCRIBE / UNSUBSCRIBE).
fn encode_sub_reply(out: &mut Vec<u8>, kind: &[u8], channel: Option<&[u8]>, count: usize) {
    radixox_lib::shared_frame::encode_array_len(out, 3);
    bulk(out, kind);
    match channel {
        Some(ch) => bulk(out, ch),
        None => out.extend_from_slice(b"$-1\r\n"),
    }
    out.push(b':');
    out.extend_from_slice(count.to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// Job qui répond à une connexion parquée (RUNTIME_ARCH.md §7, §8).
enum ConnJob {
    Keys(KeysJob),
    Unlink(UnlinkJob),
}

impl ConnJob {
    fn step(&mut self, art: &mut OxidArt, budget: usize) -> Step {
        match self {
            ConnJob::Keys(job) => job.step(art, budget),
            ConnJob::Unlink(job) => job.step(art, budget),
        }
    }

    fn respond(&mut self, out: &mut Vec<u8>) {
        match self {
            ConnJob::Keys(job) => out.extend_from_slice(job.response()),
            ConnJob::Unlink(job) => extend_encode(out, &Frame::Integer(job.count() as i64)),
        }
    }
}

/// Après cette commande, plus rien de sa connexion dans la fenêtre : barrière
/// (§2.1) ou commande qui peut parquer la connexion.
fn stops_window(cmd: &Cmd<'_>) -> bool {
    cmd.is_barrier()
        || matches!(
            cmd,
            Cmd::Keys(_)
                | Cmd::Unlink(_)
                | Cmd::BgSave
                | Cmd::BgRewriteAof
                | Cmd::Save
                | Cmd::Publish { .. }
                | Cmd::Replicate(_)
                | Cmd::ReplicaOf(_)
                | Cmd::RxHello(_)
                | Cmd::RxLoad(_)
                | Cmd::RxBlock(_)
                | Cmd::RxTick(_)
                | Cmd::Wait { .. }
        )
}

pub struct Core {
    pub art: OxidArt,
    cfg: Config,
    ins: Vec<ConnIn>,
    outs: Vec<ConnOut>,
    free: Vec<Slot>,
    bufs: BufPool,
    ops: Vec<Op>,
    /// Connexions avec des frames en attente.
    ready: Vec<Slot>,
    dirty: Vec<Slot>,
    closing: Vec<Slot>,
    /// Recv arrêtés faute de tampon, réarmés quand un tampon revient.
    starved: Vec<Slot>,
    jobs: std::collections::VecDeque<FlushJob>,
    /// Jobs des connexions parquées, avec l'époque de leur parquage.
    conn_jobs: std::collections::VecDeque<(Slot, u32, ConnJob)>,
    turn: u64,
    /// Horloge monotone du tour (`set_now`) ; le TTL suit l'horloge murale
    /// d'oxidart (`art.tick`).
    now: Instant,
    timers: BinaryHeap<Reverse<(Instant, Timer)>>,
    sweep_pos: usize,
    bgsave: Option<BgSave>,
    /// BGSAVE abandonnés (réplica qui recharge un dump) dont une opération de
    /// fichier est en vol : gardés jusqu'à son CQE ; numéro du dernier lancé.
    bgsave_dead: Vec<BgSave>,
    bgsave_seq: u32,
    /// Pub/Sub : canal → abonnés (slot, époque de l'abonnement).
    subs: HashMap<SharedByte, Vec<(Slot, u32)>>,
    /// Part du thread accordée au BGSAVE sous charge.
    save_share: f64,
    /// Dernière réception, toutes connexions : sans réception depuis 1 ms, le
    /// BGSAVE n'est plus plafonné (le thread est libre).
    last_rx: Instant,
    /// Saves automatiques, dernière save, écritures depuis.
    persist: Persistence,
    /// Flux de commandes, si `cfg.journal` ou l'AOF.
    journal: Option<Stream>,
    /// Écriture du flux dans l'AOF.
    aof: Option<AofWriter>,
    /// Écrivains remplacés (réplica qui recharge un dump) dont une opération
    /// est encore en vol : gardés jusqu'à son CQE.
    aof_old: Vec<AofWriter>,
    /// Dernière génération d'écrivain attribuée.
    aof_gen: u32,
    /// Écrivains parqués faute de place dans le flux (§7.6).
    aof_waiting: Vec<Slot>,
    /// Connexions qui ont des réponses retenues jusqu'au fsync (§7.5).
    held: Vec<Slot>,
    /// Identifiant de l'histoire servie aux réplicas : nouveau à chaque
    /// démarrage et à chaque promotion (DURABILITY_DESIGN.md §10.3).
    replid: ReplId,
    /// Position où reprend le flux s'il est créé plus tard (première cible,
    /// après une promotion : la suite de l'histoire suivie).
    resume: Cursor,
    /// Source de réplication : cibles et backlog (créée au premier REPLICATE).
    src: Option<repl::Source>,
    /// Mode suivi : ce serveur est un réplica (§10.1).
    follow: Option<repl::Follow>,
    /// Connexion de la source qui nous pousse (slot, époque).
    upstream: Option<repl::Upstream>,
    /// Synchronisation complète en cours de réception.
    loading: Option<repl::LoadIn>,
    /// Promu par `REPLICAOF NO ONE` : refuse de redevenir réplica (une
    /// ancienne source encore vivante l'écraserait).
    promoted: bool,
    /// Connexions en `WAIT`.
    waiters: Vec<repl::Waiter>,
    /// Suites de fichiers en cours, dans l'ordre de leur demande (§8.6).
    fs_jobs: Vec<FsJob>,
    fs_seq: u32,
}

impl Core {
    pub fn new(cfg: Config, art: OxidArt) -> Self {
        let n = cfg.max_conn as usize;
        let now = Instant::now();
        let mut timers = BinaryHeap::new();
        timers.push(Reverse((now + EVICT_EVERY, Timer::Evict)));
        if cfg.idle_timeout.is_some() {
            timers.push(Reverse((now + SWEEP_EVERY, Timer::Sweep)));
        }
        if !cfg.save_rules.is_empty() {
            timers.push(Reverse((now + AUTOSAVE_EVERY, Timer::AutoSave)));
        }
        Self {
            art,
            ins: (0..n).map(|_| ConnIn::default()).collect(),
            outs: (0..n).map(|_| ConnOut::free()).collect(),
            // LIFO : les slots bas d'abord, réutilisés en premier (table dense).
            free: (0..cfg.max_conn).rev().collect(),
            bufs: BufPool::new(cfg.bufs, cfg.buf_size),
            ops: vec![Op::AcceptMulti],
            ready: Vec::new(),
            dirty: Vec::new(),
            closing: Vec::new(),
            starved: Vec::new(),
            jobs: Default::default(),
            conn_jobs: Default::default(),
            turn: 0,
            now,
            timers,
            sweep_pos: 0,
            bgsave: None,
            bgsave_dead: Vec::new(),
            bgsave_seq: 0,
            subs: HashMap::new(),
            save_share: cpu_share(),
            last_rx: now,
            persist: Persistence::new(cfg.save_rules.clone(), cfg.stop_writes_on_bgsave_error, now),
            journal: cfg.journal.then(Stream::default),
            aof: None,
            aof_old: Vec::new(),
            aof_gen: 0,
            aof_waiting: Vec::new(),
            held: Vec::new(),
            replid: new_replid(),
            resume: Cursor::default(),
            src: None,
            follow: None,
            upstream: None,
            loading: None,
            promoted: false,
            waiters: Vec::new(),
            fs_jobs: Vec::new(),
            fs_seq: 0,
            cfg,
        }
    }

    // ── Temps ────────────────────────────────────────────────────────────

    /// Horloge monotone du tour.
    pub fn set_now(&mut self, now: Instant) {
        self.now = now;
    }

    /// Horloge murale du TTL, une fois par tour ; avec le flux, jamais en
    /// arrière du dernier `now` écrit (DURABILITY_DESIGN.md §4).
    pub fn tick(&mut self) {
        // Réplica : l'horloge est celle des blocs reçus (§10.1).
        if self.follow.is_some() {
            return;
        }
        self.set_wall(crate::tsc::wall_secs());
    }

    /// Pose l'horloge murale du TTL (secondes), rendue monotone avec le flux.
    pub fn set_wall(&mut self, secs: u64) {
        if self.follow.is_some() {
            return;
        }
        let secs = self.journal.as_mut().map_or(secs, |j| j.observe_now(secs));
        self.art.set_now(secs);
    }

    /// Fin de tour : le bloc en cours du flux est fermé (après le travail au
    /// repos, avant le `tick` suivant, §4).
    pub fn end_turn(&mut self) {
        if let Some(j) = &mut self.journal {
            j.end_turn();
        }
        self.aof_pump();
        self.repl_absorb();
        self.repl_pump();
    }

    /// Flux de commandes (blocs fermés), si `cfg.journal`.
    pub fn journal_mut(&mut self) -> Option<&mut Stream> {
        self.journal.as_mut()
    }

    /// Active l'AOF : le flux reprend à `stream` (après la reprise des
    /// fichiers), ses blocs sont écrits par `w`.
    pub fn set_aof(&mut self, stream: Stream, w: AofWriter) {
        self.journal = Some(stream);
        let first = self.aof.is_none() && self.aof_old.is_empty();
        let follow = w.manifest.follow;
        self.replace_aof(w);
        if first {
            self.timers.push(Reverse((self.now + self.aof_every(), Timer::Aof)));
        }
        if let Some(replid) = follow {
            self.resume_follow(replid);
        }
    }

    /// Nouvel écrivain (nouvelle génération) ; l'ancien est gardé tant qu'une
    /// de ses opérations est en vol.
    fn replace_aof(&mut self, mut w: AofWriter) {
        self.aof_gen += 1;
        w.generation = self.aof_gen;
        w.cycle_min = self.cfg.aof_cycle_min;
        w.sync_bytes = self.cfg.aof_sync_bytes;
        if let Some(old) = self.aof.replace(w)
            && old.busy()
        {
            self.aof_old.push(old);
        }
    }

    /// Période du timer de l'AOF : la seconde d'`everysec` (et des essais
    /// après erreur, de la réécriture automatique), ou le délai maximal d'un
    /// cycle `continuous`.
    fn aof_every(&self) -> Duration {
        match self.aof.as_ref().map(|w| w.fsync) {
            Some(crate::exec::aof::Fsync::Continuous) => self.cfg.aof_cycle,
            _ => AOF_EVERY,
        }
    }

    pub fn aof(&self) -> Option<&AofWriter> {
        self.aof.as_ref()
    }

    /// Octets du flux pas encore écrits dans l'AOF.
    pub fn aof_pending(&self) -> u64 {
        match (&self.aof, &self.journal) {
            (Some(w), Some(j)) => w.pending(j),
            _ => 0,
        }
    }

    /// Octets de l'écriture de l'AOF en vol (`Op::JournalWrite`) de l'écrivain `writer`.
    pub fn pending_journal_write(&self, writer: u32) -> &[u8] {
        self.aof.iter().chain(&self.aof_old).find(|w| w.generation == writer).map_or(&[], |w| w.writing())
    }

    /// Arrêt : tout le flux sur disque, fsync compris, d'une traite.
    pub fn flush_journal_sync(&mut self) -> std::io::Result<()> {
        match (&mut self.aof, &mut self.journal) {
            (Some(w), Some(j)) => w.flush_sync(j),
            _ => Ok(()),
        }
    }

    /// Soumet les opérations dues de l'AOF : écriture des blocs fermés,
    /// fsync (`everysec`, rotation).
    fn aof_pump(&mut self) {
        let (Some(w), Some(j)) = (&mut self.aof, &mut self.journal) else { return };
        let writer = w.generation;
        while let Some(op) = w.next_op(j, self.now) {
            self.ops.push(match op {
                AofOp::Write { fd, off, link } => Op::JournalWrite { fd, off, writer, link },
                AofOp::Sync { fd } => Op::JournalSync { fd, writer },
            });
        }
        let task = w.next_fs(self.now);
        let trash = w.take_trash();
        if let Some(task) = task {
            self.fs_push(Lane::Aof, FsOwner::Aof(writer), task);
        }
        self.fs_trash(trash);
    }

    // ── Suites de fichiers hors de la boucle (DURABILITY_DESIGN.md §8.6) ─

    fn fs_push(&mut self, lane: Lane, owner: FsOwner, task: FsTask) {
        self.fs_seq = self.fs_seq.wrapping_add(1);
        self.fs_jobs.push(FsJob { id: self.fs_seq, lane, owner, task, in_flight: false });
        self.fs_kick();
    }

    /// Suppressions (une suite, file `Trash`).
    fn fs_trash(&mut self, paths: Vec<std::path::PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let mut t = FsTask::new();
        for p in &paths {
            t.unlink(p, true);
        }
        self.fs_push(Lane::Trash, FsOwner::Trash, t);
    }

    /// Soumet l'étape courante de la première suite de chaque file qui n'a
    /// rien en vol.
    fn fs_kick(&mut self) {
        for lane in [Lane::Aof, Lane::Save, Lane::Trash] {
            let Some(job) = self.fs_jobs.iter_mut().find(|j| j.lane == lane) else { continue };
            if !job.in_flight && job.task.step().is_some() {
                job.in_flight = true;
                self.ops.push(Op::Fs { id: job.id });
            }
        }
    }

    /// Étape à exécuter pour `Op::Fs { id }` (pointeurs valides jusqu'au CQE).
    pub fn fs_step(&self, id: u32) -> Option<StepRef<'_>> {
        self.fs_jobs.iter().find(|j| j.id == id).and_then(|j| j.task.step())
    }

    /// Des suites de fichiers sont en cours : l'arrêt les attend.
    pub fn fs_pending(&self) -> bool {
        !self.fs_jobs.is_empty()
    }

    /// Des suites de la file `Aof` sont en cours (segments, manifeste).
    fn aof_files_busy(&self) -> bool {
        self.fs_jobs.iter().any(|j| j.lane == Lane::Aof)
    }

    fn on_fs(&mut self, id: u32, res: i32) {
        let Some(i) = self.fs_jobs.iter().position(|j| j.id == id) else {
            debug_assert!(false, "CQE d'une suite de fichiers inconnue");
            return;
        };
        let job = &mut self.fs_jobs[i];
        job.in_flight = false;
        job.task.done(res);
        if !job.task.finished() {
            self.fs_kick();
            return;
        }
        let job = self.fs_jobs.remove(i);
        match job.owner {
            FsOwner::Aof(writer) => {
                if let Some(w) = self.aof.as_mut().filter(|w| w.generation == writer) {
                    w.fs_done(job.task, self.now);
                } else if let Some(k) = self.aof_old.iter().position(|w| w.generation == writer) {
                    // Écrivain remplacé : sa suite est finie, il peut partir.
                    self.aof_old[k].fs_done(job.task, self.now);
                    if !self.aof_old[k].busy() {
                        self.aof_old.swap_remove(k);
                    }
                }
                self.aof_pump();
                self.aof_unpark();
            }
            FsOwner::Save(save) => self.on_save_fs(save, job.task),
            FsOwner::Trash => {
                if let Some(e) = job.task.error() {
                    eprintln!("[aof] suppression d'un ancien fichier : {e}");
                }
            }
        }
        self.fs_kick();
    }

    /// Le durable a avancé : les connexions dont une réponse retenue est
    /// maintenant couverte redeviennent sales (§7.5).
    fn release_held(&mut self) {
        let Some(upto) = self.aof.as_ref().and_then(|w| w.release_upto()) else { return };
        let (outs, dirty) = (&mut self.outs, &mut self.dirty);
        self.held.retain(|&slot| {
            let o = &mut outs[slot as usize];
            // Marques croissantes : les couvertes sont en tête.
            let before = o.holds.len();
            while o.holds.front().is_some_and(|&(_, m)| m <= upto) {
                o.holds.pop_front();
            }
            if o.holds.len() < before && !o.dirty {
                o.dirty = true;
                dirty.push(slot);
            }
            !o.holds.is_empty()
        });
    }

    /// Le retard du flux est repassé sous la moitié du plafond : les écrivains
    /// parqués reprennent (§7.6).
    fn aof_unpark(&mut self) {
        if self.aof_waiting.is_empty() {
            return;
        }
        let (Some(w), Some(j)) = (&self.aof, &self.journal) else { return };
        if w.pending(j) > self.cfg.aof_max_pending / 2 {
            return;
        }
        for slot in std::mem::take(&mut self.aof_waiting) {
            let s = slot as usize;
            let o = &mut self.outs[s];
            if !o.aof_wait {
                continue;
            }
            o.aof_wait = false;
            if self.ins[s].has_work() && !o.ready && matches!(o.mode, Mode::Normal | Mode::Closing) {
                o.ready = true;
                self.ready.push(slot);
            }
        }
    }

    /// Prochaine échéance : borne l'attente dans `enter` (sinon l'horloge et
    /// l'évicteur gèleraient au repos).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.peek().map(|Reverse((t, _))| *t)
    }

    /// Déclenche les échéances passées.
    pub fn fire_timers(&mut self) {
        while let Some(&Reverse((t, timer))) = self.timers.peek()
            && t <= self.now
        {
            self.timers.pop();
            match timer {
                Timer::Evict => {
                    // Pendant une save, l'évicteur est en pause (SAVE_DESIGN.md §2.C).
                    if !self.art.is_saving() {
                        self.art.evict_expired();
                    }
                    self.timers.push(Reverse((self.now + EVICT_EVERY, Timer::Evict)));
                }
                Timer::Sweep => {
                    self.sweep_idle();
                    self.timers.push(Reverse((self.now + SWEEP_EVERY, Timer::Sweep)));
                }
                Timer::Aof => {
                    if let Some(w) = &mut self.aof {
                        w.tick();
                    }
                    self.aof_pump();
                    let due = self.aof.as_ref().is_some_and(|w| {
                        w.rewrite_due(self.cfg.aof_rewrite_percentage, self.cfg.aof_rewrite_min_size)
                    });
                    if due && self.bgsave.is_none() && !self.art.is_saving() {
                        eprintln!("[aof] journal au-delà du seuil de réécriture automatique : BGREWRITEAOF");
                        if let Err(e) = self.start_bgsave() {
                            eprintln!("[aof] échec du démarrage de la réécriture : {e}");
                        }
                    }
                    self.timers.push(Reverse((self.now + self.aof_every(), Timer::Aof)));
                }
                Timer::AutoSave => {
                    if self.bgsave.is_none()
                        && !self.art.is_saving()
                        && let Some(rule) = self.persist.due(self.now)
                    {
                        eprintln!("[autosave] règle « save {} {} » atteinte : BGSAVE", rule.secs, rule.changes);
                        if let Err(e) = self.start_bgsave() {
                            eprintln!("[autosave] échec du démarrage : {e}");
                        }
                    }
                    self.timers.push(Reverse((self.now + AUTOSAVE_EVERY, Timer::AutoSave)));
                }
                Timer::Repl => {
                    self.repl_timer();
                    self.timers.push(Reverse((self.now + REPL_EVERY, Timer::Repl)));
                }
                Timer::Wait => self.wait_wake(),
            }
        }
    }

    /// Ferme, par tranches, les connexions qui n'ont rien envoyé depuis
    /// `idle_timeout` (y compris une frame partielle qui n'avance plus).
    /// Pas un timer par connexion : un balayage, comme le `clientsCron` de Redis.
    fn sweep_idle(&mut self) {
        let Some(limit) = self.cfg.idle_timeout else { return };
        let n = self.outs.len();
        for _ in 0..SWEEP_SLOTS.min(n) {
            let slot = self.sweep_pos as Slot;
            self.sweep_pos = (self.sweep_pos + 1) % n;
            let o = &self.outs[slot as usize];
            if o.mode == Mode::Normal && self.now.saturating_duration_since(o.last_active) > limit {
                self.drain(slot);
            }
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn bufs_mut(&mut self) -> &mut BufPool {
        &mut self.bufs
    }

    /// Opérations à soumettre (dans l'ordre d'émission).
    pub fn take_ops(&mut self, into: &mut Vec<Op>) {
        into.append(&mut self.ops);
    }

    /// Octets à envoyer pour `Op::Send { slot }` (le reste, après un envoi partiel).
    pub fn pending_send(&self, slot: Slot) -> &[u8] {
        let o = &self.outs[slot as usize];
        &o.sending[o.sent..]
    }

    /// Plus rien à faire sans nouvel événement : la boucle peut attendre.
    pub fn idle(&self) -> bool {
        self.quiet() && !self.has_background()
    }

    /// Rien à exécuter, à envoyer ni à soumettre pour les clients ; il peut
    /// rester du travail de fond.
    pub fn quiet(&self) -> bool {
        // Une connexion sale qui attend la fin de son send n'est pas du travail :
        // le CQE réveillera la boucle.
        self.ops.is_empty()
            && self.ready.is_empty()
            && self.dirty.iter().all(|&s| self.outs[s as usize].send_in_flight())
    }

    /// Du travail de fond peut avancer : job de connexion parquée, libération,
    /// tranche de BGSAVE (hors attente du disque), plafond de CPU ou non.
    pub fn has_background(&self) -> bool {
        !self.conn_jobs.is_empty()
            || !self.jobs.is_empty()
            || self.bgsave.as_ref().is_some_and(|b| b.wants_slice())
    }

    pub fn mode(&self, slot: Slot) -> Mode {
        self.outs[slot as usize].mode
    }

    pub fn epoch(&self, slot: Slot) -> u32 {
        self.outs[slot as usize].epoch
    }

    /// Connexions ouvertes.
    pub fn connections(&self) -> usize {
        self.cfg.max_conn as usize - self.free.len()
    }

    // ── Complétions ──────────────────────────────────────────────────────

    pub fn complete(&mut self, c: Completion) {
        match c {
            Completion::Accept { fd, more } => {
                if !more {
                    self.ops.push(Op::AcceptMulti);
                }
                if fd >= 0 {
                    self.accept(fd);
                }
            }
            Completion::Recv { slot, res, bid, buf_more, more } => {
                self.on_recv(slot, res, bid, buf_more, more)
            }
            Completion::Send { slot, res } => self.on_send(slot, res),
            Completion::File { res, save } => self.on_file(res, save),
            Completion::JournalWrite { res, writer } | Completion::JournalSync { res, writer }
                if self.aof.as_ref().is_none_or(|w| w.generation != writer) =>
            {
                // Écrivain remplacé : son opération est finie, il peut partir.
                if let Some(i) = self.aof_old.iter().position(|w| w.generation == writer) {
                    let w = &mut self.aof_old[i];
                    match c {
                        Completion::JournalWrite { .. } => w.write_done(res, self.now),
                        _ => w.sync_done(res),
                    }
                    if !w.busy() {
                        self.aof_old.swap_remove(i);
                    }
                }
            }
            Completion::JournalWrite { res, .. } => {
                if let Some(w) = &mut self.aof {
                    w.write_done(res, self.now);
                }
                self.aof_pump();
                self.aof_unpark();
            }
            Completion::JournalSync { res, .. } => {
                if let Some(w) = &mut self.aof {
                    w.sync_done(res);
                }
                self.release_held();
                self.aof_pump();
            }
            Completion::Connect { slot, res } => self.on_connect(slot, res),
            Completion::Fs { id, res } => self.on_fs(id, res),
        }
    }

    /// Octets de l'écriture en vol (`Op::FileWrite`) du BGSAVE `save`.
    pub fn pending_file_write(&self, save: u32) -> &[u8] {
        self.bgsave.iter().chain(&self.bgsave_dead).find(|b| b.id == save).map_or(&[], |b| b.writing())
    }

    /// Un BGSAVE est en cours (DFS, vidage ou écriture du fichier).
    pub fn bgsave_running(&self) -> bool {
        self.bgsave.is_some()
    }

    /// Abandonne le BGSAVE en cours (barrière fermée, fichier temporaire
    /// supprimé) : arrêt du serveur, réplica qui recharge un dump. Son
    /// opération en vol garde son tampon et son fichier jusqu'au CQE.
    pub fn abort_bgsave(&mut self) {
        if let Some(mut bg) = self.bgsave.take() {
            // Avant `abort`, qui marque la save finie.
            let in_flight = bg.in_flight();
            bg.abort(&mut self.art);
            if in_flight {
                self.bgsave_dead.push(bg);
            }
        }
    }

    fn on_file(&mut self, res: i32, save: u32) {
        if self.bgsave.as_ref().is_none_or(|b| b.id != save) {
            // BGSAVE abandonné : son opération est finie, il peut partir.
            let found = self.bgsave_dead.iter().position(|b| b.id == save);
            debug_assert!(found.is_some(), "CQE de fichier d'un BGSAVE inconnu");
            if let Some(i) = found {
                self.bgsave_dead.swap_remove(i);
            }
            return;
        }
        let bg = self.bgsave.as_mut().unwrap();
        if bg.syncing() {
            bg.sync_done(res);
        } else {
            bg.write_done(res);
            if let Some(op) = bg.resume_write() {
                self.push_file_op(op);
                return;
            }
        }
        self.bgsave_next();
    }

    fn push_file_op(&mut self, op: FileOp) {
        let save = self.bgsave.as_ref().expect("opération de fichier sans BGSAVE").id;
        self.ops.push(match op {
            FileOp::Write { fd, off } => Op::FileWrite { fd, off, save },
            FileOp::Sync { fd } => Op::FileSync { fd, save },
        });
    }

    /// Retour d'une suite de fichiers d'un BGSAVE (création, renommage final).
    fn on_save_fs(&mut self, save: u32, task: FsTask) {
        if let Some(bg) = self.bgsave.as_mut().filter(|b| b.id == save) {
            bg.fs_done(task);
            self.bgsave_next();
        } else if let Some(i) = self.bgsave_dead.iter().position(|b| b.id == save) {
            // BGSAVE abandonné : sa suite est finie, il peut partir.
            self.bgsave_dead[i].fs_done(task);
            if !self.bgsave_dead[i].in_flight() {
                self.bgsave_dead.swap_remove(i);
            }
        }
    }

    /// Soumet l'écriture suivante du BGSAVE s'il y en a une ; termine-le.
    fn bgsave_next(&mut self) {
        let Some(bg) = self.bgsave.as_mut() else { return };
        if let Some(task) = bg.take_fs() {
            let save = bg.id;
            self.fs_push(Lane::Save, FsOwner::Save(save), task);
        }
        let bg = self.bgsave.as_mut().unwrap();
        if !bg.in_flight()
            && let Some(op) = bg.next_file_op()
        {
            self.push_file_op(op);
        }
        let bg = self.bgsave.as_mut().unwrap();
        if bg.in_flight() {
            return;
        }
        if let Some(res) = bg.finished(&mut self.art) {
            if let Err(e) = &res {
                eprintln!("[bgsave] échec : {e}");
            }
            self.persist.bgsave_finished(Instant::now(), res.is_ok());
            let target = bg.target().clone();
            if let Some(w) = &mut self.aof {
                w.rewrite_done(&target, res.is_ok());
            }
            self.bgsave = None;
            self.aof_pump();
            self.repl_bgsave_done(&target, res.is_ok());
        }
    }

    /// Lance un BGSAVE (commande ou règle de save automatique). Avec l'AOF,
    /// c'est une réécriture : coupure du flux à cet instant, nouvelle base
    /// (DURABILITY_DESIGN.md §8).
    fn start_bgsave(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let target = match (&mut self.aof, &mut self.journal) {
            (Some(w), _) if w.rewriting() => return Err("Background append only file rewriting already in progress".into()),
            (Some(w), Some(j)) => w.begin_rewrite(j),
            // Flux sans AOF (réplication) : le dump est coupé au même point,
            // il sert aux synchronisations complètes (§10.3).
            (None, Some(j)) => {
                let cut = j.cut();
                let pos = oxidart::JournalPos { seq: cut.seq, offset: cut.offset, replid: self.replid };
                Target { pos, ..self.dump_target() }
            }
            _ => self.dump_target(),
        };
        self.persist.bgsave_started(now);
        match BgSave::start(&mut self.art, zstd_conf(), target.clone()) {
            Ok(mut bg) => {
                self.bgsave_seq += 1;
                bg.id = self.bgsave_seq;
                self.bgsave = Some(bg);
                // Création du fichier temporaire, hors de la boucle.
                self.bgsave_next();
                Ok(())
            }
            Err(e) => {
                self.persist.bgsave_finished(now, false);
                if let Some(w) = &mut self.aof {
                    w.rewrite_done(&target, false);
                }
                Err(e.to_string())
            }
        }
    }

    /// Dossier des dumps (`Config::dir`, sinon `RADIXOX_DIR`).
    fn dir(&self) -> std::path::PathBuf {
        self.cfg.dir.clone().unwrap_or_else(save_dir)
    }

    /// `dump.rxd` de ce serveur, sans flux.
    fn dump_target(&self) -> Target {
        Target { dir: self.dir(), ..Target::dump() }
    }

    /// Les règles de save automatique sont actives (save à l'arrêt, comme Redis).
    pub fn autosave_on(&self) -> bool {
        self.persist.has_rules()
    }

    /// Une tranche de BGSAVE, sous le plafond de CPU quand des clients sont
    /// actifs (SAVE_DESIGN.md, « Plafond de CPU du BGSAVE »).
    fn bgsave_slice(&mut self) {
        // Plusieurs tranches par tour jusqu'à sa part de CPU (une seule par tour
        // ne lui en donnait que ~6 % avec des tours longs : save deux fois plus
        // longue, plus de copies T0 à libérer à la fin), 250 µs au plus.
        let end = tsc::Deadline::after(Duration::from_micros(250));
        while self.bgsave_slice_due() && !end.passed() {
            self.bgsave.as_mut().unwrap().slice(&mut self.art);
            let bg = self.bgsave.as_ref().unwrap();
            if bg.in_flight() || !bg.wants_slice() {
                break;
            }
            // Le temps passé compte dans `work` : la part se recalcule à `now`.
            self.now = Instant::now();
        }
        self.bgsave_next();
    }

    fn accept(&mut self, fd: i32) {
        if self.free.len() as u32 <= self.cfg.reserved {
            self.ops.push(Op::Reject { fd });
            return;
        }
        let slot = self.free.pop().expect("slot libre");
        self.open_slot(slot);
        let o = &mut self.outs[slot as usize];
        // Le premier recv est chaîné à l'installation (Op::Install).
        o.refs = 1;
        o.recv_armed = true;
        self.ops.push(Op::Install { slot, fd });
    }

    /// Slot pris dans la liste libre : état d'une connexion neuve (nouvelle
    /// époque), sans opération en vol.
    fn open_slot(&mut self, slot: Slot) {
        let o = &mut self.outs[slot as usize];
        debug_assert_eq!(o.mode, Mode::Free);
        o.mode = Mode::Normal;
        o.epoch = o.epoch.wrapping_add(1);
        o.refs = 0;
        o.recv_armed = false;
        o.recv_cancelled = false;
        o.send_cancelled = false;
        o.out.clear();
        o.holds.clear();
        o.sending.clear();
        o.sent = 0;
        o.client = Client::new();
        o.last_active = self.now;
        o.job_pending = false;
        o.aof_wait = false;
        o.repl_wait = false;
        o.write_mark = 0;
        o.channels.clear();
        o.over_soft = None;
        o.dirty = false;
        o.ready = false;
        o.stop_turn = u64::MAX;
        o.link = false;
        o.connecting = false;
        self.ins[slot as usize] = ConnIn::default();
    }

    fn on_recv(&mut self, slot: Slot, res: i32, bid: Option<Bid>, buf_more: bool, more: bool) {
        let s = slot as usize;
        debug_assert_ne!(self.outs[s].mode, Mode::Free, "recv sur un slot libre");
        if self.outs[s].link {
            return self.link_recv(slot, res, bid, buf_more, more);
        }
        if !more {
            let o = &mut self.outs[s];
            o.refs -= 1;
            o.recv_armed = false;
        }
        match res {
            n if n > 0 => {
                let bid = bid.expect("recv avec données sans tampon");
                let off = self.bufs.chunk(bid, n as u32, buf_more);
                self.outs[s].last_active = self.now;
                self.last_rx = self.now;
                if matches!(self.outs[s].mode, Mode::Normal | Mode::Closing) && !self.ins[s].broken {
                    self.push_span(slot, Span { bid, off, len: n as u32 });
                }
                self.try_recycle(bid);
                if !more && self.outs[s].mode == Mode::Normal && !self.ins[s].broken {
                    self.arm_recv(slot);
                }
            }
            0 => self.begin_closing(slot, false),
            e if e == -ENOBUFS => {
                // Un tampon a pu revenir entre l'échec du noyau et ce CQE (morceau
                // sans frame complète, rendu aussitôt) : sinon, on attend le prochain.
                if self.outs[s].mode == Mode::Normal && !self.ins[s].broken {
                    if self.bufs.any_available() {
                        self.arm_recv(slot);
                    } else {
                        self.starved.push(slot);
                    }
                }
            }
            e if e == -ECANCELED => {}
            _ => self.drain(slot),
        }
        self.maybe_close(slot);
    }

    /// Range un morceau reçu dans la file de sa connexion, sans le lire. Un
    /// morceau contigu au précédent (même tampon, à la suite) le prolonge :
    /// une frame à cheval entre les deux n'a alors pas besoin d'être recopiée.
    fn push_span(&mut self, slot: Slot, span: Span) {
        let s = slot as usize;
        let i = &mut self.ins[s];
        if self.outs[s].parked() {
            // Parquée : copié tout de suite, l'anneau n'est jamais retenu (§7).
            i.rx.extend_from_slice(self.bufs.slice(span.bid, span.off, span.len));
            i.rx_check = true;
            if i.rx.len() > self.cfg.query_limit {
                self.drain(slot);
            }
            return;
        }
        match i.spans.back_mut() {
            Some(last) if last.bid == span.bid && last.off + last.len == span.off => last.len += span.len,
            _ => {
                i.spans.push_back(span);
                self.bufs.retain(span.bid, 1);
            }
        }
        let o = &mut self.outs[s];
        if !o.ready {
            o.ready = true;
            self.ready.push(slot);
        }
    }

    fn arm_recv(&mut self, slot: Slot) {
        let o = &mut self.outs[slot as usize];
        o.refs += 1;
        o.recv_armed = true;
        o.recv_cancelled = false;
        self.ops.push(Op::RecvMulti { slot });
    }

    fn try_recycle(&mut self, bid: Bid) {
        if !self.bufs.recyclable(bid) {
            return;
        }
        self.bufs.recycle(bid);
        self.ops.push(Op::ProvideBuf { bid });
        for slot in std::mem::take(&mut self.starved) {
            let o = &self.outs[slot as usize];
            if o.mode == Mode::Normal && !o.recv_armed && !self.ins[slot as usize].broken {
                self.arm_recv(slot);
            }
        }
    }

    fn on_send(&mut self, slot: Slot, res: i32) {
        let s = slot as usize;
        let o = &mut self.outs[s];
        debug_assert!(o.send_in_flight(), "CQE de send sans send en vol");
        o.refs -= 1;
        if res < 0 {
            o.sending.clear();
            o.sent = 0;
            if o.mode != Mode::Draining {
                self.drain(slot);
            }
            self.maybe_close(slot);
            return;
        }
        o.sent += res as usize;
        if o.sent < o.sending.len() && o.mode != Mode::Draining {
            // Envoi partiel (ne devrait pas arriver avec MSG_WAITALL) : le reste.
            o.refs += 1;
            self.ops.push(Op::Send { slot });
            return;
        }
        o.sending.clear();
        o.sent = 0;
        // Rien n'attend : on écrira dans le tampon qui vient d'être envoyé,
        // encore chaud en cache, plutôt que dans l'autre (+1,4 défaut L1 par
        // commande mesuré avec l'alternance).
        if o.out.is_empty() {
            std::mem::swap(&mut o.out, &mut o.sending);
        }
        if !o.out.is_empty() && !o.dirty {
            o.dirty = true;
            self.dirty.push(slot);
        }
        self.maybe_close(slot);
    }

    // ── Lot ──────────────────────────────────────────────────────────────

    /// Une tranche de tour (au plus `batch_cmds` commandes), par fenêtres de
    /// `prefetch` frames : parse (une seule passe, directement dans les morceaux
    /// reçus), préchargement des clés (§8bis), exécution, puis on avance dans les
    /// morceaux.
    ///
    /// On reste sur une connexion tant qu'elle a des frames : son lot finit tôt
    /// et sa réponse peut partir entre deux tranches, comme l'ancien serveur monoio qui traitait
    /// un pipeline d'un bloc (entrelacer 8 par 8 faisait finir tous les lots en
    /// même temps, à la fin du tour : p99 +40 % en p256, mesuré). Chaque fenêtre
    /// garde une place par autre connexion prête (une frame chacune) : un client
    /// sans pipeline n'attend pas la fin d'un gros pipeline.
    ///
    /// Rend `true` si le budget est épuisé alors qu'il reste du travail : la
    /// boucle envoie les réponses prêtes puis rappelle (§2.3).
    pub fn run_batch(&mut self) -> bool {
        self.turn += 1;
        let n = self.ready.len();
        if n == 0 {
            return false;
        }
        // Ordre d'arrivée (liste prête), sans rotation : chaque connexion garde sa
        // place d'un tour à l'autre, comme la file FIFO de l'ancien serveur monoio. Une rotation
        // pouvait faire passer une connexion première puis dernière (presque
        // deux tours d'attente) ; l'équité vient des tranches, qui servent toutes
        // les connexions prêtes à chaque tour.
        let mut pos = 0;
        let width = self.cfg.prefetch.clamp(1, oxidart::PREFETCH_BATCH);
        let mut budget = self.cfg.batch_cmds;
        let mut picked: Window<Slot> = SmallVec::new();
        let mut idle_run = 0;
        while budget > 0 && idle_run < n {
            let first = self.ready[pos % n];
            if !(self.drawable(first) && self.prepare(first)) {
                pos += 1;
                idle_run += 1;
                continue;
            }
            picked.clear();
            picked.push(first);
            let mut k = 1;
            while picked.len() < width && k < n {
                let slot = self.ready[(pos + k) % n];
                k += 1;
                if self.drawable(slot) && self.prepare(slot) {
                    picked.push(slot);
                }
            }
            let done = self.exec_window(&picked, width.min(budget));
            if done == 0 {
                pos += 1;
                idle_run += 1;
                continue;
            }
            idle_run = 0;
            budget = budget.saturating_sub(done);
            if !(self.ins[first as usize].has_work() && self.drawable(first)) {
                pos += 1;
            }
        }
        let more = budget == 0;
        let (ins, outs) = (&self.ins, &mut self.outs);
        self.ready.retain(|&slot| {
            let s = slot as usize;
            let keep = ins[s].has_work()
                && !outs[s].parked()
                && matches!(outs[s].mode, Mode::Normal | Mode::Closing);
            outs[s].ready = keep;
            keep
        });
        more && !self.ready.is_empty()
    }

    /// La connexion peut donner des frames à ce tour-ci.
    fn drawable(&self, slot: Slot) -> bool {
        let o = &self.outs[slot as usize];
        o.stop_turn != self.turn
            && !o.parked()
            && matches!(o.mode, Mode::Normal | Mode::Closing)
            && !(o.send_in_flight() && o.out.len() >= self.cfg.out_backpressure)
    }

    /// Rend la tête de la connexion exécutable. `rx` passe avant les morceaux :
    /// une frame coupée entre deux morceaux y est complétée avec le début des
    /// morceaux suivants, et les octets recopiés pendant un parquage y
    /// attendent. `false` : rien d'exécutable (frame incomplète, en attente).
    fn prepare(&mut self, slot: Slot) -> bool {
        let s = slot as usize;
        let Core { ins, bufs, cfg, .. } = self;
        let i = &mut ins[s];
        if i.proto_err.is_some() || i.rx_ready > 0 {
            return true;
        }
        if i.rx.is_empty() {
            i.rx_check = false;
            return !i.spans.is_empty();
        }
        // Morceau par morceau de 4 Ko au plus : une petite frame coupée ne
        // recopie que ce qu'il lui faut ; l'excédent du dernier morceau copié
        // lui est rendu.
        const PIECE: u32 = 4096;
        let mut freed: Window<Bid> = SmallVec::new();
        let mut last_piece = 0u32;
        loop {
            match frame_len(&i.rx) {
                FrameLen::Done(n) => {
                    let give_back = ((i.rx.len() - n) as u32).min(last_piece);
                    if give_back > 0 {
                        let front = i.spans.front_mut().expect("excédent pris dans un morceau");
                        front.off -= give_back;
                        front.len += give_back;
                        i.rx.truncate(i.rx.len() - give_back as usize);
                    }
                    i.rx_ready = n;
                    break;
                }
                FrameLen::Invalid(msg) => {
                    i.proto_err = Some(msg);
                    i.broken = true;
                    break;
                }
                FrameLen::Incomplete => {
                    // Morceau de tête épuisé : il retourne à l'anneau.
                    while let Some(front) = i.spans.front()
                        && front.len == 0
                    {
                        freed.push(front.bid);
                        i.spans.pop_front();
                    }
                    let Some(front) = i.spans.front_mut() else {
                        break;
                    };
                    let take = front.len.min(PIECE);
                    i.rx.extend_from_slice(bufs.slice(front.bid, front.off, take));
                    front.off += take;
                    front.len -= take;
                    last_piece = take;
                }
            }
        }
        i.rx_check = false;
        let too_big = i.rx.len() > cfg.query_limit;
        let ready = i.rx_ready > 0 || i.proto_err.is_some();
        // Morceaux vidés par la reconstitution : rendus à l'anneau.
        while let Some(front) = i.spans.front()
            && front.len == 0
        {
            freed.push(front.bid);
            i.spans.pop_front();
        }
        for bid in freed {
            self.bufs.release(bid);
            self.try_recycle(bid);
        }
        if too_big {
            self.drain(slot);
            return false;
        }
        ready
    }

    /// Une fenêtre : parse au plus `width` frames des connexions `picked` (dans
    /// cet ordre, frames consécutives d'abord), précharge, exécute ; puis avance
    /// dans les morceaux. Rend le nombre de frames prises.
    fn exec_window(&mut self, picked: &[Slot], width: usize) -> usize {
        // Ce que la fenêtre a pris à chaque connexion.
        struct Plan {
            slot: Slot,
            /// La frame de `rx` en fait partie.
            from_rx: bool,
            /// Octets consommés dans le morceau de tête.
            consumed: u32,
            /// Le morceau de tête finit par une frame incomplète.
            tail: bool,
            err: Option<&'static str>,
            /// Index de la première frame de cette connexion dans `frames`.
            first: usize,
        }
        let mut plans: Window<Plan> = SmallVec::new();
        let mut afters: Window<(Slot, After)> = SmallVec::new();
        // Écrivains arrêtés par le plafond du flux : (connexion, début dans le
        // morceau de tête de la frame arrêtée ; `None` : la frame de `rx`).
        let mut aof_stops: Window<(Slot, Option<u32>)> = SmallVec::new();
        // Réplica avec AOF : la fin d'un `RXLOAD` refait les fichiers de l'AOF
        // (`rebase_aof`) ; elle attend qu'aucune suite de la file `Aof` ne soit
        // en cours (une rotation de l'ancien écrivain écrirait par-dessus).
        let aof_files_busy = self.aof_files_busy();
        {
            let job_budget = self.cfg.job_budget;
            let Core { art, ins, outs, bufs, persist, journal, aof, cfg, held, follow, .. } = self;
            // Réplica : les écritures des clients sont refusées (§10.1).
            let read_only = follow.is_some();
            // `always` : les réponses attendent le fsync qui couvre leur marque.
            let release_upto = aof.as_ref().and_then(|w| w.release_upto());
            // Écritures refusées : BGSAVE en échec, AOF en erreur (§7.7).
            let refuse_writes = if persist.refuse_writes() {
                Some(misconf())
            } else {
                aof.as_ref().and_then(|w| w.error()).map(aof_misconf)
            };
            // Flux trop en retard sur le disque : les écrivains attendent (§7.6).
            let over_cap = match (aof.as_ref(), journal.as_ref()) {
                (Some(w), Some(j)) => w.pending(j) > cfg.aof_max_pending,
                _ => false,
            };
            // (connexion, commande, octets de sa frame, fin de la frame dans le
            // morceau de tête ; `None` : la frame de `rx`)
            let mut frames: Window<(Slot, Cmd<'_>, &[u8], Option<u32>)> = SmallVec::new();
            for (rank, &slot) in picked.iter().enumerate() {
                if frames.len() >= width {
                    break;
                }
                // La première connexion prend la fenêtre moins une place par autre
                // connexion prête ; les autres, une frame chacune.
                let quota = if rank == 0 { width.saturating_sub(picked.len() - 1).max(1) } else { 1 };
                let stop_at = (frames.len() + quota).min(width);
                let i = &ins[slot as usize];
                let mut plan = Plan { slot, from_rx: false, consumed: 0, tail: false, err: None, first: frames.len() };
                let mut barrier = false;
                if i.rx_ready > 0 {
                    let raw = &i.rx[..i.rx_ready];
                    let Parse::Done(cmd, _) = Cmd::parse(raw) else {
                        unreachable!("rx_ready : frame complète");
                    };
                    barrier = stops_window(&cmd);
                    frames.push((slot, cmd, raw, None));
                    plan.from_rx = true;
                }
                // Des octets attendent encore dans `rx` après cette frame : les
                // morceaux viennent après eux, pas dans cette fenêtre.
                if !barrier
                    && i.proto_err.is_none()
                    && i.rx.len() == i.rx_ready
                    && let Some(front) = i.spans.front()
                {
                    let data = bufs.slice(front.bid, front.off, front.len);
                    let mut pos = 0;
                    while frames.len() < stop_at && pos < data.len() {
                        match Cmd::parse(&data[pos..]) {
                            Parse::Done(cmd, n) => {
                                let raw = &data[pos..pos + n];
                                pos += n;
                                let b = stops_window(&cmd);
                                frames.push((slot, cmd, raw, Some(pos as u32)));
                                if b {
                                    break;
                                }
                            }
                            Parse::Incomplete => {
                                plan.tail = true;
                                break;
                            }
                            Parse::Invalid(msg) => {
                                plan.err = Some(msg);
                                break;
                            }
                        }
                    }
                    plan.consumed = pos as u32;
                }
                if plan.from_rx || plan.consumed > 0 || plan.tail || plan.err.is_some() || i.proto_err.is_some() {
                    plans.push(plan);
                }
            }
            if frames.len() > 1 {
                let keys: Window<&[u8]> = frames.iter().filter_map(|(_, c, _, _)| c.main_key()).collect();
                if keys.len() > 1 {
                    art.prefetch_keys(&keys);
                }
            }
            for (slot, cmd, raw, end) in frames {
                if aof_stops.iter().any(|&(s, _)| s == slot) {
                    continue;
                }
                let o = &mut outs[slot as usize];
                // Réplica avec AOF : les blocs reçus attendent aussi le disque.
                if (over_cap && matches!(cmd, Cmd::RxBlock(_)))
                    || (aof_files_busy && matches!(cmd, Cmd::RxLoad(radixox_lib::cmd::RxLoad::End)))
                {
                    aof_stops.push((slot, end.map(|e| e - raw.len() as u32)));
                    continue;
                }
                let weight = cmd.write_weight();
                if weight > 0 && o.client.authed {
                    if read_only {
                        extend_encode(&mut o.out, &Frame::Error("READONLY You can't write against a read only replica.".into()));
                        afters.push((slot, After::Continue));
                        continue;
                    }
                    if let Some(err) = &refuse_writes {
                        extend_encode(&mut o.out, err);
                        afters.push((slot, After::Continue));
                        continue;
                    }
                    if over_cap {
                        // Cette frame et les suivantes de la connexion restent à
                        // lire ; elle reprendra quand le disque aura rattrapé.
                        aof_stops.push((slot, end.map(|e| e - raw.len() as u32)));
                        continue;
                    }
                    // FLUSHDB : toutes les clés changent (le `dirty` de Redis).
                    persist.note_writes(if matches!(cmd, Cmd::FlushDb) { art.len() as u64 } else { weight });
                }
                let Some(stream) = journal.as_mut() else {
                    afters.push((slot, run_cmd(cmd, art, o, job_budget)));
                    continue;
                };
                // Le flux : la frame reçue, décidée avant (la commande est
                // consommée), confirmée par la réponse (DURABILITY_DESIGN.md §3).
                let plan = journal::plan(&cmd);
                let start = o.out.len();
                afters.push((slot, run_cmd(cmd, art, o, job_budget)));
                let mark = stream.end_mark();
                journal::finish(plan, &o.out[start..], raw, art.now, stream);
                if stream.end_mark() != mark {
                    o.write_mark = stream.end_mark();
                }
                // Écriture ou lecture : elle a pu voir du non-durable (§7.5).
                if let Some(durable) = release_upto {
                    let mark = stream.end_mark();
                    if mark > durable {
                        o.holds.push_back((start, mark));
                        if o.holds.len() == 1 {
                            held.push(slot);
                        }
                    }
                }
            }
        }
        let done = afters.len();
        // Écrivains arrêtés : rien de consommé à partir de la frame arrêtée.
        for &(slot, start) in &aof_stops {
            let plan = plans.iter_mut().find(|p| p.slot == slot).expect("plan de la connexion");
            match start {
                None => {
                    plan.from_rx = false;
                    plan.consumed = 0;
                }
                Some(start) => plan.consumed = start,
            }
            plan.tail = false;
            plan.err = None;
        }
        // Avancer dans les morceaux, rendre les tampons vidés.
        for plan in &plans {
            let s = plan.slot as usize;
            let i = &mut self.ins[s];
            if plan.from_rx {
                i.rx.drain(..i.rx_ready);
                i.rx_ready = 0;
                i.rx_check = !i.rx.is_empty();
            }
            let mut freed = None;
            if let Some(front) = i.spans.front_mut() {
                front.off += plan.consumed;
                front.len -= plan.consumed;
                if plan.tail {
                    // Frame coupée en fin de morceau : recopiée, le tampon est rendu.
                    i.rx.extend_from_slice(self.bufs.slice(front.bid, front.off, front.len));
                    front.len = 0;
                }
                if front.len == 0 {
                    freed = Some(front.bid);
                    i.spans.pop_front();
                }
            }
            if let Some(msg) = plan.err {
                i.proto_err = Some(msg);
                i.broken = true;
            }
            if let Some(bid) = freed {
                self.bufs.release(bid);
                self.try_recycle(bid);
            }
        }
        for (slot, after) in afters {
            self.finish_frame(slot, after);
        }
        for &(slot, _) in &aof_stops {
            self.aof_park(slot);
        }
        // Erreur de protocole : après les réponses des frames d'avant.
        for plan in &plans {
            let s = plan.slot as usize;
            if let Some(msg) = self.ins[s].proto_err.take() {
                let o = &mut self.outs[s];
                extend_encode(&mut o.out, &Frame::Error(format!("ERR Protocol error: {msg}")));
                self.finish_frame(plan.slot, After::Close);
            }
            let _ = plan.first;
        }
        done.max(plans.len())
    }

    fn finish_frame(&mut self, slot: Slot, after: After) {
        let s = slot as usize;
        match after {
            After::Continue => {}
            After::Barrier => self.outs[s].stop_turn = self.turn,
            After::Close => self.begin_closing(slot, true),
            After::Background(job) => {
                if !job.is_done() {
                    self.jobs.push_back(job);
                }
            }
            After::Park(job) => self.park(slot, job),
            After::BgSave | After::BgRewriteAof => {
                let rewrite = matches!(after, After::BgRewriteAof);
                let frame = if rewrite && self.aof.is_none() {
                    Frame::Error("ERR AOF is disabled (RADIXOX_APPENDONLY=yes)".into())
                } else if self.bgsave.is_some() || self.art.is_saving() {
                    Frame::Error("ERR Background save already in progress".into())
                } else {
                    match self.start_bgsave() {
                        Ok(()) if rewrite => {
                            Frame::SimpleString("Background append only file rewriting started")
                        }
                        Ok(()) => Frame::SimpleString("Background saving started"),
                        Err(e) => Frame::Error(format!("ERR {e}")),
                    }
                };
                extend_encode(&mut self.outs[s].out, &frame);
            }
            After::Save => {
                let frame = if self.bgsave.is_some() {
                    Frame::Error("ERR Background save already in progress".into())
                } else {
                    let target = self.dump_target();
                    match save_sync_to(&mut self.art, sync_level(), &target) {
                        Ok(st) => {
                            eprintln!(
                                "[save] {:.1} MB ({:.1} MB bruts) en {:.2?} (DFS {:.2?})",
                                st.written as f64 / 1e6,
                                st.raw as f64 / 1e6,
                                st.total,
                                st.walk
                            );
                            self.persist.save_done(Instant::now());
                            resp_ok()
                        }
                        Err(e) => Frame::Error(format!("ERR {e}")),
                    }
                };
                extend_encode(&mut self.outs[s].out, &frame);
            }
            After::LastSave => {
                extend_encode(&mut self.outs[s].out, &Frame::Integer(self.persist.last_save_unix() as i64));
            }
            After::Info => {
                let mut info = String::new();
                crate::exec::info_server(&mut info);
                self.persist.info(self.now, &mut info);
                match (&self.aof, &self.journal) {
                    (Some(w), Some(j)) => w.info(j, &mut info),
                    _ => info.push_str("aof_enabled:0\r\n"),
                }
                info.push_str("\r\n");
                self.repl_info(&mut info);
                info.push_str("\r\n");
                crate::exec::info_stats(&mut info);
                info.push_str("\r\n");
                crate::exec::info_memory(&mut info);
                extend_encode(&mut self.outs[s].out, &Frame::BulkString(SharedByte::from_slice(info.as_bytes())));
            }
            After::Subscribe(channels) => self.subscribe(slot, &channels),
            After::Unsubscribe(channels) => self.unsubscribe(slot, &channels),
            After::Publish { channel, message } => {
                let n = self.publish(&channel, &message);
                extend_encode(&mut self.outs[s].out, &Frame::Integer(n as i64));
            }
            After::Replicate { .. }
            | After::ReplicateStop { .. }
            | After::ReplicaOfNoOne
            | After::RxHello(_)
            | After::RxLoadBegin(_)
            | After::RxLoadData(_)
            | After::RxLoadEnd
            | After::RxBlock(_)
            | After::RxTick(_)
            | After::Wait { .. } => {
                if let Err(e) = self.repl_command(slot, after) {
                    extend_encode(&mut self.outs[s].out, &Frame::Error(e.text));
                    if e.close {
                        self.begin_closing(slot, true);
                    }
                }
            }
        }
        let o = &mut self.outs[s];
        if !o.out.is_empty() {
            if o.out.len() >= self.cfg.flush_early && !o.send_in_flight() && o.holds.is_empty() {
                self.send(slot);
            } else if !o.dirty {
                o.dirty = true;
                self.dirty.push(slot);
            }
        }
    }

    // ── Pub/Sub (§10) ────────────────────────────────────────────────────

    fn subscribe(&mut self, slot: Slot, channels: &[SharedByte]) {
        let s = slot as usize;
        let epoch = self.outs[s].epoch;
        for ch in channels {
            let o = &mut self.outs[s];
            // Déjà abonné : pas compté deux fois (comme Redis).
            if !o.channels.contains(ch) {
                // Externe : vit hors de l'arbre, le GC ne le voit pas (MEMORY_ARCH.md §16.9).
                let ch = ch.clone().into_external();
                o.channels.push(ch.clone());
                self.subs.entry(ch).or_default().push((slot, epoch));
            }
            let o = &mut self.outs[s];
            encode_sub_reply(&mut o.out, b"subscribe", Some(ch), o.channels.len());
        }
    }

    /// Sans argument : tous les canaux. Plus aucun canal : mode normal.
    fn unsubscribe(&mut self, slot: Slot, channels: &[SharedByte]) {
        let s = slot as usize;
        let targets: Vec<SharedByte> =
            if channels.is_empty() { self.outs[s].channels.clone() } else { channels.to_vec() };
        if targets.is_empty() {
            encode_sub_reply(&mut self.outs[s].out, b"unsubscribe", None, 0);
            return;
        }
        for ch in &targets {
            let o = &mut self.outs[s];
            if let Some(pos) = o.channels.iter().position(|c| c == ch) {
                o.channels.swap_remove(pos);
                self.remove_sub(ch, slot);
            }
            let o = &mut self.outs[s];
            encode_sub_reply(&mut o.out, b"unsubscribe", Some(ch), o.channels.len());
        }
    }

    fn remove_sub(&mut self, ch: &SharedByte, slot: Slot) {
        if let Some(list) = self.subs.get_mut(ch) {
            list.retain(|&(sl, _)| sl != slot);
            if list.is_empty() {
                self.subs.remove(ch);
            }
        }
    }

    /// Retire la connexion de tous ses canaux (fermeture).
    fn unsubscribe_all(&mut self, slot: Slot) {
        for ch in std::mem::take(&mut self.outs[slot as usize].channels) {
            self.remove_sub(&ch, slot);
        }
    }

    /// Encode le message une fois, l'ajoute à l'`out` de chaque abonné ; rend
    /// le nombre d'abonnés atteints. Abonné au-delà de ses limites de sortie :
    /// fermé.
    fn publish(&mut self, channel: &SharedByte, message: &SharedByte) -> usize {
        let Some(list) = self.subs.get(channel) else { return 0 };
        let mut msg = Vec::with_capacity(32 + channel.len() + message.len());
        radixox_lib::shared_frame::encode_array_len(&mut msg, 3);
        bulk(&mut msg, b"message");
        bulk(&mut msg, channel);
        bulk(&mut msg, message);
        let mut sent = 0;
        let mut over: Window<Slot> = SmallVec::new();
        for &(slot, epoch) in list {
            let o = &mut self.outs[slot as usize];
            if o.epoch != epoch || !matches!(o.mode, Mode::Normal | Mode::Closing) {
                continue;
            }
            o.out.extend_from_slice(&msg);
            sent += 1;
            if o.out.len() > PUBSUB_HARD {
                over.push(slot);
            } else if o.out.len() > PUBSUB_SOFT {
                let since = *o.over_soft.get_or_insert(self.now);
                if self.now.saturating_duration_since(since) > PUBSUB_SOFT_FOR {
                    over.push(slot);
                }
            } else {
                o.over_soft = None;
            }
            if !o.dirty {
                o.dirty = true;
                self.dirty.push(slot);
            }
        }
        for slot in over {
            eprintln!("[pubsub] abonné {slot} trop lent : limite de sortie dépassée, connexion fermée");
            self.drain(slot);
        }
        sent
    }

    /// Du travail de fond attend une tranche (job de connexion parquée,
    /// libération, BGSAVE) : la boucle doit envoyer les réponses du tour avant
    /// de la faire (RUNTIME_ARCH.md §2).
    pub fn has_jobs(&self) -> bool {
        !self.conn_jobs.is_empty() || !self.jobs.is_empty() || self.bgsave_slice_due()
    }

    /// Une tranche de BGSAVE est à faire à ce tour-ci (pas de tranche à
    /// faire, ou plafond de CPU atteint pendant que des clients sont actifs).
    fn bgsave_slice_due(&self) -> bool {
        let Some(bg) = self.bgsave.as_ref() else { return false };
        if !bg.wants_slice() {
            return false;
        }
        let elapsed = self.now.saturating_duration_since(bg.started()).as_secs_f64();
        // Plancher : le travail fait au repos compte, le tour ne prend sur les
        // clients que ce qui manque pour atteindre la part (RUNTIME_ARCH.md §8.2).
        !(self.save_share < 1.0 && self.clients_active() && bg.work().as_secs_f64() > self.save_share * elapsed)
    }

    /// Une lecture réseau dans la dernière milliseconde (SAVE_DESIGN.md,
    /// « Plafond de CPU du BGSAVE »).
    fn clients_active(&self) -> bool {
        self.now.saturating_duration_since(self.last_rx) < Duration::from_millis(1)
    }

    /// Parque la connexion : son job répondra. Ses morceaux en attente sont
    /// recopiés dans `rx` (rien ne retient l'anneau pendant le parquage).
    fn park(&mut self, slot: Slot, job: ConnJob) {
        let s = slot as usize;
        let o = &mut self.outs[s];
        o.job_pending = true;
        o.epoch = o.epoch.wrapping_add(1);
        let epoch = o.epoch;
        self.spans_to_rx(slot);
        self.conn_jobs.push_back((slot, epoch, job));
    }

    /// Parque un écrivain jusqu'à ce que le flux rattrape le disque (§7.6) :
    /// ses morceaux sont recopiés dans `rx`, l'anneau n'est pas retenu.
    fn aof_park(&mut self, slot: Slot) {
        let s = slot as usize;
        if self.outs[s].aof_wait {
            return;
        }
        self.outs[s].aof_wait = true;
        self.aof_waiting.push(slot);
        self.spans_to_rx(slot);
    }

    /// Connexion qui se parque : ses morceaux en attente sont recopiés dans
    /// `rx`, l'anneau n'est pas retenu pendant l'attente.
    fn spans_to_rx(&mut self, slot: Slot) {
        let s = slot as usize;
        let spans = std::mem::take(&mut self.ins[s].spans);
        for span in spans {
            self.ins[s].rx.extend_from_slice(self.bufs.slice(span.bid, span.off, span.len));
            self.bufs.release(span.bid);
            self.try_recycle(span.bid);
        }
        self.ins[s].rx_check = !self.ins[s].rx.is_empty();
    }

    /// Une tranche de travail de fond : un job de connexion parquée (tour à
    /// tour), puis la libération de FLUSHDB.
    pub fn run_jobs(&mut self) {
        self.conn_job_step();
        self.free_step();
        self.bgsave_slice();
    }

    /// Une étape de travail de fond quand personne n'attend : ni client ici,
    /// ni complétion côté noyau (la boucle a lu `IORING_SQ_TASKRUN` à 0). Sans
    /// plafond de CPU : ce temps n'est pris à personne (RUNTIME_ARCH.md §8.2).
    /// Un client parqué d'abord, puis la libération, puis la save. Rend `true`
    /// s'il reste du travail de fond.
    pub fn idle_step(&mut self) -> bool {
        let _ = self.conn_job_step() || self.free_step() || self.bgsave_idle_slice();
        self.has_background()
    }

    fn conn_job_step(&mut self) -> bool {
        let Some((slot, epoch, mut job)) = self.conn_jobs.pop_front() else { return false };
        if job.step(&mut self.art, self.cfg.job_budget) == Step::More {
            self.conn_jobs.push_back((slot, epoch, job));
        } else {
            self.unpark(slot, epoch, job);
        }
        true
    }

    fn free_step(&mut self) -> bool {
        let Some(mut job) = self.jobs.pop_front() else { return false };
        if job.step(&mut self.art, FREE_BUDGET) == Step::More {
            self.jobs.push_back(job);
        }
        true
    }

    fn bgsave_idle_slice(&mut self) -> bool {
        let Some(bg) = self.bgsave.as_mut().filter(|b| b.wants_slice()) else { return false };
        bg.slice(&mut self.art);
        self.bgsave_next();
        true
    }

    /// Le job a fini : sa réponse part, la connexion reprend (ce qu'elle a reçu
    /// entre-temps est exécuté aux tours suivants). Époque différente : la
    /// connexion n'est plus celle qui attendait, la réponse est jetée.
    fn unpark(&mut self, slot: Slot, epoch: u32, mut job: ConnJob) {
        let s = slot as usize;
        let o = &mut self.outs[s];
        if o.epoch != epoch || !o.job_pending || !matches!(o.mode, Mode::Normal | Mode::Closing) {
            return;
        }
        o.job_pending = false;
        let start = o.out.len();
        job.respond(&mut o.out);
        if let (Some(durable), Some(j)) = (self.aof.as_ref().and_then(|w| w.release_upto()), &self.journal) {
            let mark = j.end_mark();
            if mark > durable {
                o.holds.push_back((start, mark));
                if o.holds.len() == 1 {
                    self.held.push(slot);
                }
            }
        }
        if !o.dirty {
            o.dirty = true;
            self.dirty.push(slot);
        }
        if self.ins[s].has_work() && !o.ready {
            o.ready = true;
            self.ready.push(slot);
        }
    }

    // ── Envoi et fermeture ───────────────────────────────────────────────

    /// Fin de tour : un send par connexion sale, fermetures terminées.
    ///
    /// Une connexion qui a encore des frames en file (pipeline plus long que sa
    /// part du tour) garde ses réponses pour le tour suivant, jusqu'au seuil de
    /// flush anticipé : sinon un pipeline de 256 commandes partait en ~20
    /// petits sends au lieu d'un (débit pipeliné divisé par deux, mesuré).
    pub fn flush(&mut self) {
        for slot in std::mem::take(&mut self.dirty) {
            let s = slot as usize;
            let o = &mut self.outs[s];
            if !matches!(o.mode, Mode::Normal | Mode::Closing) || o.out.is_empty() {
                o.dirty = false;
                continue;
            }
            // Parquée : la suite attend, ce qui est prêt part.
            let more_coming = o.mode == Mode::Normal && self.ins[s].has_work() && !o.parked();
            if o.send_in_flight() || more_coming {
                // Repart au CQE du send, ou au tour suivant.
                self.dirty.push(slot);
                continue;
            }
            o.dirty = false;
            self.send(slot);
        }
        for slot in std::mem::take(&mut self.closing) {
            let (i, o) = (&self.ins[slot as usize], &self.outs[slot as usize]);
            if o.mode != Mode::Closing {
                continue;
            }
            if !i.has_work() && !o.job_pending && !o.repl_wait && o.out.is_empty() && !o.send_in_flight() {
                self.drain(slot);
            } else {
                self.closing.push(slot);
            }
        }
    }

    /// Envoie ce qui peut partir de `out` (tout, sauf à partir d'une réponse
    /// retenue). Rien à envoyer : la connexion attend le fsync (`release_held`).
    fn send(&mut self, slot: Slot) {
        let upto = self.aof.as_ref().and_then(|w| w.release_upto());
        let o = &mut self.outs[slot as usize];
        debug_assert!(!o.send_in_flight());
        let n = o.sendable(upto);
        if n == 0 {
            return;
        }
        if n == o.out.len() {
            std::mem::swap(&mut o.out, &mut o.sending);
        } else {
            o.sending.extend_from_slice(&o.out[..n]);
            o.out.drain(..n);
            for h in o.holds.iter_mut() {
                h.0 -= n;
            }
        }
        o.sent = 0;
        o.send_cancelled = false;
        o.refs += 1;
        self.ops.push(Op::Send { slot });
    }

    /// Fin propre : plus de lecture, ce qui est reçu est exécuté (sauf
    /// `drop_queue` : QUIT, erreur de protocole), les réponses partent.
    fn begin_closing(&mut self, slot: Slot, drop_queue: bool) {
        let s = slot as usize;
        if self.outs[s].mode == Mode::Normal {
            self.outs[s].mode = Mode::Closing;
            self.unsubscribe_all(slot);
            self.ins[s].broken = true;
            self.cancel_recv(slot);
            self.closing.push(slot);
        }
        if drop_queue && self.outs[s].mode == Mode::Closing {
            self.release_queue(slot);
        }
    }

    /// Fin brutale (erreur, reset, limite) : rien n'est plus exécuté ni envoyé.
    fn drain(&mut self, slot: Slot) {
        let s = slot as usize;
        if matches!(self.outs[s].mode, Mode::Draining | Mode::Free) {
            return;
        }
        if self.outs[s].link {
            self.link_lost(slot);
        }
        self.outs[s].mode = Mode::Draining;
        self.unsubscribe_all(slot);
        self.release_queue(slot);
        self.ins[s] = ConnIn { broken: true, ..ConnIn::default() };
        self.outs[s].out.clear();
        self.outs[s].holds.clear();
        if self.outs[s].repl_wait {
            self.outs[s].repl_wait = false;
            self.waiters.retain(|w| w.slot != slot);
        }
        self.cancel_recv(slot);
        let o = &mut self.outs[s];
        if o.send_in_flight() && !o.send_cancelled {
            o.send_cancelled = true;
            self.ops.push(Op::CancelSend { slot });
        }
        self.maybe_close(slot);
    }

    fn cancel_recv(&mut self, slot: Slot) {
        let o = &mut self.outs[slot as usize];
        if o.recv_armed && !o.recv_cancelled {
            o.recv_cancelled = true;
            self.ops.push(Op::CancelRecv { slot });
        }
    }

    fn release_queue(&mut self, slot: Slot) {
        let i = &mut self.ins[slot as usize];
        let spans = std::mem::take(&mut i.spans);
        i.rx.clear();
        i.rx_ready = 0;
        for span in spans {
            self.bufs.release(span.bid);
            self.try_recycle(span.bid);
        }
    }

    /// `Draining` et plus rien en vol : on ferme et le slot redevient libre.
    fn maybe_close(&mut self, slot: Slot) {
        let s = slot as usize;
        let o = &mut self.outs[s];
        if o.mode != Mode::Draining || o.refs != 0 {
            return;
        }
        o.mode = Mode::Free;
        o.out = Vec::new();
        o.holds.clear();
        o.sending = Vec::new();
        o.link = false;
        o.connecting = false;
        self.ins[s] = ConnIn::default();
        self.free.push(slot);
        self.ops.push(Op::Close { slot });
    }

    // ── Invariants (RUNTIME_ARCH.md §1.1) ────────────────────────────────

    /// Vérifie la cohérence de tout l'état ; panique au premier écart.
    pub fn check_invariants(&self) {
        let mut ring_items = vec![0u32; self.bufs.count() as usize];
        let mut in_free = vec![false; self.outs.len()];
        for &slot in &self.free {
            assert!(!in_free[slot as usize], "slot {slot} deux fois libre");
            in_free[slot as usize] = true;
        }
        for (s, (i, o)) in self.ins.iter().zip(&self.outs).enumerate() {
            assert_eq!(o.mode == Mode::Free, in_free[s], "slot {s} : mode {:?}", o.mode);
            if o.mode == Mode::Free {
                assert!(i.spans.is_empty() && o.refs == 0, "slot libre {s} encore utilisé");
                continue;
            }
            let expected = o.recv_armed as u8 + o.send_in_flight() as u8 + o.connecting as u8;
            assert_eq!(o.refs, expected, "slot {s} : refs {} pour recv {} send {}", o.refs, o.recv_armed, o.send_in_flight());
            for span in &i.spans {
                ring_items[span.bid as usize] += 1;
            }
            assert!(i.rx_ready <= i.rx.len());
            for ch in &o.channels {
                assert!(
                    self.subs.get(ch).is_some_and(|l| l.contains(&(s as Slot, o.epoch))),
                    "slot {s} abonné hors registre"
                );
            }
            if o.aof_wait {
                assert!(i.spans.is_empty(), "slot {s} parqué (AOF) : l'anneau ne doit pas être retenu");
                assert!(self.aof_waiting.contains(&(s as Slot)), "slot {s} parqué (AOF) hors liste");
            }
            if o.job_pending {
                assert!(i.spans.is_empty(), "slot {s} parqué : l'anneau ne doit pas être retenu");
                assert!(
                    self.conn_jobs.iter().any(|&(js, je, _)| js as usize == s && je == o.epoch),
                    "slot {s} parqué sans job"
                );
            }
            if o.repl_wait {
                assert!(i.spans.is_empty(), "slot {s} en WAIT : l'anneau ne doit pas être retenu");
                assert!(self.waiters.iter().any(|w| w.slot == s as Slot && w.epoch == o.epoch), "slot {s} en WAIT hors liste");
            }
            if i.has_work() && !o.parked() && matches!(o.mode, Mode::Normal | Mode::Closing) {
                assert!(o.ready && self.ready.contains(&(s as Slot)), "slot {s} a des frames hors de la liste prête");
            }
            assert!(o.sent <= o.sending.len());
            if o.link {
                assert!(!i.has_work() && !o.ready, "lien {s} : rien n'est exécuté");
                assert!(
                    self.src.as_ref().is_some_and(|src| src.links.iter().any(|l| l.slot == Some((s as Slot, o.epoch))))
                        || o.mode == Mode::Draining,
                    "lien {s} hors des cibles"
                );
            }
            assert!(
                o.holds.iter().zip(o.holds.iter().skip(1)).all(|(a, b)| a.0 <= b.0 && a.1 <= b.1)
                    && o.holds.back().is_none_or(|h| h.0 <= o.out.len()),
                "slot {s} : réponses retenues désordonnées"
            );
            if !o.holds.is_empty() {
                assert!(self.held.contains(&(s as Slot)), "slot {s} : réponses retenues hors liste");
            }
        }
        for bid in 0..self.bufs.count() {
            assert_eq!(self.bufs.refs(bid), ring_items[bid as usize], "tampon {bid} : refs");
        }
    }
}

/// Écriture refusée après un BGSAVE en échec (texte de Redis / Valkey).
fn misconf() -> Frame {
    Frame::Error(
        "MISCONF RadixOx is configured to save snapshots, but it's currently unable to persist to disk. \
         Commands that may modify the data set are disabled, because this instance is configured to report \
         errors during writes if snapshotting fails (stop-writes-on-bgsave-error option). Please check the \
         RadixOx logs for details about the error."
            .into(),
    )
}

/// Écriture refusée après une erreur de l'AOF (texte de Valkey).
fn aof_misconf(err: &str) -> Frame {
    Frame::Error(format!("MISCONF Errors writing to the AOF file: {err}"))
}

/// Exécute une commande d'une connexion.
fn run_cmd(cmd: Cmd<'_>, art: &mut OxidArt, o: &mut ConnOut, job_budget: usize) -> After {
    // Mode abonné : seules les commandes Pub/Sub, PING et QUIT passent.
    if !o.channels.is_empty() && o.client.authed {
        match cmd {
            Cmd::Ping(msg) => {
                // RESP2 en mode abonné : [pong, message], comme Redis.
                radixox_lib::shared_frame::encode_array_len(&mut o.out, 2);
                bulk(&mut o.out, b"pong");
                bulk(&mut o.out, msg.unwrap_or(b""));
                return After::Continue;
            }
            ref c if !c.allowed_in_pubsub() => {
                extend_encode(
                    &mut o.out,
                    &Frame::Error("ERR only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT allowed in this context".into()),
                );
                return After::Continue;
            }
            _ => {}
        }
    }
    let barrier = cmd.is_barrier();
    match execute_into(cmd, art, &mut o.client, &mut o.out) {
        Executed::Done if barrier => After::Barrier,
        Executed::Done => After::Continue,
        Executed::Runtime(cmd) => runtime(cmd, art, o, job_budget),
    }
}

/// Commandes qui dépendent du runtime. KEYS et UNLINK s'exécutent encore d'une
/// traite (parquage + jobs : étape 4) ; Pub/Sub et saves : étapes 4–5.
fn runtime(cmd: Cmd<'_>, art: &mut OxidArt, o: &mut ConnOut, job_budget: usize) -> After {
    match cmd {
        Cmd::Quit => {
            extend_encode(&mut o.out, &resp_ok());
            After::Close
        }
        // Une première tranche tout de suite : un petit KEYS ou un UNLINK de clés
        // exactes répond sans parquer la connexion.
        Cmd::Keys(pattern) => {
            let mut job = ConnJob::Keys(KeysJob::new(pattern));
            if job.step(art, job_budget) == Step::Done {
                job.respond(&mut o.out);
                return After::Continue;
            }
            After::Park(job)
        }
        Cmd::Unlink(keys) => {
            let mut job = ConnJob::Unlink(UnlinkJob::start(art, &keys));
            if job.step(art, job_budget) == Step::Done {
                job.respond(&mut o.out);
                return After::Continue;
            }
            After::Park(job)
        }
        Cmd::FlushDb => {
            let job = FlushJob::start(art);
            extend_encode(&mut o.out, &resp_ok());
            After::Background(job)
        }
        // Dépendent d'une save en cours : traitées par le cœur (`finish_frame`).
        Cmd::BgSave => After::BgSave,
        Cmd::BgRewriteAof => After::BgRewriteAof,
        Cmd::Save => After::Save,
        Cmd::LastSave => After::LastSave,
        Cmd::Info => After::Info,
        Cmd::Subscribe(ch) => After::Subscribe(ch.into_iter().collect()),
        Cmd::Unsubscribe(ch) => After::Unsubscribe(ch.into_iter().collect()),
        Cmd::Publish { channel, message } => After::Publish { channel, message },
        Cmd::DebugDigest(at) => {
            let d = art.content_digest(at.unwrap_or(art.now)).value();
            bulk(&mut o.out, format!("{d:08x}").as_bytes());
            After::Continue
        }
        Cmd::Replicate(radixox_lib::cmd::Replicate::Start { host, port, auth }) => {
            After::Replicate { host: String::from_utf8_lossy(host).into_owned(), port, auth: auth.map(<[u8]>::to_vec) }
        }
        Cmd::Replicate(radixox_lib::cmd::Replicate::Stop { host, port }) => {
            After::ReplicateStop { host: String::from_utf8_lossy(host).into_owned(), port }
        }
        Cmd::ReplicaOf(None) => After::ReplicaOfNoOne,
        Cmd::ReplicaOf(Some(_)) => {
            extend_encode(
                &mut o.out,
                &Frame::Error(
                    "ERR RadixOx replication is pushed by the primary: run REPLICATE <host> <port> on the primary"
                        .into(),
                ),
            );
            After::Continue
        }
        Cmd::RxHello(id) => After::RxHello(id.to_vec()),
        Cmd::RxLoad(radixox_lib::cmd::RxLoad::Begin(len)) => After::RxLoadBegin(len),
        Cmd::RxLoad(radixox_lib::cmd::RxLoad::Data(d)) => After::RxLoadData(d.to_vec()),
        Cmd::RxLoad(radixox_lib::cmd::RxLoad::End) => After::RxLoadEnd,
        Cmd::RxBlock(b) => After::RxBlock(b.to_vec()),
        Cmd::RxTick(now) => After::RxTick(now),
        Cmd::Wait { replicas, timeout_ms } => After::Wait { replicas, timeout_ms },
        _ => unreachable!("commande sans dépendance au runtime"),
    }
}
