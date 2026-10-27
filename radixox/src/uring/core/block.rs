//! Commandes bloquantes (ANALYSE_ART_UNIFIE.md §14.3) : `BLPOP` `BRPOP` `BLMOVE`
//! `BRPOPLPUSH` `BZPOPMIN` `BZPOPMAX`.
//!
//! Principe proactif : celui qui produit sert. Un client n'attend que sur une clé absente
//! (sur une clé non vide il est servi tout de suite) ; une clé qui existe n'a donc jamais
//! d'inscrit vivant, et seule la création d'une clé regarde le registre (rien à payer quand
//! il est vide). Un `LPUSH` / `RPUSH` sur une clé attendue donne ses éléments directement
//! aux inscrits, dans leur ordre d'arrivée, puis pose seulement le reste ; les autres
//! producteurs (`LMOVE`, `ZADD`, `ZINCRBY`…) s'exécutent puis servent aussitôt, dans la
//! même fenêtre. Sémantique et journal de Redis : le push tel qu'envoyé, puis un pop par
//! client servi.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use oxidart::OxidArt;
use oxidart::lcommand::End;
use oxidart::value::RedisType;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::{SharedFrame as Frame, encode_array_len, encode_bulk, extend_encode};
use smallvec::SmallVec;

use crate::exec::journal::Stream;

use super::super::conn::{ConnIn, ConnOut, Mode};
use super::{Core, Slot};

const WRONGTYPE: &[u8] = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";

/// Ce qu'attend un inscrit.
#[derive(Clone)]
pub(in super::super) enum Kind {
    /// BLPOP (`right` faux) / BRPOP.
    Pop { right: bool },
    /// BLMOVE / BRPOPLPUSH : l'élément pris va dans `dst`.
    Move { dst: SharedByte, from_right: bool, to_right: bool },
    /// BZPOPMIN / BZPOPMAX.
    ZPop { max: bool },
}

impl Kind {
    fn wants_list(&self) -> bool {
        !matches!(self, Kind::ZPop { .. })
    }
}

/// Commande bloquante pas servie tout de suite : à inscrire, la connexion attend.
pub(in super::super) struct Block {
    pub keys: SmallVec<[SharedByte; 2]>,
    pub kind: Kind,
    /// Secondes ; 0 : sans fin.
    pub timeout: f64,
}

struct Waiter {
    slot: Slot,
    epoch: u32,
    kind: Kind,
    deadline: Option<Instant>,
}

/// Registre des attentes : par clé, les inscriptions dans l'ordre d'arrivée. Une
/// inscription servie, échue ou abandonnée quitte `waiters` ; ses entrées dans les files
/// des autres clés partent quand on les croise.
#[derive(Default)]
pub(in super::super) struct Blocked {
    by_key: HashMap<SharedByte, VecDeque<u64>>,
    waiters: HashMap<u64, Waiter>,
    next: u64,
}

impl Blocked {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }

    /// Des inscrits attendent peut-être `key` (vérifié au service).
    #[inline]
    pub fn watched(&self, key: &[u8]) -> bool {
        !self.waiters.is_empty() && self.by_key.contains_key(key)
    }

    pub fn register(&mut self, slot: Slot, epoch: u32, b: Block, now: Instant) -> u64 {
        let id = self.next;
        self.next += 1;
        // externes : vivent hors de l'arbre, le GC ne les voit pas (MEMORY_ARCH.md §16.9)
        let keys: SmallVec<[SharedByte; 2]> = b.keys.into_iter().map(SharedByte::into_external).collect();
        for k in &keys {
            self.by_key.entry(k.clone()).or_default().push_back(id);
        }
        let deadline = (b.timeout > 0.0).then(|| now + std::time::Duration::from_secs_f64(b.timeout));
        let kind = match b.kind {
            Kind::Move { dst, from_right, to_right } => Kind::Move { dst: dst.into_external(), from_right, to_right },
            k => k,
        };
        self.waiters.insert(id, Waiter { slot, epoch, kind, deadline });
        id
    }

    pub fn deadline(&self, id: u64) -> Option<Instant> {
        self.waiters.get(&id).and_then(|w| w.deadline)
    }

    /// Abandon (connexion fermée).
    pub fn remove(&mut self, id: u64) {
        self.waiters.remove(&id);
    }

    /// Inscription `id` toujours en attente.
    pub fn contains(&self, id: u64) -> bool {
        self.waiters.contains_key(&id)
    }

    /// Prochain inscrit vivant de `key` qui attend ce type (liste ou ZSet), retiré du
    /// registre. Les inscriptions mortes (connexion fermée ou réutilisée) partent au
    /// passage.
    fn next_live(&mut self, key: &[u8], list: bool, outs: &[ConnOut]) -> Option<Waiter> {
        let q = self.by_key.get_mut(key)?;
        let mut i = 0;
        let mut found = None;
        while i < q.len() {
            let id = q[i];
            let Some(w) = self.waiters.get(&id) else {
                q.remove(i);
                continue;
            };
            let o = &outs[w.slot as usize];
            if o.epoch != w.epoch || o.block_wait != Some(id) || !matches!(o.mode, Mode::Normal | Mode::Closing) {
                self.waiters.remove(&id);
                q.remove(i);
                continue;
            }
            if w.kind.wants_list() != list {
                i += 1;
                continue;
            }
            q.remove(i);
            found = Some(id);
            break;
        }
        if q.is_empty() {
            self.by_key.remove(key);
        }
        self.waiters.remove(&found?)
    }

    /// Inscriptions échues à `now` (retirées).
    fn take_expired(&mut self, now: Instant) -> Vec<Waiter> {
        let ids: Vec<u64> =
            self.waiters.iter().filter(|(_, w)| w.deadline.is_some_and(|d| d <= now)).map(|(&id, _)| id).collect();
        ids.into_iter().filter_map(|id| self.waiters.remove(&id)).collect()
    }
}

#[inline]
fn end(right: bool) -> End {
    if right { End::Right } else { End::Left }
}

fn side(right: bool) -> &'static [u8] {
    if right { b"RIGHT" } else { b"LEFT" }
}

/// Ce dont le service a besoin, pris à part dans le cœur (la fenêtre d'exécution en tient
/// déjà d'autres morceaux).
pub(in super::super) struct Serve<'a> {
    pub art: &'a mut OxidArt,
    pub outs: &'a mut [ConnOut],
    pub ins: &'a [ConnIn],
    pub journal: Option<&'a mut Stream>,
    /// `appendfsync always` : réponses retenues jusqu'au fsync qui les couvre.
    pub release_upto: Option<u64>,
    pub held: &'a mut Vec<Slot>,
    pub dirty: &'a mut Vec<Slot>,
    pub ready: &'a mut Vec<Slot>,
}

impl Serve<'_> {
    /// Journalise une commande de service (un pop par client servi).
    fn log(&mut self, args: &[&[u8]]) {
        if let Some(j) = self.journal.as_deref_mut() {
            let mut cmd = Vec::with_capacity(16 + args.iter().map(|a| a.len() + 16).sum::<usize>());
            encode_array_len(&mut cmd, args.len());
            for a in args {
                encode_bulk(&mut cmd, a);
            }
            j.push(self.art.now, &cmd);
        }
    }

    /// Réponse au client servi, qui reprend (ce qu'il a reçu entre-temps passe aux tours
    /// suivants).
    fn answer(&mut self, w: &Waiter, write: impl FnOnce(&mut Vec<u8>)) {
        let s = w.slot as usize;
        let o = &mut self.outs[s];
        o.block_wait = None;
        let start = o.out.len();
        write(&mut o.out);
        if let Some(j) = self.journal.as_deref() {
            let mark = j.end_mark();
            o.write_mark = o.write_mark.max(mark);
            if let Some(durable) = self.release_upto
                && mark > durable
            {
                o.holds.push_back((start, mark));
                if o.holds.len() == 1 {
                    self.held.push(w.slot);
                }
            }
        }
        if !o.dirty {
            o.dirty = true;
            self.dirty.push(w.slot);
        }
        if self.ins[s].has_work() && !o.ready {
            o.ready = true;
            self.ready.push(w.slot);
        }
    }

    /// Destination de BLMOVE d'un autre type : Redis laisse l'élément à la source ; ici
    /// le client reçoit l'erreur (cas sans usage réel).
    fn dst_ok(&mut self, dst: &[u8]) -> bool {
        matches!(self.art.key_type(dst), RedisType::None | RedisType::List)
    }

    /// `LPUSH` / `RPUSH` de `vals` sur `key`, absente et attendue : la liste qu'aurait faite
    /// le push est servie directement aux inscrits, puis seul le reste est posé. Le journal
    /// a déjà le push ; ici, un pop par client servi.
    pub fn push_hand_off(&mut self, blocked: &mut Blocked, key: &[u8], right: bool, vals: &[SharedByte]) {
        let mut d: VecDeque<SharedByte> = VecDeque::with_capacity(vals.len());
        for v in vals {
            if right { d.push_back(v.clone()) } else { d.push_front(v.clone()) }
        }
        let mut next_keys: Vec<SharedByte> = Vec::new();
        while !d.is_empty() {
            let Some(w) = blocked.next_live(key, true, self.outs) else { break };
            match &w.kind {
                Kind::Pop { right } => {
                    let v = if *right { d.pop_back() } else { d.pop_front() }.expect("élément");
                    self.log(&[if *right { b"RPOP" } else { b"LPOP" }, key]);
                    self.answer(&w, |out| {
                        encode_array_len(out, 2);
                        encode_bulk(out, key);
                        encode_bulk(out, &v);
                    });
                }
                Kind::Move { dst, from_right, to_right } => {
                    if dst.as_slice() != key && !self.dst_ok(dst) {
                        self.answer(&w, |out| out.extend_from_slice(WRONGTYPE));
                        continue;
                    }
                    let v = if *from_right { d.pop_back() } else { d.pop_front() }.expect("élément");
                    self.log(&[b"LMOVE", key, dst, side(*from_right), side(*to_right)]);
                    if dst.as_slice() == key {
                        if *to_right { d.push_back(v.clone()) } else { d.push_front(v.clone()) }
                    } else {
                        let _ = self.art.cmd_lpush(dst, end(*to_right), std::slice::from_ref(&v), false);
                        if blocked.watched(dst) {
                            next_keys.push(dst.clone());
                        }
                    }
                    self.answer(&w, |out| encode_bulk(out, &v));
                }
                Kind::ZPop { .. } => unreachable!("inscrit de liste attendu"),
            }
        }
        if !d.is_empty() {
            let rest: Vec<SharedByte> = d.into();
            let _ = self.art.cmd_lpush(key, End::Right, &rest, false);
        }
        for k in next_keys {
            self.serve_key(blocked, &k);
        }
    }

    /// `key` vient d'être créée ou remplie : ses inscrits sont servis tant qu'elle a de quoi
    /// (liste ou ZSet), en chaîne pour les BLMOVE.
    pub fn serve_key(&mut self, blocked: &mut Blocked, key: &[u8]) {
        let mut work = vec![SharedByte::from_slice(key)];
        while let Some(k) = work.pop() {
            loop {
                let list = match self.art.key_type(&k) {
                    RedisType::List => true,
                    RedisType::ZSet => false,
                    _ => break,
                };
                let Some(w) = blocked.next_live(&k, list, self.outs) else { break };
                match &w.kind {
                    Kind::Pop { right } => {
                        let v = self.art.cmd_lpop(&k, end(*right), 1).ok().flatten().and_then(|mut v| v.pop()).expect("élément");
                        self.log(&[if *right { b"RPOP" } else { b"LPOP" }, &k]);
                        self.answer(&w, |out| {
                            encode_array_len(out, 2);
                            encode_bulk(out, &k);
                            encode_bulk(out, &v);
                        });
                    }
                    Kind::Move { dst, from_right, to_right } => {
                        if *dst != k && !self.dst_ok(dst) {
                            self.answer(&w, |out| out.extend_from_slice(WRONGTYPE));
                            continue;
                        }
                        let v = self.art.cmd_lmove(&k, dst, end(*from_right), end(*to_right)).ok().flatten().expect("élément");
                        self.log(&[b"LMOVE", &k, dst, side(*from_right), side(*to_right)]);
                        self.answer(&w, |out| encode_bulk(out, &v));
                        if *dst != k && blocked.watched(dst) {
                            work.push(dst.clone());
                        }
                    }
                    Kind::ZPop { max } => {
                        let (m, s) = self.art.cmd_zpop(&k, *max, 1).ok().and_then(|mut v| v.pop()).expect("membre");
                        self.log(&[if *max { b"ZPOPMAX" } else { b"ZPOPMIN" }, &k]);
                        self.answer(&w, |out| {
                            encode_array_len(out, 3);
                            encode_bulk(out, &k);
                            encode_bulk(out, &m);
                            encode_bulk(out, &oxidart::zset::format_score(s));
                        });
                    }
                }
            }
        }
    }
}

impl Core {
    /// Fin de la frame bloquante : si l'inscription attend encore (aucun producteur ne l'a
    /// servie dans la fenêtre), la connexion se parque et le délai est posé.
    pub(super) fn block_park(&mut self, slot: Slot) {
        let Some(id) = self.outs[slot as usize].block_wait else { return };
        self.spans_to_rx(slot);
        if let Some(d) = self.blocked.deadline(id) {
            self.timers.push(std::cmp::Reverse((d, super::Timer::Block)));
        }
    }

    /// Timer : réponse nulle aux inscriptions échues.
    pub(super) fn block_timeouts(&mut self) {
        let now = self.now;
        for w in self.blocked.take_expired(now) {
            let s = w.slot as usize;
            let o = &mut self.outs[s];
            if o.epoch != w.epoch || o.block_wait.is_none() || !matches!(o.mode, Mode::Normal | Mode::Closing) {
                continue;
            }
            o.block_wait = None;
            // BLMOVE / BRPOPLPUSH : nul ; BLPOP / BRPOP / BZPOP* : tableau nul (comme Redis)
            extend_encode(&mut o.out, &Frame::NullArray);
            if !o.dirty {
                o.dirty = true;
                self.dirty.push(w.slot);
            }
            if self.ins[s].has_work() && !o.ready {
                o.ready = true;
                self.ready.push(w.slot);
            }
        }
    }

    /// Fermeture d'une connexion : son inscription part.
    pub(super) fn block_forget(&mut self, slot: Slot) {
        if let Some(id) = self.outs[slot as usize].block_wait.take() {
            self.blocked.remove(id);
        }
    }
}
