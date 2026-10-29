//! Chaque fonction contre une référence octet par octet : toutes les tailles jusqu'à 300 et
//! autour de `REP_MIN`, tous les décalages jusqu'à 33, chevauchements dans les deux sens.

use radixox_mem::{REP_MIN, bcmp, memcmp, memcpy, memmove, memset};

fn sizes() -> Vec<usize> {
    let mut v: Vec<usize> = (0..=300).collect();
    v.extend([REP_MIN - 1, REP_MIN, REP_MIN + 1, REP_MIN + 33, 5000, 70_001]);
    v
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

#[test]
fn memcpy_all_sizes_and_offsets() {
    for n in sizes() {
        for off in [0usize, 1, 3, 7, 8, 15, 16, 31, 33] {
            let src = pattern(n + 64, 5);
            let mut dst = vec![0xAAu8; n + 128];
            let r = unsafe { memcpy(dst.as_mut_ptr().add(off), src.as_ptr().add(off / 2), n) };
            assert_eq!(r, unsafe { dst.as_mut_ptr().add(off) });
            assert_eq!(&dst[off..off + n], &src[off / 2..off / 2 + n], "n {n} off {off}");
            assert!(dst[..off].iter().all(|&b| b == 0xAA), "avant, n {n} off {off}");
            assert!(dst[off + n..].iter().all(|&b| b == 0xAA), "après, n {n} off {off}");
        }
    }
}

#[test]
fn memmove_overlapping_both_ways() {
    for n in sizes() {
        for delta in [-70isize, -33, -32, -17, -8, -1, 0, 1, 8, 17, 32, 33, 70] {
            let len = n + 200;
            let mut buf = pattern(len, 9);
            let mut want = buf.clone();
            let (s, d) = (100usize, (100 + delta) as usize);
            want.copy_within(s..s + n, d);
            unsafe { memmove(buf.as_mut_ptr().add(d), buf.as_ptr().add(s), n) };
            assert_eq!(buf, want, "n {n} delta {delta}");
        }
    }
}

#[test]
fn memset_all_sizes() {
    for n in sizes() {
        for off in [0usize, 1, 7, 16, 31] {
            let mut buf = vec![0x11u8; n + 64];
            unsafe { memset(buf.as_mut_ptr().add(off), 0x1C7, n) };
            assert!(buf[off..off + n].iter().all(|&b| b == 0xC7), "n {n} off {off}");
            assert!(buf[..off].iter().chain(&buf[off + n..]).all(|&b| b == 0x11), "bords, n {n} off {off}");
        }
    }
}

#[test]
fn memcmp_and_bcmp_every_difference_position() {
    for n in sizes().into_iter().filter(|&n| n <= 300 || n == REP_MIN + 1) {
        let a = pattern(n, 3);
        assert_eq!(unsafe { memcmp(a.as_ptr(), a.clone().as_ptr(), n) }, 0, "égaux, n {n}");
        assert_eq!(unsafe { bcmp(a.as_ptr(), a.clone().as_ptr(), n) }, 0, "égaux, n {n}");
        for k in 0..n {
            for (x, y) in [(0x00u8, 0xFFu8), (0xFF, 0x00), (0x7F, 0x80), (5, 6)] {
                let (mut p, mut q) = (a.clone(), a.clone());
                p[k] = x;
                q[k] = y;
                let got = unsafe { memcmp(p.as_ptr(), q.as_ptr(), n) };
                assert_eq!(got.signum(), p.cmp(&q) as i32, "memcmp n {n} k {k} {x:#x}/{y:#x}");
                assert_ne!(unsafe { bcmp(p.as_ptr(), q.as_ptr(), n) }, 0, "bcmp n {n} k {k}");
            }
        }
    }
}
