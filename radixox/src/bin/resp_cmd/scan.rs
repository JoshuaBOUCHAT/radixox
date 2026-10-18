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

use std::cell::RefCell;

use oxidart::OxidArt;
use oxidart::krange::ScanPage;
use oxidart::regex::{GlobKind, glob_kind, glob_literal_prefix, glob_match};
use oxidart::value::RedisType;

use crate::Frame;

const NIL: u32 = u32::MAX;
const DEFAULT_IDLE_SECS: u64 = 60;
const DEFAULT_MAX_CURSORS: u32 = 65536;
/// COUNT par défaut, comme Redis.
const DEFAULT_COUNT: usize = 10;

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
    warned_full: bool,
}

thread_local! {
    static CURSORS: RefCell<Cursors> = RefCell::new(Cursors::from_env());
}

/// Nombre de curseurs actifs (pour INFO).
pub(crate) fn active_cursors() -> usize {
    CURSORS.with(|c| c.borrow().active)
}

impl Cursors {
    fn from_env() -> Self {
        let idle_secs = env_or("RADIXOX_SCAN_IDLE_SECS", DEFAULT_IDLE_SECS, |_| true);
        let max = env_or("RADIXOX_SCAN_MAX_CURSORS", DEFAULT_MAX_CURSORS, |n| (1..NIL).contains(n));
        Self::new(max, idle_secs)
    }

    fn new(max: u32, idle_secs: u64) -> Self {
        let gen_limit = (i64::MAX as u64 / max as u64 - 1).min(u32::MAX as u64) as u32;
        Self {
            slots: Vec::new(),
            head: NIL,
            tail: NIL,
            max,
            idle_secs,
            gen_limit,
            active: 0,
            warned_full: false,
        }
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
                let slot = &mut self.slots[h as usize];
                if slot.from.take().is_some() {
                    self.active -= 1; // scan abandonné, évincé
                }
                slot.generation = if slot.generation >= self.gen_limit { 0 } else { slot.generation + 1 };
                self.unlink(h);
                return Some(h);
            }
        }
        if self.slots.len() < self.max as usize {
            self.slots.push(Slot { generation: 0, prev: NIL, next: NIL, last_used: 0, from: None });
            return Some(self.slots.len() as u32 - 1);
        }
        None
    }

    /// Enregistre la reprise de `idx` et le place en queue (le plus récent).
    /// `idx` doit être hors de la liste.
    fn store(&mut self, idx: u32, from: Vec<u8>, now: u64) {
        let slot = &mut self.slots[idx as usize];
        slot.last_used = now;
        if slot.from.replace(from).is_none() {
            self.active += 1;
        }
        self.push_back(idx);
    }

    /// Libère `idx` (scan terminé) et le place en tête. `idx` doit être hors
    /// de la liste.
    fn release(&mut self, idx: u32) {
        if self.slots[idx as usize].from.take().is_some() {
            self.active -= 1;
        }
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
pub(crate) fn cmd_scan(
    art: &OxidArt,
    cursor: &[u8],
    pattern: Option<&[u8]>,
    count: Option<usize>,
    ty: Option<&[u8]>,
) -> Result<(u64, ScanPage), Frame> {
    let cursor: u64 = std::str::from_utf8(cursor)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Frame::Error("ERR invalid cursor".into()))?;
    let ty = match ty {
        None => None,
        Some(t) => Some(redis_type(t).ok_or_else(|| Frame::Error("ERR unknown type name".into()))?),
    };
    let count = count.unwrap_or(DEFAULT_COUNT);
    let now = art.now;

    CURSORS.with(|c| {
        let mut c = c.borrow_mut();
        // Reprise : le slot sort de la liste le temps de l'appel.
        let (slot, from) = if cursor == 0 {
            (None, Vec::new())
        } else {
            let idx = c
                .lookup(cursor)
                .ok_or_else(|| Frame::Error("ERR invalid or expired cursor".into()))?;
            c.unlink(idx);
            let from = c.slots[idx as usize].from.take().expect("slot actif");
            c.active -= 1;
            (Some(idx), from)
        };

        let page = scan_page(art, &from, pattern, count, ty);

        let Some(next) = page.next.clone() else {
            if let Some(idx) = slot {
                c.release(idx);
            }
            return Ok((0, page));
        };
        let idx = match slot {
            Some(idx) => idx,
            None => match c.alloc(now) {
                Some(idx) => idx,
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
        c.store(idx, next, now);
        Ok((c.encode(idx), page))
    })
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
        let idx = c.alloc(now)?;
        c.store(idx, b"k".to_vec(), now);
        Some(c.encode(idx))
    }

    /// Reprise d'un curseur, comme `cmd_scan` : sort de la liste puis revient
    /// en queue (`done = false`) ou en tête, libre (`done = true`).
    fn resume(c: &mut Cursors, cursor: u64, now: u64, done: bool) -> Option<()> {
        let idx = c.lookup(cursor)?;
        c.unlink(idx);
        let from = c.slots[idx as usize].from.take().unwrap();
        c.active -= 1;
        if done { c.release(idx) } else { c.store(idx, from, now) }
        Some(())
    }

    #[test]
    fn grows_while_cursors_are_recent_then_refuses() {
        let mut c = Cursors::new(3, 60);
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
        let mut c = Cursors::new(2, 60);
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
        let mut c = Cursors::new(2, 60);
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
        let mut c = Cursors::new(u32::MAX - 1, 0);
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
}
