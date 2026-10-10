//! Varints à préfixe façon UTF-8 : le nombre d'octets se lit dans le premier.
//!
//! | 1er octet   | octets | bits de valeur |
//! |-------------|--------|----------------|
//! | `0xxxxxxx`  | 1      | 7              |
//! | `10xxxxxx`  | 2      | 14             |
//! | `110xxxxx`  | 3      | 21             |
//! | …           | …      | 7 + 7n         |
//! | `11111110`  | 8      | 56             |
//! | `11111111`  | 9      | 64             |
//!
//! Le reste de la valeur suit en big-endian. Le décodeur connaît la longueur
//! dès le premier octet (un seul branchement, pas de boucle sur un bit de
//! continuation).

/// Nombre d'octets de l'encodage de `v`.
#[inline]
pub fn varint_len(v: u64) -> usize {
    let bits = 64 - v.leading_zeros() as usize;
    if bits > 56 { 9 } else { 1 + bits.saturating_sub(1) / 7 }
}

#[inline]
pub fn put_varint(out: &mut Vec<u8>, v: u64) {
    let len = varint_len(v);
    if len == 9 {
        out.push(0xFF);
        out.extend_from_slice(&v.to_be_bytes());
        return;
    }
    let extra = len - 1;
    // Préfixe : `extra` uns puis un zéro, dans l'octet de tête.
    let prefix: u8 = !(0xFFu8 >> extra);
    let be = v.to_be_bytes();
    let head = prefix | be[7 - extra];
    out.push(head);
    out.extend_from_slice(&be[8 - extra..]);
}

/// Lit un varint à `*pos` et avance `*pos`. `None` si le buffer est tronqué.
#[inline]
pub fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let head = *buf.get(*pos)?;
    let extra = head.leading_ones() as usize;
    if extra == 8 {
        let bytes = buf.get(*pos + 1..*pos + 9)?;
        *pos += 9;
        return Some(u64::from_be_bytes(bytes.try_into().unwrap()));
    }
    let rest = buf.get(*pos + 1..*pos + 1 + extra)?;
    let mut v = (head & (0x7F >> extra)) as u64;
    for &b in rest {
        v = (v << 8) | b as u64;
    }
    *pos += 1 + extra;
    Some(v)
}

/// `varint len | octets`.
#[inline]
pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Lit `varint len | octets` à `*pos`.
#[inline]
pub fn get_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = get_varint(buf, pos)? as usize;
    let bytes = buf.get(*pos..pos.checked_add(len)?)?;
    *pos += len;
    Some(bytes)
}

#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: u64) {
        let mut out = Vec::new();
        put_varint(&mut out, v);
        assert_eq!(out.len(), varint_len(v), "v = {v:#x}");
        let mut pos = 0;
        assert_eq!(get_varint(&out, &mut pos), Some(v), "v = {v:#x}");
        assert_eq!(pos, out.len());
    }

    #[test]
    fn boundaries() {
        for n in 0..=7u32 {
            let bits = 7 + 7 * n;
            let max = (1u64 << bits) - 1;
            roundtrip(max);
            roundtrip(max + 1);
            assert_eq!(varint_len(max), n as usize + 1);
            assert_eq!(varint_len(max + 1), n as usize + 2);
        }
        for v in [0, 1, 127, 128, u32::MAX as u64, u64::MAX, u64::MAX - 1, 1 << 56] {
            roundtrip(v);
        }
        assert_eq!(varint_len(u64::MAX), 9);
        assert_eq!(varint_len((1 << 56) - 1), 8);
    }

    #[test]
    fn length_from_first_byte() {
        let mut out = Vec::new();
        put_varint(&mut out, 300);
        assert_eq!(out, [0x81, 0x2C]); // 10|000001 00101100
    }

    #[test]
    fn many_values_and_truncation() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut out = Vec::new();
        let mut vals = Vec::new();
        for i in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let v = x >> (i % 64);
            vals.push(v);
            put_varint(&mut out, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(get_varint(&out, &mut pos), Some(v));
        }
        assert_eq!(get_varint(&out, &mut pos), None);
        let mut short = Vec::new();
        put_varint(&mut short, 1 << 40);
        short.pop();
        assert_eq!(get_varint(&short, &mut 0), None);
    }

    #[test]
    fn zigzag_roundtrip() {
        for v in [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN, 12345, -12345] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
    }
}
