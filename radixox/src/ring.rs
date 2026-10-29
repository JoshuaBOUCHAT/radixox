//! Anneau io_uring à nous, sans `std` (NOSTD_PLAN.md, étape 2) : mise en place,
//! `mmap` des anneaux, soumission, récolte et `register` par `rustix::io_uring`.
//! Seulement ce que la boucle utilise ; la crate `io-uring` (0.7) sert de référence
//! de comportement (même logique de `submit`, même encodage des opcodes).
//!
//! Les SQE / CQE sont des `repr(C)` à champs simples (64 o / 16 o, la disposition du
//! noyau) plutôt que les unions de rustix : chaque constructeur écrit les champs que
//! lit l'opcode, comme `include/uapi/linux/io_uring.h` les nomme.

#[allow(unused_imports)]
use crate::prelude::*;

use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering, fence};
use core::time::Duration;

use rustix::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use rustix::io::Errno;
use rustix::io_uring::{
    IoringEnterFlags, IoringFeatureFlags, IoringOp, IoringRegisterOp, IoringSetupFlags, IoringSqFlags,
    io_uring_getevents_arg, io_uring_params, io_uring_ptr,
};
use rustix::mm::{MapFlags, ProtFlags};

pub type Result<T> = core::result::Result<T, Errno>;

// Constantes absentes de rustix 1.1 (valeurs de `io_uring.h`, noyau 6.12).
/// `IORING_ENTER_NO_IOWAIT`.
pub const ENTER_NO_IOWAIT: u32 = 1 << 7;
/// `IORING_FEAT_MIN_TIMEOUT`.
const FEAT_MIN_TIMEOUT: u32 = 1 << 15;
/// `IORING_FEAT_NO_IOWAIT`.
const FEAT_NO_IOWAIT: u32 = 1 << 17;
/// `IOU_PBUF_RING_INC` : anneau de tampons incrémental.
pub const PBUF_RING_INC: u16 = 2;
const CQE_F_BUFFER: u32 = 1 << 0;
const CQE_F_MORE: u32 = 1 << 1;
const CQE_F_BUF_MORE: u32 = 1 << 4;
const CQE_BUFFER_SHIFT: u32 = 16;
const RECV_MULTISHOT: u16 = 1 << 1;
const ACCEPT_MULTISHOT: u16 = 1 << 0;
/// `IORING_FSYNC_DATASYNC`.
pub const FSYNC_DATASYNC: u32 = 1;

/// Drapeaux de SQE (`IOSQE_*`).
pub mod flags {
    pub const FIXED_FILE: u8 = 1 << 0;
    pub const IO_LINK: u8 = 1 << 2;
    pub const ASYNC: u8 = 1 << 4;
    pub const BUFFER_SELECT: u8 = 1 << 5;
    pub const SKIP_SUCCESS: u8 = 1 << 6;
}

/// Drapeaux d'`io_uring_enter` (`IORING_ENTER_*`).
pub mod enter {
    pub const GETEVENTS: u32 = 1 << 0;
    pub const SQ_WAKEUP: u32 = 1 << 1;
    pub const EXT_ARG: u32 = 1 << 3;
    pub use super::ENTER_NO_IOWAIT as NO_IOWAIT;
}

// ── Entrées ─────────────────────────────────────────────────────────────────

/// Entrée de soumission, disposition de `struct io_uring_sqe`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    /// `off` / `addr2`.
    off: u64,
    addr: u64,
    len: u32,
    /// `rw_flags`, `msg_flags`, `fsync_flags`, `open_flags`…
    op_flags: u32,
    user_data: u64,
    /// `buf_index` / `buf_group`.
    buf_group: u16,
    personality: u16,
    /// `file_index` / `splice_fd_in` / `addr_len`.
    file_index: u32,
    addr3: u64,
    pad: u64,
}

const _: () = assert!(size_of::<Sqe>() == 64);

/// Cible d'une opération : descripteur, ou slot de la table des fichiers enregistrés.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    Fd(RawFd),
    Fixed(u32),
}

impl Sqe {
    fn new(op: IoringOp, fd: Target) -> Self {
        let mut s = Self { opcode: op as u8, ..Self::default() };
        match fd {
            Target::Fd(fd) => s.fd = fd,
            Target::Fixed(slot) => {
                s.fd = slot as i32;
                s.flags = flags::FIXED_FILE;
            }
        }
        s
    }

    pub fn read(fd: Target, buf: *mut u8, len: u32) -> Self {
        Self { addr: buf as u64, len, ..Self::new(IoringOp::Read, fd) }
    }

    /// `pwrite` à `off` (0 pour un descripteur non positionnable).
    pub fn write(fd: Target, buf: *const u8, len: u32, off: u64) -> Self {
        Self { addr: buf as u64, len, off, ..Self::new(IoringOp::Write, fd) }
    }

    pub fn send(fd: Target, buf: *const u8, len: u32, msg_flags: u32) -> Self {
        Self { addr: buf as u64, len, op_flags: msg_flags, ..Self::new(IoringOp::Send, fd) }
    }

    /// recv simple, tampon choisi dans le groupe `bgid`.
    pub fn recv_select(fd: Target, bgid: u16) -> Self {
        let mut s = Self { buf_group: bgid, ..Self::new(IoringOp::Recv, fd) };
        s.flags |= flags::BUFFER_SELECT;
        s
    }

    /// recv multishot, tampons du groupe `bgid`.
    pub fn recv_multi(fd: Target, bgid: u16) -> Self {
        let mut s = Self::recv_select(fd, bgid);
        s.ioprio = RECV_MULTISHOT;
        s
    }

    pub fn accept_multi(fd: Target) -> Self {
        Self { ioprio: ACCEPT_MULTISHOT, ..Self::new(IoringOp::Accept, fd) }
    }

    /// Annule l'opération de `user_data`.
    pub fn cancel(user_data: u64) -> Self {
        Self { addr: user_data, ..Self::new(IoringOp::AsyncCancel, Target::Fd(-1)) }
    }

    pub fn close(fd: Target) -> Self {
        match fd {
            Target::Fd(fd) => Self::new(IoringOp::Close, Target::Fd(fd)),
            Target::Fixed(slot) => Self { file_index: slot + 1, ..Self::new(IoringOp::Close, Target::Fd(0)) },
        }
    }

    /// `fsync_flags` : 0 ou [`FSYNC_DATASYNC`].
    pub fn fsync(fd: Target, fsync_flags: u32) -> Self {
        Self { op_flags: fsync_flags, ..Self::new(IoringOp::Fsync, fd) }
    }

    pub fn connect(fd: Target, addr: *const u8, addr_len: u32) -> Self {
        Self { addr: addr as u64, off: addr_len as u64, ..Self::new(IoringOp::Connect, fd) }
    }

    pub fn openat(dirfd: RawFd, path: *const core::ffi::c_char, open_flags: u32, mode: u32) -> Self {
        Self { addr: path as u64, len: mode, op_flags: open_flags, ..Self::new(IoringOp::Openat, Target::Fd(dirfd)) }
    }

    /// Mode 0 : réserve `[off, off + len)`.
    pub fn fallocate(fd: Target, off: u64, len: u64) -> Self {
        Self { addr: len, off, ..Self::new(IoringOp::Fallocate, fd) }
    }

    pub fn ftruncate(fd: Target, len: u64) -> Self {
        Self { off: len, ..Self::new(IoringOp::Ftruncate, fd) }
    }

    pub fn renameat(olddir: RawFd, from: *const core::ffi::c_char, newdir: RawFd, to: *const core::ffi::c_char) -> Self {
        Self { addr: from as u64, len: newdir as u32, off: to as u64, ..Self::new(IoringOp::Renameat, Target::Fd(olddir)) }
    }

    pub fn unlinkat(dirfd: RawFd, path: *const core::ffi::c_char) -> Self {
        Self { addr: path as u64, ..Self::new(IoringOp::Unlinkat, Target::Fd(dirfd)) }
    }

    /// Ajoute des drapeaux `IOSQE_*` ([`flags`]).
    pub fn flags(mut self, f: u8) -> Self {
        self.flags |= f;
        self
    }

    pub fn user_data(mut self, ud: u64) -> Self {
        self.user_data = ud;
        self
    }
}

/// Entrée de complétion, disposition de `struct io_uring_cqe` (sans `CQE32`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Cqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

const _: () = assert!(size_of::<Cqe>() == 16);

impl Cqe {
    /// Multishot encore armé (`IORING_CQE_F_MORE`).
    pub fn more(&self) -> bool {
        self.flags & CQE_F_MORE != 0
    }

    /// Tampon choisi par le noyau (`IORING_CQE_F_BUFFER`).
    pub fn buffer(&self) -> Option<u16> {
        (self.flags & CQE_F_BUFFER != 0).then_some((self.flags >> CQE_BUFFER_SHIFT) as u16)
    }

    /// Anneau incrémental : le tampon n'est pas fini, la suite y sera écrite.
    pub fn buffer_more(&self) -> bool {
        self.flags & CQE_F_BUF_MORE != 0
    }
}

/// Entrée de l'anneau de tampons fournis (`struct io_uring_buf`). Le `resv` de la
/// première entrée est la queue de l'anneau, écrite par nous ([`buf_ring_tail`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BufRingEntry {
    pub addr: u64,
    pub len: u32,
    pub bid: u16,
    resv: u16,
}

const _: () = assert!(size_of::<BufRingEntry>() == 16);

/// Queue d'un anneau de tampons qui commence à `base`.
///
/// # Safety
/// `base` est l'anneau enregistré, vivant.
pub unsafe fn buf_ring_tail<'a>(base: *mut BufRingEntry) -> &'a AtomicU16 {
    // SAFETY: champ `resv` de la première entrée, aligné sur 2 ; le noyau le lit en concurrence.
    unsafe { &*(&raw mut (*base).resv as *const AtomicU16) }
}

/// Arguments étendus d'`enter` (`EXT_ARG`) : délai maximal et attente groupée.
pub struct WaitArgs {
    ts: Timespec,
    arg: io_uring_getevents_arg,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

impl WaitArgs {
    /// Attente d'au plus `timeout` ; `min_wait_usec` : `IORING_FEAT_MIN_TIMEOUT`.
    pub fn new(timeout: Duration, min_wait_usec: u32) -> Self {
        let ts = Timespec { tv_sec: timeout.as_secs() as i64, tv_nsec: timeout.subsec_nanos() as i64 };
        let mut arg = io_uring_getevents_arg::default();
        arg.min_wait_usec = min_wait_usec;
        Self { ts, arg }
    }
}

// ── Anneau ──────────────────────────────────────────────────────────────────

/// Plage partagée avec le noyau (`mmap` de l'anneau), rendue à la destruction.
struct Map {
    ptr: NonNull<u8>,
    len: usize,
}

impl Map {
    fn new(fd: &OwnedFd, offset: u64, len: usize) -> Result<Self> {
        // SAFETY: plage nouvelle, partagée avec le noyau, rendue dans `Drop`.
        let p = unsafe {
            rustix::mm::mmap(null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, MapFlags::SHARED | MapFlags::POPULATE, fd, offset)?
        };
        Ok(Self { ptr: NonNull::new(p.cast()).expect("mmap non nul"), len })
    }

    /// # Safety
    /// `off` est un décalage fourni par le noyau dans cette plage.
    unsafe fn at<T>(&self, off: u32) -> *mut T {
        unsafe { self.ptr.as_ptr().add(off as usize).cast() }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: plage à nous, plus aucune référence (l'anneau est détruit).
        let _ = unsafe { rustix::mm::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// Mise en place (`io_uring_setup`).
pub struct Builder {
    p: io_uring_params,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self { p: io_uring_params::default() }
    }

    pub fn single_issuer(&mut self) -> &mut Self {
        self.p.flags |= IoringSetupFlags::SINGLE_ISSUER;
        self
    }

    pub fn cqsize(&mut self, entries: u32) -> &mut Self {
        self.p.flags |= IoringSetupFlags::CQSIZE;
        self.p.cq_entries = entries;
        self
    }

    /// Thread noyau qui lit la SQ ; il s'endort après `idle_ms` sans soumission.
    pub fn sqpoll(&mut self, idle_ms: u32) -> &mut Self {
        self.p.flags |= IoringSetupFlags::SQPOLL;
        self.p.sq_thread_idle = idle_ms;
        self
    }

    pub fn sqpoll_cpu(&mut self, cpu: u32) -> &mut Self {
        self.p.flags |= IoringSetupFlags::SQ_AFF;
        self.p.sq_thread_cpu = cpu;
        self
    }

    pub fn defer_taskrun(&mut self) -> &mut Self {
        self.p.flags |= IoringSetupFlags::DEFER_TASKRUN;
        self
    }

    pub fn taskrun_flag(&mut self) -> &mut Self {
        self.p.flags |= IoringSetupFlags::TASKRUN_FLAG;
        self
    }

    pub fn build(&self, entries: u32) -> Result<Ring> {
        let mut p = self.p;
        // SAFETY: `p` valide pendant l'appel.
        let fd = unsafe { rustix::io_uring::io_uring_setup(entries, &mut p)? };
        Ring::map(fd, p)
    }
}

/// Un anneau : SQ, CQ et SQE partagées avec le noyau. Mono-thread : `&mut self`
/// pour tout ce qui touche à nos têtes / queues.
pub struct Ring {
    // Champs dans l'ordre de destruction : les plages avant le descripteur. Gardées
    // seulement pour être rendues (`Drop`).
    #[allow(dead_code)]
    sq_map: Map,
    #[allow(dead_code)]
    sqe_map: Map,
    #[allow(dead_code)]
    cq_map: Option<Map>,
    fd: OwnedFd,
    p: io_uring_params,

    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_flags: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    sqes: *mut Sqe,
    /// Queue locale de la SQ (seul écrivain), publiée à chaque `push`.
    tail: u32,

    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const Cqe,
    /// Tête locale de la CQ (seul lecteur), publiée après chaque récolte.
    head: u32,
}

impl Ring {
    fn map(fd: OwnedFd, p: io_uring_params) -> Result<Self> {
        let sq_len = p.sq_off.array as usize + p.sq_entries as usize * size_of::<u32>();
        let cq_len = p.cq_off.cqes as usize + p.cq_entries as usize * size_of::<Cqe>();
        let sqe_map = Map::new(&fd, rustix::io_uring::IORING_OFF_SQES, p.sq_entries as usize * size_of::<Sqe>())?;
        let (sq_map, cq_map) = if p.features.contains(IoringFeatureFlags::SINGLE_MMAP) {
            (Map::new(&fd, rustix::io_uring::IORING_OFF_SQ_RING, sq_len.max(cq_len))?, None)
        } else {
            let sq = Map::new(&fd, rustix::io_uring::IORING_OFF_SQ_RING, sq_len)?;
            (sq, Some(Map::new(&fd, rustix::io_uring::IORING_OFF_CQ_RING, cq_len)?))
        };
        let cq = cq_map.as_ref().unwrap_or(&sq_map);
        // SAFETY: décalages rendus par `io_uring_setup` dans les plages qu'il décrit.
        unsafe {
            let sq_entries = *sq_map.at::<u32>(p.sq_off.ring_entries);
            // Tableau d'indirection : identité (pas de `NO_SQARRAY`).
            let array = sq_map.at::<u32>(p.sq_off.array);
            for i in 0..sq_entries {
                array.add(i as usize).write_volatile(i);
            }
            let sq_tail = sq_map.at::<AtomicU32>(p.sq_off.tail);
            let cq_head = cq.at::<AtomicU32>(p.cq_off.head);
            Ok(Self {
                sq_head: sq_map.at(p.sq_off.head),
                sq_flags: sq_map.at(p.sq_off.flags),
                sq_mask: *sq_map.at::<u32>(p.sq_off.ring_mask),
                sq_entries,
                sqes: sqe_map.ptr.as_ptr().cast(),
                tail: (*sq_tail).load(Ordering::Relaxed),
                sq_tail,
                cq_tail: cq.at(p.cq_off.tail),
                cq_mask: *cq.at::<u32>(p.cq_off.ring_mask),
                cqes: cq.at(p.cq_off.cqes),
                head: (*cq_head).load(Ordering::Relaxed),
                cq_head,
                sq_map,
                sqe_map,
                cq_map,
                fd,
                p,
            })
        }
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Le noyau accepte l'attente groupée (`min_wait_usec`).
    pub fn feature_min_timeout(&self) -> bool {
        self.p.features.bits() & FEAT_MIN_TIMEOUT != 0
    }

    /// Le noyau accepte `IORING_ENTER_NO_IOWAIT`.
    pub fn feature_no_iowait(&self) -> bool {
        self.p.features.bits() & FEAT_NO_IOWAIT != 0
    }

    fn sqpoll(&self) -> bool {
        self.p.flags.contains(IoringSetupFlags::SQPOLL)
    }

    fn sq_flags(&self) -> u32 {
        // SAFETY: champ de l'anneau mappé, vivant tant que `self`.
        unsafe { (*self.sq_flags).load(Ordering::Acquire) }
    }

    // ── SQ ──

    /// SQE écrites et pas encore prises par le noyau.
    pub fn sq_len(&self) -> usize {
        // SAFETY: comme `sq_flags`.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        self.tail.wrapping_sub(head) as usize
    }

    pub fn sq_capacity(&self) -> usize {
        self.sq_entries as usize
    }

    pub fn sq_is_empty(&self) -> bool {
        self.sq_len() == 0
    }

    /// Écrit `sqe` en queue de la SQ ; `Err(())` si elle est pleine.
    ///
    /// # Safety
    /// Les tampons et descripteurs que vise `sqe` vivent jusqu'à son CQE.
    pub unsafe fn push(&mut self, sqe: &Sqe) -> core::result::Result<(), ()> {
        if self.sq_len() >= self.sq_entries as usize {
            return Err(());
        }
        // SAFETY: index masqué dans le tableau des SQE ; l'entrée n'est plus au noyau
        // (`head` l'a dépassée).
        unsafe {
            *self.sqes.add((self.tail & self.sq_mask) as usize) = *sqe;
            self.tail = self.tail.wrapping_add(1);
            (*self.sq_tail).store(self.tail, Ordering::Release);
        }
        Ok(())
    }

    /// SQPOLL : le thread noyau dort, il faut `SQ_WAKEUP` pour qu'il lise la SQ.
    /// Barrière complète d'abord : notre queue doit être visible du noyau avant la
    /// lecture du drapeau (même raisonnement que liburing, io-uring #197).
    pub fn need_wakeup(&self) -> bool {
        fence(Ordering::SeqCst);
        // SAFETY: comme `sq_flags`.
        unsafe { (*self.sq_flags).load(Ordering::Relaxed) & IoringSqFlags::NEED_WAKEUP.bits() != 0 }
    }

    /// `DEFER_TASKRUN` + `TASKRUN_FLAG` : du travail attend notre prochain `enter`.
    pub fn taskrun(&self) -> bool {
        self.sq_flags() & IoringSqFlags::TASKRUN.bits() != 0
    }

    /// Soumet ce qui est en SQ, sans attendre.
    pub fn submit(&self) -> Result<usize> {
        self.submit_and_wait(0)
    }

    /// Soumet et attend `want` complétions. Comme la crate io-uring : `GETEVENTS`
    /// aussi quand le CQ a débordé (le noyau garde les CQE, `FEAT_NODROP`) ou que du
    /// travail différé attend ; en SQPOLL, pas d'appel du tout s'il n'y a rien à
    /// réveiller ni à récolter.
    pub fn submit_and_wait(&self, want: u32) -> Result<usize> {
        let len = self.sq_len();
        let f = self.sq_flags();
        let overflow = f & IoringSqFlags::CQ_OVERFLOW.bits() != 0;
        let taskrun = f & IoringSqFlags::TASKRUN.bits() != 0;
        let need_syscall = (overflow && self.p.features.contains(IoringFeatureFlags::NODROP)) || taskrun;
        let mut flags = 0;
        if want > 0 || overflow || taskrun {
            flags |= enter::GETEVENTS;
        }
        if self.sqpoll() {
            if self.need_wakeup() {
                flags |= enter::SQ_WAKEUP;
            } else if want == 0 && !need_syscall {
                return Ok(len);
            }
        }
        // SAFETY: sans argument étendu.
        let n = unsafe {
            rustix::io_uring::io_uring_enter(&self.fd, len as u32, want, IoringEnterFlags::from_bits_retain(flags))?
        };
        Ok(n as usize)
    }

    /// `io_uring_enter` brut avec arguments étendus (`flags` doit contenir `EXT_ARG`).
    pub fn enter(&self, to_submit: u32, min_complete: u32, flags: u32, args: &mut WaitArgs) -> Result<usize> {
        debug_assert!(flags & enter::EXT_ARG != 0);
        args.arg.ts = io_uring_ptr::new((&raw mut args.ts).cast());
        // SAFETY: `args` (et son `ts`) vit pendant l'appel.
        let n = unsafe {
            rustix::io_uring::io_uring_enter_arg(
                &self.fd,
                to_submit,
                min_complete,
                IoringEnterFlags::from_bits_retain(flags),
                Some(&args.arg),
            )?
        };
        Ok(n as usize)
    }

    // ── CQ ──

    pub fn cq_is_empty(&self) -> bool {
        // SAFETY: comme `sq_flags`.
        self.head == unsafe { (*self.cq_tail).load(Ordering::Acquire) }
    }

    /// Recopie toutes les CQE disponibles dans `out` et les rend au noyau.
    pub fn harvest(&mut self, out: &mut Vec<Cqe>) {
        // SAFETY: entrées `[head, tail)` publiées par le noyau (Acquire sur `tail`) ;
        // la tête n'est rendue qu'après la copie.
        unsafe {
            let tail = (*self.cq_tail).load(Ordering::Acquire);
            while self.head != tail {
                out.push(*self.cqes.add((self.head & self.cq_mask) as usize));
                self.head = self.head.wrapping_add(1);
            }
            (*self.cq_head).store(self.head, Ordering::Release);
        }
    }

    // ── register ──

    /// # Safety
    /// `arg` pointe sur `nr_args` éléments du type que demande `op`.
    unsafe fn register(&self, op: IoringRegisterOp, arg: *const core::ffi::c_void, nr_args: u32) -> Result<u32> {
        unsafe { rustix::io_uring::io_uring_register(&self.fd, op, arg, nr_args) }
    }

    /// Table de `nr` fichiers enregistrés, vide (`IORING_RSRC_REGISTER_SPARSE`).
    pub fn register_files_sparse(&self, nr: u32) -> Result<()> {
        let mut rr = rustix::io_uring::io_uring_rsrc_register::default();
        rr.nr = nr;
        rr.flags = rustix::io_uring::IoringRsrcFlags::REGISTER_SPARSE;
        // SAFETY: une structure, vivante pendant l'appel.
        unsafe {
            self.register(IoringRegisterOp::RegisterFiles2, (&raw const rr).cast(), size_of_val(&rr) as u32).map(drop)
        }
    }

    /// Place `fds` dans la table à partir de `offset` (le noyau les duplique).
    pub fn register_files_update(&self, offset: u32, fds: &[RawFd]) -> Result<u32> {
        let mut fu = rustix::io_uring::io_uring_files_update::default();
        fu.offset = offset;
        fu.fds = io_uring_ptr::new(fds.as_ptr() as *mut _);
        // SAFETY: `fds.len()` descripteurs, vivants pendant l'appel.
        unsafe { self.register(IoringRegisterOp::RegisterFilesUpdate, (&raw const fu).cast(), fds.len() as u32) }
    }

    /// Anneau de tampons fournis du groupe `bgid`.
    ///
    /// # Safety
    /// `base` : `entries` entrées alignées sur une page, vivantes tant que l'anneau.
    pub unsafe fn register_buf_ring(&self, base: *mut BufRingEntry, entries: u16, bgid: u16, flags: u16) -> Result<()> {
        let mut reg = rustix::io_uring::io_uring_buf_reg::default();
        reg.ring_addr = io_uring_ptr::new(base.cast());
        reg.ring_entries = entries as u32;
        reg.bgid = bgid;
        reg.flags = flags;
        // SAFETY: une structure, vivante pendant l'appel ; l'anneau, voir la fonction.
        unsafe { self.register(IoringRegisterOp::RegisterPbufRing, (&raw const reg).cast(), 1).map(drop) }
    }

    /// Plafonds de workers io-wq `[bornés (disque), non bornés (réseau)]` ; 0 = inchangé.
    pub fn register_iowq_max_workers(&self, max: &mut [u32; 2]) -> Result<()> {
        // SAFETY: deux `u32`, le noyau y rend les anciennes valeurs.
        unsafe { self.register(IoringRegisterOp::RegisterIowqMaxWorkers, max.as_mut_ptr().cast(), 2).map(drop) }
    }

    /// Affinité des workers io-wq : `cpus` est un masque (bit `n` = CPU `n`), la
    /// taille d'un `cpu_set_t` de la libc (1024 CPU).
    pub fn register_iowq_aff(&self, cpus: &[u64; 16]) -> Result<()> {
        // SAFETY: masque de 128 octets vivant pendant l'appel.
        unsafe { self.register(IoringRegisterOp::RegisterIowqAff, cpus.as_ptr().cast(), size_of_val(cpus) as u32).map(drop) }
    }
}

impl AsFd for Ring {
    fn as_fd(&self) -> rustix::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::*;

    /// Écriture puis fsync liés dans un fichier, lecture, cancel d'un recv multishot
    /// sur un socket enregistré : les champs encodés sont ceux qu'attend le noyau.
    #[test]
    fn write_read_cancel_on_a_real_ring() {
        let mut ring = Builder::new().single_issuer().cqsize(64).defer_taskrun().taskrun_flag().build(8).unwrap();
        let path = format!("{}/radixox-ring-{}", std::env::temp_dir().display(), std::process::id());
        let f = crate::fs::open(&path, rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE, rustix::fs::Mode::from_raw_mode(0o600))
            .unwrap();
        let data = b"hello ring";
        unsafe {
            ring.push(&Sqe::write(Target::Fd(f.as_raw_fd()), data.as_ptr(), data.len() as u32, 3).flags(flags::IO_LINK).user_data(1))
                .unwrap();
            ring.push(&Sqe::fsync(Target::Fd(f.as_raw_fd()), FSYNC_DATASYNC).user_data(2)).unwrap();
        }
        assert_eq!(ring.sq_len(), 2);
        ring.submit_and_wait(2).unwrap();
        let mut out = Vec::new();
        ring.harvest(&mut out);
        let got: Vec<(u64, i32)> = out.iter().map(|c| (c.user_data, c.res)).collect();
        assert_eq!(got, [(1, data.len() as i32), (2, 0)]);
        assert!(ring.cq_is_empty());

        let mut back = [0u8; 16];
        out.clear();
        unsafe { ring.push(&Sqe::read(Target::Fd(f.as_raw_fd()), back.as_mut_ptr(), 16).user_data(3)).unwrap() };
        ring.submit_and_wait(1).unwrap();
        ring.harvest(&mut out);
        assert_eq!((out[0].user_data, out[0].res), (3, 13));
        assert_eq!(&back[3..13], data);
        drop(f);
        rustix::fs::unlink(&path).unwrap();

        // Socket dans la table des fichiers, recv multishot sans tampons fournis puis cancel.
        let (a, _b) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        ring.register_files_sparse(4).unwrap();
        assert_eq!(ring.register_files_update(2, &[a.as_raw_fd()]), Ok(1));
        out.clear();
        unsafe {
            ring.push(&Sqe::recv_multi(Target::Fixed(2), 9).user_data(4)).unwrap();
            ring.push(&Sqe::cancel(4).user_data(5)).unwrap();
        }
        ring.submit_and_wait(2).unwrap();
        ring.harvest(&mut out);
        let mut got: Vec<(u64, i32)> = out.iter().map(|c| (c.user_data, c.res)).collect();
        got.sort();
        // Groupe 9 absent : le recv finit en ENOBUFS (ou annulé), la cancel rend 0 ou ENOENT.
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].0 == 4 && got[0].1 < 0, "{got:?}");
        assert!(got[1].0 == 5 && (got[1].1 == 0 || got[1].1 == -Errno::NOENT.raw_os_error()), "{got:?}");
        let mut max = [0, 0];
        ring.register_iowq_max_workers(&mut max).unwrap();
    }
}
