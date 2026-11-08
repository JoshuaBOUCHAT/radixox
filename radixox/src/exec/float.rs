//! Addition d'INCRBYFLOAT / HINCRBYFLOAT.
//!
//! Redis calcule en `long double` (80 bits sur x86) et écrit le résultat avec
//! `%.17Lf`, zéros de fin retirés : `0.1 + 0.2` y donne `0.3`. Un `f64` donnerait
//! `0.30000000000000004`. On additionne donc en décimal exact (mantisse `i128`, au plus
//! 17 décimales) quand les deux opérandes s'y prêtent, ce qui rend la même chose que
//! Redis sur les valeurs usuelles ; sinon (trop de chiffres, `inf`…) en `f64`, écrit
//! sans exposant et avec au plus 17 décimales, comme Redis.

#[allow(unused_imports)]
use crate::prelude::*;

/// Décimales gardées (`%.17Lf` de Redis).
const SCALE: u32 = 17;

/// Pourquoi l'addition est refusée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatErr {
    /// La valeur stockée n'est pas un flottant.
    Value,
    /// L'incrément n'est pas un flottant.
    Incr,
    /// Résultat NaN ou infini.
    NanInf,
}

/// Flottant valide pour Redis (`string2ld`) : non vide, sans espace, pas NaN.
fn parse_f64(s: &[u8]) -> Option<f64> {
    if s.first().is_none_or(|b| b.is_ascii_whitespace()) {
        return None;
    }
    core::str::from_utf8(s).ok()?.parse::<f64>().ok().filter(|f| !f.is_nan())
}

/// `[+-]chiffres[.chiffres][e[+-]chiffres]` en mantisse et nombre de décimales
/// (`≤ SCALE`). `None` : pas de cette forme, ou hors de portée (on passe au `f64`).
fn parse_dec(s: &[u8]) -> Option<(i128, u32)> {
    let (neg, s) = match s.first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (body, exp) = match s.iter().position(|&b| b == b'e' || b == b'E') {
        Some(i) => {
            let e = &s[i + 1..];
            let (eneg, ed) = match e.first()? {
                b'-' => (true, &e[1..]),
                b'+' => (false, &e[1..]),
                _ => (false, e),
            };
            if ed.is_empty() || ed.len() > 3 || !ed.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let v: i32 = core::str::from_utf8(ed).ok()?.parse().ok()?;
            (&s[..i], if eneg { -v } else { v })
        }
        None => (s, 0),
    };
    let (int, frac) = match body.iter().position(|&b| b == b'.') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, &body[..0]),
    };
    if int.len() + frac.len() == 0 || !int.iter().chain(frac).all(u8::is_ascii_digit) {
        return None;
    }
    let mut m: i128 = 0;
    for &d in int.iter().chain(frac) {
        m = m.checked_mul(10)?.checked_add((d - b'0') as i128)?;
    }
    // décimales effectives : celles écrites, moins l'exposant
    let mut scale = frac.len() as i32 - exp;
    while scale < 0 {
        m = m.checked_mul(10)?;
        scale += 1;
    }
    if scale as u32 > SCALE {
        return None;
    }
    Some((if neg { -m } else { m }, scale as u32))
}

/// Mantisse ramenée à `to` décimales (`to ≥ scale`).
fn rescale(m: i128, scale: u32, to: u32) -> Option<i128> {
    m.checked_mul(10i128.checked_pow(to - scale)?)
}

fn format_dec(m: i128, scale: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    if m < 0 {
        out.push(b'-');
    }
    let digits = m.unsigned_abs().to_string();
    let d = digits.as_bytes();
    let s = scale as usize;
    if d.len() > s {
        out.extend_from_slice(&d[..d.len() - s]);
        if s > 0 {
            out.push(b'.');
            out.extend_from_slice(&d[d.len() - s..]);
        }
    } else {
        out.extend_from_slice(b"0.");
        out.resize(out.len() + s - d.len(), b'0');
        out.extend_from_slice(d);
    }
    trim(out)
}

/// Zéros de fin après la virgule, puis la virgule seule ; `-0` → `0`.
fn trim(mut out: Vec<u8>) -> Vec<u8> {
    if out.contains(&b'.') {
        while out.last() == Some(&b'0') {
            out.pop();
        }
        if out.last() == Some(&b'.') {
            out.pop();
        }
    }
    if out == b"-0" {
        out.remove(0);
    }
    out
}

fn format_f64(x: f64) -> Vec<u8> {
    // sans exposant (`Display` de Rust n'en met jamais) ; au plus 17 décimales
    let short = x.to_string();
    let frac = short.split_once('.').map_or(0, |(_, f)| f.len());
    if frac as u32 <= SCALE {
        return trim(short.into_bytes());
    }
    trim(format!("{x:.17}").into_bytes())
}

/// `cur + incr`, écrit comme Redis. `cur` : `None` pour une clé ou un champ absent (0).
pub fn add(cur: Option<&[u8]>, incr: &[u8]) -> Result<Vec<u8>, FloatErr> {
    let cur = cur.unwrap_or(b"0");
    let a = parse_f64(cur).ok_or(FloatErr::Value)?;
    let b = parse_f64(incr).ok_or(FloatErr::Incr)?;
    if let (Some((ma, sa)), Some((mb, sb))) = (parse_dec(cur), parse_dec(incr)) {
        let s = sa.max(sb);
        if let (Some(ma), Some(mb)) = (rescale(ma, sa, s), rescale(mb, sb, s))
            && let Some(m) = ma.checked_add(mb)
        {
            return Ok(format_dec(m, s));
        }
    }
    let r = a + b;
    if !r.is_finite() {
        return Err(FloatErr::NanInf);
    }
    Ok(format_f64(r))
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::{FloatErr, add};

    fn s(cur: Option<&str>, incr: &str) -> String {
        String::from_utf8(add(cur.map(str::as_bytes), incr.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn like_redis() {
        assert_eq!(s(None, "0.1"), "0.1");
        assert_eq!(s(Some("0.1"), "0.2"), "0.3");
        assert_eq!(s(Some("10.50"), "0.1"), "10.6");
        assert_eq!(s(Some("5.0e3"), "2.0e2"), "5200");
        assert_eq!(s(Some("10"), "-10"), "0");
        assert_eq!(s(Some("-0.5"), "0.25"), "-0.25");
        assert_eq!(s(Some("3"), "1.5e-3"), "3.0015");
        assert_eq!(s(Some("1"), "1e-20"), "1");
        assert_eq!(s(Some("1e300"), "1"), format!("{}", 1e300f64 + 1.0));
        assert_eq!(add(Some(b"abc"), b"1"), Err(FloatErr::Value));
        assert_eq!(add(Some(b"1"), b" 1"), Err(FloatErr::Incr));
        assert_eq!(add(Some(b"1"), b"nan"), Err(FloatErr::Incr));
        assert_eq!(add(Some(b"1"), b"inf"), Err(FloatErr::NanInf));
        assert_eq!(add(Some(b"1e308"), b"1e308"), Err(FloatErr::NanInf));
    }
}
