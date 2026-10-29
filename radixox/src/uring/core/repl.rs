//! Réplication dans le cœur (DURABILITY_DESIGN.md §10).
//!
//! **Source** : chaque cible (`REPLICATE host port`) est un lien sortant, une
//! connexion du cœur dont les réponses sont lues ici, jamais exécutées. Les
//! blocs fermés du flux (`Stream::tap`) vont au backlog et aux liens en ligne
//! (`RXBLOCK`, les octets du disque). Une cible qui ne peut pas reprendre dans
//! le backlog reçoit un dump (`RXLOAD`, pris par un BGSAVE coupé dans le flux) ;
//! les blocs d'après la coupure attendent dans la file du lien.
//!
//! **Cible** : `RXHELLO`, `RXLOAD`, `RXBLOCK`, `RXTICK`, d'une connexion
//! authentifiée. Le mode suivi ([`Follow`]) prend l'horloge des blocs
//! (`Core::tick`) et refuse les écritures des clients (`exec_window`).

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write as _;
use std::net::ToSocketAddrs;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use core::time::Duration;
use radixox_lib::sys::Instant;

use radixox_lib::shared_frame::{SharedFrame as Frame, encode_array_len, encode_bulk, extend_encode};

use crate::exec::aof::{self, AofWriter, Manifest, ReplId, Resume, hex, new_replid, unhex};
use crate::exec::jobs::{FlushJob, Step};
use crate::exec::journal::{self, Blk, Cursor, Stream};
use crate::exec::persist::Target;
use crate::exec::resp_ok;

use super::super::conn::{ConnOut, Mode};
use super::{After, Core, Op, REPL_EVERY, Slot, Timer};
use std::cmp::Reverse;

/// Morceau de dump par `RXLOAD DATA`.
const LOAD_CHUNK: u64 = 1 << 20;
/// Dump en attente d'envoi au plus, par lien : la lecture du fichier suit
/// l'envoi.
const LOAD_WINDOW: usize = 4 << 20;
/// Nouvel essai après une coupure ou un refus.
const RETRY: Duration = Duration::from_secs(1);
/// Dump reçu par une cible, avant son chargement (dans `RADIXOX_DIR`).
const LOAD_TMP: &str = "replica-sync.rxdb.tmp";

// ── Source ──────────────────────────────────────────────────────────────────

pub(in super::super) enum State {
    /// Pas de connexion : nouvel essai à `retry_at`.
    Down { retry_at: Instant },
    Connecting,
    /// `RXHELLO` envoyé, réponse attendue.
    Hello,
    /// Synchronisation complète : attend un BGSAVE (`Link::base` une fois lancé).
    WaitSave,
    /// Envoi du dump, de `off` à `len`.
    Loading { file: File, off: u64, len: u64 },
    Online,
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::Down { .. } => "down",
            State::Connecting => "connecting",
            State::Hello => "handshake",
            State::WaitSave | State::Loading { .. } => "sync",
            State::Online => "online",
        }
    }
}

/// Réponse attendue de la cible, dans l'ordre des commandes envoyées.
#[derive(Debug)]
enum Expect {
    Auth,
    Hello,
    Load,
    /// Fin du dump : la cible est à la coupure `seq`, offset `end`.
    LoadEnd { seq: u64, end: u64 },
    /// `:seq` ; `end` : offset du flux après ce bloc.
    Block { seq: u64, end: u64 },
    Tick,
}

#[derive(Debug)]
enum Reply<'a> {
    Ok,
    Err(&'a [u8]),
    Int(i64),
}

pub(in super::super) struct Link {
    pub host: String,
    pub port: u16,
    auth: Option<Vec<u8>>,
    pub state: State,
    /// Connexion en cours : (slot, époque).
    pub slot: Option<(Slot, u32)>,
    expect: VecDeque<Expect>,
    /// Synchronisation complète : coupure du dump envoyé (`seq`, offset du
    /// flux après elle) ; les blocs d'après attendent dans `queue` jusqu'à la
    /// fin du dump.
    base: Option<(u64, u64)>,
    queue: VecDeque<Blk>,
    queue_bytes: u64,
    /// Prochain bloc à envoyer (en ligne).
    next_seq: u64,
    /// Dernier bloc acquitté, offset du flux après lui.
    acked_seq: u64,
    acked_offset: u64,
    /// Raison de la prochaine coupure (posée avant `drain`).
    why: Option<String>,
    /// Dernière erreur journalisée : un essai qui échoue pareil toutes les
    /// secondes ne remplit pas les logs.
    last_err: Option<String>,
}

impl Link {
    fn new(host: String, port: u16, auth: Option<Vec<u8>>, now: Instant) -> Self {
        Self {
            host,
            port,
            auth,
            state: State::Down { retry_at: now },
            slot: None,
            expect: VecDeque::new(),
            base: None,
            queue: VecDeque::new(),
            queue_bytes: 0,
            next_seq: 0,
            acked_seq: 0,
            acked_offset: 0,
            why: None,
            last_err: None,
        }
    }

    /// Ajoute `RXBLOCK <b>` à `out` (le bloc doit être le suivant).
    fn push_block(&mut self, o: &mut ConnOut, b: &[u8]) {
        debug_assert_eq!(blk_seq(b), self.next_seq);
        o.out.extend_from_slice(b"*2\r\n$7\r\nRXBLOCK\r\n");
        encode_bulk(&mut o.out, b);
        self.expect.push_back(Expect::Block { seq: self.next_seq, end: blk_end(b) });
        self.next_seq += 1;
    }

    /// Octets qui attendent de partir vers la cible.
    fn lag_bytes(&self, o: &ConnOut) -> u64 {
        (o.out.len() + o.sending.len()) as u64 + self.queue_bytes
    }
}

/// Source de réplication : cibles et backlog des derniers blocs.
pub(in super::super) struct Source {
    pub links: Vec<Link>,
    backlog: VecDeque<Blk>,
    backlog_bytes: u64,
    /// Synchronisations complètes / reprises partielles (`sync_full`,
    /// `sync_partial_ok` de Valkey).
    full_syncs: u64,
    partial_syncs: u64,
}

impl Source {
    /// Octets tenus par le backlog (`maxmemory` : déjà au registre).
    pub fn backlog_bytes(&self) -> u64 {
        self.backlog_bytes
    }
}

// ── Cible ───────────────────────────────────────────────────────────────────

/// Mode suivi : l'histoire suivie et la position du dernier bloc appliqué.
pub(in super::super) struct Follow {
    pub replid: ReplId,
    pub cur: Cursor,
}

/// La connexion de la source qui nous pousse.
#[derive(Clone, Copy)]
pub(in super::super) struct Upstream {
    pub slot: Slot,
    pub epoch: u32,
    /// L'histoire annoncée par son `RXHELLO` (adoptée au `RXLOAD END`).
    replid: ReplId,
}

/// Connexion en `WAIT` : réveillée quand `replicas` cibles ont acquitté
/// `mark`, ou à `deadline`.
pub(in super::super) struct Waiter {
    pub slot: Slot,
    pub epoch: u32,
    mark: u64,
    replicas: u64,
    deadline: Option<Instant>,
}

/// Dump d'une synchronisation complète en cours de réception.
pub(in super::super) struct LoadIn {
    file: File,
    path: PathBuf,
    len: u64,
    got: u64,
}

/// Erreur d'une commande de réplication : texte de la réponse, et fermeture
/// de la connexion (la source repassera par `RXHELLO`).
pub(in super::super) struct ReplError {
    pub text: String,
    pub close: bool,
}

fn refuse(text: impl Into<String>) -> ReplError {
    ReplError { text: text.into(), close: false }
}

fn fatal(text: impl Into<String>) -> ReplError {
    ReplError { text: text.into(), close: true }
}

fn blk_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn blk_seq(b: &[u8]) -> u64 {
    blk_u64(b, 16)
}

fn blk_offset(b: &[u8]) -> u64 {
    blk_u64(b, 24)
}

fn blk_end(b: &[u8]) -> u64 {
    blk_offset(b) + b.len() as u64
}

/// Commande RESP (tableau de bulk strings) à la suite de `out`.
fn command(out: &mut Vec<u8>, args: &[&[u8]]) {
    encode_array_len(out, args.len());
    for a in args {
        encode_bulk(out, a);
    }
}

fn mark_dirty(o: &mut ConnOut, dirty: &mut Vec<Slot>, slot: Slot) {
    if !o.dirty && !o.out.is_empty() {
        o.dirty = true;
        dirty.push(slot);
    }
}

impl Core {
    // ── Source : cibles ──────────────────────────────────────────────────

    /// Première cible : le flux (créé s'il n'existe pas, sans AOF : rien n'est
    /// gardé hors du backlog) passe ses blocs à la réplication.
    fn ensure_source(&mut self) {
        if self.src.is_some() {
            return;
        }
        // Réplica qui chaîne : le flux suit la source (blocs reçus, retransmis).
        let (resume, follow) = match &self.follow {
            Some(f) => (f.cur, true),
            None => (self.resume, false),
        };
        let j = self.journal.get_or_insert_with(|| {
            let mut s = Stream::new(resume);
            s.keep = false;
            s.follow = follow;
            s
        });
        j.observe_now(self.art.now);
        j.tap = Some(Vec::new());
        self.src = Some(Source { links: Vec::new(), backlog: VecDeque::new(), backlog_bytes: 0, full_syncs: 0, partial_syncs: 0 });
        self.timers.push(Reverse((self.now + REPL_EVERY, Timer::Repl)));
    }

    /// `REPLICATE host port [AUTH pass]` (et `RADIXOX_REPLICATE`) : ajoute une
    /// cible, connectée tout de suite puis à chaque coupure.
    pub fn replicate(&mut self, host: String, port: u16, auth: Option<Vec<u8>>) -> Result<(), String> {
        self.ensure_source();
        let now = self.now;
        let src = self.src.as_mut().unwrap();
        if let Some(l) = src.links.iter_mut().find(|l| l.host == host && l.port == port) {
            l.auth = auth;
            return Ok(());
        }
        eprintln!("[repl] nouvelle cible {host}:{port}");
        src.links.push(Link::new(host, port, auth, now));
        let i = src.links.len() - 1;
        self.link_connect(i);
        Ok(())
    }

    fn replicate_stop(&mut self, host: &str, port: u16) -> Result<(), ReplError> {
        let src = self.src.as_mut().ok_or_else(|| refuse("ERR no such replica"))?;
        let i = src.links.iter().position(|l| l.host == host && l.port == port).ok_or_else(|| refuse("ERR no such replica"))?;
        let l = src.links.remove(i);
        eprintln!("[repl] cible {host}:{port} retirée");
        if let Some((slot, _)) = l.slot {
            self.drain(slot);
        }
        Ok(())
    }

    fn link_of(&self, slot: Slot) -> Option<usize> {
        let epoch = self.outs[slot as usize].epoch;
        self.src.as_ref()?.links.iter().position(|l| l.slot == Some((slot, epoch)))
    }

    /// Ouvre la connexion de la cible `i` (résolution synchrone : une adresse
    /// IP en pratique, résolue à chaque essai).
    fn link_connect(&mut self, i: usize) {
        let l = &self.src.as_ref().unwrap().links[i];
        let addr = match (l.host.as_str(), l.port).to_socket_addrs().map(|mut a| a.next()) {
            Ok(Some(a)) => a,
            Ok(None) => return self.link_failed(i, "adresse introuvable".into()),
            Err(e) => return self.link_failed(i, format!("résolution : {e}")),
        };
        // La réserve de slots sert aussi : un lien n'attend pas les clients.
        let Some(slot) = self.free.pop() else { return self.link_failed(i, "plus de slot libre".into()) };
        self.open_slot(slot);
        let o = &mut self.outs[slot as usize];
        o.link = true;
        o.connecting = true;
        o.refs = 1;
        let epoch = o.epoch;
        let l = &mut self.src.as_mut().unwrap().links[i];
        l.slot = Some((slot, epoch));
        l.state = State::Connecting;
        self.ops.push(Op::Connect { slot, addr });
    }

    /// Essai impossible (pas de connexion ouverte) : nouvel essai plus tard.
    fn link_failed(&mut self, i: usize, why: String) {
        let now = self.now;
        let l = &mut self.src.as_mut().unwrap().links[i];
        if l.last_err.as_deref() != Some(why.as_str()) {
            eprintln!("[repl] {}:{} : {why} ; nouvel essai toutes les secondes", l.host, l.port);
        }
        l.last_err = Some(why);
        l.state = State::Down { retry_at: now + RETRY };
    }

    /// La connexion d'un lien se ferme (`drain`) : la cible repart de zéro
    /// (`RXHELLO`) au prochain essai.
    pub(super) fn link_lost(&mut self, slot: Slot) {
        let Some(i) = self.link_of(slot) else { return };
        let now = self.now;
        let l = &mut self.src.as_mut().unwrap().links[i];
        let why = l.why.take().unwrap_or_else(|| "connexion perdue".into());
        if l.last_err.as_deref() != Some(why.as_str()) {
            eprintln!("[repl] {}:{} : {why} ; nouvel essai dans 1 s", l.host, l.port);
        }
        l.last_err = Some(why);
        l.slot = None;
        l.state = State::Down { retry_at: now + RETRY };
        l.expect.clear();
        l.queue.clear();
        l.queue_bytes = 0;
        l.base = None;
    }

    /// Coupe le lien `i` avec une raison.
    fn link_drop(&mut self, i: usize, why: String) {
        let l = &mut self.src.as_mut().unwrap().links[i];
        l.why = Some(why);
        if let Some((slot, _)) = l.slot {
            self.drain(slot);
        }
    }

    /// CQE du `connect` : `AUTH` si besoin, puis `RXHELLO <replid>`.
    pub(super) fn on_connect(&mut self, slot: Slot, res: i32) {
        let s = slot as usize;
        let o = &mut self.outs[s];
        debug_assert!(o.connecting, "connect sans connexion en vol");
        o.connecting = false;
        o.refs -= 1;
        let i = self.link_of(slot);
        if res < 0 || i.is_none() || self.outs[s].mode != Mode::Normal {
            if let Some(i) = i {
                self.src.as_mut().unwrap().links[i].why = Some(format!("connexion : {}", std::io::Error::from_raw_os_error(-res)));
            }
            self.drain(slot);
            self.maybe_close(slot);
            return;
        }
        let i = i.unwrap();
        self.arm_recv(slot);
        // Réplica qui chaîne : l'histoire suivie, retransmise telle quelle (§10.7).
        let replid = hex(self.follow.as_ref().map_or(&self.replid, |f| &f.replid));
        let Core { src, outs, dirty, .. } = self;
        let l = &mut src.as_mut().unwrap().links[i];
        let o = &mut outs[s];
        if let Some(pass) = &l.auth {
            command(&mut o.out, &[b"AUTH", pass]);
            l.expect.push_back(Expect::Auth);
        }
        command(&mut o.out, &[b"RXHELLO", replid.as_bytes()]);
        l.expect.push_back(Expect::Hello);
        l.state = State::Hello;
        mark_dirty(o, dirty, slot);
    }

    /// Réception sur un lien : des réponses de la cible, recopiées dans `rx`
    /// (petites), le tampon rendu tout de suite.
    pub(super) fn link_recv(&mut self, slot: Slot, res: i32, bid: Option<super::Bid>, buf_more: bool, more: bool) {
        let s = slot as usize;
        if !more {
            let o = &mut self.outs[s];
            o.refs -= 1;
            o.recv_armed = false;
        }
        match res {
            n if n > 0 => {
                let bid = bid.expect("recv avec données sans tampon");
                let off = self.bufs.chunk(bid, n as u32, buf_more);
                let live = self.outs[s].mode == Mode::Normal;
                if live {
                    self.ins[s].rx.extend_from_slice(self.bufs.slice(bid, off, n as u32));
                    self.outs[s].last_active = self.now;
                }
                self.try_recycle(bid);
                if live {
                    if !more {
                        self.arm_recv(slot);
                    }
                    self.link_replies(slot);
                }
            }
            0 => {
                if let Some(i) = self.link_of(slot) {
                    self.src.as_mut().unwrap().links[i].why = Some("connexion fermée par la cible".into());
                }
                self.drain(slot);
            }
            e if e == -super::ENOBUFS => {
                if self.outs[s].mode == Mode::Normal {
                    if self.bufs.any_available() {
                        self.arm_recv(slot);
                    } else {
                        self.starved.push(slot);
                    }
                }
            }
            e if e == -super::ECANCELED => {}
            e => {
                if let Some(i) = self.link_of(slot) {
                    self.src.as_mut().unwrap().links[i].why =
                        Some(format!("réception : {}", std::io::Error::from_raw_os_error(-e)));
                }
                self.drain(slot);
            }
        }
        self.maybe_close(slot);
    }

    /// Traite les réponses complètes reçues sur le lien.
    fn link_replies(&mut self, slot: Slot) {
        let s = slot as usize;
        let data = std::mem::take(&mut self.ins[s].rx);
        let mut pos = 0;
        let mut res = Ok(());
        while let Some(n) = data[pos..].windows(2).position(|w| w == b"\r\n") {
            let line = &data[pos..pos + n];
            pos += n + 2;
            res = self.link_reply(slot, line);
            if res.is_err() || self.outs[s].mode != Mode::Normal {
                break;
            }
        }
        // Une ligne sans fin démesurée : la cible ne parle pas ce protocole.
        if res.is_ok() && data.len() - pos > 64 << 10 {
            res = Err("réponse illisible".into());
        }
        if self.outs[s].mode == Mode::Normal {
            self.ins[s].rx = data[pos..].to_vec();
        }
        if let Err(why) = res
            && let Some(i) = self.link_of(slot)
        {
            self.link_drop(i, why);
        }
        self.wait_wake();
    }

    fn link_reply(&mut self, slot: Slot, line: &[u8]) -> Result<(), String> {
        let i = self.link_of(slot).ok_or("lien inconnu")?;
        let reply = match line.split_first() {
            Some((b'+', _)) => Reply::Ok,
            Some((b'-', e)) => Reply::Err(e),
            Some((b':', n)) => Reply::Int(
                std::str::from_utf8(n).ok().and_then(|n| n.parse().ok()).ok_or_else(|| format!("entier illisible {:?}", n.escape_ascii().to_string()))?,
            ),
            _ => return Err(format!("réponse inattendue {:?}", line.escape_ascii().to_string())),
        };
        let l = &mut self.src.as_mut().unwrap().links[i];
        let exp = l.expect.pop_front().ok_or("réponse sans commande")?;
        match (exp, reply) {
            (_, Reply::Err(e)) => Err(format!("refusé : {}", String::from_utf8_lossy(e))),
            (Expect::Auth | Expect::Load | Expect::Tick, Reply::Ok) => Ok(()),
            (Expect::LoadEnd { seq, end }, Reply::Ok) => {
                eprintln!("[repl] {}:{} : dump chargé par la cible", l.host, l.port);
                l.acked_seq = seq;
                l.acked_offset = end;
                Ok(())
            }
            (Expect::Block { seq, end }, Reply::Int(n)) if n >= 0 && n as u64 == seq => {
                l.acked_seq = seq;
                l.acked_offset = end;
                Ok(())
            }
            (Expect::Hello, Reply::Int(s)) => {
                self.link_hello(i, s);
                Ok(())
            }
            (e, r) => Err(format!("réponse {r:?} à {e:?}")),
        }
    }

    /// Réponse à `RXHELLO` : `s` = dernier bloc de notre histoire que la cible
    /// a appliqué (`-1` : autre histoire). Dans le backlog : reprise partielle ;
    /// sinon, synchronisation complète.
    fn link_hello(&mut self, i: usize, s: i64) {
        self.repl_absorb();
        let (last, offset) = self.journal.as_ref().map_or((0, 0), |j| (j.cursor().seq, j.cursor().offset));
        let Core { src, outs, dirty, .. } = self;
        let src = src.as_mut().unwrap();
        let first = src.backlog.front().map(|b| blk_seq(b));
        let l = &mut src.links[i];
        let Some((slot, _)) = l.slot else { return };
        let partial = u64::try_from(s).is_ok_and(|s| s == last || (first.is_some_and(|f| f <= s + 1) && s < last));
        if !partial {
            eprintln!(
                "[repl] {}:{} : synchronisation complète ({})",
                l.host,
                l.port,
                if s < 0 { "autre histoire".to_string() } else { format!("bloc {s} hors du backlog, dernier {last}") }
            );
            l.state = State::WaitSave;
            l.base = None;
            src.full_syncs += 1;
            self.repl_start_sync();
            return;
        }
        let s = s as u64;
        src.partial_syncs += 1;
        let l = &mut src.links[i];
        let o = &mut outs[slot as usize];
        l.next_seq = s + 1;
        l.acked_seq = s;
        l.acked_offset = offset;
        let mut n = 0;
        for b in src.backlog.iter().filter(|b| blk_seq(b) > s) {
            if n == 0 {
                l.acked_offset = blk_offset(b);
            }
            l.push_block(o, b);
            n += 1;
        }
        l.state = State::Online;
        if l.last_err.take().is_some() || n > 0 || s > 0 {
            eprintln!("[repl] {}:{} : en ligne, reprise après le bloc {s} ({n} blocs du backlog)", l.host, l.port);
        } else {
            eprintln!("[repl] {}:{} : en ligne", l.host, l.port);
        }
        mark_dirty(o, dirty, slot);
    }

    /// Lance le BGSAVE des cibles qui attendent un dump, si aucun n'est en
    /// cours (sinon : à sa fin). Les cibles prennent sa coupure comme base.
    fn repl_start_sync(&mut self) {
        let Some(src) = &self.src else { return };
        if !src.links.iter().any(|l| matches!(l.state, State::WaitSave) && l.base.is_none()) {
            return;
        }
        if self.bgsave.is_some() || self.art.is_saving() {
            return;
        }
        match self.start_bgsave() {
            Ok(()) => {
                let pos = self.bgsave.as_ref().unwrap().target().pos;
                for l in &mut self.src.as_mut().unwrap().links {
                    if matches!(l.state, State::WaitSave) && l.base.is_none() {
                        l.base = Some((pos.seq, pos.offset));
                    }
                }
            }
            // Réécriture de l'AOF pas finie (rotation) : nouvel essai au timer.
            Err(e) => eprintln!("[repl] BGSAVE de synchronisation : {e} ; nouvel essai dans 1 s"),
        }
    }

    /// Fin d'un BGSAVE : les cibles qui l'attendaient reçoivent son fichier.
    pub(super) fn repl_bgsave_done(&mut self, target: &Target, ok: bool) {
        let Some(src) = &mut self.src else { return };
        let path = target.dir.join(&target.name);
        let mut failed = Vec::new();
        for (i, l) in src.links.iter_mut().enumerate() {
            if !(matches!(l.state, State::WaitSave) && l.base.is_some_and(|(seq, _)| seq == target.pos.seq)) {
                continue;
            }
            if !ok {
                l.base = None;
                l.queue.clear();
                l.queue_bytes = 0;
                continue;
            }
            let file = File::open(&path).and_then(|f| f.metadata().map(|m| (f, m.len())));
            match file {
                Ok((file, len)) => {
                    let Some((slot, _)) = l.slot else { continue };
                    let o = &mut self.outs[slot as usize];
                    command(&mut o.out, &[b"RXLOAD", b"BEGIN", len.to_string().as_bytes()]);
                    l.expect.push_back(Expect::Load);
                    eprintln!("[repl] {}:{} : envoi du dump ({:.1} MB, bloc {})", l.host, l.port, len as f64 / 1e6, target.pos.seq);
                    l.state = State::Loading { file, off: 0, len };
                    mark_dirty(o, &mut self.dirty, slot);
                }
                Err(e) => failed.push((i, format!("{} : {e}", path.display()))),
            }
        }
        for (i, why) in failed {
            self.link_drop(i, why);
        }
        self.repl_start_sync();
    }

    /// Fin de tour : les blocs fermés vont au backlog et aux cibles (en ligne :
    /// envoyés ; en synchronisation : en file après leur base).
    pub(super) fn repl_absorb(&mut self) {
        let Some(tap) = self.journal.as_mut().and_then(|j| j.tap.as_mut()) else { return };
        if tap.is_empty() {
            return;
        }
        let blocks = std::mem::take(tap);
        let (backlog_max, max_lag) = (self.cfg.repl_backlog, self.cfg.repl_max_lag);
        let biggest = blocks.iter().map(|b| b.len() as u64).max().unwrap_or(0);
        let online_lag = self.cfg.repl_online_lag + biggest;
        let Core { src, outs, dirty, .. } = self;
        let src = src.as_mut().expect("flux branché sans source");
        let mut slow = Vec::new();
        for b in &blocks {
            src.backlog_bytes += b.len() as u64;
            src.backlog.push_back(b.clone());
            while src.backlog_bytes > backlog_max && src.backlog.len() > 1 {
                let old = src.backlog.pop_front().unwrap();
                src.backlog_bytes -= old.len() as u64;
            }
            let seq = blk_seq(b);
            for l in &mut src.links {
                let Some((slot, _)) = l.slot else { continue };
                match l.state {
                    State::Online => {
                        let o = &mut outs[slot as usize];
                        l.push_block(o, b);
                        mark_dirty(o, dirty, slot);
                    }
                    State::WaitSave | State::Loading { .. } if l.base.is_some_and(|(base, _)| seq > base) => {
                        l.queue_bytes += b.len() as u64;
                        l.queue.push_back(b.clone());
                    }
                    _ => {}
                }
            }
        }
        for (i, l) in src.links.iter().enumerate() {
            if let Some((slot, _)) = l.slot {
                let limit = if matches!(l.state, State::Online) { online_lag } else { max_lag };
                if l.lag_bytes(&outs[slot as usize]) > limit {
                    slow.push((i, limit));
                }
            }
        }
        for (i, limit) in slow.into_iter().rev() {
            self.link_drop(i, format!("réplica trop lent (plus de {} Ko en attente)", limit >> 10));
        }
    }

    /// Envoi du dump aux cibles qui chargent, au rythme de l'envoi ; à la fin,
    /// `RXLOAD END` puis les blocs mis en file : la cible est en ligne.
    pub(super) fn repl_pump(&mut self) {
        let Some(src) = &mut self.src else { return };
        let mut failed = Vec::new();
        for (i, l) in src.links.iter_mut().enumerate() {
            let State::Loading { file, off, len } = &mut l.state else { continue };
            let Some((slot, _)) = l.slot else { continue };
            let o = &mut self.outs[slot as usize];
            while o.out.len() + o.sending.len() < LOAD_WINDOW && *off < *len {
                let n = LOAD_CHUNK.min(*len - *off) as usize;
                o.out.extend_from_slice(b"*3\r\n$6\r\nRXLOAD\r\n$4\r\nDATA\r\n$");
                o.out.extend_from_slice(n.to_string().as_bytes());
                o.out.extend_from_slice(b"\r\n");
                let start = o.out.len();
                o.out.resize(start + n, 0);
                if let Err(e) = file.read_exact_at(&mut o.out[start..], *off) {
                    o.out.truncate(start);
                    failed.push((i, format!("lecture du dump : {e}")));
                    break;
                }
                o.out.extend_from_slice(b"\r\n");
                l.expect.push_back(Expect::Load);
                *off += n as u64;
            }
            if *off == *len {
                command(&mut o.out, &[b"RXLOAD", b"END"]);
                let (base, base_offset) = l.base.expect("chargement sans base");
                l.expect.push_back(Expect::LoadEnd { seq: base, end: base_offset });
                l.next_seq = base + 1;
                for b in std::mem::take(&mut l.queue) {
                    l.push_block(o, &b);
                }
                l.queue_bytes = 0;
                l.state = State::Online;
                l.last_err = None;
                eprintln!("[repl] {}:{} : dump envoyé, en ligne à partir du bloc {}", l.host, l.port, base + 1);
            }
            mark_dirty(o, &mut self.dirty, slot);
        }
        for (i, why) in failed {
            self.link_drop(i, why);
        }
    }

    /// Chaque seconde : reconnexion des cibles perdues, horloge du flux aux
    /// cibles en ligne (`RXTICK` : sans écriture, le réplica fait quand même
    /// expirer ses clés), BGSAVE de synchronisation à relancer.
    pub(super) fn repl_timer(&mut self) {
        let Some(src) = &self.src else { return };
        let now = self.now;
        let tick = self.journal.as_ref().map(|j| j.cursor().now);
        for i in 0..src.links.len() {
            let Core { src, outs, dirty, .. } = self;
            let l = &mut src.as_mut().unwrap().links[i];
            match l.state {
                State::Down { retry_at } if retry_at <= now => self.link_connect(i),
                State::Online => {
                    let (Some(t), Some((slot, _))) = (tick, l.slot) else { continue };
                    let o = &mut outs[slot as usize];
                    command(&mut o.out, &[b"RXTICK", t.to_string().as_bytes()]);
                    l.expect.push_back(Expect::Tick);
                    mark_dirty(o, dirty, slot);
                }
                _ => {}
            }
        }
        self.repl_start_sync();
    }

    // ── WAIT (§10.6) ─────────────────────────────────────────────────────

    /// Cibles en ligne qui ont acquitté le flux jusqu'à `mark`.
    fn acked_count(&self, mark: u64) -> u64 {
        self.src.as_ref().map_or(0, |src| {
            src.links.iter().filter(|l| matches!(l.state, State::Online) && l.acked_offset >= mark).count() as u64
        })
    }

    /// `WAIT n timeout` : tout de suite si `n` cibles ont acquitté la dernière
    /// écriture de la connexion, sinon la connexion attend (acquittements ou
    /// délai, `0` = sans fin). Réponse : le nombre de cibles qui l'ont.
    fn wait(&mut self, slot: Slot, replicas: u64, timeout_ms: u64) -> Result<(), ReplError> {
        if self.follow.is_some() {
            return Err(refuse("ERR WAIT cannot be used with replica instances"));
        }
        let s = slot as usize;
        let mark = self.outs[s].write_mark;
        let n = self.acked_count(mark);
        if n >= replicas {
            extend_encode(&mut self.outs[s].out, &Frame::Integer(n as i64));
            return Ok(());
        }
        let deadline = (timeout_ms > 0).then(|| self.now + Duration::from_millis(timeout_ms));
        if let Some(d) = deadline {
            self.timers.push(Reverse((d, Timer::Wait)));
        }
        let epoch = self.outs[s].epoch;
        self.outs[s].repl_wait = true;
        self.spans_to_rx(slot);
        self.waiters.push(Waiter { slot, epoch, mark, replicas, deadline });
        Ok(())
    }

    /// Réveille les `WAIT` satisfaits ou échus (acquittement reçu, timer).
    pub(super) fn wait_wake(&mut self) {
        if self.waiters.is_empty() {
            return;
        }
        let now = self.now;
        for w in std::mem::take(&mut self.waiters) {
            let n = self.acked_count(w.mark);
            let o = &self.outs[w.slot as usize];
            if o.epoch != w.epoch || !o.repl_wait {
                continue;
            }
            if n < w.replicas && w.deadline.is_none_or(|d| now < d) {
                self.waiters.push(w);
                continue;
            }
            let o = &mut self.outs[w.slot as usize];
            o.repl_wait = false;
            extend_encode(&mut o.out, &Frame::Integer(n as i64));
            if !o.dirty {
                o.dirty = true;
                self.dirty.push(w.slot);
            }
            if self.ins[w.slot as usize].has_work() && !o.ready {
                o.ready = true;
                self.ready.push(w.slot);
            }
        }
    }

    // ── Cible ────────────────────────────────────────────────────────────

    fn upstream_live(&self) -> Option<Upstream> {
        let up = self.upstream?;
        let o = &self.outs[up.slot as usize];
        (o.epoch == up.epoch && o.mode == Mode::Normal).then_some(up)
    }

    fn check_upstream(&self, slot: Slot) -> Result<Upstream, ReplError> {
        match self.upstream_live() {
            Some(up) if up.slot == slot => Ok(up),
            _ => Err(refuse("ERR not the replication source of this server (RXHELLO first)")),
        }
    }

    /// Commandes de réplication (après la fenêtre : `finish_frame`).
    pub(super) fn repl_command(&mut self, slot: Slot, after: After) -> Result<(), ReplError> {
        let s = slot as usize;
        let reply = |core: &mut Core, f: Frame| extend_encode(&mut core.outs[s].out, &f);
        match after {
            After::Replicate { host, port, auth } => {
                self.replicate(host, port, auth).map_err(refuse)?;
                reply(self, resp_ok());
            }
            After::ReplicateStop { host, port } => {
                self.replicate_stop(&host, port)?;
                reply(self, resp_ok());
            }
            After::ReplicaOfNoOne => {
                self.promote();
                reply(self, resp_ok());
            }
            After::RxHello(id) => {
                let id = std::str::from_utf8(&id).ok().and_then(unhex).ok_or_else(|| refuse("ERR invalid replication id"))?;
                if self.promoted {
                    return Err(refuse(
                        "ERR this server was promoted (REPLICAOF NO ONE): restart it to make it a replica again",
                    ));
                }
                let epoch = self.outs[s].epoch;
                if let Some(up) = self.upstream_live()
                    && up.slot != slot
                {
                    return Err(refuse(format!("ERR already following {}", hex(&up.replid))));
                }
                self.upstream = Some(Upstream { slot, epoch, replid: id });
                let seq = match &self.follow {
                    Some(f) if f.replid == id => f.cur.seq as i64,
                    _ => -1,
                };
                reply(self, Frame::Integer(seq));
            }
            After::RxLoadBegin(len) => {
                self.check_upstream(slot)?;
                let path = self.dir().join(LOAD_TMP);
                let file = File::create(&path).map_err(|e| fatal(format!("ERR {}: {e}", path.display())))?;
                self.loading = Some(LoadIn { file, path, len, got: 0 });
                reply(self, resp_ok());
            }
            After::RxLoadData(d) => {
                self.check_upstream(slot)?;
                let l = self.loading.as_mut().ok_or_else(|| fatal("ERR RXLOAD BEGIN first"))?;
                if l.got + d.len() as u64 > l.len {
                    return Err(fatal("ERR RXLOAD: more data than announced"));
                }
                l.file.write_all(&d).map_err(|e| fatal(format!("ERR {}: {e}", l.path.display())))?;
                l.got += d.len() as u64;
                reply(self, resp_ok());
            }
            After::RxLoadEnd => {
                let up = self.check_upstream(slot)?;
                let l = self.loading.take().ok_or_else(|| fatal("ERR RXLOAD BEGIN first"))?;
                if l.got != l.len {
                    let _ = std::fs::remove_file(&l.path);
                    return Err(fatal(format!("ERR RXLOAD: {} bytes received, {} announced", l.got, l.len)));
                }
                self.load_sync(l, up.replid)?;
                reply(self, resp_ok());
            }
            After::RxBlock(b) => {
                self.check_upstream(slot)?;
                if self.loading.is_some() {
                    return Err(fatal("ERR RXBLOCK during RXLOAD"));
                }
                let f = self.follow.as_mut().ok_or_else(|| fatal("ERR not following: RXLOAD first"))?;
                let mut cur = f.cur;
                let (blk, payload) =
                    journal::read_block(&b, &mut cur).map_err(|e| fatal(format!("ERR bad block after {}: {e:?}", f.cur.seq)))?;
                if blk.size() != b.len() as u64 {
                    return Err(fatal("ERR RXBLOCK: trailing bytes after the block"));
                }
                // Jamais en arrière d'un RXTICK (≤ au `now` des blocs suivants).
                self.art.set_now(blk.now.max(self.art.now));
                let mut pending = Vec::new();
                let res = journal::apply_into(&mut self.art, payload, &mut pending);
                self.jobs.extend(pending);
                if let Err(e) = res {
                    // L'état n'est plus celui de la source : la prochaine
                    // connexion repartira d'un dump.
                    f.replid = [0; 20];
                    eprintln!("[repl] bloc {} inapplicable : {} ; synchronisation complète à la reconnexion", blk.seq, e.0);
                    return Err(fatal(format!("ERR block {}: {}", blk.seq, e.0)));
                }
                f.cur = cur;
                let start = self.outs[s].out.len();
                reply(self, Frame::Integer(blk.seq as i64));
                // Réplica avec AOF ou qui chaîne : le bloc reçu, tel quel.
                if let Some(j) = &mut self.journal {
                    j.push_raw(std::rc::Rc::new(b), &blk);
                    // `always` : l'acquittement attend le fsync du bloc.
                    if let Some(durable) = self.aof.as_ref().and_then(|w| w.release_upto()) {
                        let mark = j.end_mark();
                        let o = &mut self.outs[s];
                        if mark > durable {
                            o.holds.push_back((start, mark));
                            if o.holds.len() == 1 {
                                self.held.push(slot);
                            }
                        }
                    }
                }
            }
            After::Wait { replicas, timeout_ms } => self.wait(slot, replicas, timeout_ms)?,
            After::RxTick(now) => {
                self.check_upstream(slot)?;
                if self.follow.is_some() {
                    if now > self.art.now {
                        self.art.set_now(now);
                    }
                    // Retransmise aux réplicas chaînés.
                    if let Some(j) = &mut self.journal {
                        j.observe_now(now);
                    }
                }
                reply(self, resp_ok());
            }
            _ => unreachable!("commande hors réplication"),
        }
        Ok(())
    }

    /// Fin de `RXLOAD` : la base est vidée, le dump chargé (CRC et condensé
    /// vérifiés), le serveur suit l'histoire de la source à sa coupure.
    /// Synchrone : le serveur ne répond à personne pendant le chargement
    /// (comme le chargement d'un RDB par un réplica Valkey).
    fn load_sync(&mut self, l: LoadIn, replid: ReplId) -> Result<(), ReplError> {
        let start = Instant::now();
        drop(l.file);
        self.abort_bgsave();
        self.flush_tree();
        // Toutes les clés du dump : celles qui expirent sont absentes pour les
        // commandes des blocs suivants (DET2), l'évicteur les retirera.
        self.art.set_now(0);
        let res = File::open(&l.path)
            .map_err(|e| e.to_string())
            .and_then(|f| self.art.load_file(f).map_err(|e| e.to_string()))
            .and_then(|stats| if stats.digest_checked { Ok(stats) } else { Err("dump sans condensé (v1)".into()) });
        // Nouvelle histoire : les réplicas chaînés repartent d'un dump.
        if let Some(src) = &mut self.src {
            src.backlog.clear();
            src.backlog_bytes = 0;
            for i in 0..src.links.len() {
                self.link_drop(i, "la source de ce serveur a changé d'histoire".into());
            }
        }
        let res = res.and_then(|stats| {
            let cur = Cursor { seq: stats.pos.seq, offset: stats.pos.offset, now: stats.t0 };
            // Avec l'AOF, le dump reçu devient la base, suivie d'un segment vide.
            if self.aof.is_some() {
                self.rebase_aof(&l.path, stats.pos, cur, replid).map_err(|e| format!("AOF du réplica : {e}"))?;
            }
            Ok((stats, cur))
        });
        let _ = std::fs::remove_file(&l.path);
        match res {
            Ok((stats, cur)) => {
                self.art.set_now(stats.t0);
                self.follow = Some(Follow { replid, cur });
                // Flux : pour l'AOF (blocs reçus écrits tels quels) et les réplicas chaînés.
                self.journal = (self.aof.is_some() || self.src.is_some()).then(|| {
                    let mut j = Stream::new(cur);
                    j.follow = true;
                    j.keep = self.aof.is_some();
                    j.tap = self.src.is_some().then(Vec::new);
                    j
                });
                eprintln!(
                    "[repl] synchronisé avec {} : {} clés en {:.2?}, bloc {}",
                    hex(&replid),
                    stats.keys,
                    start.elapsed(),
                    stats.pos.seq
                );
                Ok(())
            }
            Err(why) => {
                // Contenu indéterminé : vidé ; en lecture seule jusqu'au prochain dump.
                self.flush_tree();
                self.follow = Some(Follow { replid: [0; 20], cur: Cursor::default() });
                eprintln!("[repl] chargement du dump de synchronisation : {why}");
                Err(fatal(format!("ERR RXLOAD: {why}")))
            }
        }
    }

    /// Vide l'arbre d'une traite.
    fn flush_tree(&mut self) {
        let mut job = FlushJob::start(&mut self.art);
        while job.step(&mut self.art, usize::MAX) == Step::More {}
    }

    /// `REPLICAOF NO ONE` : sortie du mode suivi, nouvelle histoire qui
    /// continue la numérotation, horloge locale (§10.1, §10.8).
    fn promote(&mut self) {
        let Some(f) = self.follow.take() else { return };
        self.resume = Cursor { now: f.cur.now.max(self.art.now), ..f.cur };
        self.replid = new_replid();
        self.promoted = true;
        // Le flux continue la numérotation, avec les écritures locales.
        if let Some(j) = &mut self.journal {
            j.follow = false;
            j.observe_now(self.art.now);
        }
        // Manifeste démarqué hors de la boucle (en cas d'échec, l'écrivain le
        // dit : un redémarrage reviendrait en réplica).
        if let Some(w) = &mut self.aof {
            w.set_follow(None);
        }
        self.aof_pump();
        if let Some(l) = self.loading.take() {
            let _ = std::fs::remove_file(&l.path);
        }
        if let Some(up) = self.upstream_live() {
            self.drain(up.slot);
        }
        self.upstream = None;
        eprintln!("[repl] promu (REPLICAOF NO ONE) : nouvelle histoire {}, après le bloc {}", hex(&self.replid), f.cur.seq);
    }

    /// Réplica avec AOF qui reçoit un dump : il devient la base (renommé
    /// dans `appendonlydir`), suivi d'un segment vide, manifeste marqué
    /// `follow` ; nouvel écrivain (l'ancien finit ses opérations en vol).
    fn rebase_aof(&mut self, dump: &std::path::Path, pos: oxidart::JournalPos, cur: Cursor, follow: ReplId) -> std::io::Result<()> {
        let w = self.aof.as_ref().expect("AOF");
        let (dir, fsync) = (w.dir.clone(), w.fsync);
        let index = w.manifest.last_index() + 1;
        let (base, seg) = (aof::base_name(index), aof::segment_name(index));
        // Le dump sur disque avant que le manifeste ne le cite.
        File::open(dump)?.sync_all()?;
        std::fs::rename(dump, dir.join(&base))?;
        let _ = std::fs::remove_file(dir.join(&seg));
        aof::create_segment(&dir, &seg)?;
        let manifest = Manifest {
            replid: pos.replid,
            base,
            base_seq: pos.seq,
            base_offset: pos.offset,
            incrs: vec![seg],
            follow: Some(follow),
        };
        manifest.write(&dir)?;
        let trash = aof::unlisted(&dir, &manifest)?;
        let res = Resume { manifest, cursor: cur, tail_end: 0, tail_start_offset: pos.offset };
        let w = AofWriter::open(&dir, &res, fsync)?;
        self.replace_aof(w);
        self.fs_trash(trash);
        // Les marques des réponses retenues sont celles de l'ancienne histoire.
        for slot in std::mem::take(&mut self.held) {
            let o = &mut self.outs[slot as usize];
            o.holds.clear();
            if !o.dirty && !o.out.is_empty() {
                o.dirty = true;
                self.dirty.push(slot);
            }
        }
        Ok(())
    }

    /// Démarrage d'un réplica avec AOF (manifeste marqué `follow`) : il
    /// reprend en mode suivi, à la fin de ses fichiers.
    pub(super) fn resume_follow(&mut self, replid: ReplId) {
        let Some(j) = &mut self.journal else { return };
        j.follow = true;
        let cur = j.cursor();
        self.art.set_now(cur.now);
        self.follow = Some(Follow { replid, cur });
        eprintln!("[repl] réplica de {} (manifeste de l'AOF), après le bloc {}", hex(&replid), cur.seq);
    }

    // ── INFO ─────────────────────────────────────────────────────────────

    /// Section `# Replication`.
    pub(super) fn repl_info(&self, out: &mut String) {
        use std::fmt::Write as _;
        out.push_str("# Replication\r\n");
        let cur = match &self.follow {
            Some(f) => {
                let _ = write!(
                    out,
                    "role:replica\r\nmaster_replid:{}\r\nmaster_link_status:{}\r\nrepl_seq:{}\r\nmaster_repl_offset:{}\r\nmaster_sync_in_progress:{}\r\n",
                    hex(&f.replid),
                    if self.upstream_live().is_some() { "up" } else { "down" },
                    f.cur.seq,
                    f.cur.offset,
                    u8::from(self.loading.is_some()),
                );
                f.cur
            }
            None => {
                let cur = self.journal.as_ref().map_or(self.resume, |j| j.cursor());
                let _ = write!(
                    out,
                    "role:master\r\nreplid:{}\r\nmaster_repl_offset:{}\r\nrepl_seq:{}\r\n",
                    hex(&self.replid),
                    cur.offset,
                    cur.seq,
                );
                cur
            }
        };
        let links = self.src.as_ref().map_or(&[][..], |s| &s.links[..]);
        let online = links.iter().filter(|l| matches!(l.state, State::Online)).count();
        let _ = write!(out, "connected_replicas:{online}\r\n");
        for (i, l) in links.iter().enumerate() {
            let _ = write!(
                out,
                "replica{i}:ip={},port={},state={},seq={},lag_bytes={}\r\n",
                l.host,
                l.port,
                l.state.name(),
                l.acked_seq,
                cur.offset.saturating_sub(l.acked_offset),
            );
        }
        if let Some(src) = &self.src {
            let _ = write!(
                out,
                "repl_backlog_active:1\r\nrepl_backlog_size:{}\r\nrepl_backlog_first_seq:{}\r\nsync_full:{}\r\nsync_partial_ok:{}\r\n",
                src.backlog_bytes,
                src.backlog.front().map_or(0, |b| blk_seq(b)),
                src.full_syncs,
                src.partial_syncs,
            );
        }
    }
}
