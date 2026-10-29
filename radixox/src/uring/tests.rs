//! Tests du cœur sur un faux noyau (RUNTIME_ARCH.md §1.1 règle 3, §16) :
//! séquences de complétions fabriquées, invariants vérifiés à chaque tour.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use radixox_lib::sys::Instant;

use oxidart::OxidArt;

use super::bufs::Bid;
use super::conn::Mode;
use super::core::{Completion, Config, Core, ECANCELED, ENOBUFS, Op, Slot};
use crate::exec::client::Client;
use crate::exec::{Executed, execute_into};

/// Côté client d'une connexion simulée.
#[derive(Default)]
struct Peer {
    /// Recv multishot armé dans le noyau.
    armed: bool,
    /// Octets envoyés par le client, pas encore reçus par le serveur.
    inbound: VecDeque<u8>,
    /// Le client a fermé son côté écriture, après `inbound`.
    eof: bool,
    /// Réponses reçues par le client.
    received: Vec<u8>,
    /// Le client ne lit plus : les sends restent en vol.
    stalled: bool,
    /// Send en vol bloqué (client qui ne lit plus).
    send_pending: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sends {
    Full,
    /// Chaque send n'envoie que la moitié (au moins un octet).
    Half,
}

struct Fake {
    core: Core,
    /// Tampons fournis au noyau, tête d'anneau en premier.
    ring: VecDeque<Bid>,
    /// Prochain octet du tampon de tête (mode incrémental).
    k_off: u32,
    /// Taille max d'un morceau reçu.
    max_chunk: usize,
    sends: Sends,
    peers: HashMap<Slot, Peer>,
    /// Clients fermés par le serveur, avec ce qu'ils ont reçu.
    closed: Vec<(Slot, Vec<u8>)>,
    rejected: usize,
    next_fd: i32,
    fd_slot: HashMap<i32, Slot>,
    ops: Vec<Op>,
    completions: Vec<Completion>,
    /// Horloge du faux noyau : avance de 50 µs par tour, déplaçable par le test.
    clock: Instant,
    /// Comme la boucle : travail de fond à la fin du tour tant que rien
    /// n'arrive (ici : ni complétion, ni donnée en attente chez un client).
    idle_work: bool,
    /// Horloge murale du TTL (secondes), posée à chaque tour si définie.
    wall: Option<u64>,
    /// Écritures du segment de l'AOF partielles (au plus ce nombre d'octets).
    journal_write_max: Option<usize>,
    /// Disque bloqué : les écritures de l'AOF restent en vol.
    journal_stalled: bool,
    journal_held: Vec<Op>,
    /// fsync de l'AOF retenus (disque lent à commiter).
    sync_stalled: bool,
    sync_held: Vec<Op>,
    /// Une écriture liée vient d'être retenue : son fsync la suit.
    link_held: bool,
    /// L'écriture liée précédente est restée partielle : son fsync est annulé.
    cancel_sync: bool,
    /// Écritures du segment exécutées alors qu'un fsync était en vol.
    writes_during_sync: u64,
    /// `connect` sortants pas encore aboutis (le test les accepte ou refuse).
    connecting: Vec<(Slot, std::net::SocketAddr)>,
    /// Étapes des suites de fichiers retenues (disque lent à créer, renommer).
    fs_stalled: bool,
    fs_held: Vec<Op>,
}

impl Fake {
    fn new(cfg: Config) -> Self {
        let ring = (0..cfg.bufs).collect();
        Self {
            core: Core::new(cfg, OxidArt::new()),
            ring,
            k_off: 0,
            max_chunk: usize::MAX,
            sends: Sends::Full,
            peers: HashMap::new(),
            closed: Vec::new(),
            rejected: 0,
            next_fd: 100,
            fd_slot: HashMap::new(),
            ops: Vec::new(),
            completions: Vec::new(),
            clock: Instant::now(),
            idle_work: false,
            wall: None,
            journal_write_max: None,
            journal_stalled: false,
            journal_held: Vec::new(),
            sync_stalled: false,
            sync_held: Vec::new(),
            link_held: false,
            cancel_sync: false,
            writes_during_sync: 0,
            connecting: Vec::new(),
            fs_stalled: false,
            fs_held: Vec::new(),
        }
    }

    fn small(bufs: Bid, buf_size: u32) -> Self {
        Self::new(Config { max_conn: 16, reserved: 1, bufs, buf_size, ..Config::default() })
    }

    /// Le noyau exécute les opérations soumises au tour précédent.
    fn apply_ops(&mut self) {
        if !self.journal_stalled {
            let held = std::mem::take(&mut self.journal_held);
            self.ops.splice(0..0, held);
        }
        if !self.sync_stalled {
            let held = std::mem::take(&mut self.sync_held);
            self.ops.splice(0..0, held);
        }
        if !self.fs_stalled {
            let held = std::mem::take(&mut self.fs_held);
            self.ops.splice(0..0, held);
        }
        for op in std::mem::take(&mut self.ops) {
            match op {
                Op::AcceptMulti => {}
                Op::Install { slot, fd } => {
                    self.fd_slot.insert(fd, slot);
                    self.peers.insert(slot, Peer { armed: true, ..Peer::default() });
                }
                Op::Reject { .. } => self.rejected += 1,
                Op::RecvMulti { slot } => self.peers.get_mut(&slot).expect("peer").armed = true,
                Op::Send { slot } => {
                    let peer = self.peers.get_mut(&slot).expect("peer");
                    if peer.stalled {
                        peer.send_pending = true;
                        continue;
                    }
                    let bytes = self.core.pending_send(slot);
                    assert!(!bytes.is_empty(), "send vide");
                    let n = match self.sends {
                        Sends::Full => bytes.len(),
                        Sends::Half => bytes.len().div_ceil(2),
                    };
                    peer.received.extend_from_slice(&bytes[..n]);
                    self.completions.push(Completion::Send { slot, res: n as i32 });
                }
                Op::CancelRecv { slot } => {
                    let peer = self.peers.get_mut(&slot).expect("peer");
                    if peer.armed {
                        peer.armed = false;
                        self.completions.push(Completion::Recv {
                            slot,
                            res: -ECANCELED,
                            bid: None,
                            buf_more: false,
                            more: false,
                        });
                    }
                }
                Op::CancelSend { slot } => {
                    let peer = self.peers.get_mut(&slot).expect("peer");
                    if peer.send_pending {
                        peer.send_pending = false;
                        self.completions.push(Completion::Send { slot, res: -ECANCELED });
                    }
                }
                Op::Close { slot } => {
                    let peer = self.peers.remove(&slot).expect("close d'un slot inconnu");
                    assert!(!peer.armed && !peer.send_pending, "close avec une op en vol");
                    self.closed.push((slot, peer.received));
                }
                Op::ProvideBuf { bid } => self.ring.push_back(bid),
                // Fichier du BGSAVE : écritures réelles (le dump est relu par le test).
                Op::FileWrite { fd, off, save } => {
                    let buf = self.core.pending_file_write(save);
                    // SAFETY: fd ouvert par le BGSAVE, tampon valide.
                    let res = unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len(), off as i64) };
                    self.completions.push(Completion::File { res: res as i32, save });
                }
                Op::FileSync { fd, save } => {
                    // SAFETY: fd ouvert par le BGSAVE.
                    let res = unsafe { libc::fdatasync(fd) };
                    self.completions.push(Completion::File { res, save });
                }
                // Segment de l'AOF : écritures réelles (relues par le test).
                Op::JournalWrite { link, .. } if self.journal_stalled => {
                    self.link_held = link;
                    self.journal_held.push(op);
                }
                // Chaîne liée (IOSQE_IO_LINK) : le fsync suit son écriture.
                Op::JournalSync { .. } if std::mem::take(&mut self.link_held) => self.journal_held.push(op),
                Op::JournalSync { writer, .. } if std::mem::take(&mut self.cancel_sync) => {
                    self.completions.push(Completion::JournalSync { res: -libc::ECANCELED, writer });
                }
                Op::JournalSync { .. } if self.sync_stalled => self.sync_held.push(op),
                Op::JournalWrite { fd, off, writer, link } => {
                    self.writes_during_sync += u64::from(!self.sync_held.is_empty());
                    let buf = self.core.pending_journal_write(writer);
                    let n = match self.journal_write_max {
                        Some(max) => buf.len().min(max),
                        None => buf.len(),
                    };
                    // SAFETY: fd ouvert par l'écrivain, tampon valide.
                    let res = unsafe { libc::pwrite(fd, buf.as_ptr().cast(), n, off as i64) };
                    self.cancel_sync = link && (res as usize) < buf.len();
                    self.completions.push(Completion::JournalWrite { res: res as i32, writer });
                }
                Op::JournalSync { fd, writer } => {
                    // SAFETY: fd ouvert par l'écrivain.
                    let res = unsafe { libc::fdatasync(fd) };
                    self.completions.push(Completion::JournalSync { res, writer });
                }
                Op::Connect { slot, addr } => self.connecting.push((slot, addr)),
                Op::Fs { .. } if self.fs_stalled => self.fs_held.push(op),
                // Suites de fichiers : appels réels, comme le ferait un worker io-wq.
                Op::Fs { id } => {
                    let step = self.core.fs_step(id).expect("étape d'une suite en cours");
                    let res = crate::exec::fs_task::FsTask::exec_blocking(step);
                    self.completions.push(Completion::Fs { id, res });
                }
            }
        }
    }

    /// Le noyau livre ce que les clients ont envoyé, morceau par morceau,
    /// dans le tampon de tête (connexions entrelacées).
    fn deliver(&mut self) {
        let size = self.core.config().buf_size;
        let mut slots: Vec<Slot> = self.peers.keys().copied().collect();
        slots.sort_unstable();
        for slot in slots {
            let peer = self.peers.get_mut(&slot).unwrap();
            while peer.armed && !peer.inbound.is_empty() {
                let Some(&bid) = self.ring.front() else {
                    peer.armed = false;
                    self.completions.push(Completion::Recv { slot, res: -ENOBUFS, bid: None, buf_more: false, more: false });
                    break;
                };
                let n = peer.inbound.len().min((size - self.k_off) as usize).min(self.max_chunk);
                let bytes: Vec<u8> = peer.inbound.drain(..n).collect();
                self.core.bufs_mut().kernel_write(bid, self.k_off, &bytes);
                self.k_off += n as u32;
                let buf_more = self.k_off < size;
                if !buf_more {
                    self.ring.pop_front();
                    self.k_off = 0;
                }
                self.completions.push(Completion::Recv { slot, res: n as i32, bid: Some(bid), buf_more, more: true });
            }
            if peer.armed && peer.inbound.is_empty() && peer.eof {
                peer.armed = false;
                peer.eof = false;
                self.completions.push(Completion::Recv { slot, res: 0, bid: None, buf_more: false, more: false });
            }
        }
    }

    /// Un tour de boucle.
    fn step(&mut self) -> bool {
        self.clock += Duration::from_micros(50);
        self.core.set_now(self.clock);
        if let Some(w) = self.wall {
            self.core.set_wall(w);
        }
        self.apply_ops();
        self.deliver();
        let busy = !self.completions.is_empty() || !self.core.idle();
        // `FAKE_TRACE=1` : déroulé des tours (débogage).
        if std::env::var("FAKE_TRACE").is_ok() {
            eprintln!("ring={:?} k_off={} completions={:?}", self.ring, self.k_off, self.completions);
        }
        for c in std::mem::take(&mut self.completions) {
            self.core.complete(c);
        }
        self.core.run_batch();
        self.core.run_jobs();
        self.core.flush();
        self.core.take_ops(&mut self.ops);
        if self.idle_work && self.core.quiet() && self.core.has_background() {
            while self.completions.is_empty()
                && self.peers.values().all(|p| p.inbound.is_empty())
                && self.core.quiet()
                && self.core.idle_step()
            {}
            self.core.flush();
            self.core.take_ops(&mut self.ops);
        }
        self.core.end_turn();
        // Comme la boucle : les opérations de la fin de tour partent avec lui.
        self.core.take_ops(&mut self.ops);
        if std::env::var("FAKE_TRACE").is_ok() {
            eprintln!("  ops={:?}", self.ops);
        }
        self.core.check_invariants();
        busy || !self.ops.is_empty()
    }

    /// Tourne jusqu'au repos.
    fn settle(&mut self) {
        for _ in 0..100_000 {
            if !self.step() {
                return;
            }
        }
        self.core.debug_dump();
        panic!("pas de repos après 100 000 tours");
    }

    fn connect(&mut self) -> Slot {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.completions.push(Completion::Accept { fd, more: true });
        self.settle();
        *self.fd_slot.get(&fd).expect("connexion acceptée")
    }

    fn send(&mut self, slot: Slot, bytes: &[u8]) {
        self.peers.get_mut(&slot).expect("peer").inbound.extend(bytes);
    }

    fn take(&mut self, slot: Slot) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.peers.get_mut(&slot).expect("peer").received)).into()
    }

    /// Le `connect` sortant vers `slot` aboutit (`res` 0) ou échoue.
    fn finish_connect(&mut self, slot: Slot, res: i32) {
        let i = self.connecting.iter().position(|&(s, _)| s == slot).expect("connect en vol");
        self.connecting.remove(i);
        if res == 0 {
            self.peers.insert(slot, Peer::default());
        }
        self.completions.push(Completion::Connect { slot, res });
    }

    fn closed_with(&self, slot: Slot) -> Option<String> {
        self.closed.iter().rev().find(|(s, _)| *s == slot).map(|(_, r)| String::from_utf8_lossy(r).into())
    }
}

#[test]
fn set_then_get() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    f.send(c, b"SET a 1\r\n*2\r\n$3\r\nGET\r\n$1\r\na\r\n");
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n$1\r\n1\r\n");
}

/// Frames coupées octet par octet : reliquat dans `rx`, frames locales.
#[test]
fn frames_split_byte_by_byte() {
    let mut f = Fake::small(4, 64);
    f.max_chunk = 1;
    let c = f.connect();
    f.send(c, b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nhello\r\nGET key\r\nPING\r\n");
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n$5\r\nhello\r\n+PONG\r\n");
}

/// Plusieurs connexions entrelacées dans les mêmes petits tampons, qui
/// reviennent à l'anneau ; plus de données que l'anneau n'en contient.
#[test]
fn interleaved_connections_and_buffer_recycling() {
    let mut f = Fake::small(3, 32);
    f.max_chunk = 7;
    let conns: Vec<Slot> = (0..4).map(|_| f.connect()).collect();
    for (n, &c) in conns.iter().enumerate() {
        for i in 0..20 {
            f.send(c, format!("SET k{n}:{i} v{i}\r\n").as_bytes());
        }
        f.send(c, format!("GET k{n}:19\r\n").as_bytes());
    }
    f.settle();
    for &c in &conns {
        assert_eq!(f.take(c), format!("{}$3\r\nv19\r\n", "+OK\r\n".repeat(20)));
    }
}

/// Anneau épuisé : ENOBUFS, recv réarmé quand un tampon revient.
#[test]
fn enobufs_then_rearm() {
    let mut f = Fake::small(1, 16);
    let c = f.connect();
    let big: String = (0..50).map(|i| format!("PING m{i}\r\n")).collect();
    f.send(c, big.as_bytes());
    f.settle();
    let expect: String = (0..50).map(|i| format!("${}\r\nm{i}\r\n", format!("m{i}").len())).collect();
    assert_eq!(f.take(c), expect);
}

/// Équité : une connexion à gros pipeline ne retarde pas un client sans
/// pipeline, servi dès le premier tour (§2.1).
#[test]
fn round_robin_serves_single_commands_first() {
    let mut f = Fake::new(Config { max_conn: 8, batch_cmds: 10, ..Config::default() });
    let heavy = f.connect();
    let light = f.connect();
    f.send(heavy, "PING\r\n".repeat(100).as_bytes());
    f.send(light, b"PING\r\n");
    f.step(); // réception
    f.step(); // premier lot : 10 commandes, dont celle de `light`
    f.apply_ops();
    assert_eq!(f.take(light), "+PONG\r\n");
    assert!(f.take(heavy).len() < 100 * 7, "le pipeline s'étale sur plusieurs tours");
    f.settle();
}

/// QUIT est une barrière et ferme : la suite de la file n'est pas exécutée.
#[test]
fn quit_closes_after_replying() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    f.send(c, b"SET q 1\r\nQUIT\r\nSET q 2\r\n");
    f.settle();
    assert_eq!(f.closed_with(c).as_deref(), Some("+OK\r\n+OK\r\n"));
    let c2 = f.connect();
    f.send(c2, b"GET q\r\n");
    f.settle();
    assert_eq!(f.take(c2), "$1\r\n1\r\n");
}

/// EOF juste après un pipeline (`nc`, scripts) : tout est exécuté et envoyé
/// avant la fermeture (Closing, §4.2).
#[test]
fn eof_after_pipeline_still_replies() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    f.send(c, b"SET e 1\r\nINCR e\r\nGET e\r\n");
    f.peers.get_mut(&c).unwrap().eof = true;
    f.settle();
    assert_eq!(f.closed_with(c).as_deref(), Some("+OK\r\n:2\r\n$1\r\n2\r\n"));
}

/// Erreur de protocole : réponses d'avant, erreur, fermeture ; la suite du
/// flux est ignorée.
#[test]
fn protocol_error_replies_then_closes() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    f.send(c, b"PING\r\n*1\r\n$-5\r\nPING\r\n");
    f.settle();
    assert_eq!(f.closed_with(c).as_deref(), Some("+PONG\r\n-ERR Protocol error: invalid bulk length\r\n"));
}

/// Annulation croisée : après QUIT, le noyau livre encore des données avant
/// d'exécuter l'annulation du recv ; elles sont ignorées.
#[test]
fn data_arriving_after_quit_is_ignored() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    f.send(c, b"QUIT\r\n");
    while !f.ops.contains(&Op::CancelRecv { slot: c }) {
        f.step();
    }
    f.send(c, b"SET late 1\r\n");
    f.deliver(); // avant que l'annulation ne soit appliquée
    f.settle();
    assert_eq!(f.closed_with(c).as_deref(), Some("+OK\r\n"));
    let c2 = f.connect();
    f.send(c2, b"EXISTS late\r\n");
    f.settle();
    assert_eq!(f.take(c2), ":0\r\n");
}

/// Envois partiels : le reste est renvoyé, la sortie est intacte.
#[test]
fn partial_sends_are_completed() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    f.sends = Sends::Half;
    let c = f.connect();
    f.send(c, b"SET p hello-world\r\nGET p\r\nGET p\r\n");
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n$11\r\nhello-world\r\n$11\r\nhello-world\r\n");
}

/// Client qui ne lit plus, puis reset : le send en vol est annulé, `refs`
/// retombe à 0, le slot est fermé puis réutilisé avec une nouvelle époque.
#[test]
fn stalled_send_is_cancelled_on_reset() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    let epoch = f.core.epoch(c);
    f.peers.get_mut(&c).unwrap().stalled = true;
    f.send(c, b"PING\r\n");
    f.settle();
    assert!(f.peers[&c].send_pending);
    // Reset : le recv rend une erreur.
    f.peers.get_mut(&c).unwrap().armed = false;
    f.completions.push(Completion::Recv { slot: c, res: -104, bid: None, buf_more: false, more: false });
    f.settle();
    assert!(f.closed_with(c).is_some());
    assert_eq!(f.core.mode(c), Mode::Free);
    let c2 = f.connect();
    assert_eq!(c2, c, "slot réutilisé (LIFO)");
    assert_eq!(f.core.epoch(c2), epoch + 1);
}

/// Plus de slot libre (hors réserve) : refus.
#[test]
fn max_clients() {
    let mut f = Fake::new(Config { max_conn: 3, reserved: 1, ..Config::default() });
    f.connect();
    f.connect();
    let fd = f.next_fd;
    f.completions.push(Completion::Accept { fd, more: true });
    f.settle();
    assert_eq!(f.rejected, 1);
    assert_eq!(f.core.connections(), 2);
}

/// Erreurs d'arguments et commandes inconnues : la connexion continue.
#[test]
fn invalid_commands_keep_the_connection() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    f.send(c, b"DUMP k\r\nGET\r\nSELECT 3\r\nPING\r\n");
    f.settle();
    assert_eq!(
        f.take(c),
        "-ERR unknown command 'dump'\r\n-ERR wrong number of arguments for 'get' command\r\n\
         -ERR DB index is out of range\r\n+PONG\r\n"
    );
}

/// FLUSHDB répond tout de suite ; la libération se fait en fond, par tranches.
#[test]
fn flushdb_frees_in_background() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    let sets: String = (0..2000).map(|i| format!("SET f{i} v\r\n")).collect();
    f.send(c, sets.as_bytes());
    f.settle();
    f.take(c);
    f.send(c, b"FLUSHDB\r\nDBSIZE\r\n");
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n:0\r\n");
    assert_eq!(f.core.art.len(), 0);
}

/// KEYS long : la connexion est parquée, les autres sont servies pendant ce
/// temps ; la réponse arrive ensuite, puis les commandes envoyées derrière.
#[test]
fn long_keys_parks_without_blocking_others() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let a = f.connect();
    let b = f.connect();
    let sets: String = (0..3000).map(|i| format!("SET k{i:04} v\r\n")).collect();
    f.send(a, sets.as_bytes());
    f.settle();
    f.take(a);
    f.send(a, b"KEYS k*\r\nPING after\r\n");
    f.step();
    f.step();
    assert!(f.peers[&a].received.is_empty(), "KEYS ne répond pas d'un coup : connexion parquée");
    f.send(b, b"PING\r\n");
    f.step();
    f.step();
    f.apply_ops();
    assert_eq!(f.take(b), "+PONG\r\n", "servie pendant le KEYS");
    f.settle();
    let got = f.take(a);
    assert!(got.starts_with("*3000\r\n$5\r\nk0000\r\n"), "{}", &got[..40]);
    assert!(got.ends_with("$5\r\nk2999\r\n$5\r\nafter\r\n"));
}

/// Grosse réponse de KEYS envoyée depuis son tampon (sans copie dans `out`),
/// en envois partiels : contenu intact, puis les réponses suivantes.
#[test]
fn big_keys_response_partial_sends() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let a = f.connect();
    let sets: String = (0..3000).map(|i| format!("SET k{i:04} v\r\n")).collect();
    f.send(a, sets.as_bytes());
    f.settle();
    f.take(a);
    f.sends = Sends::Half;
    f.send(a, b"KEYS k*\r\nPING after\r\n");
    f.settle();
    let want: String = (0..3000).map(|i| format!("$5\r\nk{i:04}\r\n")).collect();
    assert_eq!(f.take(a), format!("*3000\r\n{want}$5\r\nafter\r\n"));
    assert_eq!(radixox_lib::big_buf::bytes(), 0, "tampon rendu après l'envoi");
}

/// KEYS fini pendant qu'une réponse précédente est encore en vol : l'ordre
/// reste réponse précédente → KEYS → suivantes.
#[test]
fn big_keys_waits_for_send_in_flight() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let a = f.connect();
    let sets: String = (0..3000).map(|i| format!("SET k{i:04} v\r\n")).collect();
    f.send(a, sets.as_bytes());
    f.settle();
    f.take(a);
    f.peers.get_mut(&a).unwrap().stalled = true;
    f.send(a, b"PING first\r\nKEYS k*\r\nPING after\r\n");
    f.settle();
    // le client relit : le send retenu part, puis le reste
    let p = f.peers.get_mut(&a).unwrap();
    p.stalled = false;
    if std::mem::take(&mut p.send_pending) {
        let bytes = f.core.pending_send(a).to_vec();
        f.peers.get_mut(&a).unwrap().received.extend_from_slice(&bytes);
        f.completions.push(Completion::Send { slot: a, res: bytes.len() as i32 });
    }
    f.settle();
    let got = f.take(a);
    assert!(got.starts_with("$5\r\nfirst\r\n*3000\r\n$5\r\nk0000\r\n"), "{}", &got[..60]);
    assert!(got.ends_with("$5\r\nk2999\r\n$5\r\nafter\r\n"));
}

/// Au repos, le travail de fond avance sans attendre les tours : le KEYS
/// répond au tour suivant. La passe s'arrête dès qu'un client a quelque chose
/// à recevoir : la réponse part avant que le second KEYS ne reprenne.
#[test]
fn idle_work_runs_jobs_until_someone_waits() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    f.idle_work = true;
    let a = f.connect();
    let b = f.connect();
    let sets: String = (0..3000).map(|i| format!("SET k{i:04} v\r\n")).collect();
    f.send(a, sets.as_bytes());
    f.settle();
    f.take(a);
    f.send(a, b"KEYS k*\r\n");
    f.step();
    f.apply_ops();
    assert!(f.take(a).starts_with("*3000\r\n"), "KEYS fini pendant le repos");

    f.send(a, b"KEYS k*\r\n");
    f.send(b, b"KEYS k*\r\n");
    f.step();
    f.apply_ops();
    let (ra, rb) = (f.take(a), f.take(b));
    assert!(ra.is_empty() != rb.is_empty(), "une seule réponse avant la fin de la passe");
    assert!(f.core.has_background());
    f.settle();
    let rest = f.take(a) + &f.take(b);
    assert!(rest.starts_with("*3000\r\n"));
}

/// UNLINK préfixe* : parqué, compte exact, clés invisibles ; une commande
/// reçue pendant le parquage attend la réponse.
#[test]
fn unlink_prefix_parks_and_counts() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    let sets: String = (0..2000).map(|i| format!("SET u:{i} v\r\n")).collect();
    f.send(c, sets.as_bytes());
    f.send(c, b"SET keep 1\r\n");
    f.settle();
    f.take(c);
    f.send(c, b"UNLINK u:*\r\n");
    f.step();
    f.step();
    f.send(c, b"DBSIZE\r\n");
    f.settle();
    assert_eq!(f.take(c), ":2000\r\n:1\r\n");
}

/// BGSAVE dans la boucle : les clients sont servis pendant la save, les
/// écritures et le fsync passent par des ops ; le dump relu contient la vue
/// T0 (les écritures faites pendant la save n'y sont pas).
#[test]
fn bgsave_writes_a_consistent_dump() {
    let dir = std::env::temp_dir().join(format!("radixox-uring-bgsave-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: tests en mono-thread (RUST_TEST_THREADS=1).
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    let sets: String = (0..5000).map(|i| format!("SET b{i} v{i}\r\n")).collect();
    f.send(c, sets.as_bytes());
    f.settle();
    f.take(c);
    f.send(c, b"BGSAVE\r\nSET b0 changed\r\nBGSAVE\r\nSAVE\r\n");
    f.step();
    f.step();
    f.apply_ops();
    assert_eq!(
        f.take(c),
        "+Background saving started\r\n+OK\r\n-ERR Background save already in progress\r\n\
         -ERR Background save already in progress\r\n"
    );
    f.settle();
    assert!(!f.core.bgsave_running());
    crate::exec::persist::set_save_dir(None);
    let mut art = OxidArt::new();
    art.load_file(std::fs::File::open(dir.join("dump.rxd")).unwrap()).unwrap();
    assert_eq!(art.len(), 5000);
    assert_eq!(art.get(b"b0"), Some(oxidart::Value::from_str("v0")), "vue T0");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pub/Sub : abonnement, publication (compte des abonnés atteints), mode
/// abonné (seules les commandes Pub/Sub, PING et QUIT), désabonnement total
/// puis retour au mode normal.
#[test]
fn pubsub_flow() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let sub = f.connect();
    let publ = f.connect();
    f.send(sub, b"SUBSCRIBE news sport news\r\n");
    f.settle();
    assert_eq!(
        f.take(sub),
        "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n\
         *3\r\n$9\r\nsubscribe\r\n$5\r\nsport\r\n:2\r\n\
         *3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:2\r\n"
    );
    f.send(publ, b"PUBLISH news hello\r\nPUBLISH nobody x\r\n");
    f.settle();
    assert_eq!(f.take(publ), ":1\r\n:0\r\n");
    assert_eq!(f.take(sub), "*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n");
    f.send(sub, b"GET k\r\nPING\r\n");
    f.settle();
    assert_eq!(
        f.take(sub),
        "-ERR only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT allowed in this context\r\n\
         *2\r\n$4\r\npong\r\n$0\r\n\r\n"
    );
    f.send(sub, b"UNSUBSCRIBE\r\nPING\r\nUNSUBSCRIBE\r\n");
    f.settle();
    let got = f.take(sub);
    assert!(got.ends_with(":0\r\n+PONG\r\n*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n"), "{got}");
    f.send(publ, b"PUBLISH news again\r\n");
    f.settle();
    assert_eq!(f.take(publ), ":0\r\n");
}

/// Abonné qui ne lit plus : au-delà de la limite dure de sortie, il est fermé
/// et ne compte plus parmi les abonnés.
#[test]
fn slow_subscriber_is_closed() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let sub = f.connect();
    let publ = f.connect();
    f.send(sub, b"SUBSCRIBE ch\r\n");
    f.settle();
    f.take(sub);
    f.peers.get_mut(&sub).unwrap().stalled = true;
    let big = "x".repeat(1 << 20);
    for _ in 0..40 {
        // En RESP : une commande inline est limitée à 64 Ko.
        f.send(publ, format!("*3\r\n$7\r\nPUBLISH\r\n$2\r\nch\r\n${}\r\n{big}\r\n", big.len()).as_bytes());
        f.settle();
    }
    assert!(f.closed_with(sub).is_some(), "abonné lent fermé");
    let got = f.take(publ);
    assert!(got.ends_with(":0\r\n"), "plus d'abonné à la fin");
}

/// Balayage d'inactivité : une connexion muette au-delà du délai est fermée,
/// une connexion active reste ouverte (horloge pilotée par le test).
#[test]
fn idle_connections_are_closed() {
    let mut f = Fake::new(Config { max_conn: 8, idle_timeout: Some(Duration::from_secs(5)), ..Config::default() });
    let t0 = f.clock;
    let idle = f.connect();
    let busy = f.connect();
    f.clock = t0 + Duration::from_secs(3);
    f.send(busy, b"PING\r\n");
    f.settle();
    f.clock = t0 + Duration::from_secs(7);
    f.step();
    f.core.fire_timers();
    f.settle();
    assert!(f.closed_with(idle).is_some(), "connexion muette fermée");
    assert_eq!(f.core.mode(busy), Mode::Normal);
    assert_eq!(f.take(busy), "+PONG\r\n");
}

// ── Aléatoire contre un oracle ───────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Une commande aléatoire sur les clés de la connexion `n`, en RESP ou inline.
fn random_cmd(rng: &mut Rng, n: usize) -> Vec<u8> {
    let key = format!("c{n}:k{}", rng.below(6));
    let val = format!("v{}", rng.below(1000));
    let args: Vec<String> = match rng.below(10) {
        8 => vec!["KEYS".into(), format!("c{n}:k*")],
        9 => vec!["UNLINK".into(), format!("c{n}:k{}*", rng.below(3))],
        0 => vec!["SET".into(), key, val],
        1 => vec!["GET".into(), key],
        2 => vec!["INCR".into(), format!("c{n}:ctr")],
        3 => vec!["DEL".into(), key],
        4 => vec!["HSET".into(), format!("c{n}:h"), key, val],
        5 => vec!["HGET".into(), format!("c{n}:h"), key],
        6 => vec!["EXISTS".into(), key, format!("c{n}:ctr")],
        _ => vec!["PING".into(), val],
    };
    if rng.below(2) == 0 {
        let mut out = format!("*{}\r\n", args.len());
        for a in &args {
            out += &format!("${}\r\n{a}\r\n", a.len());
        }
        out.into_bytes()
    } else {
        (args.join(" ") + "\r\n").into_bytes()
    }
}

/// Réponses attendues : les commandes de la connexion exécutées seules, dans
/// l'ordre (clés disjointes entre connexions : l'entrelacement ne change rien).
fn oracle(stream: &[u8]) -> Vec<u8> {
    let mut art = OxidArt::new();
    let mut client = Client::new();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < stream.len() {
        let radixox_lib::cmd::Parse::Done(cmd, n) = radixox_lib::cmd::Cmd::parse(&stream[pos..]) else {
            panic!("flux de test invalide");
        };
        pos += n;
        match execute_into(cmd, &mut art, &mut client, &mut out) {
            Executed::Done => {}
            Executed::Runtime(radixox_lib::cmd::Cmd::Keys(p)) => {
                let mut job = crate::exec::jobs::KeysJob::new(p);
                while job.step(&mut art, usize::MAX) == crate::exec::jobs::Step::More {}
                out.extend_from_slice(job.response());
            }
            Executed::Runtime(radixox_lib::cmd::Cmd::Unlink(keys)) => {
                let mut job = crate::exec::jobs::UnlinkJob::start(&mut art, &keys);
                while job.step(&mut art, usize::MAX) == crate::exec::jobs::Step::More {}
                radixox_lib::shared_frame::extend_encode(
                    &mut out,
                    &radixox_lib::shared_frame::SharedFrame::Integer(job.count() as i64),
                );
            }
            Executed::Runtime(_) => panic!("commande inattendue dans le flux de test"),
        }
    }
    out
}

#[test]
fn random_streams_match_oracle() {
    // `URING_SEEDS=3000` pour une passe longue.
    let seeds: u64 = std::env::var("URING_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let bufs = 1 + rng.below(4) as Bid;
        let mut f = Fake::new(Config {
            max_conn: 8,
            reserved: 0,
            bufs,
            buf_size: 16 + rng.below(200) as u32,
            batch_cmds: 1 + rng.below(20) as usize,
            flush_early: 1 + rng.below(200) as usize,
            out_backpressure: 1 + rng.below(400) as usize,
            prefetch: 1 + rng.below(10) as usize,
            job_budget: 1 + rng.below(3) as usize,
            ..Config::default()
        });
        f.max_chunk = 1 + rng.below(40) as usize;
        f.sends = if rng.below(2) == 0 { Sends::Full } else { Sends::Half };
        f.idle_work = seed % 2 == 0;
        let conns: Vec<Slot> = (0..1 + rng.below(5)).map(|_| f.connect()).collect();
        let mut streams = vec![Vec::new(); conns.len()];
        // Envois en plusieurs vagues, entrecoupées de tours de boucle.
        for _ in 0..5 {
            for (n, &c) in conns.iter().enumerate() {
                for _ in 0..rng.below(30) {
                    let cmd = random_cmd(&mut rng, n);
                    streams[n].extend_from_slice(&cmd);
                    f.send(c, &cmd);
                }
            }
            for _ in 0..rng.below(4) {
                f.step();
            }
        }
        f.settle();
        for (n, &c) in conns.iter().enumerate() {
            let got = std::mem::take(&mut f.peers.get_mut(&c).unwrap().received);
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&oracle(&streams[n])),
                "graine {seed}, connexion {n}"
            );
        }
    }
}

// ── Flux de commandes (DURABILITY_DESIGN.md §12, tests 1 et 2) ──────────────

/// Une écriture ou une lecture aléatoire, tous types, TTL courts, sur un petit
/// jeu de clés partagé entre types (WRONGTYPE compris). `spop` : avec SPOP
/// (non déterministe, réécrit par le flux).
fn random_write(rng: &mut Rng, spop: bool) -> Vec<String> {
    let k = |rng: &mut Rng| format!("k{}", rng.below(8));
    let v = |rng: &mut Rng| format!("v{}", rng.below(20));
    let m = |rng: &mut Rng| format!("m{}", rng.below(12));
    let ttl = |rng: &mut Rng| (1 + rng.below(3)).to_string();
    let s = |x: &str| x.to_string();
    // `s0..s2` : sets seulement, pour que SPOP tire souvent dans un set plein.
    let sk = |rng: &mut Rng| format!("s{}", rng.below(3));
    match rng.below(if spop { 37 } else { 35 }) {
        0 => vec![s("SET"), k(rng), v(rng)],
        1 => vec![s("SET"), k(rng), v(rng), s("EX"), ttl(rng)],
        2 => vec![s("SET"), k(rng), v(rng), s("NX")],
        3 => vec![s("SET"), k(rng), v(rng), s("XX"), s("EX"), ttl(rng)],
        4 => vec![s("SETNX"), k(rng), v(rng)],
        5 => vec![s("MSET"), k(rng), v(rng), k(rng), v(rng)],
        6 => vec![s("DEL"), k(rng), k(rng)],
        7 => vec![s("UNLINK"), k(rng)],
        8 => vec![s("EXPIRE"), k(rng), ttl(rng)],
        9 => vec![s("PERSIST"), k(rng)],
        10 => vec![s("INCR"), k(rng)],
        11 => vec![s("DECRBY"), k(rng), rng.below(5).to_string()],
        12 => vec![s("HSET"), k(rng), m(rng), v(rng)],
        13 => vec![s("HMSET"), k(rng), m(rng), v(rng), m(rng), v(rng)],
        14 => vec![s("HDEL"), k(rng), m(rng)],
        15 => vec![s("HINCRBY"), k(rng), m(rng), rng.below(5).to_string()],
        16 => vec![s("SADD"), k(rng), m(rng), m(rng)],
        17 => vec![s("SREM"), k(rng), m(rng)],
        18 => vec![s("ZADD"), k(rng), rng.below(9).to_string(), m(rng)],
        19 => vec![s("ZREM"), k(rng), m(rng)],
        20 => vec![s("ZINCRBY"), k(rng), s("1.5"), m(rng)],
        21 => vec![s("GET"), k(rng)],
        22 => vec![s("TTL"), k(rng)],
        23 => vec![s("EXISTS"), k(rng), k(rng)],
        24 => vec![s("HGET"), k(rng), m(rng)],
        25 => vec![s("SISMEMBER"), k(rng), m(rng)],
        26 => vec![s("ZSCORE"), k(rng), m(rng)],
        27 => vec![s("TYPE"), k(rng)],
        28 => vec![s("INCRBY"), k(rng), rng.below(5).to_string()],
        29 => vec![s("SETEX"), k(rng), ttl(rng), v(rng)],
        30 => vec![s("UNLINK"), format!("k{}*", rng.below(3))],
        31 if rng.below(8) == 0 => vec![s("FLUSHDB")],
        31 | 32 => vec![s("SCARD"), k(rng)],
        33 => vec![s("SADD"), sk(rng), m(rng), m(rng), m(rng)],
        34 => vec![s("SREM"), sk(rng), m(rng)],
        35 => vec![s("SPOP"), sk(rng)],
        _ => vec![s("SPOP"), k(rng), (1 + rng.below(2)).to_string()],
    }
}

fn resp(args: &[String]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len());
    for a in args {
        out += &format!("${}\r\n{a}\r\n", a.len());
    }
    out.into_bytes()
}

/// État métier, indépendant de l'encodage : clés vivantes à `art.now`, type,
/// valeur canonique (contenu trié), TTL restant.
fn state(art: &mut OxidArt) -> Vec<String> {
    use oxidart::value::Value;
    use radixox_lib::shared_byte::SharedByte;
    let mut out = Vec::new();
    let mut entries = art.getn(SharedByte::from_slice(b""));
    entries.sort_by(|a, b| a.0.as_slice().cmp(b.0.as_slice()));
    for (key, val) in entries {
        let ttl = art.get_ttl(key.clone());
        let show = |b: &SharedByte| String::from_utf8_lossy(b).into_owned();
        let v = match val {
            Value::String(b) => format!("str {}", show(&b)),
            Value::Int(n) => format!("int {n}"),
            Value::Hash(mut f) => {
                f.sort_by(|a, b| a.0.as_slice().cmp(b.0.as_slice()));
                format!("hash {:?}", f.iter().map(|(a, b)| (show(a), show(b))).collect::<Vec<_>>())
            }
            Value::Set(mut m) => {
                m.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));
                format!("set {:?}", m.iter().map(show).collect::<Vec<_>>())
            }
            Value::ZSet(_) => {
                let r = art.cmd_zrange(&key, 0, -1, true).unwrap();
                format!("zset {:?}", r.iter().map(show).collect::<Vec<_>>())
            }
            Value::List(_) => "list".into(),
        };
        out.push(format!("{} {v} {ttl:?}", show(&key)));
    }
    out
}

/// Rejoue les blocs fermés sur un arbre neuf.
fn replay(blocks: &[u8]) -> OxidArt {
    use crate::exec::journal::{Cursor, apply, read_block};
    let mut art = OxidArt::new();
    let mut cur = Cursor::default();
    let mut pos = 0;
    while pos < blocks.len() {
        let (b, payload) = read_block(&blocks[pos..], &mut cur).expect("bloc valide");
        pos += b.size() as usize;
        art.set_now(b.now);
        apply(&mut art, payload).expect("charge valide");
    }
    art
}

#[test]
fn journal_replay_matches_state() {
    // `URING_SEEDS=3000` pour une passe longue.
    let seeds: u64 = std::env::var("URING_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut f = Fake::new(Config {
            max_conn: 8,
            reserved: 0,
            bufs: 1 + rng.below(4) as Bid,
            buf_size: 64 + rng.below(400) as u32,
            batch_cmds: 1 + rng.below(20) as usize,
            prefetch: 1 + rng.below(10) as usize,
            job_budget: 1 + rng.below(3) as usize,
            journal: true,
            ..Config::default()
        });
        f.max_chunk = 1 + rng.below(60) as usize;
        f.idle_work = seed % 2 == 0;
        let mut wall = 1_000u64;
        f.wall = Some(wall);
        let conns: Vec<Slot> = (0..1 + rng.below(4)).map(|_| f.connect()).collect();
        for _ in 0..30 {
            for &c in &conns {
                for _ in 0..rng.below(12) {
                    let cmd = resp(&random_write(&mut rng, true));
                    f.send(c, &cmd);
                }
            }
            for _ in 0..1 + rng.below(4) {
                f.step();
            }
            // L'horloge avance, parfois recule (ramenée au dernier `now` écrit).
            wall = match rng.below(6) {
                0 => wall.saturating_sub(2),
                1 | 2 => wall,
                _ => wall + rng.below(3),
            };
            f.wall = Some(wall);
            // L'évicteur passe quand il veut : ses suppressions ne sont pas journalisées.
            if rng.below(3) == 0 {
                f.core.art.evict_expired();
            }
        }
        f.settle();
        let blocks: Vec<u8> = f.core.journal_mut().unwrap().closed.iter().flat_map(|b| b.iter()).copied().collect();
        let mut replayed = replay(&blocks);
        replayed.set_now(f.core.art.now);
        let (got, want) = (state(&mut replayed), state(&mut f.core.art));
        assert_eq!(got, want, "graine {seed}");
    }
}

/// DET2 : une écriture ne voit pas une clé expirée, qu'elle soit encore dans
/// l'arbre ou déjà supprimée : avec ou sans passage de l'évicteur juste avant,
/// même réponse et même état.
#[test]
fn expired_keys_are_absent_for_writes() {
    use crate::exec::jobs::{FlushJob, Step, UnlinkJob};
    use radixox_lib::cmd::{Cmd, Parse};
    fn run(art: &mut OxidArt, raw: &[u8]) -> Vec<u8> {
        let mut client = Client::new();
        client.authed = true;
        let mut out = Vec::new();
        let Parse::Done(cmd, _) = Cmd::parse(raw) else { panic!("commande de test") };
        match execute_into(cmd, art, &mut client, &mut out) {
            Executed::Done => {}
            Executed::Runtime(Cmd::Unlink(keys)) => {
                let mut job = UnlinkJob::start(art, &keys);
                while job.step(art, usize::MAX) == Step::More {}
                out = format!(":{}\r\n", job.count()).into_bytes();
            }
            Executed::Runtime(Cmd::FlushDb) => {
                let mut job = FlushJob::start(art);
                while job.step(art, usize::MAX) == Step::More {}
                out = b"+OK\r\n".to_vec();
            }
            Executed::Runtime(_) => panic!("commande inattendue"),
        }
        out
    }
    let seeds: u64 = std::env::var("URING_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
        let (mut a, mut b) = (OxidArt::new(), OxidArt::new());
        let mut now = 1_000;
        for i in 0..400 {
            if rng.below(4) == 0 {
                now += rng.below(3);
            }
            a.set_now(now);
            b.set_now(now);
            // `a` purge tout ce qui est expiré (l'évicteur échantillonne).
            for _ in 0..4 {
                a.evict_expired();
            }
            let cmd = resp(&random_write(&mut rng, false));
            let (ra, rb) = (run(&mut a, &cmd), run(&mut b, &cmd));
            assert_eq!(
                String::from_utf8_lossy(&ra),
                String::from_utf8_lossy(&rb),
                "graine {seed}, commande {i} : {}",
                String::from_utf8_lossy(&cmd).escape_debug()
            );
        }
        assert_eq!(state(&mut a), state(&mut b), "graine {seed}");
    }
}

// ── Reprise depuis les fichiers de l'AOF (DURABILITY_DESIGN.md §9, test 4) ──

/// Dossier de test neuf (supprimé s'il existe).
/// Chemin d'un dossier de test, pour les API à `&str`.
fn p(d: &std::path::Path) -> &str {
    d.to_str().unwrap()
}

fn test_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("radixox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Un flux aléatoire sur le faux noyau ; rend les blocs fermés, un par
/// élément, et l'arbre du serveur.
fn journaled_run(seed: u64) -> (Vec<Vec<u8>>, Fake) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut f = Fake::new(Config { max_conn: 8, reserved: 0, journal: true, ..Config::default() });
    let mut wall = 1_000u64;
    f.wall = Some(wall);
    let conns: Vec<Slot> = (0..1 + rng.below(3)).map(|_| f.connect()).collect();
    for _ in 0..20 {
        for &c in &conns {
            for _ in 0..rng.below(10) {
                f.send(c, &resp(&random_write(&mut rng, true)));
            }
        }
        f.step();
        wall += rng.below(3);
        f.wall = Some(wall);
    }
    f.settle();
    let blocks = f.core.journal_mut().unwrap().closed.drain(..).map(|b| b.to_vec()).collect();
    (blocks, f)
}

/// Base vide + segments contenant `parts` (des listes de blocs), manifeste.
fn write_aof(dir: &std::path::Path, parts: &[&[Vec<u8>]]) {
    use crate::exec::aof::{self, Manifest};
    let mut empty = OxidArt::new();
    let res = aof::create(&mut empty, p(dir)).unwrap();
    let mut m: Manifest = res.manifest;
    m.incrs.clear();
    for (i, blocks) in parts.iter().enumerate() {
        let name = aof::segment_name(i as u64 + 1);
        let _ = std::fs::remove_file(dir.join(&name));
        let file = aof::create_segment(p(dir), &name).unwrap();
        let bytes: Vec<u8> = blocks.iter().flatten().copied().collect();
        crate::fs::pwrite_all(&file, &bytes, 0).unwrap();
        m.incrs.push(name);
    }
    m.write(p(dir)).unwrap();
}

fn recover_state(dir: &std::path::Path, now: u64, truncated: bool) -> std::io::Result<(Vec<String>, crate::exec::aof::Resume)> {
    let mut art = OxidArt::new();
    let res = crate::exec::aof::recover(&mut art, p(dir), truncated).map_err(std::io::Error::from)?.expect("manifeste");
    art.set_now(now);
    Ok((state(&mut art), res))
}

#[test]
fn aof_recovery_matches_state_across_segments() {
    let dir = test_dir("aof-recover");
    for seed in 1..=15 {
        let (blocks, mut f) = journaled_run(seed);
        let cut = blocks.len() / 2;
        write_aof(&dir, &[&blocks[..cut], &blocks[cut..]]);
        let now = f.core.art.now;
        let (got, res) = recover_state(&dir, now, false).unwrap();
        assert_eq!(got, state(&mut f.core.art), "graine {seed}");
        let total: usize = blocks.iter().map(Vec::len).sum();
        assert_eq!(res.cursor.offset as usize, total);
        assert_eq!(res.cursor.seq as usize, blocks.len());
        // Le segment courant est ramené à sa fin logique (plus de préallocation).
        let tail: usize = blocks[cut..].iter().map(Vec::len).sum();
        assert_eq!(res.tail_end as usize, tail);
        assert_eq!(std::fs::metadata(dir.join("journal.2.rxj")).unwrap().len() as usize, tail);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn aof_truncated_tail_is_cut_only_at_the_end() {
    let dir = test_dir("aof-trunc");
    let (blocks, _) = journaled_run(7);
    assert!(blocks.len() >= 3);
    let n = blocks.len();
    // État de référence de chaque préfixe de blocs.
    let prefix_state = |k: usize| {
        let bytes: Vec<u8> = blocks[..k].iter().flatten().copied().collect();
        let mut art = replay(&bytes);
        art.set_now(10_000);
        state(&mut art)
    };
    let full: Vec<u8> = blocks.iter().flatten().copied().collect();
    let last_start = full.len() - blocks[n - 1].len();
    let want_prefix = prefix_state(n - 1);
    // Coupé à chaque octet du dernier bloc : préfixe exact, accepté seulement
    // avec RADIXOX_AOF_LOAD_TRUNCATED.
    // Chaque octet de l'en-tête, puis un octet sur 13 de la charge.
    let cuts = (last_start..last_start + 41).chain((last_start + 41..full.len()).step_by(13));
    for cut in cuts {
        write_aof(&dir, &[&[full[..cut].to_vec()]]);
        assert!(recover_state(&dir, 10_000, false).is_err() || cut == last_start, "coupé à {cut}");
        write_aof(&dir, &[&[full[..cut].to_vec()]]);
        let (got, res) = recover_state(&dir, 10_000, true).unwrap();
        assert_eq!(got, want_prefix, "coupé à {cut}");
        assert_eq!(res.tail_end as usize, last_start);
    }
    // Octet altéré dans le dernier bloc : même chose.
    let mut bad = full.clone();
    bad[last_start + 20] ^= 1;
    write_aof(&dir, &[&[bad.clone()]]);
    assert!(recover_state(&dir, 10_000, false).is_err());
    write_aof(&dir, &[&[bad]]);
    assert_eq!(recover_state(&dir, 10_000, true).unwrap().0, want_prefix);
    // Bloc altéré au milieu d'un segment qui n'est pas le dernier : refus.
    let mut first = blocks[..n - 1].iter().flatten().copied().collect::<Vec<u8>>();
    first[50] ^= 1;
    write_aof(&dir, &[&[first], &blocks[n - 1..]]);
    assert!(recover_state(&dir, 10_000, true).is_err());
    // Un bloc sauté entre deux segments, ou dans le dernier : refus, même
    // avec la troncature permise (ce n'est pas une écriture coupée).
    write_aof(&dir, &[&blocks[..1], &blocks[2..]]);
    assert!(recover_state(&dir, 10_000, true).is_err());
    let skipped = [blocks[..1].concat(), blocks[2..].concat()].concat();
    write_aof(&dir, &[&[skipped]]);
    assert!(recover_state(&dir, 10_000, true).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn aof_unlisted_files_are_removed() {
    use crate::exec::aof;
    let dir = test_dir("aof-unlisted");
    write_aof(&dir, &[&[]]);
    std::fs::write(dir.join("journal.9.rxj"), b"reste").unwrap();
    std::fs::write(dir.join("base.9.rxdb.tmp"), b"reste").unwrap();
    std::fs::write(dir.join("notes.txt"), b"pas a nous").unwrap();
    let mut art = OxidArt::new();
    let res = aof::recover(&mut art, p(&dir), true).unwrap().unwrap();
    aof::remove_unlisted(p(&dir), &res.manifest).unwrap();
    let mut names: Vec<String> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["base.1.rxdb", "journal.1.rxj", "manifest", "notes.txt"]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── AOF pendant le service (DURABILITY_DESIGN.md §7) ────────────────────────

/// Serveur sur le faux noyau avec l'AOF dans `dir` (créé vide).
fn fake_with_aof(dir: &std::path::Path, cfg: Config) -> Fake {
    fake_with_aof_fsync(dir, cfg, crate::exec::aof::Fsync::EverySec)
}

fn fake_with_aof_fsync(dir: &std::path::Path, cfg: Config, fsync: crate::exec::aof::Fsync) -> Fake {
    use crate::exec::aof;
    let mut f = Fake::new(cfg);
    let res = aof::create(&mut f.core.art, p(dir)).unwrap();
    let w = aof::AofWriter::open(p(dir), &res, fsync).unwrap();
    f.core.set_aof(crate::exec::journal::Stream::new(res.cursor), w);
    f
}

#[test]
fn aof_files_replay_to_the_served_state() {
    let dir = test_dir("aof-serve");
    for seed in 1..=10u64 {
        let _ = std::fs::remove_dir_all(&dir);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut f = fake_with_aof(&dir, Config { max_conn: 8, reserved: 0, ..Config::default() });
        f.journal_write_max = (seed % 2 == 0).then(|| 1 + rng.below(100) as usize);
        let mut wall = 1_000u64;
        f.wall = Some(wall);
        let conns: Vec<Slot> = (0..1 + rng.below(3)).map(|_| f.connect()).collect();
        for round in 0..20 {
            for &c in &conns {
                for _ in 0..rng.below(10) {
                    f.send(c, &resp(&random_write(&mut rng, true)));
                }
            }
            f.step();
            wall += rng.below(3);
            f.wall = Some(wall);
            if round % 7 == 0 {
                // Timer d'une seconde : fsync `everysec`.
                f.clock += Duration::from_secs(1);
                f.core.set_now(f.clock);
                f.core.fire_timers();
            }
        }
        f.settle();
        assert_eq!(f.core.aof_pending(), 0);
        let mut back = OxidArt::new();
        crate::exec::aof::recover(&mut back, p(&dir), false).unwrap().unwrap();
        back.set_now(f.core.art.now);
        assert_eq!(state(&mut back), state(&mut f.core.art), "graine {seed}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Disque bloqué au-delà du plafond : les écritures attendent sans réponse,
/// les lectures sont servies ; au déblocage, tout s'exécute dans l'ordre.
#[test]
fn aof_backlog_parks_writers_not_readers() {
    let dir = test_dir("aof-cap");
    let mut f = fake_with_aof(&dir, Config { max_conn: 8, reserved: 0, aof_max_pending: 200, ..Config::default() });
    let (w, r) = (f.connect(), f.connect());
    f.journal_stalled = true;
    // Assez d'écritures pour dépasser 200 octets de flux fermé.
    for i in 0..20 {
        f.send(w, format!("SET k{i} {i}\r\n").as_bytes());
        f.step();
    }
    f.settle();
    let before = f.take(w);
    assert!(before.matches("+OK").count() < 20, "des écritures attendent : {before:?}");
    assert!(f.core.aof_pending() > 200);
    f.send(r, b"GET k0\r\n");
    f.settle();
    assert_eq!(f.take(r), "$1\r\n0\r\n");
    // Disque débloqué : la suite s'exécute.
    f.journal_stalled = false;
    f.settle();
    let after = f.take(w);
    assert_eq!(before.matches("+OK").count() + after.matches("+OK").count(), 20);
    f.send(r, b"GET k19\r\n");
    f.settle();
    assert_eq!(f.take(r), "$2\r\n19\r\n");
    let mut back = OxidArt::new();
    crate::exec::aof::recover(&mut back, p(&dir), false).unwrap().unwrap();
    back.set_now(f.core.art.now);
    assert_eq!(state(&mut back), state(&mut f.core.art));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `everysec` : sans timer, un fsync part dès que l'écrit non fsyncé atteint
/// le seuil ; sous le seuil, rien avant la seconde.
#[test]
fn everysec_syncs_early_past_the_threshold() {
    let dir = test_dir("aof-sync-bytes");
    for (sync_bytes, early) in [(100, true), (1 << 20, false)] {
        let _ = std::fs::remove_dir_all(&dir);
        let mut f = fake_with_aof(&dir, Config { max_conn: 8, reserved: 0, aof_sync_bytes: sync_bytes, ..Config::default() });
        let c = f.connect();
        let start = f.core.aof().unwrap().durable;
        for i in 0..20 {
            f.send(c, format!("SET k{i} {i}\r\n").as_bytes());
            f.step();
        }
        f.settle();
        assert_eq!(f.take(c).matches("+OK").count(), 20);
        let w = f.core.aof().unwrap();
        assert!(w.written - start > 100);
        if early {
            assert!(w.durable > start && w.written - w.durable < 100, "{} / {}", w.durable, w.written);
        } else {
            assert_eq!(w.durable, start);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `continuous` : cycles « écriture + fsync liés », un seul en vol, aucune
/// écriture pendant un fsync ; écritures partielles (fsync lié annulé) et
/// fsync lents au hasard ; à la fin tout est fsyncé et se rejoue.
#[test]
fn continuous_cycles_never_overlap_and_replay() {
    use crate::exec::aof::Fsync;
    let dir = test_dir("aof-continuous");
    let mut rewritten = 0;
    for seed in 1..=20u64 {
        let _ = std::fs::remove_dir_all(&dir);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let cfg = Config { max_conn: 8, reserved: 0, aof_cycle_min: 1 + rng.below(4096), ..Config::default() };
        let mut f = fake_with_aof_fsync(&dir, cfg, Fsync::Continuous);
        f.journal_write_max = (seed % 2 == 0).then(|| 1 + rng.below(100) as usize);
        // Réécritures (rotations de segment) au fil du trafic.
        f.idle_work = true;
        let mut wall = 1_000u64;
        f.wall = Some(wall);
        let conns: Vec<Slot> = (0..1 + rng.below(3)).map(|_| f.connect()).collect();
        for _ in 0..30 {
            f.sync_stalled = rng.below(3) == 0;
            for &c in &conns {
                for _ in 0..rng.below(10) {
                    f.send(c, &resp(&random_write(&mut rng, true)));
                }
                if rng.below(8) == 0 {
                    f.send(c, b"BGREWRITEAOF\r\n");
                }
            }
            f.step();
            if rng.below(4) == 0 {
                f.clock += cfg_cycle();
                f.core.set_now(f.clock);
                f.core.fire_timers();
            }
            wall += rng.below(2);
            f.wall = Some(wall);
        }
        f.sync_stalled = false;
        for _ in 0..3 {
            f.settle();
            f.clock += cfg_cycle();
            f.core.set_now(f.clock);
            f.core.fire_timers();
        }
        f.settle();
        assert_eq!(f.writes_during_sync, 0, "graine {seed} : écriture pendant un fsync");
        let w = f.core.aof().unwrap();
        assert_eq!((f.core.aof_pending(), w.durable), (0, w.written), "graine {seed}");
        let mut back = OxidArt::new();
        crate::exec::aof::recover(&mut back, p(&dir), false).unwrap().unwrap();
        back.set_now(f.core.art.now);
        assert_eq!(state(&mut back), state(&mut f.core.art), "graine {seed}");
        rewritten += u32::from(manifest_of(&dir).base != "base.1.rxdb");
    }
    assert!(rewritten > 5, "réécritures : {rewritten} graines sur 20");
    let _ = std::fs::remove_dir_all(&dir);
}

fn cfg_cycle() -> Duration {
    Config::default().aof_cycle
}

/// `continuous` : les réponses n'attendent ni l'écriture ni le fsync ; un
/// reste sous `aof_cycle_min` attend le timer, au-delà le cycle part tout de
/// suite.
#[test]
fn continuous_replies_never_wait_and_small_rests_wait_the_timer() {
    use crate::exec::aof::Fsync;
    let dir = test_dir("aof-continuous-min");
    // 6 Mio s'accumulent plus bas sans que les écrivains attendent : plafond au-dessus.
    let cfg = Config { max_conn: 8, reserved: 0, aof_max_pending: 64 << 20, ..Config::default() };
    let mut f = fake_with_aof_fsync(&dir, cfg, Fsync::Continuous);
    let c = f.connect();
    let start = f.core.aof().unwrap().durable;
    // Petit : rien n'est écrit avant le timer.
    f.send(c, b"SET a 1\r\n");
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n");
    assert_eq!(f.core.aof().unwrap().written, start);
    f.clock += cfg_cycle();
    f.core.set_now(f.clock);
    f.core.fire_timers();
    f.settle();
    let w = f.core.aof().unwrap();
    assert!(w.durable > start && w.durable == w.written);
    // fsync bloqué : les réponses partent quand même, les blocs s'accumulent.
    f.sync_stalled = true;
    let big = "x".repeat(5000);
    for i in 0..10 {
        f.send(c, format!("SET k{i} {big}\r\n").as_bytes());
        f.settle();
        assert_eq!(f.take(c), "+OK\r\n");
    }
    let w = f.core.aof().unwrap();
    assert!(f.core.aof_pending() > 0 && w.durable < w.written);
    f.sync_stalled = false;
    f.settle();
    let w = f.core.aof().unwrap();
    assert_eq!((f.core.aof_pending(), w.durable), (0, w.written));
    assert_eq!(f.writes_during_sync, 0);
    // Disque bloqué pendant que 6 Mio s'accumulent : un seul cycle de
    // plusieurs écritures (au-delà de WRITE_MAX), un seul fsync, pas un
    // fsync par écriture.
    let fsyncs = |f: &mut Fake| {
        f.send(c, b"INFO persistence\r\n");
        f.settle();
        let info = f.take(c);
        info.split("aof_fsyncs:").nth(1).unwrap().split('\r').next().unwrap().parse::<u64>().unwrap()
    };
    let before = fsyncs(&mut f);
    f.journal_stalled = true;
    let big = "y".repeat(60_000);
    f.send(c, format!("SET first {big}\r\n").as_bytes());
    f.settle();
    for i in 0..100 {
        f.send(c, format!("SET b{i} {big}\r\n").as_bytes());
    }
    f.settle();
    assert_eq!(f.take(c).matches("+OK").count(), 101);
    assert!(f.core.aof_pending() > 5 << 20);
    f.journal_stalled = false;
    f.settle();
    let w = f.core.aof().unwrap();
    assert_eq!((f.core.aof_pending(), w.durable), (0, w.written));
    // Le cycle bloqué (SET first), puis le cycle des 6 Mio.
    assert_eq!(fsyncs(&mut f) - before, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Réécriture (DURABILITY_DESIGN.md §8, tests 6 et 1 depuis une base) ──────

fn manifest_of(dir: &std::path::Path) -> crate::exec::aof::Manifest {
    crate::exec::aof::Manifest::read(p(dir)).unwrap().unwrap()
}

/// `SET a 1`, `BGSAVE`, `SET a 2` dans un même pipeline : la base contient
/// `a=1`, le segment suivant `SET a 2`, la reprise donne `a=2`.
#[test]
fn rewrite_cut_is_exact_inside_a_pipeline() {
    let dir = test_dir("aof-cut");
    let mut f = fake_with_aof(&dir, Config { max_conn: 8, reserved: 0, ..Config::default() });
    f.idle_work = true;
    f.wall = Some(1_000);
    let c = f.connect();
    f.send(c, b"SET a 1\r\nBGSAVE\r\nSET a 2\r\n");
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n+Background saving started\r\n+OK\r\n");
    assert!(!f.core.bgsave_running());
    let m = manifest_of(&dir);
    assert_eq!(m.base, "base.2.rxdb");
    assert_eq!(m.incrs, ["journal.2.rxj"]);
    // La base : a=1, au bloc CUT.
    let mut base = OxidArt::new();
    let stats = base.load_file(std::fs::File::open(dir.join(&m.base)).unwrap()).unwrap();
    assert_eq!((stats.pos.seq, stats.pos.offset), (m.base_seq, m.base_offset));
    assert!(matches!(base.get(b"a"), Some(oxidart::value::Value::String(v)) if v.as_slice() == b"1"));
    // Le segment suivant : SET a 2 seulement.
    let seg = std::fs::read(dir.join("journal.2.rxj")).unwrap();
    let mut cur = crate::exec::journal::Cursor { seq: m.base_seq, offset: m.base_offset, now: 0 };
    let (_, payload) = crate::exec::journal::read_block(&seg, &mut cur).unwrap();
    assert_eq!(payload, b"SET a 2\r\n");
    // Anciens fichiers supprimés ; reprise = a=2.
    assert!(!dir.join("base.1.rxdb").exists() && !dir.join("journal.1.rxj").exists());
    let (got, _) = recover_state(&dir, 1_000, false).unwrap();
    assert_eq!(got, ["a str 2 KeyWithoutTtl"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Flux aléatoire avec des BGSAVE (réécritures) intercalés, écritures du
/// segment partielles : la reprise depuis la dernière base et ses segments
/// redonne l'état servi.
#[test]
fn rewrites_under_random_traffic_replay_to_the_served_state() {
    let dir = test_dir("aof-rewrite");
    for seed in 1..=12u64 {
        let _ = std::fs::remove_dir_all(&dir);
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
        let mut f = fake_with_aof(&dir, Config { max_conn: 8, reserved: 0, ..Config::default() });
        f.idle_work = seed % 2 == 0;
        f.journal_write_max = (seed % 3 == 0).then(|| 1 + rng.below(300) as usize);
        let mut wall = 1_000u64;
        f.wall = Some(wall);
        let conns: Vec<Slot> = (0..1 + rng.below(3)).map(|_| f.connect()).collect();
        for _ in 0..40 {
            for &c in &conns {
                for _ in 0..rng.below(10) {
                    let cmd = if rng.below(40) == 0 { vec!["BGSAVE".to_string()] } else { random_write(&mut rng, true) };
                    f.send(c, &resp(&cmd));
                }
            }
            for _ in 0..1 + rng.below(3) {
                f.step();
            }
            wall += rng.below(3);
            f.wall = Some(wall);
            if rng.below(5) == 0 {
                f.clock += Duration::from_secs(1);
                f.core.set_now(f.clock);
                f.core.fire_timers();
            }
        }
        f.settle();
        // Une dernière réécriture menée à son terme, anciens fichiers supprimés.
        while f.core.bgsave_running() || f.core.aof().unwrap().rewriting() || f.core.fs_pending() {
            f.step();
        }
        let m = manifest_of(&dir);
        let files = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(files, 2 + m.incrs.len(), "graine {seed} : fichiers non cités restants");
        let (got, _) = recover_state(&dir, f.core.art.now, false).unwrap();
        assert_eq!(got, state(&mut f.core.art), "graine {seed}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Disque lent à créer, renommer, supprimer (§8.6) : pendant une réécriture
/// dont les suites de fichiers sont retenues, les clients restent servis ; la
/// suite du flux attend le segment suivant ; au déblocage, la base est publiée,
/// les anciens fichiers supprimés, et la reprise redonne l'état servi.
#[test]
fn rewrite_with_slow_file_operations_keeps_serving() {
    let dir = test_dir("aof-slow-files");
    let mut f = fake_with_aof(&dir, Config { max_conn: 8, reserved: 0, ..Config::default() });
    f.idle_work = true;
    f.wall = Some(1_000);
    let c = f.connect();
    f.send(c, b"SET a 1\r\n");
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n");
    f.fs_stalled = true;
    f.send(c, b"BGSAVE\r\n");
    f.settle();
    assert_eq!(f.take(c), "+Background saving started\r\n");
    for i in 0..50 {
        f.send(c, format!("SET k{i} {i}\r\n").as_bytes());
        f.step();
    }
    f.settle();
    assert_eq!(f.take(c).matches("+OK").count(), 50, "les écritures sont servies pendant que le disque traîne");
    f.send(c, b"GET k49\r\n");
    f.settle();
    assert_eq!(f.take(c), "$2\r\n49\r\n");
    assert!(f.core.bgsave_running(), "fichier du dump pas encore créé");
    assert!(f.core.aof().unwrap().rewriting() && f.core.fs_pending());
    assert!(f.core.aof_pending() > 0, "la suite du flux attend le segment suivant");
    f.fs_stalled = false;
    while f.core.bgsave_running() || f.core.aof().unwrap().rewriting() || f.core.fs_pending() {
        f.step();
    }
    f.settle();
    assert_eq!(f.core.aof_pending(), 0);
    let m = manifest_of(&dir);
    assert_eq!((m.base.as_str(), m.incrs.as_slice()), ("base.2.rxdb", &["journal.2.rxj".to_string()][..]));
    assert!(!dir.join("base.1.rxdb").exists() && !dir.join("journal.1.rxj").exists());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3, "manifeste, base, segment");
    let (got, _) = recover_state(&dir, f.core.art.now, false).unwrap();
    assert_eq!(got, state(&mut f.core.art));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── appendfsync always : commit groupé (DURABILITY_DESIGN.md §7.5, test 8) ──

#[test]
fn always_holds_replies_until_their_fsync() {
    use crate::exec::aof::Fsync;
    let dir = test_dir("aof-always");
    let mut f = fake_with_aof_fsync(&dir, Config { max_conn: 8, reserved: 0, ..Config::default() }, Fsync::Always);
    f.wall = Some(1_000);
    let (w, r) = (f.connect(), f.connect());
    // Rien de non durable : une lecture répond tout de suite.
    f.send(r, b"GET a\r\n");
    f.settle();
    assert_eq!(f.take(r), "$-1\r\n");
    f.sync_stalled = true;
    f.send(w, b"SET a 1\r\n");
    f.settle();
    assert_eq!(f.take(w), "", "écriture acquittée avant son fsync");
    // Une lecture qui voit l'écriture non durable attend aussi.
    f.send(r, b"GET a\r\nPING\r\n");
    f.settle();
    assert_eq!(f.take(r), "", "lecture d'une donnée non durable");
    f.sync_stalled = false;
    f.settle();
    assert_eq!(f.take(w), "+OK\r\n");
    assert_eq!(f.take(r), "$1\r\n1\r\n+PONG\r\n");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Flux aléatoire en `always`, fsync retenus par moments : toutes les
/// réponses finissent par partir, dans l'ordre, et les fichiers redonnent
/// l'état servi.
#[test]
fn always_random_traffic_replays_and_answers() {
    use crate::exec::aof::Fsync;
    let dir = test_dir("aof-always-rand");
    for seed in 1..=10u64 {
        let _ = std::fs::remove_dir_all(&dir);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut f = fake_with_aof_fsync(&dir, Config { max_conn: 8, reserved: 0, flush_early: 1 + rng.below(100) as usize, ..Config::default() }, Fsync::Always);
        f.journal_write_max = (seed % 2 == 0).then(|| 1 + rng.below(100) as usize);
        f.wall = Some(1_000);
        let conns: Vec<Slot> = (0..1 + rng.below(3)).map(|_| f.connect()).collect();
        let mut sent = vec![0usize; conns.len()];
        for _ in 0..30 {
            f.sync_stalled = rng.below(3) == 0;
            for (i, &c) in conns.iter().enumerate() {
                for _ in 0..rng.below(8) {
                    f.send(c, b"INCR n\r\n");
                    sent[i] += 1;
                }
            }
            f.step();
        }
        f.sync_stalled = false;
        f.settle();
        for (i, &c) in conns.iter().enumerate() {
            let got = f.take(c);
            assert_eq!(got.matches(':').count(), sent[i], "graine {seed}");
        }
        let (got, _) = recover_state(&dir, 1_000, false).unwrap();
        assert_eq!(got, state(&mut f.core.art), "graine {seed}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Réplication (DURABILITY_DESIGN.md §10, test 7) ──────────────────────────

/// Des serveurs sur des faux noyaux, reliés par des fils simulés : le nœud
/// `i` écoute sur le port `7000 + i` ; un lien sortant vers ce port arrive à
/// une connexion acceptée par ce nœud, et inversement.
struct Net {
    n: Vec<Fake>,
    wires: Vec<Wire>,
    /// Réseau coupé : les `connect` échouent.
    down: bool,
    /// Fils bloqués : les octets attendent (connexions vivantes, rien ne passe).
    hold: bool,
}

/// Un fil : lien `link` du nœud `from` vers une connexion du nœud `to`
/// (`Err(fd)` : acceptée, pas encore installée).
struct Wire {
    from: usize,
    link: Slot,
    to: usize,
    conn: Result<Slot, i32>,
}

/// Port du nœud 1, cible de réplication du nœud 0.
const REPLICA_PORT: u16 = 7001;

impl Net {
    fn new(cfg: Config, nodes: usize) -> Self {
        Self { n: (0..nodes).map(|_| Fake::new(cfg.clone())).collect(), wires: Vec::new(), down: false, hold: false }
    }

    /// Un dossier de dumps par nœud (`dir/n<i>`), comme des serveurs séparés.
    fn with_dirs(cfg: Config, nodes: usize, dir: &std::path::Path) -> Self {
        let n = (0..nodes)
            .map(|i| {
                let d = dir.join(format!("n{i}"));
                std::fs::create_dir_all(&d).unwrap();
                Fake::new(Config { dir: Some(p(&d).into()), ..cfg.clone() })
            })
            .collect();
        Self { n, wires: Vec::new(), down: false, hold: false }
    }

    /// Les fils : connexions sortantes acceptées (ou refusées), octets passés.
    fn pump(&mut self) {
        for from in 0..self.n.len() {
            for (slot, addr) in std::mem::take(&mut self.n[from].connecting) {
                self.n[from].connecting.push((slot, addr));
                let to = (addr.port() - 7000) as usize;
                assert!(to < self.n.len() && to != from, "port {}", addr.port());
                if self.down {
                    self.n[from].finish_connect(slot, -111);
                    continue;
                }
                self.n[from].finish_connect(slot, 0);
                let fd = self.n[to].next_fd;
                self.n[to].next_fd += 1;
                self.n[to].completions.push(Completion::Accept { fd, more: true });
                self.wires.push(Wire { from, link: slot, to, conn: Err(fd) });
            }
        }
        let mut i = 0;
        while i < self.wires.len() {
            let Wire { from, link, to, conn } = self.wires[i];
            let c = match conn {
                Ok(c) => c,
                Err(fd) => match self.n[to].fd_slot.get(&fd) {
                    Some(&c) if self.n[to].peers.contains_key(&c) => {
                        self.wires[i].conn = Ok(c);
                        c
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                },
            };
            // Un côté fermé par son serveur : l'autre voit la fin.
            let (pl, rc) = (self.n[from].peers.contains_key(&link), self.n[to].peers.contains_key(&c));
            if !pl || !rc {
                self.wires.swap_remove(i);
                if pl {
                    self.n[from].peers.get_mut(&link).unwrap().eof = true;
                }
                if rc {
                    self.n[to].peers.get_mut(&c).unwrap().eof = true;
                }
                continue;
            }
            if !self.hold {
                let out = std::mem::take(&mut self.n[from].peers.get_mut(&link).unwrap().received);
                self.n[to].peers.get_mut(&c).unwrap().inbound.extend(out);
                let back = std::mem::take(&mut self.n[to].peers.get_mut(&c).unwrap().received);
                self.n[from].peers.get_mut(&link).unwrap().inbound.extend(back);
            }
            i += 1;
        }
    }

    fn step(&mut self) -> bool {
        let mut busy = false;
        for i in 0..self.n.len() {
            busy |= self.n[i].step();
            self.pump();
        }
        let in_flight = !self.hold
            && self.wires.iter().any(|w| {
                let live = |f: &Fake, s: Slot| f.peers.get(&s).is_some_and(|p| !p.received.is_empty() || !p.inbound.is_empty());
                live(&self.n[w.from], w.link) || w.conn.is_ok_and(|c| live(&self.n[w.to], c))
            });
        busy || in_flight || self.n.iter().any(|f| !f.connecting.is_empty())
    }

    fn settle(&mut self) {
        for _ in 0..200_000 {
            if !self.step() {
                return;
            }
        }
        panic!("pas de repos après 200 000 tours");
    }

    /// Une seconde passe : timers (reconnexion, RXTICK, fsync).
    fn second(&mut self) {
        for f in &mut self.n {
            f.clock += Duration::from_secs(1);
            f.core.set_now(f.clock);
            f.core.fire_timers();
        }
    }

    /// Coupure des fils (tous, ou ceux vers `to`) : les octets en route sont
    /// perdus, chaque côté voit une erreur (reset) ou la fin.
    fn cut_to(&mut self, to: Option<usize>) {
        let mut kept = Vec::new();
        for w in std::mem::take(&mut self.wires) {
            if to.is_some_and(|t| t != w.to) {
                kept.push(w);
                continue;
            }
            if let Some(peer) = self.n[w.from].peers.get_mut(&w.link) {
                peer.inbound.clear();
                peer.received.clear();
                if peer.armed {
                    peer.armed = false;
                    self.n[w.from].completions.push(Completion::Recv {
                        slot: w.link,
                        res: -104,
                        bid: None,
                        buf_more: false,
                        more: false,
                    });
                } else {
                    // Recv pas encore armé : la fin sera vue à l'armement.
                    peer.eof = true;
                }
            }
            if let Ok(c) = w.conn
                && let Some(peer) = self.n[w.to].peers.get_mut(&c)
            {
                peer.inbound.clear();
                peer.received.clear();
                peer.eof = true;
            }
        }
        self.wires = kept;
    }

    fn cut(&mut self) {
        self.cut_to(None);
    }

    fn cmd(f: &mut Fake, c: Slot, cmd: &[u8]) -> String {
        f.send(c, cmd);
        f.settle();
        f.take(c)
    }

    /// Jusqu'au repos, BGSAVE de synchronisation compris.
    fn settle_all(&mut self) {
        self.settle();
        while self.n.iter().any(|f| f.core.bgsave_running()) {
            self.step();
        }
        self.settle();
    }

    /// États métier égaux à celui du nœud 0 (vus à son horloge).
    fn assert_same(&mut self, what: &str) {
        let now = self.n[0].core.art.now;
        let want = state(&mut self.n[0].core.art);
        for i in 1..self.n.len() {
            self.n[i].core.art.set_now(now);
            assert_eq!(state(&mut self.n[i].core.art), want, "{what}, nœud {i}");
        }
    }
}

fn info_field(info: &str, name: &str) -> Option<String> {
    info.lines().find_map(|l| l.strip_prefix(&format!("{name}:")).map(str::to_string))
}

/// Réplica ≡ primaire après un flux aléatoire (tous types, TTL, horloge du
/// réplica décalée), avec des coupures du réseau : reprise partielle dans le
/// backlog, synchronisation complète au-delà ; BGSAVE des clients mêlés.
#[test]
fn replica_follows_random_traffic_across_cuts() {
    let dir = test_dir("repl-rand");
    // SAFETY: tests en mono-thread (RUST_TEST_THREADS=1).
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let seeds: u64 = std::env::var("URING_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    // `URING_SEED_FROM=n` : reprendre à une graine ; `SEED_LOG=1` : les afficher.
    let from: u64 = std::env::var("URING_SEED_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for seed in from..=seeds {
        if std::env::var("SEED_LOG").is_ok() {
            eprintln!("==== graine {seed}");
        }
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let backlog = if seed % 3 == 0 { 200 } else { 64 << 20 };
        let mut pair = Net::new(Config { max_conn: 16, reserved: 1, repl_backlog: backlog, ..Config::default() }, 2);
        pair.n[0].idle_work = seed % 2 == 0;
        let mut wall = 1_000u64;
        pair.n[0].wall = Some(wall);
        // L'horloge du réplica est ailleurs (±1 h) : elle ne doit pas compter.
        pair.n[1].wall = Some(if seed % 2 == 0 { wall + 3600 } else { wall - 900 });
        let conns: Vec<Slot> = (0..1 + rng.below(3)).map(|_| pair.n[0].connect()).collect();
        let admin = pair.n[0].connect();
        let start_at = rng.below(10);
        for round in 0..40 {
            if round == start_at {
                let r = Net::cmd(&mut pair.n[0], admin, format!("REPLICATE 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes());
                assert_eq!(r, "+OK\r\n");
            }
            for &c in &conns {
                for _ in 0..rng.below(10) {
                    let cmd = match rng.below(80) {
                        0 => vec!["BGSAVE".to_string()],
                        1 => vec!["WAIT".to_string(), "1".to_string(), "100".to_string()],
                        _ => random_write(&mut rng, true),
                    };
                    pair.n[0].send(c, &resp(&cmd));
                }
            }
            for _ in 0..1 + rng.below(3) {
                pair.step();
            }
            wall += rng.below(3);
            pair.n[0].wall = Some(wall);
            match rng.below(12) {
                0 => pair.cut(),
                1 | 2 => pair.second(),
                _ => {}
            }
        }
        // Réseau rétabli, la cible se reconnecte et rattrape.
        for _ in 0..3 {
            pair.second();
            pair.settle();
        }
        while pair.n[0].core.bgsave_running() {
            pair.step();
        }
        pair.settle();
        let info = Net::cmd(&mut pair.n[0], admin, b"INFO\r\n");
        assert_eq!(info_field(&info, "connected_replicas").as_deref(), Some("1"), "graine {seed} : {info}");
        pair.assert_same(&format!("graine {seed}"));
        // Le réplica sert les lectures, refuse les écritures.
        let rc = pair.n[1].connect();
        assert!(Net::cmd(&mut pair.n[1], rc, b"SET z 1\r\n").starts_with("-READONLY"), "graine {seed}");
        assert!(Net::cmd(&mut pair.n[1], rc, b"DBSIZE\r\n").starts_with(':'));
        let rinfo = Net::cmd(&mut pair.n[1], rc, b"INFO\r\n");
        assert_eq!(info_field(&rinfo, "role").as_deref(), Some("replica"));
        assert_eq!(info_field(&rinfo, "repl_seq"), info_field(&info, "repl_seq"), "graine {seed}");
    }
    crate::exec::persist::set_save_dir(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Coupure courte : reprise partielle dans le backlog. Backlog dépassé :
/// synchronisation complète. Les deux redonnent le même état.
#[test]
fn replica_resumes_from_backlog_or_resyncs() {
    let dir = test_dir("repl-backlog");
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let mut pair = Net::new(Config { max_conn: 16, reserved: 1, repl_backlog: 4096, ..Config::default() }, 2);
    pair.n[0].wall = Some(1_000);
    let (c, admin) = (pair.n[0].connect(), pair.n[0].connect());
    Net::cmd(&mut pair.n[0], c, b"SET a 1\r\n");
    assert_eq!(Net::cmd(&mut pair.n[0], admin, format!("REPLICATE 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes()), "+OK\r\n");
    pair.settle();
    while pair.n[0].core.bgsave_running() {
        pair.step();
    }
    pair.settle();
    pair.assert_same("première synchronisation");
    let sync = |pair: &mut Net, admin| {
        let info = Net::cmd(&mut pair.n[0], admin, b"INFO\r\n");
        (info_field(&info, "sync_full").unwrap(), info_field(&info, "sync_partial_ok").unwrap())
    };
    assert_eq!(sync(&mut pair, admin), ("1".into(), "0".into()));
    // Coupure courte : quelques blocs, dans le backlog.
    pair.cut();
    pair.down = true;
    pair.settle();
    for i in 0..5 {
        pair.n[0].send(c, format!("SET k{i} {i}\r\n").as_bytes());
        pair.settle();
    }
    pair.down = false;
    pair.second();
    pair.settle();
    pair.assert_same("reprise partielle");
    assert_eq!(sync(&mut pair, admin), ("1".into(), "1".into()));
    // Coupure longue : bien plus que 4 Ko de blocs.
    pair.cut();
    pair.down = true;
    pair.settle();
    for i in 0..300 {
        pair.n[0].send(c, format!("SET big{i} {}\r\n", "x".repeat(40)).as_bytes());
        pair.settle();
    }
    pair.down = false;
    pair.second();
    pair.settle();
    while pair.n[0].core.bgsave_running() {
        pair.step();
    }
    pair.settle();
    pair.assert_same("synchronisation complète");
    assert_eq!(sync(&mut pair, admin), ("2".into(), "1".into()));
    crate::exec::persist::set_save_dir(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Promotion : nouvel identifiant, écritures acceptées ; l'ancienne source,
/// toujours configurée, est refusée et n'écrase pas le promu. Une seconde
/// source pendant que la première est connectée est refusée.
#[test]
fn promotion_and_second_source() {
    let dir = test_dir("repl-promote");
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let mut pair = Net::new(Config { max_conn: 16, reserved: 1, ..Config::default() }, 2);
    pair.n[0].wall = Some(1_000);
    let c = pair.n[0].connect();
    Net::cmd(&mut pair.n[0], c, b"SET a 1\r\nSET b 2\r\n");
    Net::cmd(&mut pair.n[0], c, format!("REPLICATE 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes());
    pair.settle();
    while pair.n[0].core.bgsave_running() {
        pair.step();
    }
    pair.settle();
    pair.assert_same("synchronisé");
    // Une seconde source : refusée.
    let other = pair.n[1].connect();
    let id = "ab".repeat(20);
    assert!(Net::cmd(&mut pair.n[1], other, format!("RXHELLO {id}\r\n").as_bytes()).starts_with("-ERR already following"));
    // Un client qui n'est pas la source ne pousse pas de bloc.
    assert!(Net::cmd(&mut pair.n[1], other, b"RXTICK 5\r\n").starts_with("-ERR not the replication source"));
    let rc = pair.n[1].connect();
    let before = info_field(&Net::cmd(&mut pair.n[1], rc, b"INFO\r\n"), "master_replid").unwrap();
    assert_eq!(Net::cmd(&mut pair.n[1], rc, b"REPLICAOF NO ONE\r\n"), "+OK\r\n");
    let info = Net::cmd(&mut pair.n[1], rc, b"INFO\r\n");
    assert_eq!(info_field(&info, "role").as_deref(), Some("master"));
    assert_ne!(info_field(&info, "replid").unwrap(), before);
    assert_eq!(Net::cmd(&mut pair.n[1], rc, b"SET c 3\r\n"), "+OK\r\n");
    // L'ancienne source se reconnecte : refusée, le promu garde ses données.
    pair.second();
    pair.settle();
    pair.second();
    pair.settle();
    assert_eq!(Net::cmd(&mut pair.n[1], rc, b"GET c\r\n"), "$1\r\n3\r\n");
    let pinfo = Net::cmd(&mut pair.n[0], c, b"INFO\r\n");
    assert_eq!(info_field(&pinfo, "connected_replicas").as_deref(), Some("0"), "{pinfo}");
    assert_eq!(Net::cmd(&mut pair.n[0], c, format!("REPLICATE STOP 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes()), "+OK\r\n");
    crate::exec::persist::set_save_dir(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Un BGSAVE sur le réplica pendant qu'un bloc apporte FLUSHDB et
/// `UNLINK prefix*` : les clés disparaissent tout de suite, la mémoire est
/// rendue après la save (pas d'attente active), les états restent égaux.
#[test]
fn replica_bgsave_during_flush_blocks() {
    let dir = test_dir("repl-bgsave");
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let mut pair = Net::new(Config { max_conn: 16, reserved: 1, ..Config::default() }, 2);
    pair.n[0].wall = Some(1_000);
    let c = pair.n[0].connect();
    let sets: String = (0..30_000).map(|i| format!("SET u:{i} {i}\r\n")).collect();
    Net::cmd(&mut pair.n[0], c, sets.as_bytes());
    Net::cmd(&mut pair.n[0], c, format!("REPLICATE 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes());
    pair.settle();
    while pair.n[0].core.bgsave_running() {
        pair.step();
    }
    pair.settle();
    pair.assert_same("synchronisé");
    let rc = pair.n[1].connect();
    pair.n[1].send(rc, b"BGSAVE\r\n");
    while !pair.n[1].core.bgsave_running() {
        pair.step();
    }
    pair.n[0].send(c, b"UNLINK u:1*\r\nSET after 1\r\nFLUSHDB\r\nSET last 2\r\n");
    for _ in 0..20 {
        pair.step();
    }
    assert!(pair.n[1].core.bgsave_running(), "la save du réplica dure encore");
    pair.assert_same("pendant la save");
    pair.settle();
    assert!(!pair.n[1].core.bgsave_running());
    pair.assert_same("après la save");
    assert_eq!(pair.n[1].core.art.len(), 1);
    crate::exec::persist::set_save_dir(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// WAIT : répond quand le réplica a acquitté la dernière écriture de la
/// connexion ; réseau bloqué : au délai, avec le nombre qui l'a.
#[test]
fn wait_counts_acknowledged_replicas() {
    let dir = test_dir("repl-wait");
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let mut pair = Net::new(Config { max_conn: 16, reserved: 1, ..Config::default() }, 2);
    pair.n[0].wall = Some(1_000);
    let c = pair.n[0].connect();
    // Sans réplica : 0 tout de suite si on n'en demande aucun (WAIT 1 0
    // attendrait sans fin, comme Valkey).
    assert_eq!(Net::cmd(&mut pair.n[0], c, b"SET a 1\r\nWAIT 0 0\r\n"), "+OK\r\n:0\r\n");
    Net::cmd(&mut pair.n[0], c, format!("REPLICATE 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes());
    pair.settle();
    while pair.n[0].core.bgsave_running() {
        pair.step();
    }
    pair.settle();
    pair.n[0].send(c, b"SET b 2\r\nWAIT 1 0\r\nGET b\r\n");
    pair.settle();
    assert_eq!(pair.n[0].take(c), "+OK\r\n:1\r\n$1\r\n2\r\n");
    // Fil bloqué : rien n'arrive au réplica, WAIT attend son délai.
    pair.hold = true;
    pair.n[0].send(c, b"SET c 3\r\nWAIT 1 500\r\nPING\r\n");
    for _ in 0..50 {
        pair.step();
    }
    assert_eq!(pair.n[0].take(c), "+OK\r\n", "WAIT attend, la suite aussi");
    pair.n[0].clock += Duration::from_millis(600);
    let clock = pair.n[0].clock;
    pair.n[0].core.set_now(clock);
    pair.n[0].core.fire_timers();
    pair.settle();
    assert_eq!(pair.n[0].take(c), ":0\r\n+PONG\r\n");
    // Fil rétabli : le WAIT suivant compte le réplica.
    pair.hold = false;
    pair.n[0].send(c, b"WAIT 1 0\r\n");
    pair.settle();
    assert_eq!(pair.n[0].take(c), ":1\r\n");
    // Sur le réplica : refusé.
    let rc = pair.n[1].connect();
    assert!(Net::cmd(&mut pair.n[1], rc, b"WAIT 1 0\r\n").starts_with("-ERR WAIT cannot be used with replica"));
    crate::exec::persist::set_save_dir(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Serveur sur le faux noyau qui reprend l'AOF existant de `dir`.
fn fake_recover(dir: &std::path::Path, cfg: Config) -> Fake {
    use crate::exec::aof;
    let mut f = Fake::new(cfg);
    let res = aof::recover(&mut f.core.art, p(dir), true).unwrap().expect("manifeste");
    let w = aof::AofWriter::open(p(dir), &res, aof::Fsync::EverySec).unwrap();
    f.core.set_aof(crate::exec::journal::Stream::new(res.cursor), w);
    f
}

/// Flux aléatoire du nœud 0 (tous types, TTL, BGSAVE, WAIT), coupures au
/// hasard des fils vers `cut_targets`.
fn net_traffic(net: &mut Net, rng: &mut Rng, conns: &[Slot], rounds: usize, cut_targets: &[usize]) {
    let mut wall = net.n[0].wall.unwrap_or(1_000);
    for _ in 0..rounds {
        for &c in conns {
            for _ in 0..rng.below(10) {
                let cmd = match rng.below(80) {
                    0 => vec!["BGSAVE".to_string()],
                    1 => vec!["WAIT".to_string(), "1".to_string(), "100".to_string()],
                    _ => random_write(rng, true),
                };
                net.n[0].send(c, &resp(&cmd));
            }
        }
        for _ in 0..1 + rng.below(3) {
            net.step();
        }
        wall += rng.below(3);
        net.n[0].wall = Some(wall);
        match rng.below(12) {
            0 if !cut_targets.is_empty() => net.cut_to(Some(cut_targets[rng.below(cut_targets.len() as u64) as usize])),
            1 | 2 => net.second(),
            _ => {}
        }
    }
    for _ in 0..3 {
        net.second();
        net.settle_all();
    }
}

/// Réplica avec AOF : ses fichiers redonnent l'état servi, le manifeste le
/// marque réplica ; redémarré sur ses fichiers, il revient en mode suivi et
/// reprend dans le backlog du primaire (pas de nouveau dump).
#[test]
fn replica_with_aof_restarts_as_replica() {
    let dir = test_dir("repl-raof");
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let seeds: u64 = std::env::var("URING_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    for seed in 1..=seeds {
        let rdir = dir.join(format!("r{seed}"));
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
        let cfg = Config { max_conn: 16, reserved: 1, ..Config::default() };
        let mut net = Net::new(cfg.clone(), 2);
        net.n[1] = fake_with_aof(&rdir, cfg.clone());
        net.n[1].journal_write_max = (seed % 2 == 0).then(|| 1 + rng.below(200) as usize);
        net.n[0].wall = Some(1_000);
        let conns: Vec<Slot> = (0..2).map(|_| net.n[0].connect()).collect();
        let admin = net.n[0].connect();
        Net::cmd(&mut net.n[0], admin, format!("REPLICATE 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes());
        net_traffic(&mut net, &mut rng, &conns, 30, &[1]);
        net.assert_same(&format!("graine {seed}"));
        let m = crate::exec::aof::Manifest::read(p(&rdir)).unwrap().unwrap();
        assert!(m.follow.is_some(), "graine {seed} : manifeste du réplica");
        let mut back = OxidArt::new();
        crate::exec::aof::recover(&mut back, p(&rdir), false).unwrap().unwrap();
        back.set_now(net.n[0].core.art.now);
        assert_eq!(state(&mut back), state(&mut net.n[0].core.art), "graine {seed} : fichiers du réplica");
        // Redémarrage du réplica sur ses fichiers.
        let before = info_field(&Net::cmd(&mut net.n[0], admin, b"INFO\r\n"), "sync_partial_ok").unwrap();
        net.cut_to(Some(1));
        net.n[1] = fake_recover(&rdir, cfg.clone());
        let rc = net.n[1].connect();
        let rinfo = Net::cmd(&mut net.n[1], rc, b"INFO\r\n");
        assert_eq!(info_field(&rinfo, "role").as_deref(), Some("replica"), "graine {seed}");
        assert!(Net::cmd(&mut net.n[1], rc, b"SET x 1\r\n").starts_with("-READONLY"));
        for i in 0..5 {
            net.n[0].send(conns[0], format!("SET after{i} {i}\r\n").as_bytes());
        }
        net.settle();
        for _ in 0..2 {
            net.second();
            net.settle_all();
        }
        net.assert_same(&format!("graine {seed}, après redémarrage"));
        let after = info_field(&Net::cmd(&mut net.n[0], admin, b"INFO\r\n"), "sync_partial_ok").unwrap();
        assert!(after.parse::<u64>().unwrap() > before.parse::<u64>().unwrap(), "graine {seed} : reprise partielle");
        drop(net);
        let _ = std::fs::remove_dir_all(&rdir);
    }
    crate::exec::persist::set_save_dir(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Chaîne primaire → réplica → réplica (le réplica du milieu avec ou sans
/// AOF) : coupures des deux fils, mêmes états au bout, même histoire.
#[test]
fn chained_replicas_follow_the_primary() {
    let dir = test_dir("repl-chain");
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let seeds: u64 = std::env::var("URING_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    let from: u64 = std::env::var("URING_SEED_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for seed in from..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let backlog = if seed % 3 == 0 { 300 } else { 64 << 20 };
        let cfg = Config { max_conn: 16, reserved: 1, repl_backlog: backlog, ..Config::default() };
        let ndir = dir.join(format!("s{seed}"));
        let mut net = Net::with_dirs(cfg.clone(), 3, &ndir);
        if seed % 2 == 0 {
            let mcfg = Config { dir: Some(p(&ndir.join("n1")).into()), ..cfg.clone() };
            net.n[1] = fake_with_aof(&ndir.join("m-aof"), mcfg);
        }
        net.n[0].wall = Some(1_000);
        let conns: Vec<Slot> = (0..2).map(|_| net.n[0].connect()).collect();
        let (a0, a1) = (net.n[0].connect(), net.n[1].connect());
        Net::cmd(&mut net.n[0], a0, b"REPLICATE 127.0.0.1 7001\r\n");
        assert_eq!(Net::cmd(&mut net.n[1], a1, b"REPLICATE 127.0.0.1 7002\r\n"), "+OK\r\n");
        net_traffic(&mut net, &mut rng, &conns, 40, &[1, 2]);
        net.assert_same(&format!("graine {seed}"));
        let id = |net: &mut Net, i: usize| {
            let c = net.n[i].connect();
            let info = Net::cmd(&mut net.n[i], c, b"INFO\r\n");
            (info_field(&info, "replid").or(info_field(&info, "master_replid")), info_field(&info, "repl_seq"))
        };
        let (p, m, r) = (id(&mut net, 0), id(&mut net, 1), id(&mut net, 2));
        assert_eq!(p, m, "graine {seed} : le milieu suit le primaire");
        assert_eq!(m, r, "graine {seed} : le bout suit la même histoire");
        // Segments préalloués (64 Mio) : pas de reste d'une graine à l'autre.
        drop(net);
        let _ = std::fs::remove_dir_all(&ndir);
    }
    crate::exec::persist::set_save_dir(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plafond de sortie (MEMORY_ARCH.md §10.5) : un client qui ne lit plus et envoie un long
/// pipeline de grosses réponses ne fait pas grossir sa sortie au-delà de ~2 plafonds ; la
/// suite attend dans l'entrée, et tout arrive dans l'ordre quand il se remet à lire.
#[test]
fn output_is_capped_for_a_client_that_does_not_read() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    let c = f.connect();
    let val = "v".repeat(100_000);
    f.send(c, format!("*3\r\n$3\r\nSET\r\n$3\r\nbig\r\n$100000\r\n{val}\r\n").as_bytes());
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n");
    f.peers.get_mut(&c).unwrap().stalled = true;
    let n = 300; // 30 Mo de réponses
    for _ in 0..n {
        f.send(c, b"GET big\r\n");
    }
    f.settle();
    let cap = Config::default().out_backpressure;
    let pending = f.core.pending_out_of(c);
    assert!(pending <= 2 * cap + 2 * 100_100, "sortie en attente : {pending} o (plafond {cap})");
    // Le client relit : tout part, dans l'ordre.
    let p = f.peers.get_mut(&c).unwrap();
    p.stalled = false;
    if std::mem::take(&mut p.send_pending) {
        let bytes = f.core.pending_send(c).to_vec();
        f.peers.get_mut(&c).unwrap().received.extend_from_slice(&bytes);
        f.completions.push(Completion::Send { slot: c, res: bytes.len() as i32 });
    }
    f.settle();
    let got = f.take(c);
    let one = format!("$100000\r\n{val}\r\n");
    assert_eq!(got.len(), n * one.len());
    assert!(got.as_bytes().chunks(one.len()).all(|r| r == one.as_bytes()));
    assert_eq!(f.core.pending_out_of(c), 0);
}

/// Grande valeur (≥ `exec::BIG_REPLY`) : envoyée depuis l'arbre après tout ce qui précède,
/// sans passer par `out` ; dans l'ordre au milieu d'un pipeline, même si la clé est
/// supprimée pendant l'envoi ; la sortie en attente ne compte pas la valeur.
#[test]
fn big_value_is_sent_from_the_tree() {
    let mut f = Fake::new(Config { max_conn: 8, ..Config::default() });
    f.sends = Sends::Half;
    let c = f.connect();
    let d = f.connect();
    let val = "x".repeat(3 << 20);
    f.send(c, format!("*3\r\n$3\r\nSET\r\n$3\r\nbig\r\n${}\r\n{val}\r\n", val.len()).as_bytes());
    f.settle();
    assert_eq!(f.take(c), "+OK\r\n");
    f.peers.get_mut(&c).unwrap().stalled = true;
    f.send(c, b"PING\r\nGET big\r\nECHO after\r\nGET big\r\n");
    f.settle();
    // pendant l'envoi, un autre client supprime la clé : la valeur en vol reste valide
    f.send(d, b"DEL big\r\n");
    f.settle();
    assert_eq!(f.take(d), ":1\r\n");
    assert!(f.core.pending_out_of(c) < 1 << 20, "la valeur n'est pas recopiée : {}", f.core.pending_out_of(c));
    let p = f.peers.get_mut(&c).unwrap();
    p.stalled = false;
    if std::mem::take(&mut p.send_pending) {
        let bytes = f.core.pending_send(c).to_vec();
        let n = bytes.len().div_ceil(2);
        f.peers.get_mut(&c).unwrap().received.extend_from_slice(&bytes[..n]);
        f.completions.push(Completion::Send { slot: c, res: n as i32 });
    }
    f.settle();
    let one = format!("${}\r\n{val}\r\n", val.len());
    assert_eq!(f.take(c), format!("+PONG\r\n{one}$5\r\nafter\r\n$-1\r\n"));
}

/// Réplica en ligne qui ne lit plus : au-delà de `repl_online_lag` (+ le plus gros bloc)
/// en attente, il est déconnecté (trois tampons de 2 Mio par défaut, MEMORY_ARCH.md §10.5).
#[test]
fn online_replica_too_slow_is_dropped() {
    let dir = test_dir("repl-slow");
    crate::exec::persist::set_save_dir(Some(dir.to_str().unwrap().into()));
    let mut pair = Net::new(Config { max_conn: 16, reserved: 1, repl_online_lag: 64 << 10, ..Config::default() }, 2);
    pair.n[0].wall = Some(1_000);
    let (c, admin) = (pair.n[0].connect(), pair.n[0].connect());
    Net::cmd(&mut pair.n[0], c, b"SET a 1\r\n");
    assert_eq!(Net::cmd(&mut pair.n[0], admin, format!("REPLICATE 127.0.0.1 {REPLICA_PORT}\r\n").as_bytes()), "+OK\r\n");
    pair.settle();
    while pair.n[0].core.bgsave_running() {
        pair.step();
    }
    pair.settle();
    pair.assert_same("synchronisation");
    let online = |pair: &mut Net| {
        let info = Net::cmd(&mut pair.n[0], admin, b"INFO\r\n");
        info_field(&info, "connected_replicas").unwrap()
    };
    assert_eq!(online(&mut pair), "1");
    // Quelques écritures sous la limite : le réplica bloqué reste branché.
    let link = pair.wires[0].link;
    pair.n[0].peers.get_mut(&link).unwrap().stalled = true;
    let val = "v".repeat(1000);
    for i in 0..20 {
        pair.n[0].send(c, format!("SET k{i} {val}\r\n").as_bytes());
        pair.settle();
    }
    assert_eq!(online(&mut pair), "1", "20 Ko en attente : sous la limite");
    // Au-delà : déconnecté.
    for i in 20..200 {
        pair.n[0].send(c, format!("SET k{i} {val}\r\n").as_bytes());
        pair.settle();
    }
    assert_eq!(online(&mut pair), "0", "200 Ko en attente : trop lent");
    let _ = std::fs::remove_dir_all(&dir);
}
