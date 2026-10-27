//! SCAN avec curseur côté serveur (SCAN_DESIGN.md).
//!
//! Le client reçoit un entier ; le serveur garde, par curseur, la dernière clé
//! examinée et reprend strictement après (parcours ordonné de l'arbre : ni
//! doublon ni clé stable oubliée, même si l'arbre change entre deux appels).
//!
//! Les slots forment une seule liste doublement chaînée qui sert à la fois de
//! free list et de LRU :
//!
//! ```text
//! tête → [libres…] [actifs, du moins au plus récemment utilisé] ← queue
//! ```
//!
//! Allocation : la tête si elle est libre ou inactive depuis
//! `RADIXOX_SCAN_IDLE_SECS`, sinon la table grandit, jusqu'à
//! `RADIXOX_SCAN_MAX_CURSORS` ; au-delà, le *nouveau* scan est refusé (ceux en
//! cours continuent). Un curseur se lit `gen × max + idx + 1` : petit tant que
//! la table et les générations le sont, jamais 0, toujours ≤ i64::MAX.
//!
//! Chaque curseur appartient à la connexion qui l'a ouvert (même s'il est
//! repris depuis une autre) ; une connexion n'en garde au plus que
//! `RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT` actifs à la fois, pour qu'un client
//! qui abandonne ses scans ne prive pas les autres.

use std::cell::RefCell;
use std::collections::HashMap;

use oxidart::OxidArt;
use oxidart::krange::ScanPage;
use oxidart::regex::{GlobKind, glob_kind, glob_literal_prefix, glob_match};
use oxidart::value::RedisType;

use radixox_lib::shared_frame::SharedFrame as Frame;

const NIL: u32 = u32::MAX;
const DEFAULT_IDLE_SECS: u64 = 60;
const DEFAULT_MAX_CURSORS: u32 = 65536;
const DEFAULT_MAX_PER_CLIENT: u32 = 128;
/// Taille de page sans COUNT. Redis prend 10 (une indication chez lui) ; sans
/// COUNT, aucune taille n'est promise, et chaque page redescend depuis la
/// racine : 10 coûtait ~15× plus de CPU qu'une page de 1000 pour un scan
/// complet (redis-py `scan_iter` n'envoie pas COUNT par défaut).
const DEFAULT_COUNT: usize = 1000;

/// Clés examinées au plus par appel (expirées et rejetées par MATCH/TYPE
/// comprises) : borne la latence d'un appel quand le filtre retient peu.
/// `COUNT × 10` donnait ~320 µs par appel sur un motif rare (`*:99*`, 1M
/// clés) contre ~40 µs ici, au prix de plus d'allers-retours.
fn budget(count: usize) -> usize {
    count.max(1000)
}

struct Slot {
    generation: u32,
    prev: u32,
    next: u32,
    /// Horloge de l'arbre (secondes) au dernier appel.
    last_used: u64,
    /// Clé après laquelle reprendre ; `None` : slot libre.
    from: Option<Vec<u8>>,
    /// CLIENT ID de la connexion qui a ouvert le scan (slot actif seulement).
    owner: u64,
}

struct Cursors {
    slots: Vec<Slot>,
    head: u32,
    tail: u32,
    max: u32,
    idle_secs: u64,
    /// Génération maximale : `gen_limit × max + max ≤ i64::MAX`.
    gen_limit: u32,
    active: usize,
    per_client: u32,
    /// Curseurs actifs par propriétaire (entrées à 0 retirées).
    owners: HashMap<u64, u32>,
    warned_full: bool,
}

thread_local! {
    static CURSORS: RefCell<Cursors> = RefCell::new(Cursors::from_env());
}

/// Nombre de curseurs actifs (pour INFO).
pub fn active_cursors() -> usize {
    CURSORS.with(|c| c.borrow().active)
}

impl Cursors {
    fn from_env() -> Self {
        let idle_secs = env_or("RADIXOX_SCAN_IDLE_SECS", DEFAULT_IDLE_SECS, |_| true);
        let max = env_or("RADIXOX_SCAN_MAX_CURSORS", DEFAULT_MAX_CURSORS, |n| (1..NIL).contains(n));
        let per_client = env_or("RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT", DEFAULT_MAX_PER_CLIENT, |&n| n >= 1)
            .min(max);
        Self::new(max, idle_secs, per_client)
    }

    fn new(max: u32, idle_secs: u64, per_client: u32) -> Self {
        let gen_limit = (i64::MAX as u64 / max as u64 - 1).min(u32::MAX as u64) as u32;
        Self {
            slots: Vec::new(),
            head: NIL,
            tail: NIL,
            max,
            idle_secs,
            gen_limit,
            active: 0,
            per_client,
            owners: HashMap::new(),
            warned_full: false,
        }
    }

    /// Curseurs actifs de `owner`.
    fn owned(&self, owner: u64) -> u32 {
        self.owners.get(&owner).copied().unwrap_or(0)
    }

    /// Désactive `idx` (rend sa clé de reprise) ; compteurs à jour.
    fn deactivate(&mut self, idx: u32) -> Option<Vec<u8>> {
        let slot = &mut self.slots[idx as usize];
        let from = slot.from.take()?;
        let owner = slot.owner;
        self.active -= 1;
        let n = self.owners.get_mut(&owner).expect("propriétaire compté");
        *n -= 1;
        if *n == 0 {
            self.owners.remove(&owner);
        }
        Some(from)
    }

    fn encode(&self, idx: u32) -> u64 {
        self.slots[idx as usize].generation as u64 * self.max as u64 + idx as u64 + 1
    }

    /// Slot actif désigné par `cursor` (≠ 0), ou `None` : inconnu, terminé ou
    /// repris par un autre scan (génération changée).
    fn lookup(&self, cursor: u64) -> Option<u32> {
        let v = cursor.checked_sub(1)?;
        let (generation, idx) = (v / self.max as u64, (v % self.max as u64) as u32);
        let slot = self.slots.get(idx as usize)?;
        (slot.generation as u64 == generation && slot.from.is_some()).then_some(idx)
    }

    /// `owner` est à son plafond : libère ses curseurs inactifs depuis
    /// `idle_secs`. Les inactifs sont en tête (liste triée par dernier usage) ;
    /// ceux des autres croisés en route sont libérés aussi (ils étaient
    /// évinçables), donc chaque curseur n'est parcouru qu'une fois.
    fn reclaim_idle(&mut self, owner: u64, now: u64) -> bool {
        let mut i = self.head;
        while i != NIL {
            let slot = &self.slots[i as usize];
            let next = slot.next;
            if slot.from.is_some() {
                if now.saturating_sub(slot.last_used) < self.idle_secs {
                    return false; // la suite est plus récente
                }
                let mine = slot.owner == owner;
                self.deactivate(i);
                self.unlink(i);
                self.push_front(i);
                if mine {
                    return true;
                }
            }
            i = next;
        }
        false
    }

    /// Un slot pour un nouveau scan, retiré de la liste ; `None` : plafond
    /// atteint et aucun slot libre ni inactif.
    fn alloc(&mut self, now: u64) -> Option<u32> {
        let h = self.head;
        if h != NIL {
            let slot = &self.slots[h as usize];
            let reusable = match slot.from {
                None => true,
                Some(_) => now.saturating_sub(slot.last_used) >= self.idle_secs,
            };
            if reusable {
                self.deactivate(h); // scan abandonné, évincé (sans effet si libre)
                let slot = &mut self.slots[h as usize];
                slot.generation = if slot.generation >= self.gen_limit { 0 } else { slot.generation + 1 };
                self.unlink(h);
                return Some(h);
            }
        }
        if self.slots.len() < self.max as usize {
            self.slots.push(Slot { generation: 0, prev: NIL, next: NIL, last_used: 0, from: None, owner: 0 });
            return Some(self.slots.len() as u32 - 1);
        }
        None
    }

    /// Active `idx` pour `owner` avec sa clé de reprise et le place en queue
    /// (le plus récent). `idx` doit être hors de la liste et inactif.
    fn store(&mut self, idx: u32, owner: u64, from: Vec<u8>, now: u64) {
        let slot = &mut self.slots[idx as usize];
        debug_assert!(slot.from.is_none());
        slot.last_used = now;
        slot.from = Some(from);
        slot.owner = owner;
        self.active += 1;
        *self.owners.entry(owner).or_insert(0) += 1;
        self.push_back(idx);
    }

    /// Libère `idx` (scan terminé) et le place en tête. `idx` doit être hors
    /// de la liste.
    fn release(&mut self, idx: u32) {
        self.deactivate(idx);
        self.push_front(idx);
    }

    fn unlink(&mut self, idx: u32) {
        let (prev, next) = {
            let s = &self.slots[idx as usize];
            (s.prev, s.next)
        };
        match prev {
            NIL => self.head = next,
            p => self.slots[p as usize].next = next,
        }
        match next {
            NIL => self.tail = prev,
            n => self.slots[n as usize].prev = prev,
        }
        let s = &mut self.slots[idx as usize];
        s.prev = NIL;
        s.next = NIL;
    }

    fn push_front(&mut self, idx: u32) {
        let old = self.head;
        {
            let s = &mut self.slots[idx as usize];
            s.prev = NIL;
            s.next = old;
        }
        match old {
            NIL => self.tail = idx,
            o => self.slots[o as usize].prev = idx,
        }
        self.head = idx;
    }

    fn push_back(&mut self, idx: u32) {
        let old = self.tail;
        {
            let s = &mut self.slots[idx as usize];
            s.prev = old;
            s.next = NIL;
        }
        match old {
            NIL => self.head = idx,
            o => self.slots[o as usize].next = idx,
        }
        self.tail = idx;
    }
}

/// Variable d'environnement parsée et validée, sinon le défaut (avec message).
fn env_or<T: std::str::FromStr + std::fmt::Display + Copy>(
    name: &str,
    default: T,
    valid: impl Fn(&T) -> bool,
) -> T {
    match std::env::var(name) {
        Err(_) => default,
        Ok(v) => match v.parse::<T>() {
            Ok(n) if valid(&n) => n,
            _ => {
                eprintln!("[scan] {name}={v} invalide, {default}");
                default
            }
        },
    }
}

/// Une page de SCAN : (curseur suivant, 0 = terminé ; clés), ou l'erreur à
/// renvoyer.
pub fn cmd_scan(
    art: &OxidArt,
    client_id: u64,
    cursor: &[u8],
    pattern: Option<&[u8]>,
    count: Option<usize>,
    ty: Option<&[u8]>,
) -> Result<(u64, ScanPage), Frame> {
    let ty = match ty {
        None => None,
        Some(t) => Some(redis_type(t).ok_or_else(|| Frame::Error("ERR unknown type name".into()))?),
    };
    let count = count.unwrap_or(DEFAULT_COUNT);
    with_cursor(client_id, cursor, art.now, |from| {
        let page = scan_page(art, from, pattern, count, ty);
        let next = page.next.clone();
        (page, next)
    })
}

/// Curseur côté serveur, commun à SCAN et ZSCAN : `cursor` repris (0 : neuf), `page(from)`
/// rend sa page et la position (octets) où reprendre, `None` à la fin. Rend le curseur
/// suivant (0 : fini) et la page.
pub fn with_cursor<R>(
    client_id: u64,
    cursor: &[u8],
    now: u64,
    page: impl FnOnce(&[u8]) -> (R, Option<Vec<u8>>),
) -> Result<(u64, R), Frame> {
    let cursor: u64 = std::str::from_utf8(cursor)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Frame::Error("ERR invalid cursor".into()))?;
    CURSORS.with(|c| {
        let mut c = c.borrow_mut();
        // Reprise : le slot sort de la liste le temps de l'appel ; il garde
        // son propriétaire, même repris depuis une autre connexion.
        let (slot, from) = if cursor == 0 {
            (None, Vec::new())
        } else {
            let idx = c
                .lookup(cursor)
                .ok_or_else(|| Frame::Error("ERR invalid or expired cursor".into()))?;
            c.unlink(idx);
            let owner = c.slots[idx as usize].owner;
            let from = c.deactivate(idx).expect("slot actif");
            (Some((idx, owner)), from)
        };

        let (page, next) = page(&from);

        let Some(next) = next else {
            if let Some((idx, _)) = slot {
                c.release(idx);
            }
            return Ok((0, page));
        };
        let (idx, owner) = match slot {
            Some(s) => s,
            None if c.owned(client_id) >= c.per_client && !c.reclaim_idle(client_id, now) => {
                return Err(Frame::Error("ERR max number of SCAN cursors for this connection reached".into()));
            }
            None => match c.alloc(now) {
                Some(idx) => (idx, client_id),
                None => {
                    if !c.warned_full {
                        c.warned_full = true;
                        eprintln!(
                            "[scan] {} curseurs actifs (RADIXOX_SCAN_MAX_CURSORS) : nouveaux SCAN refusés",
                            c.max
                        );
                    }
                    return Err(Frame::Error("ERR max number of SCAN cursors reached".into()));
                }
            },
        };
        c.store(idx, owner, next, now);
        Ok((c.encode(idx), page))
    })
}

/// ZSCAN key cursor [MATCH] [COUNT] [NOSCORES] : position = score encodé (8 octets) puis
/// membre ; parcours dans l'ordre du ZSet, strictement après la dernière entrée rendue
/// (une entrée présente tout le long sort au moins une fois). Petit ZSet : en entier.
pub fn cmd_zscan(
    art: &mut OxidArt,
    client_id: u64,
    key: &[u8],
    cursor: &[u8],
    pattern: Option<&[u8]>,
    count: Option<usize>,
    noscores: bool,
    out: &mut Vec<u8>,
) {
    use radixox_lib::shared_frame::{encode_array_len, encode_bulk, extend_encode};
    let now = art.now;
    let mut err = None;
    let r = with_cursor(client_id, cursor, now, |from| {
        let from = (from.len() >= 8).then(|| (u64::from_be_bytes(from[..8].try_into().unwrap()), &from[8..]));
        match art.zscan_page(key, from, count.unwrap_or(10)) {
            Ok((items, next)) => (
                items,
                next.map(|(s, m)| {
                    let mut b = s.to_be_bytes().to_vec();
                    b.extend_from_slice(&m);
                    b
                }),
            ),
            Err(t) => {
                err = Some(t);
                (Vec::new(), None)
            }
        }
    });
    if err.is_some() {
        extend_encode(out, &Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into()));
        return;
    }
    match r {
        Err(e) => extend_encode(out, &e),
        Ok((next, items)) => {
            let items: Vec<_> = items.into_iter().filter(|(m, _)| pattern.is_none_or(|p| glob_match(p, m))).collect();
            encode_array_len(out, 2);
            encode_bulk(out, next.to_string().as_bytes());
            encode_array_len(out, items.len() * if noscores { 1 } else { 2 });
            for (m, s) in items {
                encode_bulk(out, &m);
                if !noscores {
                    oxidart::zset::with_score_text(s, |t| encode_bulk(out, t));
                }
            }
        }
    }
}

fn scan_page(
    art: &OxidArt,
    from: &[u8],
    pattern: Option<&[u8]>,
    count: usize,
    ty: Option<RedisType>,
) -> ScanPage {
    let budget = budget(count);
    let type_ok = |t: RedisType| ty.is_none_or(|want| want == t);
    match pattern.map(glob_kind) {
        // `user:*` (ou pas de MATCH) : tout le sous-arbre, sans test de motif.
        None => art.scan_keys(b"", from, count, budget, |_, t| type_ok(t)),
        Some(GlobKind::Prefix(p)) => art.scan_keys(p, from, count, budget, |_, t| type_ok(t)),
        Some(GlobKind::Exact(k)) => art.scan_keys(k, from, count, budget, |key, t| key == k && type_ok(t)),
        Some(GlobKind::Pattern) => {
            let pat = pattern.expect("motif présent");
            art.scan_keys(glob_literal_prefix(pat), from, count, budget, |key, t| {
                type_ok(t) && glob_match(pat, key)
            })
        }
    }
}

fn redis_type(name: &[u8]) -> Option<RedisType> {
    [RedisType::String, RedisType::Hash, RedisType::List, RedisType::Set, RedisType::ZSet]
        .into_iter()
        .find(|t| name.eq_ignore_ascii_case(t.as_str().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::{Cursors, NIL};

    /// Ordre de la liste, de la tête à la queue, vérifié dans les deux sens.
    fn order(c: &Cursors) -> Vec<u32> {
        let mut v = Vec::new();
        let mut i = c.head;
        while i != NIL {
            v.push(i);
            i = c.slots[i as usize].next;
        }
        let mut back = Vec::new();
        let mut i = c.tail;
        while i != NIL {
            back.push(i);
            i = c.slots[i as usize].prev;
        }
        back.reverse();
        assert_eq!(v, back, "chaînage incohérent");
        v
    }

    fn open(c: &mut Cursors, now: u64) -> Option<u64> {
        open_as(c, 1, now)
    }

    /// Ouverture comme `cmd_scan` : plafond par connexion, puis allocation.
    fn open_as(c: &mut Cursors, owner: u64, now: u64) -> Option<u64> {
        if c.owned(owner) >= c.per_client && !c.reclaim_idle(owner, now) {
            return None;
        }
        let idx = c.alloc(now)?;
        c.store(idx, owner, b"k".to_vec(), now);
        Some(c.encode(idx))
    }

    /// Compteurs cohérents avec les slots.
    fn check_counts(c: &Cursors) {
        let mut want = std::collections::HashMap::new();
        for s in c.slots.iter().filter(|s| s.from.is_some()) {
            *want.entry(s.owner).or_insert(0u32) += 1;
        }
        assert_eq!(c.owners, want);
        assert_eq!(c.active, want.values().sum::<u32>() as usize);
    }

    /// Reprise d'un curseur, comme `cmd_scan` : sort de la liste puis revient
    /// en queue (`done = false`) ou en tête, libre (`done = true`).
    fn resume(c: &mut Cursors, cursor: u64, now: u64, done: bool) -> Option<()> {
        let idx = c.lookup(cursor)?;
        c.unlink(idx);
        let owner = c.slots[idx as usize].owner;
        let from = c.deactivate(idx).unwrap();
        if done { c.release(idx) } else { c.store(idx, owner, from, now) }
        check_counts(c);
        Some(())
    }

    #[test]
    fn grows_while_cursors_are_recent_then_refuses() {
        let mut c = Cursors::new(3, 60, 3);
        let a = open(&mut c, 0).unwrap();
        let b = open(&mut c, 1).unwrap();
        let d = open(&mut c, 2).unwrap();
        assert_eq!((a, b, d), (1, 2, 3), "petits curseurs tant que gen = 0");
        assert_eq!(open(&mut c, 30), None, "plafond : tous récents");
        assert_eq!(c.active, 3);
        // les curseurs en cours restent valides
        assert!(resume(&mut c, a, 30, false).is_some());
        assert_eq!(order(&c), vec![1, 2, 0]);
    }

    #[test]
    fn idle_head_is_evicted_and_its_cursor_dies() {
        let mut c = Cursors::new(2, 60, 10);
        let a = open(&mut c, 0).unwrap();
        let b = open(&mut c, 10).unwrap();
        // a inactif depuis 61 s : repris, génération +1
        let e = open(&mut c, 61).unwrap();
        assert_ne!(e, a);
        assert_eq!(c.lookup(a), None, "ancien curseur rejeté");
        assert!(c.lookup(b).is_some());
        assert_eq!(c.active, 2);
        // b à 10 s, pas encore inactif à 61 : refus
        assert_eq!(open(&mut c, 61), None);
        // un client revenu avant l'éviction garde son scan (expiration paresseuse)
        assert!(resume(&mut c, b, 200, false).is_some());
    }

    #[test]
    fn finished_scan_frees_its_slot_at_the_head() {
        let mut c = Cursors::new(2, 60, 10);
        let a = open(&mut c, 0).unwrap();
        let b = open(&mut c, 0).unwrap();
        resume(&mut c, b, 1, true).unwrap();
        assert_eq!(c.lookup(b), None, "curseur terminé rejeté");
        assert_eq!(order(&c), vec![1, 0], "slot libre en tête");
        assert_eq!(c.active, 1);
        let f = open(&mut c, 1).unwrap();
        assert_eq!(f, 2 + 1 + 1, "slot 1 réutilisé, génération 1");
        assert!(c.lookup(a).is_some());
        assert_eq!(order(&c), vec![0, 1]);
    }

    #[test]
    fn cursor_stays_below_i64_max_when_generation_wraps() {
        let mut c = Cursors::new(u32::MAX - 1, 0, 1);
        assert!(c.gen_limit >= 1);
        open(&mut c, 0).unwrap();
        c.slots[0].generation = c.gen_limit;
        let worst = c.gen_limit as u64 * c.max as u64 + (c.max as u64 - 1) + 1;
        assert!(worst <= i64::MAX as u64);
        assert!(c.lookup(c.encode(0)).is_some());
        // inactif (idle 0) : réutilisé, la génération repart à 0
        let idx = c.alloc(1).unwrap();
        assert_eq!((idx, c.slots[0].generation), (0, 0));
    }

    #[test]
    fn per_connection_limit() {
        let mut c = Cursors::new(10, 60, 2);
        let a1 = open_as(&mut c, 7, 0).unwrap();
        let a2 = open_as(&mut c, 7, 0).unwrap();
        assert_eq!(open_as(&mut c, 7, 0), None, "plafond de la connexion 7");
        // les autres connexions ne sont pas touchées
        let b = open_as(&mut c, 8, 0).unwrap();
        check_counts(&c);
        // reprise depuis une autre connexion : le curseur reste à 7
        resume(&mut c, a1, 1, false).unwrap();
        assert_eq!((c.owned(7), c.owned(8)), (2, 1));
        // un scan terminé rend sa place
        resume(&mut c, a2, 1, true).unwrap();
        assert!(open_as(&mut c, 7, 1).is_some());
        // un scan abandonné évincé rend aussi la place de son propriétaire
        resume(&mut c, b, 1, true).unwrap();
        let mut c = Cursors::new(1, 60, 1);
        open_as(&mut c, 7, 0).unwrap();
        assert_eq!(open_as(&mut c, 8, 30), None, "table pleine, tête récente");
        open_as(&mut c, 8, 61).unwrap();
        assert_eq!((c.owned(7), c.owned(8)), (0, 1));
        check_counts(&c);
    }

    /// Un client à son plafond dont les curseurs sont abandonnés n'est pas
    /// bloqué : le balayage depuis la tête libère les inactifs jusqu'au
    /// premier des siens ; au-delà, rien n'est touché.
    #[test]
    fn capped_client_reclaims_its_idle_cursors() {
        let mut c = Cursors::new(100, 60, 2);
        let other_oldest = open_as(&mut c, 8, 0).unwrap();
        let old = open_as(&mut c, 7, 1).unwrap();
        let other_old = open_as(&mut c, 8, 5).unwrap();
        let recent = open_as(&mut c, 7, 50).unwrap();
        assert_eq!(open_as(&mut c, 7, 59), None, "rien d'inactif encore");
        let fresh = open_as(&mut c, 7, 70).unwrap();
        assert_eq!(c.lookup(other_oldest), None, "inactif croisé avant le sien : libéré");
        assert_eq!(c.lookup(old), None, "son curseur abandonné est libéré");
        assert!(c.lookup(other_old).is_some(), "après le sien : intact (expiration paresseuse)");
        assert!(c.lookup(recent).is_some() && c.lookup(fresh).is_some());
        assert_eq!((c.owned(7), c.owned(8)), (2, 1));
        check_counts(&c);
        order(&c);
    }
}

