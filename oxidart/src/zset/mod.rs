//! ZSet (ANALYSE_ART_UNIFIE.md §13) : petite forme en colonnes, grande forme = index
//! valart + B+tree compté + membres à poignée (MEMORY_ARCH.md §19).

pub mod btree;
pub mod small;

/// `f64` → `u64` dont l'ordre est celui de `total_cmp` (bijection exacte sur les bits) ;
/// `-0.0` devient `0.0` (Valkey : même score). NaN refusé avant d'arriver ici.
#[inline]
pub fn encode_score(f: f64) -> u64 {
    debug_assert!(!f.is_nan());
    let b = if f == 0.0 { 0 } else { f.to_bits() };
    if b >> 63 == 1 { !b } else { b | 1 << 63 }
}

#[inline]
pub fn decode_score(u: u64) -> f64 {
    f64::from_bits(if u >> 63 == 1 { u & !(1 << 63) } else { !u })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_encoding_keeps_order() {
        let v = [f64::NEG_INFINITY, -1e300, -2.5, -1.0, -f64::MIN_POSITIVE, 0.0, 5e-324, 1.0, 2.5, 1e300, f64::INFINITY];
        for w in v.windows(2) {
            assert!(encode_score(w[0]) < encode_score(w[1]), "{} < {}", w[0], w[1]);
        }
        for x in v {
            assert_eq!(decode_score(encode_score(x)), x);
        }
        assert_eq!(encode_score(-0.0), encode_score(0.0));
    }
}
