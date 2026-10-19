//! `radixox-uring` : serveur RESP sur la boucle io_uring maison, sans runtime
//! (RUNTIME_ARCH.md). La logique vit dans le cœur sans I/O
//! (`radixox::uring::Core`) ; ce fichier ne fait que la traduction :
//! `Op` → SQE, CQE → `Completion`, plus l'attente.
//!
//! Étape 2 (§13) : accept, recv multishot sur anneau incrémental, send, toutes
//! les commandes sync. Pas encore : timers, signalfd, parquage, BGSAVE, Pub/Sub.

#[cfg(not(target_os = "linux"))]
compile_error!("RadixOx requires Linux to run (io_uring).");

use std::io;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use io_uring::types::{BufRingEntry, Fd, Fixed, IOU_PBUF_RING_INC, SubmitArgs, Timespec};
use io_uring::{EnterFlags, IoUring, cqueue, opcode, squeue};
use io_uring_07 as io_uring;
use oxidart::OxidArt;
use radixox::uring::{Bid, Completion, Config, Core, Op, Slot};

/// Groupe de tampons du recv multishot.
const BGID: u16 = 0;
const SQ_ENTRIES: u32 = 4096;
const CQ_ENTRIES: u32 = 16384;
/// Attente maximale dans `enter` quand il n'y a rien à faire : l'horloge et
/// l'évicteur avancent quand même (timers : étape 3).
const IDLE_WAIT: Duration = Duration::from_millis(100);
const EVICT_EVERY: Duration = Duration::from_secs(1);

// ── user_data : kind << 32 | slot (RUNTIME_ARCH.md §3) ──────────────────────

const K_ACCEPT: u64 = 1;
const K_RECV: u64 = 2;
const K_SEND: u64 = 3;
const K_CANCEL: u64 = 4;
const K_CLOSE: u64 = 5;

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
                    opcode::RecvMulti::new(Fixed(slot), BGID).build().user_data(ud(K_RECV, slot))
                }
                Op::Reject { fd } => {
                    let msg = b"-ERR max number of clients reached\r\n";
                    // SAFETY: fd valide, tampon valide.
                    unsafe { libc::write(fd, msg.as_ptr().cast(), msg.len()) };
                    close(fd);
                    continue;
                }
                Op::RecvMulti { slot } => opcode::RecvMulti::new(Fixed(slot), BGID).build().user_data(ud(K_RECV, slot)),
                Op::Send { slot } => {
                    // Le tampon reste intact jusqu'au CQE (invariant du cœur).
                    let buf = self.core.pending_send(slot);
                    opcode::Send::new(Fixed(slot), buf.as_ptr(), buf.len() as u32)
                        .flags(libc::MSG_WAITALL | libc::MSG_NOSIGNAL)
                        .build()
                        .user_data(ud(K_SEND, slot))
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
                Op::ProvideBuf { bid } => {
                    self.bufring.push(&mut self.core, bid);
                    provided = true;
                    continue;
                }
            };
            self.push_sqe(&sqe)?;
        }
        if provided {
            self.bufring.publish();
        }
        Ok(())
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

    fn run(&mut self) -> io::Result<()> {
        let mut last_evict = Instant::now();
        loop {
            self.core.art.tick();
            self.harvest();
            self.core.run_batch();
            self.core.run_jobs();
            self.core.flush();
            if last_evict.elapsed() >= EVICT_EVERY {
                self.core.art.evict_expired();
                last_evict = Instant::now();
            }
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

    /// Soumet et attend au moins une complétion (ou le timeout).
    fn wait(&mut self) -> io::Result<usize> {
        let ts = Timespec::from(IDLE_WAIT);
        let args = SubmitArgs::new().timespec(&ts);
        let mut flags = EnterFlags::GETEVENTS | EnterFlags::EXT_ARG;
        if self.no_iowait {
            flags |= EnterFlags::NO_IOWAIT;
        }
        let to_submit = self.ring.submission().len() as u32;
        // SAFETY: `args` vit pendant l'appel.
        unsafe { self.ring.submitter().enter(to_submit, 1, flags.bits(), Some(&args)) }
    }
}

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
    radixox::exec::persist::load_at_startup(&mut art)?;

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
    ring.submitter().register_files_sparse(cfg.max_conn)?;
    let no_iowait = ring.params().is_feature_no_iowait();
    let mut core = Core::new(cfg, art);
    let bufring = BufRing::new(&ring, &mut core)?;
    let listener = TcpListener::bind(&addr)?;
    println!("RadixOx RESP Server (io_uring loop) listening on {addr}");

    let mut server = Server { ring, bufring, core, listener, ops: Vec::new(), no_iowait };
    server.run()
}
