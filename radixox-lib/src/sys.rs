//! Ce que la crate demande au système, sans `std` ni libc : appels directs au noyau par
//! `rustix` (mémoire, horloge, fichiers) et `sc` (ce que rustix n'a pas). Rien ici
//! n'alloue : tout est utilisable depuis l'allocateur lui-même. Les retours ignorés le sont
//! comme avant (`munmap` / `madvise` d'une plage à nous ne peut échouer qu'à tort).

use core::fmt;
use core::ops::{Add, AddAssign, Sub};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use core::time::Duration;

use rustix::mm::{self, MapFlags, MremapFlags, ProtFlags};

pub use rustix::mm::Advice;

/// `mmap` anonyme, privé, lecture-écriture ; `noreserve` : rien n'est engagé avant d'être
/// touché (`MAP_NORESERVE`).
#[inline]
pub fn map_anon(len: usize, noreserve: bool) -> rustix::io::Result<*mut u8> {
    let flags = if noreserve { MapFlags::PRIVATE | MapFlags::NORESERVE } else { MapFlags::PRIVATE };
    // SAFETY: nouvelle plage, aucune adresse imposée.
    unsafe { mm::mmap_anonymous(null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, flags) }.map(|p| p.cast())
}

/// # Safety
/// `[p, p + len)` est une plage mappée par nous, plus utilisée.
#[inline]
pub unsafe fn unmap(p: *mut u8, len: usize) {
    let _ = unsafe { mm::munmap(p.cast(), len) };
}

/// # Safety
/// `[p, p + len)` est dans une plage mappée par nous ; pour `LinuxDontNeed`, son contenu
/// n'est plus utilisé (relu comme des zéros).
#[inline]
pub unsafe fn advise(p: *mut u8, len: usize, advice: Advice) {
    let _ = unsafe { mm::madvise(p.cast(), len, advice) };
}

/// `mremap` avec `MREMAP_MAYMOVE`.
///
/// # Safety
/// `[p, p + old)` est une plage mappée par nous ; après succès, seule la nouvelle adresse vaut.
#[inline]
pub unsafe fn remap(p: *mut u8, old: usize, new: usize) -> rustix::io::Result<*mut u8> {
    unsafe { mm::mremap(p.cast(), old, new, MremapFlags::MAYMOVE) }.map(|p| p.cast())
}

/// Message sur stderr sans allouer (écriture partielle ignorée).
pub fn stderr(msg: &[u8]) {
    // SAFETY: le descripteur 2 reste ouvert pendant toute la vie du processus.
    #[allow(unused_unsafe)]
    let fd = unsafe { rustix::stdio::stderr() };
    let _ = rustix::io::write(fd, msg);
}

/// Message formaté sur stderr, sans allouer (tronqué à 512 octets).
pub fn eprint(args: fmt::Arguments<'_>) {
    struct Buf {
        b: [u8; 512],
        n: usize,
    }
    impl fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let k = s.len().min(self.b.len() - self.n);
            self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
            self.n += k;
            Ok(())
        }
    }
    let mut b = Buf { b: [0; 512], n: 0 };
    let _ = fmt::write(&mut b, args);
    stderr(&b.b[..b.n]);
}

/// `eprintln!` sans `std` ni allocation (voir [`eprint`]).
#[macro_export]
macro_rules! rx_eprintln {
    ($($t:tt)*) => {
        $crate::sys::eprint(format_args!("{}\n", format_args!($($t)*)))
    };
}

/// Arrêt immédiat (`SIGABRT`, comme `abort(3)`), sans allouer.
#[cold]
pub fn abort() -> ! {
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::ABORT);
    // SIGABRT ignoré ou bloqué : sortie directe, code d'un abort.
    // SAFETY: `exit_group` ne rend pas la main.
    unsafe { sc::syscall!(EXIT_GROUP, 134usize) };
    loop {}
}

/// Instant de l'horloge monotone (`CLOCK_MONOTONIC`, vDSO), même API que
/// `std::time::Instant` pour ce que la crate en fait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(Duration);

impl Instant {
    #[inline]
    pub fn now() -> Self {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        Self(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
    }
    #[inline]
    pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
    #[inline]
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        self.saturating_duration_since(earlier)
    }
    #[inline]
    pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
        self.0.checked_sub(earlier.0)
    }
    #[inline]
    pub fn elapsed(&self) -> Duration {
        Self::now().saturating_duration_since(*self)
    }
    #[inline]
    pub fn checked_add(&self, d: Duration) -> Option<Instant> {
        self.0.checked_add(d).map(Self)
    }
    #[inline]
    pub fn checked_sub(&self, d: Duration) -> Option<Instant> {
        self.0.checked_sub(d).map(Self)
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        Self(self.0 + d)
    }
}
impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, d: Duration) {
        self.0 += d;
    }
}
impl Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, d: Duration) -> Instant {
        Self(self.0 - d)
    }
}
impl Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, o: Instant) -> Duration {
        self.saturating_duration_since(o)
    }
}

/// Environnement du processus au démarrage (`/proc/self/environ`), lu une fois dans une
/// plage `mmap` : ni libc, ni allocation (l'allocateur le lit à son initialisation). Un
/// `setenv` ultérieur n'est pas vu ; la crate ne lit que de la configuration de démarrage.
static ENV_PTR: AtomicUsize = AtomicUsize::new(0);
static ENV_LEN: AtomicUsize = AtomicUsize::new(0);
/// 0 : pas lu ; 1 : lecture en cours ; 2 : prêt.
static ENV_STATE: AtomicU8 = AtomicU8::new(0);

#[cold]
fn load_env() {
    use rustix::fs::{Mode, OFlags};
    let Ok(fd) = rustix::fs::open("/proc/self/environ", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()) else {
        return;
    };
    // Plage réservée large (`MAP_NORESERVE`) : seules les pages lues comptent.
    const CAP: usize = 16 << 20;
    let Ok(p) = map_anon(CAP, true) else { return };
    // Sous THP `always`, la première écriture prendrait une page de 2 Mio pour quelques Kio
    // (non comptés : tests/memory.rs).
    // SAFETY: plage à nous.
    unsafe { advise(p, CAP, Advice::LinuxNoHugepage) };
    let mut n = 0;
    while n < CAP {
        // SAFETY: `[p + n, p + CAP)` est à nous.
        let buf = unsafe { core::slice::from_raw_parts_mut(p.add(n), CAP - n) };
        match rustix::io::read(&fd, buf) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => break,
        }
    }
    ENV_PTR.store(p as usize, Ordering::Relaxed);
    ENV_LEN.store(n, Ordering::Relaxed);
}

fn environ() -> &'static [u8] {
    loop {
        match ENV_STATE.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire) {
            Ok(_) => {
                load_env();
                ENV_STATE.store(2, Ordering::Release);
            }
            Err(2) => break,
            Err(_) => core::hint::spin_loop(),
        }
    }
    let (p, n) = (ENV_PTR.load(Ordering::Relaxed), ENV_LEN.load(Ordering::Relaxed));
    if p == 0 {
        return &[];
    }
    // SAFETY: plage remplie une fois par `load_env`, jamais libérée ni réécrite.
    unsafe { core::slice::from_raw_parts(p as *const u8, n) }
}

/// Valeur de la variable `name` au démarrage, si elle existe et est en UTF-8.
pub fn env(name: &str) -> Option<&'static str> {
    environ().split(|&b| b == 0).find_map(|kv| {
        let v = kv.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
        core::str::from_utf8(v).ok()
    })
}

/// Variable `name` lue comme un nombre, `default` si absente ou illisible.
pub fn env_parse<T: core::str::FromStr>(name: &str, default: T) -> T {
    env(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::*;

    #[test]
    fn env_reads_startup_environment() {
        // PATH existe sous cargo test.
        assert!(env("PATH").is_some_and(|p| !p.is_empty()));
        assert_eq!(env("RADIXOX_SURELY_UNSET_VARIABLE"), None);
        assert_eq!(env_parse("RADIXOX_SURELY_UNSET_VARIABLE", 7u32), 7);
    }

    #[test]
    fn instant_is_monotonic() {
        let a = Instant::now();
        let b = a + Duration::from_millis(5);
        assert!(b > a);
        assert_eq!(b - a, Duration::from_millis(5));
        assert_eq!(a.saturating_duration_since(b), Duration::ZERO);
        assert!(Instant::now() >= a);
    }
}
