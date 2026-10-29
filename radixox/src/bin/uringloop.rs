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

/// Allocateur de RadixOx pour tout le processus (MEMORY_ARCH.md §18.12) ; la libc sous
/// `--cfg radixox_sysalloc` (AddressSanitizer).
#[cfg(not(radixox_sysalloc))]
#[global_allocator]
static HEAP: radixox_lib::raw_alloc::heap::RxAlloc = radixox_lib::raw_alloc::heap::RxAlloc;

/// `malloc` & co. de tout le processus sur le même tas (MEMORY_ARCH.md §18.13) : la libc
/// elle-même et le C lié (zstd) n'allouent plus chez la libc. Pas sous `radixox_sysalloc`.
#[cfg(not(radixox_sysalloc))]
mod c_malloc {
    use radixox_lib::raw_alloc::heap::c;
    use std::ffi::c_void;

    #[unsafe(no_mangle)]
    pub extern "C" fn malloc(size: usize) -> *mut c_void {
        c::malloc(size) as _
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn free(p: *mut c_void) {
        unsafe { c::free(p as _) }
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn calloc(n: usize, size: usize) -> *mut c_void {
        c::calloc(n, size) as _
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn realloc(p: *mut c_void, size: usize) -> *mut c_void {
        unsafe { c::realloc(p as _, size) as _ }
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn reallocarray(p: *mut c_void, n: usize, size: usize) -> *mut c_void {
        unsafe { c::reallocarray(p as _, n, size) as _ }
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
        c::aligned_alloc(align, size) as _
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn memalign(align: usize, size: usize) -> *mut c_void {
        c::aligned_alloc(align, size) as _
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> i32 {
        unsafe { c::posix_memalign(out as _, align, size) }
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn valloc(size: usize) -> *mut c_void {
        c::valloc(size) as _
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn pvalloc(size: usize) -> *mut c_void {
        c::pvalloc(size) as _
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn malloc_usable_size(p: *mut c_void) -> usize {
        c::usable_size(p as _)
    }
}

use std::io;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use core::time::Duration;
use radixox_lib::sys::Instant;

use oxidart::OxidArt;
use radixox::exec::fs_task::StepRef;
use radixox::exec::{aof, autosave};
use radixox::ring::{self, BufRingEntry, Cqe, Ring, Sqe, Target, WaitArgs, flags as sqf};
use radixox::tsc;
use radixox::uring::{Bid, Completion, Config, Core, Op, Slot};
use rustix::io::Errno;
use rustix::net::{RecvFlags, SendFlags, SocketAddrAny, sockopt};

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
    fn new(ring: &Ring, core: &mut Core) -> io::Result<Self> {
        let count = core.bufs_mut().count();
        let entries = (count as u32).next_power_of_two().max(1) as u16;
        let layout = std::alloc::Layout::from_size_align(entries as usize * size_of::<BufRingEntry>(), 4096)
            .expect("layout");
        // SAFETY: taille non nulle ; mémoire libérée dans `Drop`, après l'anneau.
        let base = unsafe { std::alloc::alloc_zeroed(layout) } as *mut BufRingEntry;
        assert!(!base.is_null(), "allocation de l'anneau de tampons");
        // SAFETY: `base` vit aussi longtemps que l'anneau io_uring (champ du serveur).
        unsafe { ring.register_buf_ring(base, entries, BGID, ring::PBUF_RING_INC) }
            .map_err(|e| {
                io::Error::new(
                    io::Error::from(e).kind(),
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
        e.addr = addr;
        e.len = size;
        e.bid = bid;
        self.tail = self.tail.wrapping_add(1);
    }

    fn publish(&self) {
        // SAFETY: anneau enregistré, vivant aussi longtemps que `self`.
        unsafe { ring::buf_ring_tail(self.base) }.store(self.tail, Ordering::Release);
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
    ring: Ring,
    bufring: BufRing,
    /// CQE récoltées (réutilisé d'un tour à l'autre).
    cqes: Vec<Cqe>,
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
    /// Attente groupée (RUNTIME_ARCH.md §12.5) : le noyau attend `want` complétions
    /// pendant au plus `µs` avant de rendre la main pour une seule.
    min_wait: MinWait,
    /// Anneau en SQPOLL (`RADIXOX_SQPOLL_CPU`, RUNTIME_ARCH.md §6.4 bis) : un thread
    /// noyau lit la SQ et fait tourner nos recv ; sans `DEFER_TASKRUN`, les CQE
    /// arrivent dans la CQ sans `enter` de notre part.
    sqpoll: bool,
    /// SQPOLL : au repos, la boucle surveille la CQ pendant au plus cette durée
    /// avant de s'endormir dans `enter` (`RADIXOX_SQPOLL_SPIN_US`). Sinon chaque
    /// CQE coûte au thread SQPOLL un réveil de la boucle.
    spin: Duration,
    /// `signalfd` de SIGINT / SIGTERM (save à l'arrêt).
    signals: Option<Signals>,
    /// Adresses des `connect` en vol (liens de réplication) : lues par le
    /// noyau jusqu'au CQE.
    connects: std::collections::HashMap<Slot, Box<SocketAddrAny>>,
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
        // SAFETY: masque initialisé, appels sur le thread principal.
        unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGINT);
            libc::sigaddset(&mut mask, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        }
        // `signalfd4` en appel direct (absent de rustix) : masque du noyau
        // sur 8 octets, bit `n - 1` pour le signal `n`.
        let kmask: u64 = (1 << (libc::SIGINT - 1)) | (1 << (libc::SIGTERM - 1));
        // SAFETY: masque valide pendant l'appel.
        let r = unsafe { sc::syscall!(SIGNALFD4, -1isize, &kmask as *const u64, 8usize, libc::SFD_CLOEXEC) } as isize;
        if r < 0 {
            return Err(io::Error::from_raw_os_error(-r as i32));
        }
        let fd = r as RawFd;
        // SAFETY: structure C, tout à zéro est valide.
        Ok(Some(Self { fd, buf: Box::new(unsafe { std::mem::zeroed() }), save }))
    }

    fn read_sqe(&mut self) -> Sqe {
        Sqe::read(Target::Fd(self.fd), (&mut *self.buf as *mut libc::signalfd_siginfo).cast(), size_of::<libc::signalfd_siginfo>() as u32)
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
    let e = try_shutdown(core, save);
    eprintln!("[shutdown] {e}");
    std::process::exit(1)
}

/// Flux de l'AOF sur disque, save si demandée, puis sortie. Rend l'erreur sans sortir.
fn try_shutdown(core: &mut Core, save: bool) -> String {
    if let Err(e) = core.flush_journal_sync() {
        return format!("échec de l'écriture de l'AOF : {e}");
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
        Err(e) => format!("échec de la save : {e}"),
    }
}

impl Server {
    /// Traduit les opérations du cœur en SQE (ou en appels directs).
    fn submit_ops(&mut self) -> io::Result<()> {
        self.core.take_ops(&mut self.ops);
        let mut provided = false;
        for op in std::mem::take(&mut self.ops) {
            let sqe = match op {
                Op::AcceptMulti => Sqe::accept_multi(Target::Fd(self.listener.as_raw_fd())).user_data(ud(K_ACCEPT, 0)),
                Op::Install { slot, fd } => {
                    // Accept classique (§4.1) : réglages sur le fd, puis le socket
                    // passe dans la table des fichiers enregistrés et le fd est fermé.
                    tune_socket(fd);
                    let res = self.ring.register_files_update(slot, &[fd]);
                    close(fd);
                    if let Err(e) = res {
                        eprintln!("[uring] installation du slot {slot} : {e}");
                        // Le cœur attend la fin du recv chaîné : on la simule.
                        self.core.complete(Completion::Recv { slot, res: -Errno::BADF.raw_os_error(), bid: None, buf_more: false, more: false });
                        continue;
                    }
                    self.recv_sqe(slot)
                }
                Op::Reject { fd } => {
                    let msg = b"-ERR max number of clients reached\r\n";
                    // SAFETY: fd accepté, fermé juste après.
                    let _ = rustix::io::write(unsafe { BorrowedFd::borrow_raw(fd) }, msg);
                    close(fd);
                    continue;
                }
                Op::RecvMulti { slot } => self.recv_sqe(slot),
                Op::Send { slot } => {
                    // Le tampon reste intact jusqu'au CQE (invariant du cœur).
                    let buf = self.core.pending_send(slot);
                    let sqe = Sqe::send(Target::Fixed(slot), buf.as_ptr(), buf.len() as u32, RecvFlags::WAITALL.bits() | SendFlags::NOSIGNAL.bits())
                        .user_data(ud(K_SEND, slot));
                    // Délégué : la pile TCP de l'envoi tourne sur un worker io-wq,
                    // pas pendant notre `enter`.
                    if self.send_async { sqe.flags(sqf::ASYNC) } else { sqe }
                }
                Op::CancelRecv { slot } => Sqe::cancel(ud(K_RECV, slot)).flags(sqf::SKIP_SUCCESS).user_data(ud(K_CANCEL, slot)),
                Op::CancelSend { slot } => Sqe::cancel(ud(K_SEND, slot)).flags(sqf::SKIP_SUCCESS).user_data(ud(K_CANCEL, slot)),
                Op::Close { slot } => Sqe::close(Target::Fixed(slot)).flags(sqf::SKIP_SUCCESS).user_data(ud(K_CLOSE, slot)),
                // Numéro du BGSAVE dans le champ slot du user_data.
                Op::FileWrite { fd, off, save } => {
                    // Le tampon reste intact jusqu'au CQE (invariant du BGSAVE).
                    let buf = self.core.pending_file_write(save);
                    let sqe = Sqe::write(Target::Fd(fd), buf.as_ptr(), buf.len() as u32, off).user_data(ud(K_FILE, save));
                    // Sinon la copie dans le cache de pages se fait dans notre `enter`.
                    if self.file_async { sqe.flags(sqf::ASYNC) } else { sqe }
                }
                Op::FileSync { fd, save } => Sqe::fsync(Target::Fd(fd), ring::FSYNC_DATASYNC).user_data(ud(K_FILE, save)),
                // Second canal de fichiers : le segment de l'AOF (DURABILITY_DESIGN.md §7.3).
                // Génération de l'écrivain dans le champ slot du user_data.
                Op::JournalWrite { fd, off, writer, link } => {
                    // Le tampon reste intact jusqu'au CQE (invariant de l'écrivain).
                    let buf = self.core.pending_journal_write(writer);
                    let sqe = Sqe::write(Target::Fd(fd), buf.as_ptr(), buf.len() as u32, off).user_data(ud(K_JWRITE, writer));
                    let sqe = if self.file_async { sqe.flags(sqf::ASYNC) } else { sqe };
                    if link {
                        // Le fsync lié suit dans la même soumission : une chaîne
                        // coupée par une SQ pleine laisserait le fsync partir seul.
                        self.reserve_sqes(2)?;
                        sqe.flags(sqf::IO_LINK)
                    } else {
                        sqe
                    }
                }
                Op::JournalSync { fd, writer } => {
                    let sqe = Sqe::fsync(Target::Fd(fd), ring::FSYNC_DATASYNC).user_data(ud(K_JSYNC, writer));
                    if self.file_async { sqe.flags(sqf::ASYNC) } else { sqe }
                }
                Op::ProvideBuf { bid } => {
                    self.bufring.push(&mut self.core, bid);
                    provided = true;
                    continue;
                }
                // Lien de réplication (DURABILITY_DESIGN.md §10) : socket créé ici,
                // installé dans le slot comme un accept, puis `connect` asynchrone.
                Op::Connect { slot, addr } => {
                    let sa = Box::new(SocketAddrAny::from(addr));
                    let fam = sa.address_family();
                    let fd = match rustix::net::socket_with(fam, rustix::net::SocketType::STREAM, rustix::net::SocketFlags::CLOEXEC, None) {
                        Ok(fd) => fd.into_raw_fd(),
                        Err(e) => {
                            self.core.complete(Completion::Connect { slot, res: -e.raw_os_error() });
                            continue;
                        }
                    };
                    tune_socket(fd);
                    let res = self.ring.register_files_update(slot, &[fd]);
                    close(fd);
                    if let Err(e) = res {
                        self.core.complete(Completion::Connect { slot, res: -e.raw_os_error() });
                        continue;
                    }
                    let sqe = Sqe::connect(Target::Fixed(slot), sa.as_ptr().cast(), sa.addr_len()).user_data(ud(K_CONNECT, slot));
                    self.connects.insert(slot, sa);
                    sqe
                }
                // Suites de fichiers (DURABILITY_DESIGN.md §8.6) : toujours sur un
                // worker io-wq (`IOSQE_ASYNC`), jamais dans notre `enter`.
                Op::Fs { id } => {
                    let Some(step) = self.core.fs_step(id) else { continue };
                    let cwd = rustix::fs::CWD.as_raw_fd();
                    let sqe = match step {
                        StepRef::Open { path, flags, mode } => Sqe::openat(cwd, path.as_ptr(), flags.bits(), mode.bits()),
                        StepRef::Fallocate { fd, off, len } => Sqe::fallocate(Target::Fd(fd), off, len),
                        StepRef::Fsync { fd, data } => Sqe::fsync(Target::Fd(fd), if data { ring::FSYNC_DATASYNC } else { 0 }),
                        StepRef::Write { fd, buf, off } => {
                            let len = buf.len().min(u32::MAX as usize) as u32;
                            Sqe::write(Target::Fd(fd), buf.as_ptr(), len, off)
                        }
                        StepRef::Ftruncate { fd, len } => Sqe::ftruncate(Target::Fd(fd), len),
                        StepRef::Rename { from, to } => Sqe::renameat(cwd, from.as_ptr(), cwd, to.as_ptr()),
                        StepRef::Unlink { path } => Sqe::unlinkat(cwd, path.as_ptr()),
                        StepRef::Close { fd } => Sqe::close(Target::Fd(fd)),
                    };
                    sqe.flags(sqf::ASYNC).user_data(ud(K_FS, id))
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
    fn recv_sqe(&self, slot: Slot) -> Sqe {
        if self.recv_async {
            Sqe::recv_select(Target::Fixed(slot), BGID).flags(sqf::ASYNC).user_data(ud(K_RECV, slot))
        } else {
            Sqe::recv_multi(Target::Fixed(slot), BGID).user_data(ud(K_RECV, slot))
        }
    }

    fn reserve_sqes(&mut self, n: usize) -> io::Result<()> {
        loop {
            let free = self.ring.sq_capacity() - self.ring.sq_len();
            if free >= n {
                return Ok(());
            }
            self.ring.submit()?;
        }
    }

    fn push_sqe(&mut self, sqe: &Sqe) -> io::Result<()> {
        // SAFETY: les tampons visés (envoi, fds) vivent jusqu'au CQE.
        while unsafe { self.ring.push(sqe) }.is_err() {
            // SQ pleine en plein tour : on soumet et on continue (§16).
            self.ring.submit()?;
        }
        Ok(())
    }

    /// Récolte les CQE et les passe au cœur ; rend leur nombre.
    fn harvest(&mut self) -> u32 {
        let mut any = 0;
        // Recopiés d'abord : `complete` peut vouloir soumettre (rien ici, mais
        // le cœur ne doit pas dépendre de l'ordre des emprunts).
        let mut cqes = std::mem::take(&mut self.cqes);
        self.ring.harvest(&mut cqes);
        for cqe in cqes.drain(..) {
            any += 1;
            let (kind, slot) = (cqe.user_data >> 32, cqe.user_data as u32);
            let res = cqe.res;
            let c = match kind {
                K_ACCEPT => Completion::Accept { fd: res, more: cqe.more() },
                K_RECV => Completion::Recv { slot, res, bid: cqe.buffer(), buf_more: cqe.buffer_more(), more: cqe.more() },
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
                    if res < 0 && res != -Errno::NOENT.raw_os_error() && res != -Errno::ALREADY.raw_os_error() {
                        eprintln!("[uring] kind {kind} slot {slot} : erreur {res}");
                    }
                    continue;
                }
                _ => unreachable!("user_data inconnu {:#x}", cqe.user_data),
            };
            self.core.complete(c);
        }
        self.cqes = cqes;
        any
    }

    /// Arrêt : les suites de fichiers en cours (rotation de l'AOF, renommage
    /// d'un dump) vont à leur terme avant l'écriture finale du journal.
    fn finish_fs(&mut self) -> io::Result<()> {
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
            let now = Instant::now();
            self.core.set_now(now);
            let cqes = self.harvest();
            if let MinWait::Auto(auto) = &mut self.min_wait {
                auto.tick(now, cqes, self.core.open_conns());
            }
            if let Some(save) = self.stop.take() {
                eprintln!("[shutdown] signal reçu : arrêt (second signal : sortie immédiate)");
                exit_on_second_signal();
                self.finish_fs()?;
                shutdown(&mut self.core, save);
            }
            // SHUTDOWN : comme un signal (réglage par défaut identique) ; échec sans
            // FORCE : erreur au client, le serveur continue.
            if let Some((save, force)) = self.core.take_shutdown() {
                eprintln!("[shutdown] SHUTDOWN reçu : arrêt");
                self.finish_fs()?;
                let save = save.unwrap_or(self.signals.as_ref().is_some_and(|s| s.save));
                let e = try_shutdown(&mut self.core, save);
                eprintln!("[shutdown] {e}");
                if force {
                    std::process::exit(1)
                }
                self.core.shutdown_failed(&e);
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
                if !self.ring.sq_is_empty() {
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
                // (GETEVENTS ajouté quand des complétions attendent, `Ring::submit`).
                self.ring.submit()
            };
            if let Err(e) = res
                && ![Errno::TIME, Errno::INTR, Errno::BUSY].contains(&e)
            {
                return Err(e.into());
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
        while !self.work_waiting() && self.core.quiet() && self.core.idle_step() && !end.passed() {}
    }

    /// Le noyau a du travail pour nous. Avec `DEFER_TASKRUN` : `IORING_SQ_TASKRUN`
    /// (les complétions n'existent qu'à notre `enter`) ; en SQPOLL, le thread
    /// noyau les écrit lui-même : la CQ n'est pas vide.
    fn work_waiting(&mut self) -> bool {
        if self.sqpoll { !self.ring.cq_is_empty() } else { self.ring.taskrun() }
    }

    /// Soumet et attend au moins une complétion, au plus jusqu'à la prochaine
    /// échéance de timer.
    fn wait(&mut self) -> ring::Result<usize> {
        if self.sqpoll && !self.spin.is_zero() {
            let budget = self.core.next_deadline().map_or(self.spin, |t| t.saturating_duration_since(Instant::now()).min(self.spin));
            let end = tsc::Deadline::after(budget);
            while !end.passed() {
                if !self.ring.cq_is_empty() {
                    return Ok(0);
                }
                std::hint::spin_loop();
            }
        }
        let timeout = self
            .core
            .next_deadline()
            .map_or(Duration::from_secs(1), |t| t.saturating_duration_since(Instant::now()));
        let (want, usec) = self.min_wait.on_wait();
        let mut args = WaitArgs::new(timeout, usec);
        let mut flags = ring::enter::GETEVENTS | ring::enter::EXT_ARG;
        if self.no_iowait {
            flags |= ring::enter::NO_IOWAIT;
        }
        // Le thread noyau s'est endormi (`sq_thread_idle` sans SQE) : le réveiller
        // pour qu'il prenne ce qu'on vient d'écrire (barrière dans `need_wakeup`).
        if self.sqpoll && self.ring.need_wakeup() {
            flags |= ring::enter::SQ_WAKEUP;
        }
        let to_submit = self.ring.sq_len() as u32;
        self.ring.enter(to_submit, want, flags, &mut args)
    }
}

/// Durée maximale d'une passe de travail de fond sans repasser par le tour
/// (horloges, timers).
const IDLE_WORK_MAX: Duration = Duration::from_millis(1);

fn close(fd: RawFd) {
    // SAFETY: fd possédé, fermé une fois.
    drop(unsafe { OwnedFd::from_raw_fd(fd) });
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
    // SAFETY: fd ouvert par l'appelant pendant l'appel.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let opt = |name: &str, r: rustix::io::Result<()>| {
        if let Err(e) = r {
            eprintln!("[uring] setsockopt({name}) : {e}");
        }
    };
    // Sans Nagle : le p99.99 en pipeline passait de 43 ms à ~8 ms.
    opt("TCP_NODELAY", sockopt::set_tcp_nodelay(fd, true));
    // Client qui ne lit plus et ne ferme pas : le noyau coupe au bout de 60 s
    // (sinon un send resterait en vol à jamais).
    opt("TCP_USER_TIMEOUT", sockopt::set_tcp_user_timeout(fd, 60_000));
    opt("SO_KEEPALIVE", sockopt::set_socket_keepalive(fd, true));
    opt("TCP_KEEPIDLE", sockopt::set_tcp_keepidle(fd, Duration::from_secs(300)));
}

fn pin_cpu() {
    let Ok(v) = std::env::var("RADIXOX_CPU_PIN") else { return };
    match v.parse::<usize>() {
        Ok(cpu) if cpu < rustix::thread::CpuSet::MAX_CPU => {
            let mut set = rustix::thread::CpuSet::new();
            set.set(cpu);
            match rustix::thread::sched_setaffinity(None, &set) {
                Ok(()) => println!("CPU pinned: {cpu}"),
                Err(e) => eprintln!("RADIXOX_CPU_PIN={cpu} : {e}"),
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
    use rustix::process::{Resource, getrlimit, setrlimit};
    {
        // `None` : illimité.
        let mut lim = getrlimit(Resource::Nofile);
        let cur = |l: &rustix::process::Rlimit| l.current.unwrap_or(u64::MAX);
        let need = wanted as u64 + SPARE;
        if cur(&lim) < need {
            let want = rustix::process::Rlimit { current: Some(need.min(lim.maximum.unwrap_or(u64::MAX))), maximum: lim.maximum };
            if setrlimit(Resource::Nofile, want).is_ok() {
                lim = want;
            } else {
                lim = getrlimit(Resource::Nofile);
            }
        }
        let max = cur(&lim).saturating_sub(SPARE).min(wanted as u64) as u32;
        if max < wanted {
            eprintln!(
                "WARNING: RLIMIT_NOFILE = {} : connexions limitées à {max} au lieu de {wanted} \
                 (ulimit -n ou LimitNOFILE= pour en avoir plus)",
                cur(&lim)
            );
        }
        max
    }
}

/// Attente groupée : au repos, attendre jusqu'à `want` complétions pendant au plus
/// `usec` µs, pour regrouper le travail en lots au lieu de se réveiller pour une seule
/// (RUNTIME_ARCH.md §12.5 : −20 % de cycles user, −12 % noyau par commande à 10 µs / 16).
enum MinWait {
    Off,
    Fixed { want: u32, usec: u32 },
    Auto(AutoWait),
}

impl MinWait {
    /// `(want, µs)` de l'`enter` qui attend ; compte le réveil en mode automatique.
    fn on_wait(&mut self) -> (u32, u32) {
        match self {
            MinWait::Off => (1, 0),
            MinWait::Fixed { want, usec } => (*want, *usec),
            MinWait::Auto(a) => {
                a.waits += 1;
                if a.on { (a.want, a.usec) } else { (1, 0) }
            }
        }
    }
}

/// Fenêtre de décision du mode automatique.
const AUTO_WINDOW: Duration = Duration::from_millis(100);
/// Activation : au moins ce nombre de complétions par fenêtre (100 K/s), et moins de
/// `want` complétions par réveil (au-delà, le noyau rend déjà la main avec des lots
/// pleins : sans envois délégués, ~80 par réveil à 84 connexions p1).
const AUTO_ON_CQES: u32 = 10_000;
/// Désactivation sous ce nombre de complétions par fenêtre (50 K/s) : hystérésis.
const AUTO_OFF_CQES: u32 = 5_000;

/// Mode automatique : actif seulement quand beaucoup de connexions (au moins `2 × want`)
/// font réveiller la boucle souvent pour peu de complétions. Un client seul, ou une charge
/// faible, garde le réveil à la première complétion : l'attente coûterait jusqu'à `usec`
/// de latence par requête. Saturée, la boucle n'attend plus et les lots grossissent seuls.
struct AutoWait {
    want: u32,
    usec: u32,
    on: bool,
    since: Instant,
    waits: u32,
    cqes: u32,
}

impl AutoWait {
    fn tick(&mut self, now: Instant, cqes: u32, conns: usize) {
        self.cqes += cqes;
        if now.saturating_duration_since(self.since) < AUTO_WINDOW {
            return;
        }
        let many = conns >= 2 * self.want as usize;
        let on = if self.on {
            conns >= self.want as usize && self.cqes >= AUTO_OFF_CQES
        } else {
            many && self.cqes >= AUTO_ON_CQES && self.cqes < self.want * self.waits
        };
        if on != self.on {
            eprintln!(
                "[uring] attente groupée {} ({conns} connexions, {} complétions / {} réveils en 100 ms)",
                if on { "activée" } else { "désactivée" },
                self.cqes,
                self.waits
            );
            self.on = on;
        }
        (self.since, self.waits, self.cqes) = (now, 0, 0);
    }
}

/// `RADIXOX_MIN_WAIT_US` : absent = automatique (10 µs), `0` = jamais, une valeur =
/// toujours, ce délai. `RADIXOX_WAIT_NR` : complétions attendues (défaut 16).
fn min_wait(ring: &Ring) -> MinWait {
    let usec: Option<u32> = std::env::var("RADIXOX_MIN_WAIT_US").ok().and_then(|v| v.parse().ok());
    if usec == Some(0) {
        return MinWait::Off;
    }
    if !ring.feature_min_timeout() {
        if usec.is_some() {
            eprintln!("[uring] RADIXOX_MIN_WAIT_US ignoré : noyau sans IORING_FEAT_MIN_TIMEOUT");
        }
        return MinWait::Off;
    }
    let want = std::env::var("RADIXOX_WAIT_NR").ok().and_then(|v| v.parse().ok()).unwrap_or(16u32).max(1);
    match usec {
        Some(usec) => MinWait::Fixed { want, usec },
        None => MinWait::Auto(AutoWait { want, usec: 10, on: false, since: Instant::now(), waits: 0, cqes: 0 }),
    }
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
fn setup_iowq(ring: &Ring) -> io::Result<(bool, bool, bool)> {
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
    ring.register_iowq_max_workers(&mut max)?;
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
        // Masque de 1024 CPU, comme un `cpu_set_t`.
        let mut set = [0u64; 16];
        for c in cpus.iter().flat_map(|l| l.split(',')).filter_map(|c| c.trim().parse::<usize>().ok()).filter(|&c| c < 1024) {
            set[c / 64] |= 1 << (c % 64);
        }
        ring.register_iowq_aff(&set)?;
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

/// `RADIXOX_SQPOLL_CPU=<cpu>` : SQPOLL épinglé sur ce CPU (`RADIXOX_SQPOLL_IDLE_MS`,
/// défaut 1000, avant qu'il ne s'endorme ; la boucle, elle, surveille la CQ
/// `RADIXOX_SQPOLL_SPIN_US`, défaut 1000, avant de s'endormir). Le thread noyau tourne en attente active
/// et prend la soumission et la réception (copie comprise) ; à combiner avec
/// `RADIXOX_SEND_ASYNC` pour que les envois partent sur io-wq.
fn sqpoll_from_env() -> Option<(u32, u32)> {
    let cpu = std::env::var("RADIXOX_SQPOLL_CPU").ok()?.trim().parse().ok()?;
    let idle = std::env::var("RADIXOX_SQPOLL_IDLE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    Some((cpu, idle))
}

fn build_ring(sqpoll: Option<(u32, u32)>) -> io::Result<Ring> {
    let mut b = ring::Builder::new();
    b.single_issuer().cqsize(CQ_ENTRIES);
    match sqpoll {
        // Incompatible avec `DEFER_TASKRUN` et `TASKRUN_FLAG` : le travail des
        // complétions tourne dans le thread SQPOLL.
        Some((cpu, idle)) => b.sqpoll(idle).sqpoll_cpu(cpu),
        None => b.defer_taskrun().taskrun_flag(),
    };
    b.build(SQ_ENTRIES)
        .map_err(|e| {
            io::Error::new(
                io::Error::from(e).kind(),
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
                    dir,
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
                println!("[aof] {} créé (base.1.rxdb, journal.1.rxj)", dir);
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

    let sqpoll = sqpoll_from_env();
    let ring = build_ring(sqpoll)?;
    if let Some((cpu, idle)) = sqpoll {
        println!("SQPOLL : thread noyau sur le CPU {cpu}, endormi après {idle} ms sans travail");
    }
    let mut cfg = Config::default();
    if let Some(w) = std::env::var("RADIXOX_PREFETCH").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.prefetch = w.max(1);
    }
    if let Some(k) = std::env::var("RADIXOX_AMAC").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.amac = k;
    }
    if let Some(p) = std::env::var("RADIXOX_AMAC_PF").ok().and_then(|v| v.parse::<u8>().ok()) {
        cfg.amac_pf = p;
    }
    // Anneau de réception (RUNTIME_ARCH.md §5.1) : nombre et taille des tampons.
    // Recv simples sur io-wq : chaque recv en vol réserve un tampon entier, il en
    // faut au moins un par connexion active ; plus nombreux et plus petits.
    if env_yes("RADIXOX_RECV_ASYNC", false) {
        cfg.recv_oneshot = true;
        cfg.bufs = 1024;
        cfg.buf_size = 4 << 10;
    }
    if let Some(n) = std::env::var("RADIXOX_RING_BUFS").ok().and_then(|v| v.parse::<u16>().ok()) {
        cfg.bufs = n.max(1);
    }
    if let Some(kb) = std::env::var("RADIXOX_RING_BUF_KB").ok().and_then(|v| v.parse::<u32>().ok()) {
        cfg.buf_size = kb.max(4) << 10;
    }
    // Sortie bornée (RUNTIME_ARCH.md §6.2 bis) : par client normal, et pour tous.
    if let Some(mb) = std::env::var("RADIXOX_CLIENT_OUTPUT_LIMIT_MB").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.client_output_limit = if mb == 0 { usize::MAX } else { mb << 20 };
    }
    if let Some(mb) = std::env::var("RADIXOX_INPUT_BUDGET_MB").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.input_budget = if mb == 0 { usize::MAX } else { mb << 20 };
    }
    if let Some(mb) = std::env::var("RADIXOX_OUTPUT_BUDGET_MB").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.output_budget = if mb == 0 { usize::MAX } else { mb << 20 };
    }
    // maxmemory (MEMORY_ARCH.md §10.5) : total du processus, découpé en budgets.
    {
        use radixox::exec::maxmem::{Policy, parse_size};
        let size = |k: &str| -> io::Result<Option<u64>> {
            match std::env::var(k) {
                Ok(v) if v.trim().is_empty() => Ok(None),
                Ok(v) => parse_size(&v).map(Some).ok_or_else(|| io::Error::other(format!("{k} : taille invalide '{v}'"))),
                Err(_) => Ok(None),
            }
        };
        if let Ok(p) = std::env::var("RADIXOX_MAXMEMORY_POLICY") {
            cfg.maxmemory_policy = Policy::parse(&p).map_err(|e| io::Error::other(format!("RADIXOX_MAXMEMORY_POLICY : {e}")))?;
        }
        cfg.save_reserve = size("RADIXOX_SAVE_RESERVE")?.unwrap_or(0);
        let max = size("RADIXOX_MAXMEMORY")?.unwrap_or(0);
        let (input_set, output_set) =
            (std::env::var("RADIXOX_INPUT_BUDGET_MB").is_ok(), std::env::var("RADIXOX_OUTPUT_BUDGET_MB").is_ok());
        // même sans limite au démarrage : un `CONFIG SET maxmemory` plus tard les dérive
        cfg.io_budgets_auto = !input_set && !output_set;
        if max > 0 {
            if !input_set {
                cfg.input_budget = (max / 10) as usize;
            }
            if !output_set {
                cfg.output_budget = (max / 10) as usize;
            }
            let parts = cfg.input_budget as u64 + cfg.output_budget as u64 + cfg.save_reserve;
            if parts >= max {
                return Err(io::Error::other(format!(
                    "RADIXOX_MAXMEMORY ({max} o) : budgets d'entrée et de sortie et réserve de save ({parts} o) ne laissent rien aux données"
                )));
            }
        }
        cfg.maxmemory = max;
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
    ring.register_files_sparse(cfg.max_conn)?;
    let no_iowait = ring.feature_no_iowait();
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
    if let Some(mb) = std::env::var("RADIXOX_REPL_ONLINE_LAG_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
        cfg.repl_online_lag = mb.max(1) << 20;
    }
    let aof_on = aof.is_some();
    let mut core = Core::new(cfg, art);
    core.check_maxmemory().map_err(|e| io::Error::other(format!("maxmemory : {e}")))?;
    if let Some((stream, writer)) = aof {
        core.set_aof(stream, writer);
    }
    replicate_from_env(&mut core)?;
    let bufring = BufRing::new(&ring, &mut core)?;
    let listener = TcpListener::bind(&addr)?;
    println!("RadixOx RESP Server (io_uring loop) listening on {addr}");

    // Avec l'AOF, le flux suffit à l'arrêt : pas de save par défaut.
    let signals = Signals::install(core.autosave_on() && !aof_on, aof_on)?;
    let mut server = Server { ring, bufring, cqes: Vec::new(), core, listener, ops: Vec::new(), no_iowait, send_async, recv_async, file_async, min_wait, sqpoll: sqpoll.is_some(),
        spin: Duration::from_micros(std::env::var("RADIXOX_SQPOLL_SPIN_US").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)),
        signals,
        connects: Default::default(),
        stop: None,
    };
    server.run()
}
