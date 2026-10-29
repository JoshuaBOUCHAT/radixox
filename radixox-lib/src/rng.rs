//! Générateur pseudo-aléatoire du serveur (SPOP, ZRANDMEMBER) : wyrand, sans dépendance.
//! Mono-thread comme le reste du serveur. Graine tirée du noyau au premier appel :
//! `getrandom(2)` (le pool de `/dev/urandom`, sans descripteur), sinon un `read` de
//! `/dev/urandom`, sinon l'horloge — jamais de panic, le premier appel peut venir d'une
//! commande client.

/// État ; 0 = pas encore tiré (une graine nulle est remplacée par 1).
static mut STATE: u64 = 0;

/// 8 octets du noyau ; `None` si ni `getrandom` ni `/dev/urandom` ne répondent.
fn os_seed() -> Option<u64> {
    let mut b = [0u8; 8];
    let mut got = 0;
    while got < b.len() {
        match rustix::rand::getrandom(&mut b[got..], rustix::rand::GetRandomFlags::empty()) {
            Ok(n) if n > 0 => got += n,
            Err(rustix::io::Errno::INTR) => {}
            _ => break,
        }
    }
    if got < b.len() {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open("/dev/urandom", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).ok()?;
        got = 0;
        while got < b.len() {
            match rustix::io::read(&fd, &mut b[got..]) {
                Ok(n) if n > 0 => got += n,
                Err(rustix::io::Errno::INTR) => {}
                _ => return None,
            }
        }
    }
    Some(u64::from_ne_bytes(b))
}

#[cold]
#[inline(never)]
fn seed() -> u64 {
    let s = os_seed().unwrap_or_else(|| {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Realtime);
        (t.tv_sec as u64).wrapping_mul(1_000_000_000).wrapping_add(t.tv_nsec as u64)
            ^ (rustix::process::getpid().as_raw_nonzero().get() as u64).rotate_left(32)
    });
    s.max(1)
}

/// 64 bits uniformes.
#[inline]
pub fn u64() -> u64 {
    // SAFETY: mono-thread (serveur et tests, RUST_TEST_THREADS=1).
    unsafe {
        if STATE == 0 {
            STATE = seed();
        }
        STATE = STATE.wrapping_add(0xA076_1D64_78BD_642F);
        let t = STATE as u128 * (STATE ^ 0xE703_7ED1_A0B4_28DB) as u128;
        ((t >> 64) ^ t) as u64
    }
}

/// Uniforme dans `0..n` (`n > 0`), sans biais (Lemire : multiplication, rejet rare).
#[inline]
pub fn below(n: u64) -> u64 {
    debug_assert!(n > 0);
    let mut m = u64() as u128 * n as u128;
    if (m as u64) < n {
        let floor = n.wrapping_neg() % n;
        while (m as u64) < floor {
            m = u64() as u128 * n as u128;
        }
    }
    (m >> 64) as u64
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::*;

    #[test]
    fn below_in_range_and_spread() {
        let mut hits = [0u32; 7];
        for _ in 0..70_000 {
            hits[below(7) as usize] += 1;
        }
        for h in hits {
            assert!((9_000..11_000).contains(&h), "{hits:?}");
        }
        assert_eq!(below(1), 0);
    }

    #[test]
    fn os_seed_answers() {
        assert!(os_seed().is_some());
    }
}
