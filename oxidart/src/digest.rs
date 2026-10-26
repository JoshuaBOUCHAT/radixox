//! Condensé du contenu d'un dump (DURABILITY_DESIGN.md §6.4) : vérifie que
//! l'arbre rechargé est l'arbre sauvé, indépendamment de l'ordre et de
//! l'encodage. Calculé deux fois par des parcours différents : le DFS de la
//! save (`save.rs`, sur la vue T0) et un parcours de l'arbre rechargé
//! ([`OxidArt::content_digest`]).
//!
//! Un terme par élément métier, sommés (wrapping) : la somme ne dépend pas de
//! l'ordre de visite.
//!
//! ```text
//! préfixe(clé, type, échéance) = u32 len | clé | u8 type | u64 échéance (0 = sans)
//! chaîne : crc32(préfixe | part(octets))          entier : crc32(préfixe | part(i64 LE))
//! hash   : crc32(préfixe | part(champ) | part(valeur)) par champ
//!          + crc32(préfixe' | part(cardinal u64))  (préfixe' : type | HEAD)
//! set    : crc32(préfixe | part(membre)) par membre, + terme de tête
//! zset   : crc32(préfixe | part(cardinal) | part(Σ crc32(part(membre) | part(score f64))))
//! list   : idem zset, éléments crc32(part(rang u64) | part(élément))
//! part(x) = u32 len | x
//! ```
//!
//! ZSet et List : un seul terme par clé, sur la somme de leurs éléments (ils
//! sont sérialisés au T0 dans une section à part, avant les clés).

use crc32fast::Hasher;

use crate::save::format;
use crate::value::{Tag, Value, ValUnion, value_from_raw_ref};
use crate::{OxidArt, expired};

/// Marque le terme de tête d'un conteneur (cardinal), distinct de ses éléments.
const HEAD: u8 = 0x40;

/// Somme des termes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Digest(u64);

impl Digest {
    #[inline]
    pub fn add(&mut self, term: Hasher) {
        self.0 = self.0.wrapping_add(term.finalize() as u64);
    }

    #[inline]
    pub fn merge(&mut self, other: Digest) {
        self.0 = self.0.wrapping_add(other.0);
    }

    /// Valeur du pied du dump.
    pub fn value(self) -> u32 {
        self.0 as u32
    }
}

#[inline]
pub(crate) fn part(h: &mut Hasher, b: &[u8]) {
    h.update(&(b.len() as u32).to_le_bytes());
    h.update(b);
}

/// Préfixe commun aux termes d'une clé ; `ty` : `format::T_*`.
#[inline]
pub(crate) fn prefix(key: &[u8], ty: u8, exp: Option<u64>) -> Hasher {
    let mut h = Hasher::new();
    part(&mut h, key);
    h.update(&[ty]);
    h.update(&exp.unwrap_or(0).to_le_bytes());
    h
}

/// Terme de tête d'un conteneur (Hash, Set) : son cardinal.
#[inline]
pub(crate) fn head_term(key: &[u8], ty: u8, exp: Option<u64>, card: u64) -> Hasher {
    let mut h = prefix(key, ty | HEAD, exp);
    part(&mut h, &card.to_le_bytes());
    h
}

/// Somme des éléments d'un ZSet.
pub(crate) fn zset_inner<'a>(items: impl Iterator<Item = (f64, &'a [u8])>) -> (u64, u64) {
    let (mut sum, mut n) = (0u64, 0u64);
    for (score, member) in items {
        let mut h = Hasher::new();
        part(&mut h, member);
        part(&mut h, &score.to_le_bytes());
        sum = sum.wrapping_add(h.finalize() as u64);
        n += 1;
    }
    (sum, n)
}

/// Somme des éléments d'une List (rang compris : l'ordre compte).
pub(crate) fn list_inner<'a>(items: impl Iterator<Item = &'a [u8]>) -> (u64, u64) {
    let (mut sum, mut n) = (0u64, 0u64);
    for item in items {
        let mut h = Hasher::new();
        part(&mut h, &n.to_le_bytes());
        part(&mut h, item);
        sum = sum.wrapping_add(h.finalize() as u64);
        n += 1;
    }
    (sum, n)
}

/// Terme d'un ZSet ou d'une List, à partir de la somme de ses éléments.
#[inline]
pub(crate) fn seq_term(key: &[u8], ty: u8, exp: Option<u64>, (sum, card): (u64, u64)) -> Hasher {
    let mut h = prefix(key, ty, exp);
    part(&mut h, &card.to_le_bytes());
    part(&mut h, &sum.to_le_bytes());
    h
}

/// Termes d'une valeur décodée (parcours de l'arbre, clés sautées au reload).
pub(crate) fn value_terms(d: &mut Digest, key: &[u8], exp: Option<u64>, v: &Value) {
    match v {
        Value::String(b) => {
            let mut h = prefix(key, format::T_STRING, exp);
            part(&mut h, b);
            d.add(h);
        }
        Value::Int(n) => {
            let mut h = prefix(key, format::T_INT, exp);
            part(&mut h, &n.to_le_bytes());
            d.add(h);
        }
        Value::Hash(fields) => {
            let p = prefix(key, format::T_HASH, exp);
            for (f, v) in fields {
                let mut h = p.clone();
                part(&mut h, f);
                part(&mut h, v);
                d.add(h);
            }
            d.add(head_term(key, format::T_HASH, exp, fields.len() as u64));
        }
        Value::Set(members) => {
            let p = prefix(key, format::T_SET, exp);
            for m in members {
                let mut h = p.clone();
                part(&mut h, m);
                d.add(h);
            }
            d.add(head_term(key, format::T_SET, exp, members.len() as u64));
        }
        Value::ZSet(z) => {
            let inner = zset_inner(z.iter().map(|(s, m)| (crate::zset::decode_score(s), m)));
            d.add(seq_term(key, format::T_ZSET, exp, inner));
        }
        Value::List(l) => {
            let inner = list_inner(l.iter().map(|b| b.as_slice()));
            d.add(seq_term(key, format::T_LIST, exp, inner));
        }
    }
}

/// Termes d'une valeur de l'arbre, lue en place (ni clone ni `Vec` : le
/// condensé est sur le chemin du reload). ZSet / List : par la valeur
/// matérialisée (rares).
///
/// # Safety
/// `tag` / `val` : valeur réelle de l'arbre.
unsafe fn raw_terms(d: &mut Digest, key: &[u8], exp: Option<u64>, tag: Tag, val: &ValUnion) {
    unsafe {
        match tag {
            Tag::Bytes => {
                let mut h = prefix(key, format::T_STRING, exp);
                part(&mut h, &val.bytes);
                d.add(h);
            }
            Tag::Int => {
                let mut h = prefix(key, format::T_INT, exp);
                part(&mut h, &val.integer.to_le_bytes());
                d.add(h);
            }
            Tag::HashSmall | Tag::Hash => {
                let p = prefix(key, format::T_HASH, exp);
                let mut n = 0u64;
                let mut field = |f: &[u8], v: &[u8]| {
                    let mut h = p.clone();
                    part(&mut h, f);
                    part(&mut h, v);
                    d.add(h);
                    n += 1;
                };
                if tag == Tag::HashSmall {
                    for [f, v] in crate::small::entries::<2>(val.ptr) {
                        field(f, v);
                    }
                } else {
                    crate::value::valart().hash_for_each(val.idx, |f, v| field(f, v));
                }
                d.add(head_term(key, format::T_HASH, exp, n));
            }
            Tag::SetSmall | Tag::Set => {
                let p = prefix(key, format::T_SET, exp);
                let mut n = 0u64;
                let mut member = |m: &[u8]| {
                    let mut h = p.clone();
                    part(&mut h, m);
                    d.add(h);
                    n += 1;
                };
                if tag == Tag::SetSmall {
                    for [m] in crate::small::entries::<1>(val.ptr) {
                        member(m);
                    }
                } else {
                    crate::value::valart().set_for_each(val.idx, |m| member(m));
                }
                d.add(head_term(key, format::T_SET, exp, n));
            }
            Tag::ZSet | Tag::List => value_terms(d, key, exp, &value_from_raw_ref(tag, val)),
            Tag::None => unreachable!("valeur sans tag"),
        }
    }
}

impl OxidArt {
    /// Condensé du contenu (DURABILITY_DESIGN.md §6.4) : toutes les clés non
    /// expirées à `at` (`0` : toutes les clés présentes). Parcours complet,
    /// O(n) : reload et tests seulement.
    pub fn content_digest(&self, at: u64) -> Digest {
        let mut d = Digest::default();
        let mut path = Vec::new();
        self.walk(self.root_idx, &mut path, |k: &[u8], tag: Tag, val: &ValUnion, exp: Option<u64>| {
            if !expired(exp, at) {
                // SAFETY: valeur réelle, tag cohérent (parcours de l'arbre).
                unsafe { raw_terms(&mut d, k, exp, tag, val) };
            }
        });
        d
    }
}
