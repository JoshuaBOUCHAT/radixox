//! Tests du cœur sur un faux noyau (RUNTIME_ARCH.md §1.1 règle 3, §16) :
//! séquences de complétions fabriquées, invariants vérifiés à chaque tour.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

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
        }
    }

    fn small(bufs: Bid, buf_size: u32) -> Self {
        Self::new(Config { max_conn: 16, reserved: 1, bufs, buf_size, ..Config::default() })
    }

    /// Le noyau exécute les opérations soumises au tour précédent.
    fn apply_ops(&mut self) {
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
                Op::FileWrite { fd, off } => {
                    let buf = self.core.pending_file_write();
                    // SAFETY: fd ouvert par le BGSAVE, tampon valide.
                    let res = unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len(), off as i64) };
                    self.completions.push(Completion::File { res: res as i32 });
                }
                Op::FileSync { fd } => {
                    // SAFETY: fd ouvert par le BGSAVE.
                    let res = unsafe { libc::fdatasync(fd) };
                    self.completions.push(Completion::File { res });
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
    unsafe { std::env::set_var("RADIXOX_DIR", &dir) };
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
    unsafe { std::env::remove_var("RADIXOX_DIR") };
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
