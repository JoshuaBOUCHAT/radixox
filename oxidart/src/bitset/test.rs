//! Tests du bitset à deux formes : différentiels contre un `BTreeSet`, bascules dans les
//! deux sens entrelacées avec les écritures et les lectures, commandes contre un modèle
//! des réponses de Redis.

#[allow(unused_imports)]
use crate::prelude::*;
use std::collections::BTreeSet;

use super::*;
use crate::OxidArt;
use crate::bcommand::BitUnit;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Lectures croisées contre l'oracle, sur des plages au hasard.
fn check_reads(b: &Bitset, o: &BTreeSet<u32>, rng: &mut Rng, span: u64) {
    b.check();
    for _ in 0..8 {
        let from = rng.below(span);
        let to = (from + rng.below(span / 2 + 2)).min(END);
        let want: Vec<u32> = o.range(from as u32..).copied().take_while(|&k| (k as u64) < to).collect();
        assert_eq!(b.count_in(from, to), want.len() as u64, "count_in {from}..{to}");
        assert_eq!(b.next_set(from, to), want.first().copied(), "next_set {from}..{to}");
        let clear = (from..to).find(|&x| x >= END || !o.contains(&(x as u32)));
        assert_eq!(b.next_clear(from, to), clear, "next_clear {from}..{to}");
        let off = rng.below(span) as u32;
        assert_eq!(b.get(off), o.contains(&off), "get {off}");
    }
}

/// Phases qui forcent les deux bascules : dense en bas (→ classique), loin (→ compacte),
/// effacements massifs (→ compacte), à nouveau dense. Tranches minuscules et pas manuels :
/// les deux formes cohabitent longtemps.
fn phases(seed: u64, step: bool) {
    let before = radixox_lib::bitset::stats();
    let mut rng = Rng(seed);
    let mut b = Bitset::from_sorted(&[]);
    let mut o = BTreeSet::new();
    let (mut to_classic, mut to_compact) = (0, 0);
    let mut was_classic = b.is_classic();
    for phase in 0..12 {
        let n = 400 + rng.below(3000);
        for i in 0..n {
            let off = match phase % 4 {
                0 => rng.below(3000) as u32,
                1 if i % 50 == 0 => rng.next() as u32,
                1 => rng.below(3000) as u32,
                2 => rng.below(3000) as u32,
                _ => (rng.below(40_000) + 100_000) as u32,
            };
            if phase % 4 == 2 {
                assert_eq!(b.clear(off, step), o.remove(&off), "clear {off}");
            } else {
                assert_eq!(b.set(off, step), !o.insert(off), "set {off}");
            }
            if rng.below(7) == 0 {
                b.step(1 + rng.below(40) as u32);
            }
            if b.is_classic() != was_classic {
                was_classic = b.is_classic();
                if was_classic { to_classic += 1 } else { to_compact += 1 }
            }
            if i % 97 == 0 {
                check_reads(&b, &o, &mut rng, 160_000);
            }
        }
        assert_eq!(b.count(), o.len() as u64);
        assert_eq!(b.last(), o.last().copied());
    }
    assert!(to_classic > 0 && to_compact > 0, "bascules : {to_classic} → classique, {to_compact} → compacte");
    while !b.step(13) {}
    check_reads(&b, &o, &mut rng, 160_000);
    assert!(b.to_vec().iter().eq(o.iter()));
    b.release();
    while radixox_lib::bitset::free_step(64) {}
    assert_eq!(radixox_lib::bitset::stats(), before);
}

#[test]
fn migrations_both_ways_interleaved() {
    phases(0x9E37_79B9_7F4A_7C15, true);
    phases(0x0123_4567_89AB_CDEF, false);
}

#[test]
fn form_choice() {
    // petit et dense : classique ; offset lointain : compacte
    let b = Bitset::from_sorted(&(0..100).collect::<Vec<_>>());
    assert!(b.is_classic());
    b.release();
    let b = Bitset::from_sorted(&[5, 1_000_000, u32::MAX]);
    assert!(!b.is_classic());
    assert_eq!(b.next_clear(u32::MAX as u64, END), None);
    assert_eq!(b.next_clear(5, END), Some(6));
    b.release();
    // un SETBIT lointain sur un classique bascule au lieu d'allouer
    let mut b = Bitset::from_sorted(&(0..64).collect::<Vec<_>>());
    assert!(b.is_classic());
    b.set(u32::MAX, true);
    assert!(!b.is_classic());
    b.check();
    while !b.step(1000) {}
    assert_eq!(b.count(), 65);
    assert!(b.get(u32::MAX) && b.get(63) && !b.get(64));
    b.release();
    while radixox_lib::bitset::free_step(64) {}
}

#[test]
fn image_scans() {
    let mut rng = Rng(42);
    for &(n, span) in &[(10u64, 100u64), (2000, 3000), (500, 1 << 32)] {
        let o: BTreeSet<u32> = (0..n).map(|_| rng.below(span) as u32).collect();
        let v: Vec<u32> = o.iter().copied().collect();
        let b = Bitset::from_sorted(&v);
        let img = b.image();
        assert_eq!(img.count(), v.len() as u64);
        for _ in 0..50 {
            let from = rng.below(span);
            let mut got = Vec::new();
            img.scan(from, |k| {
                got.push(k);
                got.len() < 20
            });
            let want: Vec<u32> = o.range(from.min(u32::MAX as u64) as u32..).copied().take(20).collect();
            assert_eq!(got, want);
        }
        b.release();
    }
    while radixox_lib::bitset::free_step(64) {}
}

// ─── Commandes contre un modèle de Redis ─────────────────────────────────────

/// BITCOUNT / BITPOS de Redis sur la string dont les bits à 1 sont `o` (longueur : dernier
/// octet non nul, comme notre bitset).
mod redis {
    use super::*;

    fn norm(start: i64, end: i64, totlen: i64) -> Option<(i64, i64)> {
        let mut s = if start < 0 { totlen + start } else { start };
        let mut e = if end < 0 { totlen + end } else { end };
        s = s.max(0);
        e = e.max(0);
        if e >= totlen {
            e = totlen - 1;
        }
        (s <= e && totlen > 0).then_some((s, e))
    }

    fn bytes(o: &BTreeSet<u32>) -> i64 {
        o.last().map_or(0, |&l| l as i64 / 8 + 1)
    }

    fn bits(s: i64, e: i64, unit: BitUnit) -> (i64, i64) {
        match unit {
            BitUnit::Byte => (s * 8, e * 8 + 7),
            BitUnit::Bit => (s, e),
        }
    }

    pub fn bitcount(o: &BTreeSet<u32>, r: Option<(i64, i64, BitUnit)>) -> u64 {
        let Some((s, e, unit)) = r else { return o.len() as u64 };
        let tot = if unit == BitUnit::Bit { bytes(o) * 8 } else { bytes(o) };
        let Some((s, e)) = norm(s, e, tot) else { return 0 };
        let (a, z) = bits(s, e, unit);
        o.range(a as u32..=z.min(u32::MAX as i64) as u32).count() as u64
    }

    pub fn bitpos(o: &BTreeSet<u32>, bit: bool, s: Option<i64>, e: Option<i64>, unit: BitUnit) -> i64 {
        if o.is_empty() {
            return if bit { -1 } else { 0 };
        }
        let tot = if unit == BitUnit::Bit { bytes(o) * 8 } else { bytes(o) };
        let Some((s2, e2)) = norm(s.unwrap_or(0), e.unwrap_or(-1), tot) else { return -1 };
        let (a, z) = bits(s2, e2, unit);
        if bit {
            return o.range(a as u32..=z.min(u32::MAX as i64) as u32).next().map_or(-1, |&k| k as i64);
        }
        let mut x = a;
        while x <= z && o.contains(&(x as u32)) {
            x += 1;
        }
        if x <= z || e.is_none() { x } else { -1 }
    }
}

#[test]
fn commands_match_redis_model() {
    let mut rng = Rng(0xB175_E7B1_75E7_0001);
    let mut art = OxidArt::new();
    let mut o = BTreeSet::new();
    let k = b"bits";
    for i in 0..30_000 {
        let off = match rng.below(10) {
            0 => rng.next() as u32,
            1..=3 => rng.below(64) as u32,
            _ => rng.below(5000) as u32,
        };
        let on = rng.below(100) < 60;
        let old = art.cmd_setbit(k, off, on).unwrap();
        assert_eq!(old, o.contains(&off));
        if on { o.insert(off) } else { o.remove(&off) };
        if o.is_empty() {
            assert_eq!(art.key_type(k), crate::value::RedisType::None, "clé vide supprimée");
        }
        if i % 11 == 0 {
            let unit = if rng.below(2) == 0 { BitUnit::Byte } else { BitUnit::Bit };
            let r = |rng: &mut Rng| rng.below(1400) as i64 - 700;
            let range = (rng.below(3) > 0).then(|| (r(&mut rng), r(&mut rng), unit));
            assert_eq!(art.cmd_bitcount(k, range).unwrap(), redis::bitcount(&o, range), "BITCOUNT {range:?}");
            let bit = rng.below(2) == 0;
            let s = (rng.below(3) > 0).then(|| r(&mut rng));
            let e = s.and_then(|_| (rng.below(2) > 0).then(|| r(&mut rng)));
            assert_eq!(art.cmd_bitpos(k, bit, s, e, unit).unwrap(), redis::bitpos(&o, bit, s, e, unit), "BITPOS {bit} {s:?} {e:?} {unit:?}");
            assert_eq!(art.cmd_getbit(k, off).unwrap(), o.contains(&off));
        }
        if i % 500 == 0 {
            art.zset_free_step(256);
        }
        if i % 7000 == 6999 {
            // repartir de zéro de temps en temps (petites formes)
            art.del(k);
            o.clear();
        }
    }
    // mauvais type dans les deux sens
    art.set(radixox_lib::shared_byte::SharedByte::from_slice(b"s"), crate::Value::String(radixox_lib::shared_byte::SharedByte::from_slice(b"x")));
    assert!(art.cmd_setbit(b"s", 1, true).is_err());
    art.cmd_setbit(b"b", 1, true).unwrap();
    assert_eq!(art.key_type(b"b"), crate::value::RedisType::Bitset);
}
