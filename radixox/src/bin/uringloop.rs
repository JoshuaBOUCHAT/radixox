//! `radixox` : serveur RESP sur la boucle io_uring maison, sans runtime
//! (RUNTIME_ARCH.md). La logique vit dans le cœur sans I/O
//! (`radixox::uring::Core`) ; ce fichier ne fait que la traduction :
//! `Op` → SQE, CQE → `Completion`, plus l'attente.
//!
//! Accept, recv multishot sur anneau incrémental, send, toutes les commandes,
//! parquage (KEYS, UNLINK), BGSAVE et SAVE, Pub/Sub, timers (évicteur,
//! inactivité), arrêt sur signal avec save (`signalfd`).

#[cfg(not(target_os = "linux"))]
compile_error!("RadixOx requires Linux to run (io_uring).");

use std::io;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use io_uring::types::{BufRingEntry, Fd, Fixed, IOU_PBUF_RING_INC, SubmitArgs, Timespec};
use io_uring::{EnterFlags, IoUring, cqueue, opcode, squeue};
use oxidart::OxidArt;
use radixox::exec::fs_task::StepRef;
use radixox::exec::{aof, autosave};
use radixox::tsc;
use radixox::uring::{Bid, Completion, Config, Core, Op, Slot};

/// Groupe de tampons du recv multishot.
const BGID: u16 = 0;
const SQ_ENTRIES: u32 = 4096;
const CQ_ENTRIES: u32 = 16384;
/// Tranches de lot par tour au plus (une tranche = `Config::batch_cmds`).
const MAX_SLICES: u32 = 32;

// ── user_data : kind << 32 | slot (RUNTIME_ARCH.md §3) ──────────────────────

const K_ACCEPT: u64 = 1;
const K_RECV: u64 = 2;
const K_SEND: u64 = 3;
const K_CANCEL: u64 = 4;
const K_CLOSE: u64 = 5;
const K_SIGNAL: u64 = 6;
const K_FILE: u64 = 7;
const K_JWRITE: u64 = 8;
const K_JSYNC: u64 = 9;
const K_CONNECT: u64 = 10;
const K_FS: u64 = 11;

fn ud(kind: u64, slot: Slot) -> u64 {
    kind << 32 | slot as u64
}

// ── Anneau de tampons fournis (incrémental) ─────────────────────────────────

/// Les entrées de l'anneau partagé avec le noyau ; les tampons eux-mêmes
/// appartiennent au cœur (`BufPool`).
struct BufRing {
    base: *mut BufRingEntry,
    mask: u16,
    tail: u16,
    layout: std::alloc::Layout,
}

impl BufRing {
    fn new(ring: &IoUring, core: &mut Core) -> io::Result<Self> {
        let count = core.bufs_mut().count();
        let entries = (count as u32).next_power_of_two().max(1) as u16;
        let layout = std::alloc::Layout::from_size_align(entries as usize * size_of::<BufRingEntry>(), 4096)
            .expect("layout");
        // SAFETY: taille non nulle ; mémoire libérée dans `Drop`, après l'anneau.
        let base = unsafe { std::alloc::alloc_zeroed(layout) } as *mut BufRingEntry;
        assert!(!base.is_null(), "allocation de l'anneau de tampons");
        // SAFETY: `base` vit aussi longtemps que l'anneau io_uring (champ du serveur).
        unsafe { ring.submitter().register_buf_ring_with_flags(base as u64, entries, BGID, IOU_PBUF_RING_INC as u16) }
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("anneau de tampons incrémental refusé ({e}) : noyau 6.12 ou plus récent requis"),
                )
            })?;
        let mut br = Self { base, mask: entries - 1, tail: 0, layout };
        for bid in 0..count {
            br.push(core, bid);
        }
        br.publish();
        Ok(br)
    }

    /// Ajoute `bid` en queue (visible du noyau après `publish`).
    fn push(&mut self, core: &mut Core, bid: Bid) {
        let size = core.bufs_mut().size();
        let addr = core.bufs_mut().ptr(bid) as u64;
        // SAFETY: index masqué dans l'anneau alloué.
        let e = unsafe { &mut *self.base.add((self.tail & self.mask) as usize) };
        e.set_addr(addr);
        e.set_len(size);
        e.set_bid(bid);
        self.tail = self.tail.wrapping_add(1);
    }

    fn publish(&self) {
        // SAFETY: `tail` est le champ `resv` de la première entrée, partagé avec le noyau.
        let tail = unsafe { &*(BufRingEntry::tail(self.base) as *const AtomicU16) };
        tail.store(self.tail, Ordering::Release);
    }
}

impl Drop for BufRing {
    fn drop(&mut self) {
        // SAFETY: allouée dans `new` avec ce layout.
        unsafe { std::alloc::dealloc(self.base as *mut u8, self.layout) }
    }
}

// ── Serveur ─────────────────────────────────────────────────────────────────

struct Server {
    ring: IoUring,
    bufring: BufRing,
    core: Core,
    listener: TcpListener,
    ops: Vec<Op>,
    no_iowait: bool,
    /// Sends délégués aux workers io-wq (`IOSQE_ASYNC`, RUNTIME_ARCH.md §6.4).
    send_async: bool,
    /// Recv simples délégués aux workers io-wq au lieu du multishot
    /// (`RADIXOX_RECV_ASYNC`, RUNTIME_ARCH.md §6.4) : la réception TCP et la copie
    /// dans nos tampons tournent sur le worker.
    recv_async: bool,
    /// Écritures du dump sur io-wq, épinglé hors de notre cœur (`RADIXOX_FILE_CPUS`).
    file_async: bool,
    /// Attente groupée (expérimental) : `(want, µs)`, le noyau attend `want`
    /// complétions pendant au plus `µs` avant de rendre la main pour une seule.
    min_wait: Option<(u32, u32)>,
    /// `signalfd` de SIGINT / SIGTERM (save à l'arrêt).
    signals: Option<Signals>,
    /// Adresses des `connect` en vol (liens de réplication) : lues par le
    /// noyau jusqu'au CQE.
    connects: std::collections::HashMap<Slot, Box<(libc::sockaddr_storage, libc::socklen_t)>>,
    /// Signal d'arrêt reçu (`true` : save à l'arrêt) : traité en fin de récolte.
    stop: Option<bool>,
}

/// Arrêt propre sur signal : la `signalfd` est lue par io_uring ; à réception,
/// SAVE bloquante puis sortie. Un second signal pendant la save sort tout de
/// suite.
struct Signals {
    fd: RawFd,
    buf: Box<libc::signalfd_siginfo>,
    /// Save à l'arrêt (sinon : seulement le flux de l'AOF sur disque).
    save: bool,
}

impl Signals {
    /// Save à l'arrêt : `RADIXOX_SAVE_ON_EXIT`, sinon `default` (des règles de
    /// save automatique sont actives : Redis sauve aussi à l'arrêt dans ce cas).
    /// Avec l'AOF, toujours installé : le flux en mémoire part sur disque.
    fn install(default: bool, aof: bool) -> io::Result<Option<Self>> {
        let save = std::env::var("RADIXOX_SAVE_ON_EXIT").map_or(default, |v| {
            matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        });
        if !save && !aof {
            return Ok(None);
        }
        // SAFETY: masque initialisé, appels libc sur le thread principal.
        let fd = unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGINT);
            libc::sigaddset(&mut mask, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
            libc::signalfd(-1, &mask, libc::SFD_CLOEXEC)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: structure C, tout à zéro est valide.
        Ok(Some(Self { fd, buf: Box::new(unsafe { std::mem::zeroed() }), save }))
    }

    fn read_sqe(&mut self) -> squeue::Entry {
        opcode::Read::new(Fd(self.fd), (&mut *self.buf as *mut libc::signalfd_siginfo).cast(), size_of::<libc::signalfd_siginfo>() as u32)
            .build()
            .user_data(ud(K_SIGNAL, 0))
    }
}

extern "C" fn exit_now(_sig: i32) {
    // SAFETY: `_exit` est sûr dans un gestionnaire de signal.
    unsafe { libc::_exit(130) }
}

/// Signal reçu : flux de l'AOF sur disque, save si demandée, puis sortie
/// (second signal : sortie immédiate).
/// La boucle ne lira plus la signalfd : un second signal sort tout de suite
/// (gestionnaire classique).
fn exit_on_second_signal() {
    // SAFETY: gestionnaire réduit à `_exit` ; masque initialisé.
    unsafe {
        libc::signal(libc::SIGINT, exit_now as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, exit_now as *const () as libc::sighandler_t);
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
    }
}

fn shutdown(core: &mut Core, save: bool) -> ! {
    exit_on_second_signal();
    if let Err(e) = core.flush_journal_sync() {
        eprintln!("[shutdown] échec de l'écriture de l'AOF : {e}");
        std::process::exit(1)
    }
    if !save {
        std::process::exit(0)
    }
    // BGSAVE en cours : abandonné (la save d'arrêt le remplace).
    core.abort_bgsave();
    match radixox::exec::persist::save_sync(&mut core.art, radixox::exec::persist::sync_level()) {
        Ok(st) => {
            eprintln!(
                "[shutdown] {:.1} MB ({:.1} MB bruts) en {:.2?} (DFS {:.2?})",
                st.written as f64 / 1e6,
                st.raw as f64 / 1e6,
                st.total,
                st.walk
            );
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("[shutdown] échec de la save : {e}");
            std::process::exit(1)
        }
    }
}

impl Server {
    /// Traduit les opérations du cœur en SQE (ou en appels directs).
    fn submit_ops(&mut self) -> io::Result<()> {
        self.core.take_ops(&mut self.ops);
        let mut provided = false;
        for op in std::mem::take(&mut self.ops) {
            let sqe = match op {
                Op::AcceptMulti => {
                    opcode::AcceptMulti::new(Fd(self.listener.as_raw_fd())).build().user_data(ud(K_ACCEPT, 0))
                }
                Op::Install { slot, fd } => {
                    // Accept classique (§4.1) : réglages sur le fd, puis le socket
                    // passe dans la table des fichiers enregistrés et le fd est fermé.
                    tune_socket(fd);
                    let res = self.ring.submitter().register_files_update(slot, &[fd]);
                    close(fd);
                    if let Err(e) = res {
                        eprintln!("[uring] installation du slot {slot} : {e}");
                        // Le cœur attend la fin du recv chaîné : on la simule.
                        self.core.complete(Completion::Recv { slot, res: -libc::EBADF, bid: None, buf_more: false, more: false });
                        continue;
                    }
                    self.recv_sqe(slot)
                }
                Op::Reject { fd } => {
                    let msg = b"-ERR max number of clients reached\r\n";
                    // SAFETY: fd valide, tampon valide.
                    unsafe { libc::write(fd, msg.as_ptr().cast(), msg.len()) };
                    close(fd);
                    continue;
                }
                Op::RecvMulti { slot } => self.recv_sqe(slot),
                Op::Send { slot } => {
                    // Le tampon reste intact jusqu'au CQE (invariant du cœur).
                    let buf = self.core.pending_send(slot);
                    let sqe = opcode::Send::new(Fixed(slot), buf.as_ptr(), buf.len() as u32)
                        .flags(libc::MSG_WAITALL | libc::MSG_NOSIGNAL)
                        .build()
                        .user_data(ud(K_SEND, slot));
                    // Délégué : la pile TCP de l'envoi tourne sur un worker io-wq,
                    // pas pendant notre `enter`.
                    if self.send_async { sqe.flags(squeue::Flags::ASYNC) } else { sqe }
                }
                Op::CancelRecv { slot } => opcode::AsyncCancel::new(ud(K_RECV, slot))
                    .build()
                    .flags(squeue::Flags::SKIP_SUCCESS)
                    .user_data(ud(K_CANCEL, slot)),
                Op::CancelSend { slot } => opcode::AsyncCancel::new(ud(K_SEND, slot))
                    .build()
                    .flags(squeue::Flags::SKIP_SUCCESS)
                    .user_data(ud(K_CANCEL, slot)),
                Op::Close { slot } => opcode::Close::new(Fixed(slot))
                    .build()
                    .flags(squeue::Flags::SKIP_SUCCESS)
                    .user_data(ud(K_CLOSE, slot)),
                // Numéro du BGSAVE dans le champ slot du user_data.
                Op::FileWrite { fd, off, save } => {
                    // Le tampon reste intact jusqu'au CQE (invariant du BGSAVE).
                    let buf = self.core.pending_file_write(save);
                    let sqe = opcode::Write::new(Fd(fd), buf.as_ptr(), buf.len() as u32)
                        .offset(off)
                        .build()
                        .user_data(ud(K_FILE, save));
                    // Sinon la copie dans le cache de pages se fait dans notre `enter`.
                    if self.file_async { sqe.flags(squeue::Flags::ASYNC) } else { sqe }
                }
                Op::FileSync { fd, save } => opcode::Fsync::new(Fd(fd))
                    .flags(io_uring::types::FsyncFlags::DATASYNC)
                    .build()
                    .user_data(ud(K_FILE, save)),
                // Second canal de fichiers : le segment de l'AOF (DURABILITY_DESIGN.md §7.3).
                // Génération de l'écrivain dans le champ slot du user_data.
                Op::JournalWrite { fd, off, writer, link } => {
                    // Le tampon reste intact jusqu'au CQE (invariant de l'écrivain).
                    let buf = self.core.pending_journal_write(writer);
                    let sqe = opcode::Write::new(Fd(fd), buf.as_ptr(), buf.len() as u32)
                        .offset(off)
                        .build()
                        .user_data(ud(K_JWRITE, writer));
                    let sqe = if self.file_async { sqe.flags(squeue::Flags::ASYNC) } else { sqe };
                    if link {
                        // Le fsync lié suit dans la même soumission : une chaîne
                        // coupée par une SQ pleine laisserait le fsync partir seul.
                        self.reserve_sqes(2)?;
                        sqe.flags(squeue::Flags::IO_LINK)
                    } else {
                        sqe
                    }
                }
                Op::JournalSync { fd, writer } => {
                    let sqe = opcode::Fsync::new(Fd(fd))
                        .flags(io_uring::types::FsyncFlags::DATASYNC)
                        .build()
                        .user_data(ud(K_JSYNC, writer));
                    if self.file_async { sqe.flags(squeue::Flags::ASYNC) } else { sqe }
                }
                Op::ProvideBuf { bid } => {
                    self.bufring.push(&mut self.core, bid);
                    provided = true;
                    continue;
                }
                // Lien de réplication (DURABILITY_DESIGN.md §10) : socket créé ici,
                // installé dans le slot comme un accept, puis `connect` asynchrone.
                Op::Connect { slot, addr } => {
                    let sa = Box::new(sockaddr(addr));
                    // SAFETY: appel libc simple.
                    let fd = unsafe { libc::socket(sa.0.ss_family as i32, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
                    if fd < 0 {
                        let e = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL);
                        self.core.complete(Completion::Connect { slot, res: -e });
                        continue;
                    }
                    tune_socket(fd);
                    let res = self.ring.submitter().register_files_update(slot, &[fd]);
                    close(fd);
                    if let Err(e) = res {
                        self.core.complete(Completion::Connect { slot, res: -e.raw_os_error().unwrap_or(libc::EBADF) });
                        continue;
                    }
                    let sqe = opcode::Connect::new(Fixed(slot), (&sa.0 as *const libc::sockaddr_storage).cast(), sa.1)
                        .build()
                        .user_data(ud(K_CONNECT, slot));
                    self.connects.insert(slot, sa);
                    sqe
                }
                // Suites de fichiers (DURABILITY_DESIGN.md §8.6) : toujours sur un
                // worker io-wq (`IOSQE_ASYNC`), jamais dans notre `enter`.
                Op::Fs { id } => {
                    let Some(step) = self.core.fs_step(id) else { continue };
                    let cwd = Fd(libc::AT_FDCWD);
                    let sqe = match step {
                        StepRef::Open { path, flags, mode } => {
                            opcode::OpenAt::new(cwd, path.as_ptr()).flags(flags).mode(mode).build()
                        }
                        StepRef::Fallocate { fd, off, len } => opcode::Fallocate::new(Fd(fd), len).offset(off).build(),
                        StepRef::Fsync { fd, data } => {
                            let f = if data { io_uring::types::FsyncFlags::DATASYNC } else { io_uring::types::FsyncFlags::empty() };
                            opcode::Fsync::new(Fd(fd)).flags(f).build()
                        }
                        StepRef::Write { fd, buf, off } => {
                            let len = buf.len().min(u32::MAX as usize) as u32;
                            opcode::Write::new(Fd(fd), buf.as_ptr(), len).offset(off).build()
                        }
                        StepRef::Ftruncate { fd, len } => opcode::Ftruncate::new(Fd(fd), len).build(),
                        StepRef::Rename { from, to } => opcode::RenameAt::new(cwd, from.as_ptr(), cwd, to.as_ptr()).build(),
                        StepRef::Unlink { path } => opcode::UnlinkAt::new(cwd, path.as_ptr()).build(),
                        StepRef::Close { fd } => opcode::Close::new(Fd(fd)).build(),
                    };
                    sqe.flags(squeue::Flags::ASYNC).user_data(ud(K_FS, id))
                }
            };
            self.push_sqe(&sqe)?;
        }
        if provided {
            self.bufring.publish();
        }
        Ok(())
    }

    /// Place pour `n` SQE consécutives (une chaîne liée) sans soumission entre.
    /// Recv d'une connexion : multishot (défaut), ou simple sur io-wq. Le recv
    /// simple rend `more = false` à chaque CQE : le cœur le réarme comme un
    /// multishot terminé. Les deux choisissent leur tampon dans notre anneau.
    fn recv_sqe(&self, slot: Slot) -> squeue::Entry {
        if self.recv_async {
            opcode::Recv::new(Fixed(slot), std::ptr::null_mut(), 0)
                .buf_group(BGID)
                .build()
                .flags(squeue::Flags::BUFFER_SELECT | squeue::Flags::ASYNC)
                .user_data(ud(K_RECV, slot))
        } else {
            opcode::RecvMulti::new(Fixed(slot), BGID).build().user_data(ud(K_RECV, slot))
        }
    }

    fn reserve_sqes(&mut self, n: usize) -> io::Result<()> {
        loop {
            let free = {
                let sq = self.ring.submission();
                sq.capacity() - sq.len()
            };
            if free >= n {
                return Ok(());
            }
            self.ring.submit()?;
        }
    }

    fn push_sqe(&mut self, sqe: &squeue::Entry) -> io::Result<()> {
        // SAFETY: les tampons visés (envoi, fds) vivent jusqu'au CQE.
        while unsafe { self.ring.submission().push(sqe) }.is_err() {
            // SQ pleine en plein tour : on soumet et on continue (§16).
            self.ring.submit()?;
        }
        Ok(())
    }

    /// Récolte les CQE et les passe au cœur.
    fn harvest(&mut self) -> bool {
        let mut any = false;
        let mut cq = self.ring.completion();
        cq.sync();
        // Recopiés d'abord : `complete` peut vouloir soumettre (rien ici, mais
        // le cœur ne doit pas dépendre de l'ordre des emprunts).
        let cqes: Vec<cqueue::Entry> = cq.collect();
        for cqe in cqes {
            any = true;
            let (kind, slot) = (cqe.user_data() >> 32, cqe.user_data() as u32);
            let (res, flags) = (cqe.result(), cqe.flags());
            let c = match kind {
                K_ACCEPT => Completion::Accept { fd: res, more: cqueue::more(flags) },
                K_RECV => Completion::Recv {
                    slot,
                    res,
                    bid: cqueue::buffer_select(flags),
                    buf_more: cqueue::buffer_more(flags),
                    more: cqueue::more(flags),
                },
                K_SEND => Completion::Send { slot, res },
                K_SIGNAL => {
                    self.stop = Some(self.signals.as_ref().is_some_and(|s| s.save));
                    continue;
                }
                K_FILE => Completion::File { res, save: slot },
                K_JWRITE => Completion::JournalWrite { res, writer: slot },
                K_JSYNC => Completion::JournalSync { res, writer: slot },
                K_FS => Completion::Fs { id: slot, res },
                K_CONNECT => {
                    self.connects.remove(&slot);
                    Completion::Connect { slot, res }
                }
                // Annulation d'une op déjà finie, close d'un slot : rien à faire.
                K_CANCEL | K_CLOSE => {
                    if res < 0 && res != -libc::ENOENT && res != -libc::EALREADY {
                        eprintln!("[uring] kind {kind} slot {slot} : erreur {res}");
                    }
                    continue;
                }
                _ => unreachable!("user_data inconnu {:#x}", cqe.user_data()),
            };
            self.core.complete(c);
        }
        any
    }

    /// Arrêt : les suites de fichiers en cours (rotation de l'AOF, renommage
    /// d'un dump) vont à leur terme avant l'écriture finale du journal.
    fn finish_fs(&mut self) -> io::Result<()> {
        eprintln!("[shutdown] signal reçu : arrêt (second signal : sortie immédiate)");
        exit_on_second_signal();
        loop {
            self.submit_ops()?;
            if !self.core.fs_pending() {
                return Ok(());
            }
            self.ring.submit_and_wait(1)?;
            self.harvest();
        }
    }

    fn run(&mut self) -> io::Result<()> {
        if let Some(sig) = self.signals.as_mut() {
            let sqe = sig.read_sqe();
            self.push_sqe(&sqe)?;
        }
        loop {
            // Deux horloges par tour : murale pour le TTL, monotone pour les timers.
            self.core.tick();
            self.core.set_now(Instant::now());
            self.harvest();
            if let Some(save) = self.stop.take() {
                self.finish_fs()?;
                shutdown(&mut self.core, save);
            }
            self.core.fire_timers();
            // Par tranches : les réponses des connexions qui ont fini partent
            // entre deux tranches au lieu d'attendre la fin du tour.
            let mut slices = 0;
            while self.core.run_batch() && slices < MAX_SLICES {
                self.core.flush();
                self.submit_ops()?;
                self.ring.submit()?;
                slices += 1;
            }
            if self.core.has_jobs() {
                // Les réponses du tour partent avant la tranche de fond : sinon
                // chacune attendrait la tranche (p99 sous BGSAVE ×1,6 mesuré).
                self.core.flush();
                self.submit_ops()?;
                self.ring.submit()?;
                self.core.run_jobs();
            }
            self.core.flush();
            self.submit_ops()?;
            if self.core.quiet() && self.core.has_background() {
                // Les réponses du tour partent avant : le travail de fond peut
                // durer jusqu'à IDLE_WORK_MAX.
                if !self.ring.submission().is_empty() {
                    self.ring.submit()?;
                }
                self.idle_work();
            }
            // Le bloc du tour se ferme ici, travail au repos compris ; ses
            // écritures (AOF) et ses envois (réplicas) partent avec ce tour.
            self.core.end_turn();
            self.submit_ops()?;
            let res = if self.core.idle() {
                self.wait()
            } else {
                // Encore du travail : on soumet et on récolte sans attendre
                // (GETEVENTS ajouté par le crate quand des complétions attendent).
                self.ring.submit()
            };
            if let Err(e) = res
                && !matches!(e.raw_os_error(), Some(libc::ETIME) | Some(libc::EINTR) | Some(libc::EBUSY))
            {
                return Err(e);
            }
        }
    }

    /// Travail de fond tant que personne n'attend : ni client côté cœur, ni
    /// complétion côté noyau. Avec `DEFER_TASKRUN`, le noyau lève
    /// `IORING_SQ_TASKRUN` dès qu'un recv ou un send a du travail pour nous
    /// (RUNTIME_ARCH.md §8.2) : une requête qui arrive attend au plus la fin
    /// de l'étape en cours. Borné par la prochaine échéance de timer et
    /// `IDLE_WORK_MAX`, pour que l'horloge et les timers avancent.
    fn idle_work(&mut self) {
        let budget = self
            .core
            .next_deadline()
            .map_or(IDLE_WORK_MAX, |t| t.saturating_duration_since(Instant::now()).min(IDLE_WORK_MAX));
        let end = tsc::Deadline::after(budget);
        while !self.ring.submission().taskrun() && self.core.quiet() && self.core.idle_step() && !end.passed() {}
    }

    /// Soumet et attend au moins une complétion, au plus jusqu'à la prochaine
    /// échéance de timer.
    fn wait(&mut self) -> io::Result<usize> {
        let timeout = self
            .core
            .next_deadline()
            .map_or(Duration::from_secs(1), |t| t.saturating_duration_since(Instant::now()));
        let ts = Timespec::from(timeout);
        let (want, usec) = self.min_wait.unwrap_or((1, 0));
        let args = SubmitArgs::new().timespec(&ts).min_wait_usec(usec);
        let mut flags = EnterFlags::GETEVENTS | EnterFlags::EXT_ARG;
        if self.no_iowait {
            flags |= EnterFlags::NO_IOWAIT;
        }
        let to_submit = self.ring.submission().len() as u32;
        // SAFETY: `args` vit pendant l'appel.
        unsafe { self.ring.submitter().enter(to_submit, want, flags.bits(), Some(&args)) }
    }
}

/// Durée maximale d'une passe de travail de fond sans repasser par le tour
/// (horloges, timers).
const IDLE_WORK_MAX: Duration = Duration::from_millis(1);

fn close(fd: RawFd) {
    // SAFETY: fd possédé, fermé une fois.
    unsafe { libc::close(fd) };
}

fn setsockopt_i32(fd: RawFd, level: i32, name: i32, val: i32) {
    // SAFETY: option entière, pointeur valide.
    let r = unsafe {
        libc::setsockopt(fd, level, name, (&val as *const i32).cast(), size_of::<i32>() as libc::socklen_t)
    };
    if r != 0 {
        eprintln!("[uring] setsockopt({level}, {name}) : {}", io::Error::last_os_error());
    }
}

/// `SocketAddr` → adresse C pour `connect`.
fn sockaddr(addr: std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: structure C, tout à zéro est valide ; les vues `sockaddr_in(6)`
    // tiennent dans `sockaddr_storage`.
    unsafe {
        let mut st: libc::sockaddr_storage = std::mem::zeroed();
        let len = match addr {
            std::net::SocketAddr::V4(a) => {
                let sin = &mut *(&mut st as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>();
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = a.port().to_be();
                sin.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) };
                size_of::<libc::sockaddr_in>()
            }
            std::net::SocketAddr::V6(a) => {
                let sin6 = &mut *(&mut st as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>();
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = a.port().to_be();
                sin6.sin6_flowinfo = a.flowinfo();
                sin6.sin6_addr = libc::in6_addr { s6_addr: a.ip().octets() };
                sin6.sin6_scope_id = a.scope_id();
                size_of::<libc::sockaddr_in6>()
            }
        };
        (st, len as libc::socklen_t)
    }
}

/// `RADIXOX_REPLICATE="h1:p1,h2:p2"` : cibles de réplication au démarrage
/// (`RADIXOX_REPLICATE_AUTH` : mot de passe des cibles). IPv6 entre crochets.
fn replicate_from_env(core: &mut Core) -> io::Result<()> {
    let Ok(list) = std::env::var("RADIXOX_REPLICATE") else { return Ok(()) };
    let auth = std::env::var("RADIXOX_REPLICATE_AUTH").ok().filter(|p| !p.is_empty()).map(String::into_bytes);
    for t in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (host, port) = t
            .rsplit_once(':')
            .and_then(|(h, p)| Some((h.trim_start_matches('[').trim_end_matches(']'), p.parse::<u16>().ok()?)))
            .ok_or_else(|| io::Error::other(format!("RADIXOX_REPLICATE : cible {t:?} (attendu host:port)")))?;
        core.replicate(host.to_string(), port, auth.clone()).map_err(io::Error::other)?;
    }
    Ok(())
}

/// Réglages d'un socket accepté (RUNTIME_ARCH.md §4.1, §4.2).
fn tune_socket(fd: RawFd) {
    // Sans Nagle : le p99.99 en pipeline passait de 43 ms à ~8 ms.
    setsockopt_i32(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, 1);
    // Client qui ne lit plus et ne ferme pas : le noyau coupe au bout de 60 s
    // (sinon un send resterait en vol à jamais).
    setsockopt_i32(fd, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 60_000);
    setsockopt_i32(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    setsockopt_i32(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 300);
}

fn pin_cpu() {
    let Ok(v) = std::env::var("RADIXOX_CPU_PIN") else { return };
    match v.parse::<usize>() {
        Ok(cpu) if cpu < libc::CPU_SETSIZE as usize => {
            // SAFETY: cpu_set_t initialisé à zéro puis rempli.
            let ok = unsafe {
                let mut set: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_SET(cpu, &mut set);
                libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) == 0
            };
            if ok {
                println!("CPU pinned: {cpu}");
            } else {
                eprintln!("RADIXOX_CPU_PIN={cpu} : {}", io::Error::last_os_error());
            }
        }
        _ => eprintln!("RADIXOX_CPU_PIN invalide : {v}"),
    }
}

/// La table des fichiers enregistrés est bornée par `RLIMIT_NOFILE` (souvent
/// 1024 en limite souple) : on remonte la limite souple jusqu'à la dure, et si
/// ça ne suffit pas on réduit le nombre de connexions, comme Redis pour
/// `maxclients`. Rend le nombre de slots utilisables.
fn raise_nofile(wanted: u32) -> u32 {
    // Marge pour les autres fd du processus (écoute, signalfd, dump, anneau…).
    const SPARE: u64 = 32;
    // SAFETY: structure initialisée, appels libc simples.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return wanted;
        }
        let need = wanted as u64 + SPARE;
        if lim.rlim_cur < need {
            lim.rlim_cur = need.min(lim.rlim_max);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
            }
        }
        let max = lim.rlim_cur.saturating_sub(SPARE).min(wanted as u64) as u32;
        if max < wanted {
            eprintln!(
                "WARNING: RLIMIT_NOFILE = {} : connexions limitées à {max} au lieu de {wanted} \
                 (ulimit -n ou LimitNOFILE= pour en avoir plus)",
                lim.rlim_cur
            );
        }
        max
    }
}

/// `RADIXOX_MIN_WAIT_US=<µs>` (expérimental) : au repos, attendre jusqu'à
/// `RADIXOX_WAIT_NR` complétions (défaut 32) pendant au plus ce délai, pour
/// regrouper le travail en lots au lieu de se réveiller pour une seule.
fn min_wait(ring: &IoUring) -> Option<(u32, u32)> {
    let usec: u32 = std::env::var("RADIXOX_MIN_WAIT_US").ok()?.parse().ok().filter(|&u| u > 0)?;
    if !ring.params().is_feature_min_timeout() {
        eprintln!("[uring] RADIXOX_MIN_WAIT_US ignoré : noyau sans IORING_FEAT_MIN_TIMEOUT");
        return None;
    }
    let want = std::env::var("RADIXOX_WAIT_NR").ok().and_then(|v| v.parse().ok()).unwrap_or(32u32).max(1);
    Some((want, usec))
}

/// Délégation à io-wq (RUNTIME_ARCH.md §6.4), version statique ; rend
/// `(sends, fichiers)` délégués.
/// - `RADIXOX_SEND_ASYNC=1` : sends en `IOSQE_ASYNC` ; `RADIXOX_RECV_ASYNC=1` :
///   recv simples en `IOSQE_ASYNC` au lieu du multishot ; `RADIXOX_IOWQ_WORKERS`
///   fixe le plafond de workers réseau (1 par défaut : sans plafond, le noyau
///   peut en créer des dizaines) ; `RADIXOX_IOWQ_CPUS` (« 1,7 ») les épingle.
/// - `RADIXOX_FILE_CPUS` (« 1 ») : écritures du dump en `IOSQE_ASYNC`, un worker
///   fichiers épinglé sur ces CPU. La copie dans le cache de pages et le fsync
///   ne prennent plus notre cœur (l'équivalent du fils de Valkey sur un autre
///   cœur, §8.2).
///
/// L'affinité vaut pour tous les workers du ring : c'est l'union des deux listes.
fn setup_iowq(ring: &IoUring) -> io::Result<(bool, bool, bool)> {
    let send = env_yes("RADIXOX_SEND_ASYNC", false);
    let recv = env_yes("RADIXOX_RECV_ASYNC", false);
    let net = send || recv;
    let file_cpus = std::env::var("RADIXOX_FILE_CPUS").ok().filter(|v| !v.trim().is_empty());
    if !net && file_cpus.is_none() {
        return Ok((false, false, false));
    }
    let workers: u32 = std::env::var("RADIXOX_IOWQ_WORKERS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    // [bounded (disque), unbounded (réseau)] ; 0 = inchangé. Une écriture du
    // dump au plus en vol : un worker fichiers suffit.
    let mut max = [u32::from(file_cpus.is_some()), if net { workers.max(1) } else { 0 }];
    ring.submitter().register_iowq_max_workers(&mut max)?;
    let mut parts = Vec::new();
    if net {
        let what = match (send, recv) {
            (true, true) => "sends et recv",
            (true, false) => "sends",
            _ => "recv",
        };
        parts.push(format!("{what} délégués à io-wq : {} worker(s) réseau", workers.max(1)));
    }
    let cpus: Vec<String> = [std::env::var("RADIXOX_IOWQ_CPUS").ok().filter(|_| net), file_cpus.clone()]
        .into_iter()
        .flatten()
        .collect();
    if let Some(f) = &file_cpus {
        parts.push(format!("écritures du dump sur io-wq (CPU {f})"));
    }
    if !cpus.is_empty() {
        // SAFETY: cpu_set_t initialisé à zéro puis rempli.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        for c in cpus.iter().flat_map(|l| l.split(',')).filter_map(|c| c.trim().parse::<usize>().ok()) {
            unsafe { libc::CPU_SET(c, &mut set) };
        }
        ring.submitter().register_iowq_aff(&set)?;
        parts.push(format!("affinité io-wq : CPU {}", cpus.join(",")));
    }
    println!("{}", parts.join(" ; "));
    Ok((send, recv, file_cpus.is_some()))
}

/// Taille : octets, ou suffixe `kb` / `mb` / `gb` (puissances de 1024, comme Valkey).
fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim().to_ascii_lowercase();
    let (num, mul) = match v.strip_suffix("gb").or_else(|| v.strip_suffix('g')) {
        Some(n) => (n, 1u64 << 30),
        None => match v.strip_suffix("mb").or_else(|| v.strip_suffix('m')) {
            Some(n) => (n, 1 << 20),
            None => match v.strip_suffix("kb").or_else(|| v.strip_suffix('k')) {
                Some(n) => (n, 1 << 10),
                None => (v.as_str(), 1),
            },
        },
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mul)
}

/// Variable booléenne (`yes`/`no`, `1`/`0`, `true`/`false`, `on`/`off`).
fn env_yes(name: &str, default: bool) -> bool {
    std::env::var(name).map_or(default, |v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn build_ring() -> io::Result<IoUring> {
    IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .setup_taskrun_flag()
        .setup_cqsize(CQ_ENTRIES)
        .build(SQ_ENTRIES)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "io_uring indisponible ({e}). Noyau 6.12 ou plus récent requis ; en conteneur, \
                     le profil seccomp par défaut de Docker bloque io_uring \
                     (--security-opt seccomp=unconfined ou un profil qui l'autorise) ; \
                     vérifier aussi sysctl kernel.io_uring_disabled"
                ),
            )
        })
}

fn main() -> io::Result<()> {
    tsc::init();
    pin_cpu();
    let port: u16 = std::env::var("RADIXOX_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(6379);
    // 127.0.0.1 par défaut : sans `RADIXOX_BIND`, pas joignable depuis le réseau.
    let bind = std::env::var("RADIXOX_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let addr = if bind.contains(':') { format!("[{bind}]:{port}") } else { format!("{bind}:{port}") };
    if radixox::exec::auth::password().is_none() && !matches!(bind.as_str(), "127.0.0.1" | "::1" | "localhost") {
        eprintln!("WARNING: listening on {bind} without RADIXOX_PASSWORD: anyone who can reach this port has full access");
    }

    let mut art = OxidArt::new();
    art.tick();
    // AOF (DURABILITY_DESIGN.md §9) : reprise depuis `appendonlydir`, ou premier
    // démarrage depuis `dump.rxd` suivi de la création de la base.
    let aof = if env_yes("RADIXOX_APPENDONLY", false) {
        let fsync = std::env::var("RADIXOX_APPENDFSYNC")
            .map_or(Ok(aof::Fsync::EverySec), |v| aof::Fsync::parse(&v))
            .map_err(|e| io::Error::other(format!("RADIXOX_APPENDFSYNC : {e}")))?;
        let dir = aof::aof_dir();
        let start = Instant::now();
        let res = match aof::recover(&mut art, &dir, env_yes("RADIXOX_AOF_LOAD_TRUNCATED", true))? {
            Some(res) => {
                println!(
                    "[aof] {} : base {} + {} segment(s), {} blocs, {:.1} MB de flux, en {:.2?}",
                    dir.display(),
                    res.manifest.base,
                    res.manifest.incrs.len(),
                    res.cursor.seq,
                    res.cursor.offset as f64 / 1e6,
                    start.elapsed()
                );
                res
            }
            None => {
                radixox::exec::persist::load_at_startup(&mut art)?;
                let res = aof::create(&mut art, &dir)?;
                println!("[aof] {} créé (base.1.rxdb, journal.1.rxj)", dir.display());
                res
            }
        };
        aof::remove_unlisted(&dir, &res.manifest)?;
        let writer = aof::AofWriter::open(&dir, &res, fsync)?;
        art.tick();
        Some((radixox::exec::journal::Stream::new(res.cursor), writer))
    } else {
        radixox::exec::persist::load_at_startup(&mut art)?;
        None
    };

    let ring = build_ring()?;
    let mut cfg = Config::default();
    if let Some(w) = std::env::var("RADIXOX_PREFETCH").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.prefetch = w.max(1);
    }
    // Anneau de réception (RUNTIME_ARCH.md §5.1) : nombre et taille des tampons.
    if let Some(n) = std::env::var("RADIXOX_RING_BUFS").ok().and_then(|v| v.parse::<u16>().ok()) {
        cfg.bufs = n.max(1);
    }
    if let Some(kb) = std::env::var("RADIXOX_RING_BUF_KB").ok().and_then(|v| v.parse::<u32>().ok()) {
        cfg.buf_size = kb.max(4) << 10;
    }
    // Commandes par tour au plus (RUNTIME_ARCH.md §2.1, §2.3).
    if let Some(n) = std::env::var("RADIXOX_BATCH_CMDS").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.batch_cmds = n.max(1);
    }
    // `timeout` de Redis : 0 ou absent = jamais.
    if let Some(secs) = std::env::var("RADIXOX_TIMEOUT").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.idle_timeout = (secs > 0).then(|| Duration::from_secs(secs));
    }
    // `save` de Valkey : mêmes règles par défaut, `RADIXOX_SAVE=""` désactive.
    cfg.save_rules = autosave::rules_from_env().map_err(|e| io::Error::other(format!("RADIXOX_SAVE : {e}")))?;
    cfg.stop_writes_on_bgsave_error = autosave::stop_writes_from_env();
    cfg.max_conn = raise_nofile(cfg.max_conn);
    ring.submitter().register_files_sparse(cfg.max_conn)?;
    let no_iowait = ring.params().is_feature_no_iowait();
    let (send_async, recv_async, file_async) = setup_iowq(&ring)?;
    let min_wait = min_wait(&ring);
    if let Some(mb) = std::env::var("RADIXOX_AOF_MAX_PENDING_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.aof_max_pending = mb.max(1) << 20;
    }
    if let Some(ms) = std::env::var("RADIXOX_AOF_CYCLE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.aof_cycle = Duration::from_millis(ms.max(1));
    }
    if let Some(kb) = std::env::var("RADIXOX_AOF_CYCLE_MIN_KB").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.aof_cycle_min = kb.max(1) << 10;
    }
    if let Some(mb) = std::env::var("RADIXOX_AOF_SYNC_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.aof_sync_bytes = mb.max(1) << 20;
    }
    // `auto-aof-rewrite-percentage` (0 = jamais) et `auto-aof-rewrite-min-size`.
    if let Some(p) = std::env::var("RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.aof_rewrite_percentage = p;
    }
    if let Ok(v) = std::env::var("RADIXOX_AUTO_AOF_REWRITE_MIN_SIZE") {
        cfg.aof_rewrite_min_size =
            parse_size(&v).ok_or_else(|| io::Error::other(format!("RADIXOX_AUTO_AOF_REWRITE_MIN_SIZE : {v:?}")))?;
    }
    if let Some(mb) = std::env::var("RADIXOX_REPL_BACKLOG_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.repl_backlog = mb << 20;
    }
    if let Some(mb) = std::env::var("RADIXOX_REPL_MAX_LAG_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.repl_max_lag = mb.max(1) << 20;
    }
    let aof_on = aof.is_some();
    let mut core = Core::new(cfg, art);
    if let Some((stream, writer)) = aof {
        core.set_aof(stream, writer);
    }
    replicate_from_env(&mut core)?;
    let bufring = BufRing::new(&ring, &mut core)?;
    let listener = TcpListener::bind(&addr)?;
    println!("RadixOx RESP Server (io_uring loop) listening on {addr}");

    // Avec l'AOF, le flux suffit à l'arrêt : pas de save par défaut.
    let signals = Signals::install(core.autosave_on() && !aof_on, aof_on)?;
    let mut server = Server { ring, bufring, core, listener, ops: Vec::new(), no_iowait, send_async, recv_async, file_async, min_wait,
        signals,
        connects: Default::default(),
        stop: None,
    };
    server.run()
}
