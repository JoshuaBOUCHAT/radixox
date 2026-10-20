//! Flux de commandes de l'AOF et de la réplication (DURABILITY_DESIGN.md) :
//! les **commandes reçues**, octet pour octet, rangées en blocs d'un tour qui
//! portent le `now` d'oxidart. Les fonctions de commande (`exec/*.rs`) n'en
//! savent rien : le cœur appelle [`plan`] avant d'exécuter une commande et
//! [`finish`] après, avec les octets de sa réponse.
//!
//! - [`plan`] / [`finish`] : ce qu'une commande laisse dans le flux (§3) ;
//! - [`Stream`] : blocs en construction et blocs fermés (§6.1, §7.1) ;
//! - [`Block`] / [`read_block`] : format et validation d'un bloc ;
//! - [`apply`] : rejeu d'une charge sur l'arbre (§9, réplica §10).

use std::collections::VecDeque;

use oxidart::OxidArt;
use radixox_lib::cmd::{Cmd, Parse, SetCondition};
use radixox_lib::shared_frame::{encode_array_len, encode_bulk};

use super::client::Client;
use super::jobs::{FlushJob, Step, UnlinkJob};
use super::{Executed, execute_into};

// ── Ce qu'une commande laisse dans le flux (§3) ─────────────────────────────

/// Décidé avant l'exécution (la commande est consommée par l'exécuteur) ;
/// emprunte les octets reçus, pas la commande.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Journal<'a> {
    /// Lecture, commande de connexion ou de serveur.
    Skip,
    /// La frame reçue, sauf réponse d'erreur.
    Verbatim,
    /// La frame reçue, sauf erreur ou réponse qui dit « rien changé » (`:0`,
    /// nul) : `SETNX`, `SET NX|XX`, `DEL`, `UNLINK`, `EXPIRE`, `PERSIST`,
    /// `HDEL`, `SADD`, `SREM`, `ZREM`. Décidé commande par commande : un `INCR`
    /// qui répond `:0` a écrit.
    IfChanged,
    /// `SPOP` : `SREM <clé> <membres tirés>`, lus dans la réponse (D4).
    SRem { key: &'a [u8] },
}

pub fn plan<'a>(cmd: &Cmd<'a>) -> Journal<'a> {
    match cmd {
        Cmd::SPop { key, .. } => Journal::SRem { key },
        Cmd::Set { opts, .. } => match opts.condition {
            SetCondition::Always => Journal::Verbatim,
            SetCondition::IfNotExists | SetCondition::IfExists => Journal::IfChanged,
        },
        Cmd::SetNx { .. }
        | Cmd::Del(_)
        | Cmd::Unlink(_)
        | Cmd::Expire { .. }
        | Cmd::Persist(_)
        | Cmd::HDel { .. }
        | Cmd::SAdd { .. }
        | Cmd::SRem { .. }
        | Cmd::ZRem { .. } => Journal::IfChanged,
        // Toutes les écritures (`write_weight` > 0) ; le reste est lu ou
        // propre à la connexion (§3.1).
        c if c.is_write() => Journal::Verbatim,
        _ => Journal::Skip,
    }
}

/// Après exécution : `reply` = octets de la réponse (vide si la connexion est
/// parquée : `UNLINK` long, dont l'effet est décidé au départ, §5) ; `raw` =
/// la frame reçue.
pub fn finish(j: Journal<'_>, reply: &[u8], raw: &[u8], now: u64, out: &mut Stream) {
    if reply.first() == Some(&b'-') {
        return;
    }
    match j {
        Journal::Skip => {}
        Journal::Verbatim => out.push(now, raw),
        Journal::IfChanged => {
            if reply != b":0\r\n" && reply != b"$-1\r\n" {
                out.push(now, raw);
            }
        }
        Journal::SRem { key } => {
            let mut members: Vec<&[u8]> = Vec::new();
            if !spop_members(reply, &mut members) {
                debug_assert!(false, "réponse de SPOP inattendue : {:?}", reply.escape_ascii().to_string());
                return;
            }
            if members.is_empty() {
                return;
            }
            let mut cmd = Vec::with_capacity(32 + key.len() + members.iter().map(|m| m.len() + 16).sum::<usize>());
            encode_array_len(&mut cmd, 2 + members.len());
            encode_bulk(&mut cmd, b"SREM");
            encode_bulk(&mut cmd, key);
            for m in &members {
                encode_bulk(&mut cmd, m);
            }
            out.push(now, &cmd);
        }
    }
}

/// Membres d'une réponse de SPOP : `$n` (un), `*m` de bulk strings, ou nul.
/// `false` : réponse d'une autre forme.
fn spop_members<'r>(reply: &'r [u8], into: &mut Vec<&'r [u8]>) -> bool {
    fn line(d: &[u8]) -> Option<(i64, usize)> {
        let end = d.windows(2).position(|w| w == b"\r\n")?;
        let n = std::str::from_utf8(&d[1..end]).ok()?.parse().ok()?;
        Some((n, end + 2))
    }
    fn bulk<'r>(d: &'r [u8], into: &mut Vec<&'r [u8]>) -> Option<usize> {
        if d.first() != Some(&b'$') {
            return None;
        }
        let (n, h) = line(d)?;
        if n < 0 {
            return Some(h);
        }
        let end = h.checked_add(n as usize)?;
        into.push(d.get(h..end)?);
        Some(end + 2)
    }
    match reply.first() {
        Some(b'$') => bulk(reply, into) == Some(reply.len()),
        Some(b'*') => {
            let Some((n, mut pos)) = line(reply) else { return false };
            for _ in 0..n.max(0) {
                match bulk(&reply[pos..], into) {
                    Some(k) => pos += k,
                    None => return false,
                }
            }
            pos == reply.len()
        }
        _ => false,
    }
}

// ── Format d'un bloc (§6.1) ─────────────────────────────────────────────────

pub const MAGIC: &[u8; 4] = b"RXJ1";
/// Sémantique des commandes + format. Une version inconnue refuse de démarrer.
pub const VERSION: u16 = 1;
pub const HEADER: usize = 40;
/// Remplissage d'un bloc : fermé dès qu'il atteint 1 MiB. Une commande plus
/// grosse fait un bloc à elle seule (une frame reçue est bornée par
/// `query_limit`, 1 Gio).
pub const FILL: usize = 1 << 20;
/// Plus grosse charge acceptée à la relecture.
pub const MAX_PAYLOAD: usize = 1 << 30;

pub const F_CONT: u16 = 1 << 0;
pub const F_CUT: u16 = 1 << 1;
const F_KNOWN: u16 = F_CONT | F_CUT;

/// En-tête d'un bloc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub flags: u16,
    pub len: u32,
    pub seq: u64,
    pub offset: u64,
    pub now: u64,
}

impl Block {
    /// Taille du bloc sur le flux, en-tête compris.
    pub fn size(&self) -> u64 {
        HEADER as u64 + self.len as u64
    }
}

fn crc(head: &[u8], payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&head[0..12]);
    h.update(&head[16..HEADER]);
    h.update(payload);
    h.finalize()
}

/// Écrit l'en-tête de `b` dans `head` (CRC compris, sur `payload`).
fn write_header(head: &mut [u8], b: &Block, payload: &[u8]) {
    head[0..4].copy_from_slice(MAGIC);
    head[4..6].copy_from_slice(&VERSION.to_le_bytes());
    head[6..8].copy_from_slice(&b.flags.to_le_bytes());
    head[8..12].copy_from_slice(&b.len.to_le_bytes());
    head[16..24].copy_from_slice(&b.seq.to_le_bytes());
    head[24..32].copy_from_slice(&b.offset.to_le_bytes());
    head[32..40].copy_from_slice(&b.now.to_le_bytes());
    let c = crc(head, payload);
    head[12..16].copy_from_slice(&c.to_le_bytes());
}

/// Pourquoi un bloc est refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadBlock {
    /// Moins d'octets qu'annoncé (écriture coupée) ou fin du flux.
    Truncated,
    /// En-tête tout à zéro : fin normale d'un segment préalloué.
    Zero,
    Magic,
    Version(u16),
    Flags(u16),
    TooBig,
    Crc,
    /// `seq`, `offset` ou `now` ne suit pas le bloc précédent.
    Sequence,
}

/// Position attendue du prochain bloc : continuité vérifiée par [`read_block`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// `seq` du dernier bloc lu (0 : aucun).
    pub seq: u64,
    /// Offset de fin du dernier bloc lu.
    pub offset: u64,
    pub now: u64,
}

/// Lit le bloc au début de `d` : en-tête et charge, validés (§6.1). Avance
/// `cur` s'il est valide.
pub fn read_block<'d>(d: &'d [u8], cur: &mut Cursor) -> Result<(Block, &'d [u8]), BadBlock> {
    let head = d.get(..HEADER).ok_or(BadBlock::Truncated)?;
    if head.iter().all(|&b| b == 0) {
        return Err(BadBlock::Zero);
    }
    if &head[0..4] != MAGIC {
        return Err(BadBlock::Magic);
    }
    let u16_at = |i: usize| u16::from_le_bytes([head[i], head[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
    let u64_at = |i: usize| u64::from_le_bytes(head[i..i + 8].try_into().unwrap());
    let version = u16_at(4);
    if version != VERSION {
        return Err(BadBlock::Version(version));
    }
    let flags = u16_at(6);
    if flags & !F_KNOWN != 0 {
        return Err(BadBlock::Flags(flags));
    }
    let b = Block { flags, len: u32_at(8), seq: u64_at(16), offset: u64_at(24), now: u64_at(32) };
    if b.len as usize > MAX_PAYLOAD {
        return Err(BadBlock::TooBig);
    }
    let payload = d.get(HEADER..HEADER + b.len as usize).ok_or(BadBlock::Truncated)?;
    if crc(head, payload) != u32_at(12) {
        return Err(BadBlock::Crc);
    }
    if b.seq != cur.seq + 1 || b.offset != cur.offset || b.now < cur.now {
        return Err(BadBlock::Sequence);
    }
    *cur = Cursor { seq: b.seq, offset: b.offset + b.size(), now: b.now };
    Ok((b, payload))
}

// ── Construction des blocs (§7.1) ───────────────────────────────────────────

/// Le flux du serveur : bloc en cours, blocs fermés en attente de leurs
/// consommateurs (disque, réplicas ; le test les lit).
pub struct Stream {
    /// Bloc en cours : en-tête réservé puis charge.
    cur: Vec<u8>,
    cur_now: u64,
    cur_flags: u16,
    /// Blocs fermés, encodés (en-tête + charge), dans l'ordre.
    pub closed: VecDeque<Vec<u8>>,
    /// Prochain `seq` (le premier bloc d'une histoire porte 1).
    next_seq: u64,
    /// Octets du flux avant le bloc en cours.
    offset: u64,
    /// Plus grand `now` posé sur l'arbre, écrit ou non (monotonie, §4).
    last_now: u64,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new(Cursor::default())
    }
}

impl Stream {
    /// Reprend après le bloc décrit par `at` (rejeu ou base).
    pub fn new(at: Cursor) -> Self {
        Self {
            cur: vec![0; HEADER],
            cur_now: 0,
            cur_flags: 0,
            closed: VecDeque::new(),
            next_seq: at.seq + 1,
            offset: at.offset,
            last_now: at.now,
        }
    }

    /// Position après le dernier bloc fermé.
    pub fn cursor(&self) -> Cursor {
        Cursor { seq: self.next_seq - 1, offset: self.offset, now: self.last_now }
    }

    /// Horloge du tour, rendue monotone : un `now` plus petit que le plus
    /// grand déjà posé sur l'arbre y est ramené. Pas seulement le dernier
    /// écrit : une lecture à `t` sans écriture peut avoir supprimé une clé
    /// expirée à `t`, qui revivrait si l'horloge reculait ici et pas au rejeu.
    pub fn observe_now(&mut self, now: u64) -> u64 {
        self.last_now = self.last_now.max(now);
        self.last_now
    }

    /// Ajoute une commande au bloc en cours. Un `now` différent (ne devrait
    /// pas arriver dans un tour) ferme le bloc.
    pub fn push(&mut self, now: u64, cmd: &[u8]) {
        let now = self.observe_now(now);
        if self.cur.len() > HEADER && now != self.cur_now {
            self.close(0);
        }
        self.cur_now = now;
        self.cur.extend_from_slice(cmd);
        if self.cur.len() - HEADER >= FILL {
            // Suite du même tour : même `now`, drapeau CONT sur le suivant.
            self.close(0);
            self.cur_flags = F_CONT;
            self.cur_now = now;
        }
    }

    /// Le bloc en cours n'a pas de commande.
    pub fn is_empty(&self) -> bool {
        self.cur.len() == HEADER
    }

    /// Fin de tour : ferme le bloc en cours s'il n'est pas vide.
    pub fn end_turn(&mut self) {
        if !self.is_empty() {
            self.close(0);
        }
        self.cur_flags = 0;
    }

    fn close(&mut self, extra: u16) {
        let len = self.cur.len() - HEADER;
        let b = Block {
            flags: self.cur_flags | extra,
            len: len as u32,
            seq: self.next_seq,
            offset: self.offset,
            now: self.cur_now,
        };
        let mut blk = std::mem::replace(&mut self.cur, vec![0; HEADER]);
        let (head, payload) = blk.split_at_mut(HEADER);
        write_header(head, &b, payload);
        self.closed.push_back(blk);
        self.next_seq += 1;
        self.offset += b.size();
        self.cur_flags = 0;
    }
}

// ── Rejeu (§9) ──────────────────────────────────────────────────────────────

/// Charge illisible ou commande qui n'a rien à faire dans un flux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadPayload(pub String);

/// Applique la charge d'un bloc : chaque commande par l'exécuteur normal, avec
/// un client interne authentifié, sans réponse ni journalisation. L'appelant
/// a fait `art.set_now(bloc.now)`.
pub fn apply(art: &mut OxidArt, payload: &[u8]) -> Result<(), BadPayload> {
    let mut client = Client::new();
    client.authed = true;
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < payload.len() {
        let (cmd, n) = match Cmd::parse(&payload[pos..]) {
            Parse::Done(cmd, n) => (cmd, n),
            Parse::Incomplete => return Err(BadPayload(format!("commande coupée à l'octet {pos}"))),
            Parse::Invalid(e) => return Err(BadPayload(format!("octet {pos} : {e}"))),
        };
        pos += n;
        if matches!(cmd, Cmd::Invalid { .. }) || !matches!(plan(&cmd), Journal::Verbatim | Journal::IfChanged) {
            return Err(BadPayload(format!(
                "commande hors flux à l'octet {} : {:?}",
                pos - n,
                payload[pos - n..pos].escape_ascii().to_string()
            )));
        }
        out.clear();
        match execute_into(cmd, art, &mut client, &mut out) {
            Executed::Done => {}
            Executed::Runtime(Cmd::Unlink(keys)) => {
                let mut job = UnlinkJob::start(art, &keys);
                while job.step(art, usize::MAX) == Step::More {}
            }
            Executed::Runtime(Cmd::FlushDb) => {
                let mut job = FlushJob::start(art);
                while job.step(art, usize::MAX) == Step::More {}
            }
            Executed::Runtime(_) => unreachable!("plan() n'accepte que des écritures"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_with(cmds: &[(u64, &[u8])]) -> Stream {
        let mut s = Stream::default();
        for &(now, c) in cmds {
            s.push(now, c);
            s.end_turn();
        }
        s
    }

    fn concat(s: &Stream) -> Vec<u8> {
        s.closed.iter().flatten().copied().collect()
    }

    #[test]
    fn blocks_roundtrip_and_chain() {
        let s = stream_with(&[(10, b"SET a 1\r\n"), (10, b"SET b 2\r\n"), (12, b"DEL a\r\n")]);
        let d = concat(&s);
        let mut cur = Cursor::default();
        let mut pos = 0;
        let mut payloads = Vec::new();
        while pos < d.len() {
            let (b, p) = read_block(&d[pos..], &mut cur).unwrap();
            pos += b.size() as usize;
            payloads.push((b.seq, b.now, p.to_vec()));
        }
        assert_eq!(payloads.len(), 3);
        assert_eq!(payloads[2], (3, 12, b"DEL a\r\n".to_vec()));
        assert_eq!(cur, s.cursor());
    }

    #[test]
    fn now_is_monotone() {
        let s = stream_with(&[(10, b"SET a 1\r\n"), (7, b"SET b 2\r\n")]);
        let d = concat(&s);
        let mut cur = Cursor::default();
        let (b1, _) = read_block(&d, &mut cur).unwrap();
        let (b2, _) = read_block(&d[b1.size() as usize..], &mut cur).unwrap();
        assert_eq!((b1.now, b2.now), (10, 10));
    }

    #[test]
    fn big_turn_is_split_with_cont() {
        let mut s = Stream::default();
        let val = vec![b'x'; 300 << 10];
        let mut cmd = Vec::new();
        encode_array_len(&mut cmd, 3);
        encode_bulk(&mut cmd, b"SET");
        encode_bulk(&mut cmd, b"k");
        encode_bulk(&mut cmd, &val);
        for _ in 0..5 {
            s.push(5, &cmd);
        }
        s.end_turn();
        let d = concat(&s);
        let mut cur = Cursor::default();
        let mut pos = 0;
        let mut flags = Vec::new();
        while pos < d.len() {
            let (b, _) = read_block(&d[pos..], &mut cur).unwrap();
            pos += b.size() as usize;
            flags.push(b.flags);
        }
        // 4 commandes remplissent le premier bloc, la 5e part dans la suite.
        assert_eq!(flags, vec![0, F_CONT]);
    }

    #[test]
    fn every_corruption_is_refused() {
        let s = stream_with(&[(10, b"SET a 1\r\n"), (11, b"SET b 2\r\n")]);
        let d = concat(&s);
        let first = HEADER + 9;
        // Troncature à chaque offset du premier bloc.
        for cut in 0..first {
            let mut cur = Cursor::default();
            assert!(read_block(&d[..cut], &mut cur).is_err(), "coupé à {cut}");
        }
        // Chaque octet altéré (sauf ceux qui ne changent rien : aucun).
        for i in 0..first {
            let mut e = d.clone();
            e[i] ^= 0x40;
            let mut cur = Cursor::default();
            assert!(read_block(&e, &mut cur).is_err(), "octet {i}");
        }
        // Bloc sauté : seq discontinu.
        let mut cur = Cursor::default();
        assert_eq!(read_block(&d[first..], &mut cur), Err(BadBlock::Sequence));
        // En-tête nul : fin normale.
        let mut cur = Cursor::default();
        assert_eq!(read_block(&[0; HEADER], &mut cur), Err(BadBlock::Zero));
    }

    #[test]
    fn unknown_flag_is_refused() {
        let mut s = Stream::default();
        s.push(1, b"SET a 1\r\n");
        s.close(1 << 5);
        let d = concat(&s);
        let mut cur = Cursor::default();
        assert_eq!(read_block(&d, &mut cur), Err(BadBlock::Flags(1 << 5)));
    }

    #[test]
    fn spop_reply_parsing() {
        let mut m = Vec::new();
        assert!(spop_members(b"$3\r\nabc\r\n", &mut m));
        assert_eq!(m, vec![b"abc".as_slice()]);
        m.clear();
        assert!(spop_members(b"*2\r\n$1\r\na\r\n$2\r\nbc\r\n", &mut m));
        assert_eq!(m, vec![b"a".as_slice(), b"bc".as_slice()]);
        m.clear();
        assert!(spop_members(b"$-1\r\n", &mut m) && m.is_empty());
        assert!(spop_members(b"*0\r\n", &mut m) && m.is_empty());
        assert!(!spop_members(b"*2\r\n$1\r\na\r\n", &mut m));
    }
}
