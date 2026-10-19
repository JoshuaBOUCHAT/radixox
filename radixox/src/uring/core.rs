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

use oxidart::OxidArt;
use smallvec::SmallVec;
use radixox_lib::cmd::{Cmd, FrameLen, Parse, frame_len};
use radixox_lib::shared_frame::{SharedFrame as Frame, extend_encode};

use crate::exec::client::Client;
use crate::exec::jobs::{FREE_BUDGET, FlushJob, KeysJob, Step, UnlinkJob};
use crate::exec::{Executed, execute_into, resp_ok};

use super::bufs::{Bid, BufPool};
use super::conn::{ConnIn, ConnOut, Mode, Span};

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
    /// 1 = pas de préchargement). Au plus une par connexion par fenêtre.
    pub prefetch: usize,
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
            batch_cmds: 1024,
            flush_early: 64 << 10,
            out_backpressure: 1 << 20,
            prefetch: 8,
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
}

/// Une fenêtre de préchargement, sur la pile (pas d'allocation par fenêtre).
type Window<T> = SmallVec<[T; 16]>;

/// Suite à donner après l'exécution d'une frame.
enum After {
    Continue,
    /// Barrière : plus rien pour cette connexion ce tour-ci.
    Barrier,
    /// QUIT, erreur de protocole : fin propre, file abandonnée.
    Close,
    Background(FlushJob),
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
    turn: u64,
    rr: usize,
}

impl Core {
    pub fn new(cfg: Config, art: OxidArt) -> Self {
        let n = cfg.max_conn as usize;
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
            turn: 0,
            rr: 0,
            cfg,
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
        // Une connexion sale qui attend la fin de son send n'est pas du travail :
        // le CQE réveillera la boucle.
        self.ops.is_empty()
            && self.ready.is_empty()
            && self.jobs.is_empty()
            && self.dirty.iter().all(|&s| self.outs[s as usize].send_in_flight())
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
        }
    }

    fn accept(&mut self, fd: i32) {
        if self.free.len() as u32 <= self.cfg.reserved {
            self.ops.push(Op::Reject { fd });
            return;
        }
        let slot = self.free.pop().expect("slot libre");
        let o = &mut self.outs[slot as usize];
        debug_assert_eq!(o.mode, Mode::Free);
        o.mode = Mode::Normal;
        o.epoch = o.epoch.wrapping_add(1);
        // Le premier recv est chaîné à l'installation (Op::Install).
        o.refs = 1;
        o.recv_armed = true;
        o.recv_cancelled = false;
        o.send_cancelled = false;
        o.out.clear();
        o.sending.clear();
        o.sent = 0;
        o.client = Client::new();
        o.dirty = false;
        o.ready = false;
        o.stop_turn = u64::MAX;
        self.ins[slot as usize] = ConnIn::default();
        self.ops.push(Op::Install { slot, fd });
    }

    fn on_recv(&mut self, slot: Slot, res: i32, bid: Option<Bid>, buf_more: bool, more: bool) {
        let s = slot as usize;
        debug_assert_ne!(self.outs[s].mode, Mode::Free, "recv sur un slot libre");
        if !more {
            let o = &mut self.outs[s];
            o.refs -= 1;
            o.recv_armed = false;
        }
        match res {
            n if n > 0 => {
                let bid = bid.expect("recv avec données sans tampon");
                let off = self.bufs.chunk(bid, n as u32, buf_more);
                if self.outs[s].mode == Mode::Normal && !self.ins[s].broken {
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

    /// Tirage par tours (§2.1), par fenêtres de `prefetch` frames : parse (une
    /// seule passe, directement dans les morceaux reçus), préchargement des clés
    /// (§8bis), exécution, puis on avance dans les morceaux. Une fenêtre prend
    /// d'abord des frames consécutives d'une connexion, puis complète avec les
    /// suivantes : un client sans pipeline donne une frame, un pipeline au plus
    /// `prefetch` par passage. Ordre gardé par connexion.
    pub fn run_batch(&mut self) {
        self.turn += 1;
        let n = self.ready.len();
        if n == 0 {
            return;
        }
        let mut pos = self.rr % n;
        self.rr = self.rr.wrapping_add(1);
        let width = self.cfg.prefetch.clamp(1, oxidart::PREFETCH_BATCH);
        let mut budget = self.cfg.batch_cmds;
        let mut picked: Window<Slot> = SmallVec::new();
        while budget > 0 {
            // Connexions candidates de la fenêtre (au plus `width`) ; les frames
            // qui coupent deux morceaux sont reconstituées avant le parse.
            picked.clear();
            let mut tried = 0;
            while picked.len() < width && tried < n {
                let slot = self.ready[pos % n];
                pos += 1;
                tried += 1;
                if self.drawable(slot) && self.prepare(slot) {
                    picked.push(slot);
                }
            }
            if picked.is_empty() {
                break;
            }
            let done = self.exec_window(&picked, width.min(budget));
            if done == 0 {
                break;
            }
            budget = budget.saturating_sub(done);
        }
        let (ins, outs) = (&self.ins, &mut self.outs);
        self.ready.retain(|&slot| {
            let s = slot as usize;
            let keep = ins[s].has_work() && matches!(outs[s].mode, Mode::Normal | Mode::Closing);
            outs[s].ready = keep;
            keep
        });
    }

    /// La connexion peut donner des frames à ce tour-ci.
    fn drawable(&self, slot: Slot) -> bool {
        let o = &self.outs[slot as usize];
        o.stop_turn != self.turn
            && matches!(o.mode, Mode::Normal | Mode::Closing)
            && !(o.send_in_flight() && o.out.len() >= self.cfg.out_backpressure)
    }

    /// Rend la tête de la connexion exécutable : si une frame a été coupée
    /// entre deux morceaux, elle est complétée dans `rx` avec le début des
    /// morceaux suivants. `false` : rien d'exécutable (frame incomplète en
    /// attente de données).
    fn prepare(&mut self, slot: Slot) -> bool {
        let s = slot as usize;
        let Core { ins, bufs, cfg, .. } = self;
        let i = &mut ins[s];
        if i.proto_err.is_some() || i.rx_ready > 0 {
            return true;
        }
        if i.rx.is_empty() {
            return !i.spans.is_empty();
        }
        // Morceau par morceau de 4 Ko au plus : une petite frame coupée ne
        // recopie que ce qu'il lui faut ; l'excédent est rendu au morceau.
        const PIECE: u32 = 4096;
        let mut freed: Window<Bid> = SmallVec::new();
        loop {
            match frame_len(&i.rx) {
                FrameLen::Done(n) => {
                    let excess = (i.rx.len() - n) as u32;
                    if excess > 0 {
                        let front = i.spans.front_mut().expect("excédent pris dans un morceau");
                        front.off -= excess;
                        front.len += excess;
                        i.rx.truncate(n);
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
                }
            }
        }
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
        {
            let Core { art, ins, outs, bufs, .. } = self;
            let mut frames: Window<(Slot, Cmd<'_>)> = SmallVec::new();
            for &slot in picked {
                if frames.len() >= width {
                    break;
                }
                let i = &ins[slot as usize];
                let mut plan = Plan { slot, from_rx: false, consumed: 0, tail: false, err: None, first: frames.len() };
                let mut barrier = false;
                if i.rx_ready > 0 {
                    let Parse::Done(cmd, _) = Cmd::parse(&i.rx[..i.rx_ready]) else {
                        unreachable!("rx_ready : frame complète");
                    };
                    barrier = cmd.is_barrier();
                    frames.push((slot, cmd));
                    plan.from_rx = true;
                }
                if !barrier
                    && i.proto_err.is_none()
                    && let Some(front) = i.spans.front()
                {
                    let data = bufs.slice(front.bid, front.off, front.len);
                    let mut pos = 0;
                    while frames.len() < width && pos < data.len() {
                        match Cmd::parse(&data[pos..]) {
                            Parse::Done(cmd, n) => {
                                pos += n;
                                let b = cmd.is_barrier();
                                frames.push((slot, cmd));
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
                let keys: Window<&[u8]> = frames.iter().filter_map(|(_, c)| c.main_key()).collect();
                if keys.len() > 1 {
                    art.prefetch_keys(&keys);
                }
            }
            for (slot, cmd) in frames {
                afters.push((slot, run_cmd(cmd, art, &mut outs[slot as usize])));
            }
        }
        let done = afters.len();
        // Avancer dans les morceaux, rendre les tampons vidés.
        for plan in &plans {
            let s = plan.slot as usize;
            let i = &mut self.ins[s];
            if plan.from_rx {
                i.rx.clear();
                i.rx_ready = 0;
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
        }
        let o = &mut self.outs[s];
        if !o.out.is_empty() {
            if o.out.len() >= self.cfg.flush_early && !o.send_in_flight() {
                self.send(slot);
            } else if !o.dirty {
                o.dirty = true;
                self.dirty.push(slot);
            }
        }
    }

    /// Une tranche de travail de fond (libération de FLUSHDB).
    pub fn run_jobs(&mut self) {
        if let Some(mut job) = self.jobs.pop_front()
            && job.step(&mut self.art, FREE_BUDGET) == Step::More
        {
            self.jobs.push_back(job);
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
            let more_coming = o.mode == Mode::Normal && self.ins[s].has_work();
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
            if !i.has_work() && o.out.is_empty() && !o.send_in_flight() {
                self.drain(slot);
            } else {
                self.closing.push(slot);
            }
        }
    }

    fn send(&mut self, slot: Slot) {
        let o = &mut self.outs[slot as usize];
        debug_assert!(!o.send_in_flight());
        std::mem::swap(&mut o.out, &mut o.sending);
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
        self.outs[s].mode = Mode::Draining;
        self.release_queue(slot);
        self.ins[s] = ConnIn { broken: true, ..ConnIn::default() };
        self.outs[s].out.clear();
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
        o.sending = Vec::new();
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
            let expected = o.recv_armed as u8 + o.send_in_flight() as u8;
            assert_eq!(o.refs, expected, "slot {s} : refs {} pour recv {} send {}", o.refs, o.recv_armed, o.send_in_flight());
            for span in &i.spans {
                ring_items[span.bid as usize] += 1;
            }
            assert!(i.rx_ready <= i.rx.len());
            if i.has_work() && matches!(o.mode, Mode::Normal | Mode::Closing) {
                assert!(o.ready && self.ready.contains(&(s as Slot)), "slot {s} a des frames hors de la liste prête");
            }
            assert!(o.sent <= o.sending.len());
        }
        for bid in 0..self.bufs.count() {
            assert_eq!(self.bufs.refs(bid), ring_items[bid as usize], "tampon {bid} : refs");
        }
    }
}

/// Exécute une commande d'une connexion.
fn run_cmd(cmd: Cmd<'_>, art: &mut OxidArt, o: &mut ConnOut) -> After {
    let barrier = cmd.is_barrier();
    match execute_into(cmd, art, &mut o.client, &mut o.out) {
        Executed::Done if barrier => After::Barrier,
        Executed::Done => After::Continue,
        Executed::Runtime(cmd) => runtime(cmd, art, o),
    }
}

/// Commandes qui dépendent du runtime. KEYS et UNLINK s'exécutent encore d'une
/// traite (parquage + jobs : étape 4) ; Pub/Sub et saves : étapes 4–5.
fn runtime(cmd: Cmd<'_>, art: &mut OxidArt, o: &mut ConnOut) -> After {
    match cmd {
        Cmd::Quit => {
            extend_encode(&mut o.out, &resp_ok());
            After::Close
        }
        Cmd::Keys(pattern) => {
            let mut job = KeysJob::new(pattern);
            while job.step(art, usize::MAX) == Step::More {}
            o.out.extend_from_slice(job.response());
            After::Continue
        }
        Cmd::Unlink(keys) => {
            // Pas de save dans ce binaire avant l'étape 4 : la libération finit.
            debug_assert!(!art.is_saving());
            let mut job = UnlinkJob::start(art, &keys);
            while job.step(art, usize::MAX) == Step::More {}
            extend_encode(&mut o.out, &Frame::Integer(job.count() as i64));
            After::Continue
        }
        Cmd::FlushDb => {
            let job = FlushJob::start(art);
            extend_encode(&mut o.out, &resp_ok());
            After::Background(job)
        }
        Cmd::Subscribe(_) | Cmd::Unsubscribe(_) | Cmd::Publish { .. } | Cmd::BgSave | Cmd::Save => {
            extend_encode(&mut o.out, &Frame::Error("ERR not supported yet by radixox-uring".into()));
            After::Continue
        }
        _ => unreachable!("commande sans dépendance au runtime"),
    }
}
