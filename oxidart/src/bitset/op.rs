//! BITOP (BITSET_DESIGN.md §10.6) : AND / OR / XOR / NOT par tranches de mots.
//!
//! On avance par tranches de `CW` mots sur `[0, L)` (`L` : longueur de la plus longue
//! source, arrondie à l'octet). Chaque source remplit un tampon de la tranche (copie de mots
//! en classique, offsets posés en compacte), on combine mot à mot, et la tranche du résultat
//! est gardée en mots si elle est dense, en offsets sinon. Les tranches vides sont sautées
//! d'un coup (`next_set` sur les sources) : le coût suit le contenu, pas l'étendue. Le
//! résultat est construit à la fin dans la forme la moins chère, en bloc.

#[allow(unused_imports)]
use crate::prelude::*;

use radixox_lib::bitset::{BitsetId, WordBits, popcount_words};

use super::{Bitset, END, Form, classic_cost, compact_cost};

/// Mots par tranche (256 Ki bits, 32 Kio).
const CW: usize = 4096;
const CB: u64 = CW as u64 * 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOp {
    And,
    Or,
    Xor,
    Not,
}

/// Tranche du résultat : mots (dense) ou offsets (clairsemée).
enum Piece {
    Words { base: u64, words: Vec<u64> },
    Offs(Vec<u32>),
}

/// Offsets de `[from, to)` de `s` dans `out` (vidé).
fn collect(s: &Bitset, from: u64, to: u64, out: &mut Vec<u32>) {
    out.clear();
    s.scan(from, to, |k| {
        out.push(k);
        true
    });
}

/// `x op y` sur deux suites strictement croissantes, dans `out` (vidé).
fn merge(op: BitOp, x: &[u32], y: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        let (a, b) = (x[i], y[j]);
        if a == b {
            if op != BitOp::Xor {
                out.push(a);
            }
            i += 1;
            j += 1;
        } else if a < b {
            if op != BitOp::And {
                out.push(a);
            }
            i += 1;
        } else {
            if op != BitOp::And {
                out.push(b);
            }
            j += 1;
        }
    }
    if op != BitOp::And {
        out.extend_from_slice(&x[i..]);
        out.extend_from_slice(&y[j..]);
    }
}

/// Combine les sources (`None` : clé absente, vide) sur `[0, len_bits)`. `None` : résultat
/// vide. NOT : exactement une source.
pub(crate) fn bitop(op: BitOp, srcs: &[Option<&Bitset>], len_bits: u64) -> Option<Bitset> {
    let len_bits = len_bits.min(END);
    if op == BitOp::And && srcs.iter().any(Option::is_none) {
        return None;
    }
    let live: Vec<&Bitset> = srcs.iter().flatten().copied().collect();
    let mut offs: Vec<u32> = Vec::new();
    let mut a: Vec<u32> = Vec::new();
    let mut b: Vec<u32> = Vec::new();
    let mut acc = vec![0u64; CW];
    let mut tmp = vec![0u64; CW];
    let mut pieces = Vec::new();
    let (mut count, mut hi) = (0u64, 0u64);
    let mut base = 0u64;
    while base < len_bits {
        // première tranche qui peut donner un bit à 1
        if op != BitOp::Not {
            let next = match op {
                BitOp::And => live.iter().map(|b| b.next_set(base, END).map(u64::from)).try_fold(0, |m, n| n.map(|n| m.max(n))),
                _ => live.iter().filter_map(|b| b.next_set(base, END)).map(u64::from).min(),
            };
            match next {
                Some(n) if n < len_bits => base = base.max(n / CB * CB),
                _ => break,
            }
        }
        let end = (base + CB).min(len_bits);
        // sources toutes compactes sur la tranche : fusion des offsets triés, sans mots
        if op != BitOp::Not && live.iter().all(|s| s.compact_on(base, end)) {
            collect(live[0], base, end, &mut offs);
            for s in &live[1..] {
                collect(s, base, end, &mut a);
                merge(op, &offs, &a, &mut b);
                core::mem::swap(&mut offs, &mut b);
            }
            if let Some(&l) = offs.last() {
                count += offs.len() as u64;
                hi = l as u64 + 1;
                pieces.push(Piece::Offs(offs.clone()));
            }
            base += CB;
            continue;
        }
        match op {
            BitOp::Not => {
                live[0].fill_chunk(base, &mut acc);
                acc.iter_mut().for_each(|w| *w = !*w);
            }
            _ => {
                live[0].fill_chunk(base, &mut acc);
                for b in &live[1..] {
                    b.fill_chunk(base, &mut tmp);
                    let f = match op {
                        BitOp::And => |a: &mut u64, t: u64| *a &= t,
                        BitOp::Or => |a: &mut u64, t: u64| *a |= t,
                        _ => |a: &mut u64, t: u64| *a ^= t,
                    };
                    acc.iter_mut().zip(&tmp).for_each(|(a, &t)| f(a, t));
                }
            }
        }
        // rien au-delà de `len_bits` (NOT, dernière tranche)
        let valid = (end - base) as usize;
        let full = valid / 64;
        if full < CW {
            if valid % 64 != 0 {
                acc[full] &= (1u64 << (valid % 64)) - 1;
                acc[full + 1..].fill(0);
            } else {
                acc[full..].fill(0);
            }
        }
        let pop = popcount_words(&acc);
        if pop > 0 {
            count += pop;
            let lw = acc.iter().rposition(|&w| w != 0).unwrap();
            hi = base + lw as u64 * 64 + 64 - acc[lw].leading_zeros() as u64;
            // offsets (4 o) plus petits que les mots utiles : clairsemée
            if pop * 32 < (lw as u64 + 1) * 64 {
                pieces.push(Piece::Offs(WordBits::new(&acc).map(|k| (base + k as u64) as u32).collect()));
            } else {
                pieces.push(Piece::Words { base, words: acc[..=lw].to_vec() });
            }
        }
        base += CB;
    }
    if count == 0 {
        return None;
    }
    let form = if classic_cost(hi) <= compact_cost(count) {
        let mut w = vec![0u64; hi.div_ceil(64) as usize];
        for p in &pieces {
            match p {
                Piece::Words { base, words } => {
                    let b = (*base >> 6) as usize;
                    w[b..b + words.len()].copy_from_slice(words);
                }
                Piece::Offs(v) => v.iter().for_each(|&k| w[(k >> 6) as usize] |= 1 << (k & 63)),
            }
        }
        Form::Classic(w)
    } else {
        let iter = pieces.iter().flat_map(|p| -> Box<dyn Iterator<Item = u32> + '_> {
            match p {
                Piece::Words { base, words } => Box::new(WordBits::new(words).map(move |k| (*base + k as u64) as u32)),
                Piece::Offs(v) => Box::new(v.iter().copied()),
            }
        });
        Form::Compact(BitsetId::from_sorted_iter(count, iter))
    };
    Some(Bitset::with(form, count, hi))
}
