//! Une fenêtre d'exécution (`Core::exec_window`, RUNTIME_ARCH.md §8bis) : au plus
//! `width` frames des connexions tirées par `run_batch`, en cinq temps :
//!
//! 1. [`collect_frames`] découpe les frames dans `rx` et dans le morceau de tête de chaque
//!    connexion, et note dans un [`Plan`] ce qui a été pris ;
//! 2. [`Gate`] : ce que la fenêtre décide une fois pour toutes (réplica, MISCONF, plafond
//!    du flux, `maxmemory`) ;
//! 3. [`Amac`] : les séries de frames qui descendent ensemble en AMAC ;
//! 4. [`Exec::run`] exécute dans l'ordre : chemin AMAC ([`Exec::exec_hinted`]) ou chemin
//!    normal ([`Exec::exec_normal`], qui appelle `run_cmd`) ;
//! 5. `Core::commit_window` avance dans les morceaux d'après les plans et les arrêts, puis
//!    applique les suites (`finish_frame`).
//!
//! Les frames empruntent `ins` et `bufs` : de 1 à 4, rien ne passe par `&mut Core`, les
//! champs sont empruntés séparément dans [`Exec`].

#[allow(unused_imports)]
use crate::prelude::*;

use oxidart::OxidArt;
use radixox_lib::cmd::{Cmd, Parse};
use radixox_lib::shared_frame::{SharedFrame as Frame, extend_encode};
use radixox_lib::small_vec::SmallVec;
use radixox_lib::sys::Instant;

use crate::exec::autosave::Persistence;
use crate::exec::journal::{self, Stream};

use super::super::bufs::BufPool;
use super::super::conn::{ConnIn, ConnOut};
use super::{
    After, Batch, Config, ConnJob, Core, Fixed, MAXMEM_SCAN, Slot, Window, aof_misconf, block, evict_round,
    journal_finish, misconf, produced_key, run_cmd, stops_window, write_cost, zset_store_cost,
};
#[cfg(radixox_gc_debug)]
use super::GC_DEBUG_RAW;

/// Largeur maximale d'une fenêtre.
const B: usize = oxidart::PREFETCH_BATCH;

/// Ce que la fenêtre a pris à une connexion.
struct Plan {
    slot: Slot,
    /// La frame de `rx` en fait partie.
    from_rx: bool,
    /// Octets consommés dans le morceau de tête.
    consumed: u32,
    /// Le morceau de tête finit par une frame incomplète.
    tail: bool,
    err: Option<&'static str>,
}

/// Une frame de la fenêtre.
struct Item<'f> {
    slot: Slot,
    cmd: Cmd<'f>,
    /// Octets de la frame.
    raw: &'f [u8],
    /// Fin de la frame dans le morceau de tête ; `None` : la frame de `rx`.
    end: Option<u32>,
}

/// Une frame sans sa commande, passée à côté de la `Cmd` à l'exécution : la `Cmd` va
/// directement de `frames` à celle qui la consomme, sans recopie d'un `Item` entier (une
/// copie de 136 o par frame, mesurée : +60 instructions par commande en p32).
#[derive(Clone, Copy)]
struct At<'f> {
    slot: Slot,
    raw: &'f [u8],
    end: Option<u32>,
}

impl At<'_> {
    /// Début de la frame dans le morceau de tête (`None` : la frame de `rx`) : ce qui reste
    /// à lire quand elle est arrêtée avant d'être exécutée.
    fn start(&self) -> Option<u32> {
        self.end.map(|e| e - self.raw.len() as u32)
    }
}

/// Ce que la fenêtre décide une fois, avant la première frame.
struct Gate {
    /// Réplica : les écritures des clients sont refusées (§10.1).
    read_only: bool,
    /// `always` : les réponses attendent le fsync qui couvre leur marque.
    release_upto: Option<u64>,
    /// Écritures refusées : BGSAVE en échec, AOF en erreur (§7.7).
    refuse_writes: Option<Frame>,
    /// Flux trop en retard sur le disque : les écrivains attendent (§7.6).
    over_cap: bool,
    /// Réplica avec AOF : la fin d'un `RXLOAD` refait les fichiers de l'AOF
    /// (`rebase_aof`) ; elle attend qu'aucune suite de la file `Aof` ne soit en cours
    /// (une rotation de l'ancien écrivain écrirait par-dessus).
    aof_files_busy: bool,
    job_budget: usize,
    now: Instant,
}

/// `maxmemory` dans une fenêtre.
struct MemBudget {
    /// Limite des données (`Core::oom_limit`).
    limit: u64,
    /// Ce que le registre compte mais qu'une écriture réutilise sans nouvelle page.
    reusable: u64,
    /// Coût déjà admis dans la fenêtre : il a pu consommer du réutilisable sans faire
    /// monter le registre (compté deux fois sinon : prudent).
    admitted: u64,
}

impl MemBudget {
    /// Les écritures de la fenêtre peuvent dépasser la limite : chacune passe par la
    /// vérification du chemin normal (pas d'AMAC).
    fn near_limit(&self, frames: &[Item<'_>]) -> bool {
        let worst: u64 = frames.iter().filter(|it| it.cmd.may_grow()).map(|it| write_cost(&it.cmd, it.raw.len())).sum();
        radixox_lib::pages::charged() + worst > self.limit + self.reusable
    }
}

/// Réutilisable sans nouvelle page, comme `Core::mem_reusable`.
fn reusable_bytes(art: &OxidArt) -> u64 {
    radixox_lib::pool::reusable_bytes() + radixox_lib::large::reusable_bytes() + art.slab_reusable_bytes()
}

/// Ce que l'exécution laisse à `commit_window`.
struct Outcome {
    /// Suite de chaque frame prise.
    afters: Fixed<(Slot, After)>,
    /// Écrivains arrêtés par le plafond du flux : (connexion, début dans le morceau de
    /// tête de la frame arrêtée ; `None` : la frame de `rx`).
    aof_stops: Window<(Slot, Option<u32>)>,
    /// Connexions arrêtées en pleine fenêtre (lecture de plage parquée, plafond de sortie,
    /// grande valeur) : (connexion, octets consommés du morceau de tête, frame comprise) ;
    /// leurs frames suivantes restent à lire.
    park_stops: Window<(Slot, u32)>,
}

impl Outcome {
    fn new() -> Self {
        Self { afters: Fixed::new(), aof_stops: SmallVec::new(), park_stops: SmallVec::new() }
    }

    fn parked(&self, slot: Slot) -> bool {
        self.park_stops.iter().any(|&(s, _)| s == slot)
    }

    fn aof_stopped(&self, slot: Slot) -> bool {
        self.aof_stops.iter().any(|&(s, _)| s == slot)
    }
}

impl Core {
    /// Une fenêtre : parse au plus `width` frames des connexions `picked` (dans cet
    /// ordre, frames consécutives d'abord), précharge, exécute ; puis avance dans les
    /// morceaux. Rend le nombre de frames prises.
    // Forme du code d'avant le découpage (une fonction, boucle comprise, hors de
    // `Server::run`) : laissé au compilateur, `exec_window` était fondue dans `Server::run`
    // et la boucle restait seule dans `Exec::run` (mesuré : +20 instructions par commande).
    #[inline(never)]
    pub(super) fn exec_window(&mut self, picked: &[Slot], width: usize) -> usize {
        let mut plans: Fixed<Plan> = Fixed::new();
        let mut outcome = Outcome::new();
        {
            let gate = self.window_gate();
            let mem = self.oom_limit().map(|limit| MemBudget { limit, reusable: self.mem_reusable(), admitted: 0 });
            let Core { art, ins, outs, bufs, persist, journal, cfg, held, blocked, ready, dirty, oom_rejected, evicted_keys, .. } =
                self;
            // Les frames ne bougent plus une fois découpées : l'AMAC garde des pointeurs
            // sur leurs champs (`Amac::field`).
            let mut frames: Fixed<Item<'_>> = Fixed::new();
            collect_frames(ins, bufs, picked, width, &mut frames, &mut plans);
            let mut exec = Exec {
                art,
                ins,
                outs,
                persist,
                journal,
                cfg,
                held,
                blocked,
                ready,
                dirty,
                oom_rejected,
                evicted_keys,
                mem,
            };
            exec.run(&mut frames, &gate, &mut outcome);
        }
        self.commit_window(&mut plans, &mut outcome)
    }

    fn window_gate(&self) -> Gate {
        let refuse_writes = if self.persist.refuse_writes() {
            Some(misconf())
        } else {
            self.aof.as_ref().and_then(|w| w.error()).map(aof_misconf)
        };
        let over_cap = match (self.aof.as_ref(), self.journal.as_ref()) {
            (Some(w), Some(j)) => w.pending(j) > self.cfg.aof_max_pending,
            _ => false,
        };
        Gate {
            read_only: self.follow.is_some(),
            release_upto: self.aof.as_ref().and_then(|w| w.release_upto()),
            refuse_writes,
            over_cap,
            aof_files_busy: self.aof_files_busy(),
            job_budget: self.cfg.job_budget,
            now: self.now,
        }
    }

    /// Avance dans les morceaux d'après ce que la fenêtre a pris, rend les tampons
    /// vidés, puis applique les suites. Rend le nombre de frames prises.
    fn commit_window(&mut self, plans: &mut Fixed<Plan>, outcome: &mut Outcome) -> usize {
        let done = outcome.afters.len();
        // Écrivains arrêtés : rien de consommé à partir de la frame arrêtée.
        for &(slot, start) in &outcome.aof_stops {
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
        // Arrêtées en pleine fenêtre : consommé jusqu'à la fin de la frame d'arrêt.
        for &(slot, upto) in &outcome.park_stops {
            let plan = plans.iter_mut().find(|p| p.slot == slot).expect("plan de la connexion");
            plan.consumed = upto;
            plan.tail = false;
            plan.err = None;
        }
        // Avancer dans les morceaux, rendre les tampons vidés.
        for plan in plans.iter() {
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
        for plan in plans.iter() {
            self.account_rx(plan.slot);
        }
        for (slot, after) in outcome.afters.drain() {
            self.finish_frame(slot, after);
        }
        for &(slot, _) in &outcome.aof_stops {
            self.aof_park(slot);
        }
        // Erreur de protocole : après les réponses des frames d'avant.
        for plan in plans.iter() {
            let s = plan.slot as usize;
            if let Some(msg) = self.ins[s].proto_err.take() {
                let o = &mut self.outs[s];
                extend_encode(&mut o.out, &Frame::Error(format!("ERR Protocol error: {msg}")));
                self.finish_frame(plan.slot, After::Close);
            }
        }
        done.max(plans.len())
    }
}

/// Découpe au plus `width` frames des connexions `picked`. La première connexion prend
/// la fenêtre moins une place par autre connexion prête ; les autres, une frame chacune.
fn collect_frames<'f>(
    ins: &'f [ConnIn],
    bufs: &'f BufPool,
    picked: &[Slot],
    width: usize,
    frames: &mut Fixed<Item<'f>>,
    plans: &mut Fixed<Plan>,
) {
    for (rank, &slot) in picked.iter().enumerate() {
        if frames.len() >= width {
            break;
        }
        let quota = if rank == 0 { width.saturating_sub(picked.len() - 1).max(1) } else { 1 };
        let stop_at = (frames.len() + quota).min(width);
        let i = &ins[slot as usize];
        let mut plan = Plan { slot, from_rx: false, consumed: 0, tail: false, err: None };
        let mut barrier = false;
        if i.rx_ready > 0 {
            let raw = &i.rx[..i.rx_ready];
            let Parse::Done(cmd, _) = Cmd::parse(raw) else {
                unreachable!("rx_ready : frame complète");
            };
            barrier = stops_window(&cmd);
            frames.push(Item { slot, cmd, raw, end: None });
            plan.from_rx = true;
        }
        // Des octets attendent encore dans `rx` après cette frame : les morceaux
        // viennent après eux, pas dans cette fenêtre.
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
                        frames.push(Item { slot, cmd, raw, end: Some(pos as u32) });
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
}

/// Moteur AMAC (`RADIXOX_AMAC`, RUNTIME_ARCH.md §8bis) : chaque série d'au moins deux
/// frames consécutives que `execute_hinted` sait faire descend d'abord en AMAC (lecture
/// pure), puis s'exécute dans l'ordre avec les indices, fenêtre d'oxidart ouverte. Les
/// autres frames coupent les séries et passent par le chemin normal, fenêtre fermée.
struct Amac<'f> {
    /// Clés des frames avec indice, à la suite, au plus B par fenêtre (au-delà, la frame
    /// passe par le chemin normal) : frame i = `flat[start[i]..start[i + 1]]`.
    flat: [&'f [u8]; B],
    start: [u8; B + 1],
    /// Champ de la première clé de chaque frame (`exec::hint_field`), en pointeur : il
    /// appartient à la `Cmd`, consommée par la boucle d'exécution, mais seulement après
    /// la descente de sa série.
    field: [Option<(*const u8, usize, oxidart::ZProbe)>; B],
    /// Bit i : la frame i porte un champ.
    fields_in: u64,
    /// Bit i : frame i dans une série (au moins deux frames avec indice de suite).
    in_run: u64,
    hints: [oxidart::Hint; B],
    /// Fin de la série en cours (en frames) : fenêtre d'oxidart ouverte avant.
    run_end: usize,
}

impl<'f> Amac<'f> {
    /// Sans `enabled`, aucune série : tout passe par le chemin normal.
    fn plan(frames: &[Item<'f>], outs: &[ConnOut], blocked: &block::Blocked, enabled: bool) -> Self {
        debug_assert!(frames.len() <= B);
        let mut a = Amac {
            flat: [&[]; B],
            start: [0; B + 1],
            field: [None; B],
            fields_in: 0,
            in_run: 0,
            hints: [oxidart::Hint::NONE; B],
            run_end: 0,
        };
        if !enabled {
            return a;
        }
        // bit i : la frame i a ses indices
        let mut has = 0u64;
        let mut n = 0;
        for (i, it) in frames.iter().enumerate() {
            let o = &outs[it.slot as usize];
            let from = n;
            let mut fits = true;
            let hinted = o.client.authed
                && o.channels.is_empty()
                && (blocked.is_empty() || produced_key(&it.cmd).is_none_or(|k| !blocked.watched(k)))
                && crate::exec::hint_keys(&it.cmd, |k| {
                    if n < B {
                        a.flat[n] = k;
                        n += 1;
                    } else {
                        fits = false;
                    }
                });
            if hinted && fits {
                has |= 1 << i;
                a.field[from] = crate::exec::hint_field(&it.cmd).map(|(f, z)| (f.as_ptr(), f.len(), z));
                a.fields_in |= (a.field[from].is_some() as u64) << i;
            } else {
                n = from;
            }
            a.start[i + 1] = n as u8;
        }
        a.in_run = has & (has << 1 | has >> 1);
        a
    }

    #[inline]
    fn in_run(&self, i: usize) -> bool {
        self.in_run >> i & 1 != 0
    }

    #[inline]
    fn span(&self, i: usize) -> core::ops::Range<usize> {
        self.start[i] as usize..self.start[i + 1] as usize
    }

    /// Avant la frame `k` : ferme la fenêtre d'oxidart en fin de série, descend la
    /// série qui commence en `k`. Rend `true` si `k` est dans une série (chemin AMAC).
    #[inline]
    fn enter(&mut self, art: &mut OxidArt, k: usize, lanes: usize) -> bool {
        if k == self.run_end && art.in_window() {
            art.end_window();
        }
        if k >= self.run_end && self.in_run(k) {
            self.run_end = k + (!(self.in_run >> k)).trailing_zeros() as usize;
            let keys = self.start[k] as usize..self.start[self.run_end] as usize;
            art.amac_descend(&self.flat[keys.clone()], lanes, &mut self.hints[keys]);
            // bits k..run_end
            let mask = (u64::MAX >> (64 - (self.run_end - k))) << k;
            if self.fields_in & mask != 0 {
                self.descend_fields(art, k, lanes);
            }
            art.begin_window();
        }
        k < self.run_end
    }

    /// Phase 1 bis : les champs de Hash / membres de Set de la série `k..run_end`, dans
    /// un trie valart.
    fn descend_fields(&self, art: &mut OxidArt, k: usize, lanes: usize) {
        let mut items: [(oxidart::Hint, &[u8], oxidart::ZProbe); B] =
            [(oxidart::Hint::NONE, &[], oxidart::ZProbe::Index); B];
        let mut m = 0;
        for j in k..self.run_end {
            let first = self.start[j] as usize;
            if let Some((p, n, z)) = self.field[first]
                && self.hints[first].has_tree()
            {
                // SAFETY: les frames de la série (k..run_end) ne sont pas encore
                // consommées ; leurs champs vivent dans leur `Cmd`.
                items[m] = (self.hints[first], unsafe { core::slice::from_raw_parts(p, n) }, z);
                m += 1;
            }
        }
        art.amac_fields(&items[..m], lanes);
    }

    /// Préchargement en avance sur l'exécution (PF1 : nœud, PF2 : valeur), sur la
    /// première clé des frames suivantes de la série.
    #[inline]
    fn prefetch_ahead(&self, art: &OxidArt, k: usize, pf: u8) {
        if pf >= 1 && k + 4 < self.run_end {
            art.prefetch_hint_node(self.hints[self.start[k + 4] as usize]);
        }
        if pf >= 2 && k + 2 < self.run_end {
            art.prefetch_hint_value(self.hints[self.start[k + 2] as usize]);
        }
    }
}

/// Les champs de `Core` dont l'exécution a besoin, empruntés séparément : les frames
/// tiennent `ins` et `bufs`.
struct Exec<'a> {
    art: &'a mut OxidArt,
    ins: &'a [ConnIn],
    outs: &'a mut [ConnOut],
    persist: &'a mut Persistence,
    journal: &'a mut Option<Stream>,
    cfg: &'a Config,
    held: &'a mut Vec<Slot>,
    blocked: &'a mut block::Blocked,
    ready: &'a mut Vec<Slot>,
    dirty: &'a mut Vec<Slot>,
    oom_rejected: &'a mut u64,
    evicted_keys: &'a mut u64,
    /// `None` : sans `maxmemory`.
    mem: Option<MemBudget>,
}

impl Exec<'_> {
    #[inline(always)]
    fn run<'f>(&mut self, frames: &mut Fixed<Item<'f>>, gate: &Gate, out: &mut Outcome) {
        let enabled = self.cfg.amac > 0
            && !cfg!(radixox_gc_debug)
            && !gate.read_only
            && gate.refuse_writes.is_none()
            && !gate.over_cap
            && !self.mem.as_ref().is_some_and(|m| m.near_limit(frames));
        let mut amac = Amac::plan(frames, self.outs, self.blocked, enabled);
        self.prefetch_outside_runs(frames, &amac);
        // La frame ne sort du tableau qu'au moment de l'exécuter : sortie avant la
        // descente AMAC (`enter`), la `Cmd` (136 o) était recopiée deux fois, la seconde
        // lecture attendant la première écriture (mesuré : +30 cycles par commande en p32).
        let mut drain = frames.drain();
        let mut k = 0;
        while let Some(slot) = drain.peek().map(|it| it.slot) {
            let hinted = amac.enter(self.art, k, self.cfg.amac);
            let Some(Item { cmd, raw, end, .. }) = drain.next() else { unreachable!() };
            if !out.parked(slot) {
                let at = At { slot, raw, end };
                if hinted {
                    self.exec_hinted(at, cmd, &amac, k, gate, out);
                } else if !out.aof_stopped(slot) {
                    self.exec_normal(at, cmd, gate, out);
                }
            }
            k += 1;
        }
        if self.art.in_window() {
            self.art.end_window();
        }
    }

    /// Les frames hors série : un préchargement groupé de leur clé principale.
    fn prefetch_outside_runs(&mut self, frames: &[Item<'_>], amac: &Amac<'_>) {
        if frames.len() > 1 {
            let rest: Batch<&[u8]> =
                frames.iter().enumerate().filter(|(i, _)| !amac.in_run(*i)).filter_map(|(_, it)| it.cmd.main_key()).collect();
            if rest.len() > 1 {
                self.art.prefetch_keys(&rest);
            }
        }
    }

    /// Chemin AMAC : frame d'une série, indices calculés par la descente.
    #[inline]
    fn exec_hinted(&mut self, at: At<'_>, cmd: Cmd<'_>, amac: &Amac<'_>, k: usize, gate: &Gate, out: &mut Outcome) {
        let At { slot, raw, end } = at;
        amac.prefetch_ahead(self.art, k, self.cfg.amac_pf);
        // ni plafond du flux ni réplica : aucun écrivain arrêté
        debug_assert!(out.aof_stops.is_empty());
        let o = &mut self.outs[slot as usize];
        let weight = cmd.write_weight();
        if weight > 0 {
            self.persist.note_writes(weight);
        }
        let plan = self.journal.is_some().then(|| (journal::plan(&cmd), o.out.len()));
        let range = matches!(cmd, Cmd::LRange { .. } | Cmd::ZRange { .. });
        let before = o.out.len();
        let ((), tail) = crate::exec::with_big_tail(|| {
            crate::exec::execute_hinted(cmd, self.art, &amac.flat[amac.span(k)], &amac.hints[amac.span(k)], &mut o.out)
        });
        if range && park_deferred_range(o, slot, before, end, out) {
            return;
        }
        if let (Some(stream), Some((plan, start))) = (self.journal.as_mut(), plan) {
            journal_finish(stream, plan, start, gate.release_upto, self.held, slot, o, raw, self.art.now);
        }
        if tail.is_some() {
            o.tail = tail;
        }
        if output_full(o, self.cfg) {
            out.park_stops.push((slot, end.unwrap_or(0)));
        }
        out.afters.push((slot, After::Continue));
    }

    /// Chemin normal : vérifications d'écriture, `run_cmd`, puis service des clients
    /// bloqués sur la clé produite.
    fn exec_normal(&mut self, at: At<'_>, cmd: Cmd<'_>, gate: &Gate, out: &mut Outcome) {
        let slot = at.slot;
        // Réplica avec AOF : les blocs reçus attendent aussi le disque.
        if (gate.over_cap && matches!(cmd, Cmd::RxBlock(_)))
            || (gate.aof_files_busy && matches!(cmd, Cmd::RxLoad(radixox_lib::cmd::RxLoad::End)))
        {
            out.aof_stops.push((slot, at.start()));
            return;
        }
        let weight = cmd.write_weight();
        if weight > 0 && self.outs[slot as usize].client.authed && !self.admit_write(&at, &cmd, weight, gate, out) {
            return;
        }
        let At { raw, end, .. } = at;
        #[cfg(radixox_gc_debug)]
        {
            *GC_DEBUG_RAW.borrow_mut() = raw.to_vec();
        }
        let o = &mut self.outs[slot as usize];
        let plan = self.journal.is_some().then(|| (journal::plan(&cmd), o.out.len()));
        // Producteur sur une clé attendue (§14.3). LPUSH / RPUSH sur clé absente :
        // service direct, seul le reste est posé ; les autres : exécutés, puis servis.
        let produced = if self.blocked.is_empty() || !o.client.authed || !o.channels.is_empty() {
            None
        } else {
            produced_key(&cmd).filter(|k| self.blocked.watched(k))
        };
        let hand_off = match &cmd {
            Cmd::LPush { key, vals, right, exists: false }
                if produced.is_some() && self.art.key_type(key) == oxidart::value::RedisType::None =>
            {
                Some((*key, *right, vals.clone()))
            }
            _ => None,
        };
        let range = matches!(cmd, Cmd::LRange { .. } | Cmd::ZRange { .. });
        let before = o.out.len();
        let mut after = if let Some((_, _, vals)) = &hand_off {
            // longueur après le push, avant le service (comme Redis)
            extend_encode(&mut o.out, &Frame::Integer(vals.len() as i64));
            After::Continue
        } else {
            run_cmd(cmd, self.art, o, gate.job_budget)
        };
        if range && park_deferred_range(o, slot, before, end, out) {
            return;
        }
        if let After::Block(b) = after {
            o.block_wait = Some(self.blocked.register(slot, o.epoch, b, gate.now));
            after = After::Blocked;
        }
        let cont = matches!(after, After::Continue);
        out.afters.push((slot, after));
        if let (Some(stream), Some((plan, start))) = (self.journal.as_mut(), plan) {
            journal_finish(stream, plan, start, gate.release_upto, self.held, slot, o, raw, self.art.now);
        }
        if cont && output_full(o, self.cfg) {
            out.park_stops.push((slot, end.unwrap_or(0)));
        }
        if let Some(key) = produced {
            let mut sv = block::Serve {
                art: self.art,
                outs: self.outs,
                ins: self.ins,
                journal: self.journal.as_mut(),
                release_upto: gate.release_upto,
                held: self.held,
                dirty: self.dirty,
                ready: self.ready,
            };
            match hand_off {
                Some((key, right, vals)) => sv.push_hand_off(self.blocked, key, right, &vals),
                None => sv.serve_key(self.blocked, key),
            }
        }
    }

    /// Écriture d'un client authentifié : refusée (réplica, MISCONF, `-OOM`), arrêtée
    /// (flux trop en retard sur le disque) ou admise et comptée. `false` : la frame est
    /// traitée, rien à exécuter.
    fn admit_write(&mut self, at: &At<'_>, cmd: &Cmd<'_>, weight: u64, gate: &Gate, out: &mut Outcome) -> bool {
        let slot = at.slot;
        if gate.read_only {
            self.reply(slot, &Frame::Error("READONLY You can't write against a read only replica.".into()), out);
            return false;
        }
        if let Some(err) = &gate.refuse_writes {
            self.reply(slot, err, out);
            return false;
        }
        if gate.over_cap {
            // Cette frame et les suivantes de la connexion restent à lire ; elle
            // reprendra quand le disque aura rattrapé.
            out.aof_stops.push((slot, at.start()));
            return false;
        }
        if cmd.may_grow() && !self.admit_memory(cmd, at.raw.len(), gate) {
            // la place à récupérer doit l'être maintenant, pas après la pause
            radixox_lib::pool::gc_press();
            self.reply(slot, &Frame::Error(crate::exec::maxmem::OOM.into()), out);
            *self.oom_rejected += 1;
            return false;
        }
        // FLUSHDB : toutes les clés changent (le `dirty` de Redis).
        self.persist.note_writes(if matches!(cmd, Cmd::FlushDb) { self.art.len() as u64 } else { weight });
        true
    }

    /// `maxmemory` : coût borné avant d'exécuter, refus sans exécution partielle. Évince
    /// une petite tranche si la politique le permet. `true` : l'écriture tient (son coût
    /// est admis).
    fn admit_memory(&mut self, cmd: &Cmd<'_>, raw_len: usize, gate: &Gate) -> bool {
        let Some(mem) = &mut self.mem else {
            return true;
        };
        let cost = write_cost(cmd, raw_len) + zset_store_cost(self.art, cmd);
        let mut fits = radixox_lib::pages::charged() + mem.admitted + cost <= mem.limit + mem.reusable;
        // Plancher : données vivantes estimées (comme `live_estimate`) au-dessus de ¾ de la
        // limite. Évincer rend souvent de la place réutilisable tout de suite (nœuds valart,
        // grands objets), mais celle des blocs entamés du pool n'attend que le GC : sans
        // plancher, pendant une pause du GC, chaque écriture refusée évinçait encore et la
        // base se vidait (MEMORY_ARCH.md §10.5).
        let over_floor = |art: &OxidArt| {
            let used = radixox_lib::pages::charged().saturating_sub(reusable_bytes(art));
            used.saturating_sub(radixox_lib::pool::fragment_free_bytes()) > mem.limit / 4 * 3
        };
        if !fits && self.cfg.maxmemory_policy != crate::exec::maxmem::Policy::NoEviction && over_floor(self.art) {
            // Une petite tranche tout de suite, puis la même vérification stricte : ce
            // qu'elle libère dans des blocs entamés ne compte qu'après le GC.
            let journal = self.journal.as_mut().filter(|_| !gate.read_only);
            *self.evicted_keys += evict_round(self.art, journal, self.cfg.maxmemory_policy, MAXMEM_SCAN, 64).evicted as u64;
            mem.reusable = reusable_bytes(self.art);
            fits = radixox_lib::pages::charged() + mem.admitted + cost <= mem.limit + mem.reusable;
        }
        if fits {
            mem.admitted += cost;
        }
        fits
    }

    /// Réponse sans exécution (écriture refusée).
    fn reply(&mut self, slot: Slot, frame: &Frame, out: &mut Outcome) {
        extend_encode(&mut self.outs[slot as usize].out, frame);
        out.afters.push((slot, After::Continue));
    }
}

/// LRANGE / ZRANGE trop longue pour la fenêtre : réponse effacée, la connexion est
/// parquée sur le job de plage. `true` : la frame est traitée.
#[inline]
fn park_deferred_range(o: &mut ConnOut, slot: Slot, before: usize, end: Option<u32>, out: &mut Outcome) -> bool {
    let Some(job) = crate::exec::take_deferred() else {
        return false;
    };
    o.out.truncate(before);
    out.park_stops.push((slot, end.unwrap_or(0)));
    out.afters.push((slot, After::Park(ConnJob::Range(job))));
    true
}

/// Plafond de sortie, ou grande valeur à envoyer après `out` : les frames suivantes de
/// la connexion attendent.
#[inline]
fn output_full(o: &ConnOut, cfg: &Config) -> bool {
    o.tail.is_some() || o.out.len() >= cfg.out_backpressure
}
