//! Horloge de cycles pour les budgets de tranche (RUNTIME_ARCH.md §9) : `rdtsc`
//! coûte ~7 ns la lecture contre ~17 ns pour `Instant::now`, assez peu pour la
//! relire entre deux étapes de travail de fond. Étalonnée une fois contre
//! l'horloge monotone ; sans TSC invariant (ou hors x86_64), repli sur
//! `Instant`, en nanosecondes.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

struct Calib {
    /// Ticks par microseconde.
    per_us: u64,
    tsc: bool,
    origin: Instant,
}

static CALIB: OnceLock<Calib> = OnceLock::new();

/// Étalonne l'horloge (~5 ms) : à appeler au démarrage, sinon la première
/// lecture le fait.
pub fn init() {
    calib();
}

fn calib() -> &'static Calib {
    CALIB.get_or_init(|| {
        let origin = Instant::now();
        if !invariant_tsc() {
            return Calib { per_us: 1000, tsc: false, origin };
        }
        let (t0, c0) = (Instant::now(), raw());
        while t0.elapsed() < Duration::from_millis(5) {
            std::hint::spin_loop();
        }
        let (dt, dc) = (t0.elapsed(), raw().wrapping_sub(c0));
        let per_us = (u128::from(dc) * 1000 / dt.as_nanos().max(1)).max(1) as u64;
        Calib { per_us, tsc: true, origin }
    })
}

/// TSC à fréquence constante et qui ne s'arrête pas dans les états de veille :
/// sinon les ticks ne mesurent pas le temps.
fn invariant_tsc() -> bool {
    cfg!(target_arch = "x86_64")
        && std::fs::read_to_string("/proc/cpuinfo").is_ok_and(|info| {
            info.lines().find(|l| l.starts_with("flags")).is_some_and(|l| {
                let has = |f: &str| l.split_whitespace().any(|x| x == f);
                has("constant_tsc") && has("nonstop_tsc")
            })
        })
}

#[inline]
fn raw() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `rdtsc` existe sur tout x86_64.
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

/// Lecture courante, en ticks.
#[inline]
pub fn now() -> u64 {
    let c = calib();
    if c.tsc { raw() } else { c.origin.elapsed().as_nanos() as u64 }
}

pub fn ticks(d: Duration) -> u64 {
    (d.as_nanos() * u128::from(calib().per_us) / 1000) as u64
}

/// Échéance, en ticks.
#[derive(Clone, Copy, Debug)]
pub struct Deadline(u64);

impl Deadline {
    pub fn after(d: Duration) -> Self {
        Self(now().saturating_add(ticks(d)))
    }

    #[inline]
    pub fn passed(self) -> bool {
        now() >= self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_follows_wall_time() {
        let d = Deadline::after(Duration::from_millis(2));
        assert!(!d.passed());
        std::thread::sleep(Duration::from_millis(3));
        assert!(d.passed());
        // 1 ms vaut entre 100 K et 10 G ticks (fréquences plausibles, ou ns).
        assert!((100_000..10_000_000_000).contains(&ticks(Duration::from_millis(1))));
    }
}

/// Horloge murale en secondes pour le TTL, lue à chaque tour :
/// `CLOCK_REALTIME_COARSE` (~3 ns, résolution d'un tick noyau, 1–4 ms) au lieu
/// de `SystemTime::now` (~5 % du thread de la boucle avec de petits tours).
/// Une seconde vue jusqu'à 4 ms en retard ne change rien au TTL.
pub fn wall_secs() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` est un timespec valide, l'horloge existe sous Linux ≥ 2.6.32.
    let r = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut ts) };
    debug_assert_eq!(r, 0);
    ts.tv_sec as u64
}
