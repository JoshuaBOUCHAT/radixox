//! Reload d'un dump (SAVE_DESIGN.md §5 « Contiguïté des préfixes et reload en
//! masse »).
//!
//! # Construction ascendante
//!
//! Le dump est en ordre DFS : chaque sous-arbre est d'un bloc, et chaque clé
//! est codée par `lcp` (préfixe commun avec la précédente) + suffixe. L'arbre
//! se reconstruit en un seul passage, sans recherche ni split, comme un arbre
//! des suffixes à partir d'un tableau LCP :
//!
//! - la pile contient les nœuds **ouverts** du chemin de la dernière clé
//!   gardée, chacun connu par sa profondeur de fin (`end`), sa valeur et ses
//!   enfants déjà construits (dans un `Vec` partagé) ;
//! - une nouvelle clé de lcp `l` **ferme** tous les nœuds ouverts dont
//!   `end > l` : aucune clé suivante ne peut plus passer par eux (en ordre
//!   DFS, `lcp(a, c) = min(lcp(a, b), lcp(b, c))`) ;
//! - un nœud n'est alloué qu'à sa fermeture : son début est alors connu,
//!   `max(fin du nœud ouvert en dessous, l)`, donc sa compression est
//!   définitive. Si `l` tombe au milieu de lui, un nœud de branchement
//!   (sans valeur) est ouvert à la profondeur `l` et devient son parent.
//!
//! Une lecture d'un seul enregistrement d'avance ne suffirait pas à créer les
//! nœuds de haut en bas sans split : avec `abcd, abcx, ab_`, rien n'annonce
//! le branchement à la profondeur 2 quand on crée `abc`.
//!
//! Les nœuds sont alloués en post-ordre (enfants avant parents), à la suite
//! dans le HiSlab : chaque sous-arbre est contigu en mémoire. Le seul retour
//! sur un nœud déjà alloué est l'écriture de `parent_idx` sur ses enfants.
//!
//! # Lecture par fragments
//!
//! [`DumpReader`] lit n'importe quel [`Read`] par morceaux de 1 MB dans une
//! fenêtre réutilisée : seul l'enregistrement en cours doit y tenir (la plus
//! grande valeur l'agrandit). Le crc du pied est calculé au fil de la lecture
//! et vérifié en fin de flux (`OxidArt::load_file`). La compression (§5
//! « Compression ») est un `Read` qui décompresse
//! ([`ZstdReader`](crate::compress::ZstdReader)) : le constructeur ne voit que
//! le flux brut.
//!
//! Clés expirées (expiration `t0 + ttl` < `now`) : sautées. Le chemin
//! construit (`kept`) ne contient que des clés gardées ; le lcp effectif avec
//! la dernière clé gardée est le minimum des lcp lus depuis.

#[allow(unused_imports)]
use crate::prelude::*;
use alloc::collections::VecDeque;
use hashbrown::HashMap;
use core::mem::ManuallyDrop;

use radixox_lib::shared_byte::SharedByte;
use radixox_lib::varint::{get_bytes, get_varint, unzigzag};

use crate::compact_path::CompactPath;
use crate::node::{Entry, Leaf, Node};
use crate::digest::{Digest, value_terms};
use crate::save::{DumpError, JournalPos, format};
use crate::small::{self, SMALL_MAX};
use crate::value::{Cont, RawVal, Tag, ValUnion, alloc_list, alloc_zset, valart};
use crate::zset::{ZSet, encode_score};
use crate::{OxidArt, Value};

const TRUNC: DumpError = DumpError("dump tronqué");

// ─── Lecture du flux ─────────────────────────────────────────────────────────

/// Taille visée d'une lecture dans la source : la fenêtre ne garde que
/// l'enregistrement en cours plus ce qui suit (la plus grande valeur seule
/// peut l'agrandir), jamais le fichier entier.
const READ_CHUNK: usize = 1 << 20;
/// Garde-fou contre une longueur aberrante (dump corrompu sans pied) : une
/// valeur Redis fait au plus 512 MB.
const MAX_LEN: u64 = 1 << 32;

/// Erreur de reload : lecture de la source, ou dump invalide.
#[derive(Debug)]
pub enum LoadError {
    #[cfg(feature = "std")]
    Io(std::io::Error),
    Dump(DumpError),
}

impl From<DumpError> for LoadError {
    fn from(e: DumpError) -> Self {
        Self::Dump(e)
    }
}
#[cfg(feature = "std")]
impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            #[cfg(feature = "std")]
            Self::Io(e) => write!(f, "lecture : {e}"),
            Self::Dump(e) => write!(f, "dump invalide : {}", e.0),
        }
    }
}
impl core::error::Error for LoadError {}

/// Source d'octets du rechargement : tout `std::io::Read` sous `std` (fichier,
/// décompresseur zstd), une tranche en mémoire sans. `Interrupted` est réessayé ici.
pub trait Source {
    /// Comme `Read::read` : 0 à la fin de la source.
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, LoadError>;
}

#[cfg(feature = "std")]
impl<R: std::io::Read> Source for R {
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, LoadError> {
        loop {
            match self.read(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                r => return Ok(r?),
            }
        }
    }
}

#[cfg(not(feature = "std"))]
impl Source for &[u8] {
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, LoadError> {
        let n = buf.len().min(self.len());
        buf[..n].copy_from_slice(&self[..n]);
        *self = &self[n..];
        Ok(n)
    }
}

/// Le flux se termine-t-il par le pied du fichier (crc32 + `RXDE`) ?
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Footer {
    /// Flux brut (en mémoire, tests) : `T_END` puis fin.
    None,
    /// Fichier : `T_END`, pied, fin ; le crc est calculé au fil de la lecture.
    Crc,
}

/// En-tête d'un enregistrement ; la clé complète est dans
/// [`DumpReader::key`], la valeur reste à lire.
pub(crate) struct Head {
    pub(crate) ty: u8,
    pub(crate) ttl: Option<u64>,
    pub(crate) lcp: usize,
}

/// Lecteur séquentiel d'un dump, par fragments : il lit la source par
/// morceaux de [`READ_CHUNK`] dans une fenêtre et n'en garde que le reste non
/// consommé. La source est n'importe quel [`Read`] : fichier, tranche en
/// mémoire, et plus tard décompresseur (le constructeur ne voit que le flux
/// brut). Usage : `next_head`, puis exactement une lecture de valeur.
pub(crate) struct DumpReader<R> {
    src: R,
    /// Fenêtre allouée une fois (agrandie seulement pour une valeur plus
    /// grande) : `buf[pos..end]` reste à lire, `buf[end..]` est libre.
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    eof: bool,
    footer: Footer,
    /// crc des octets déjà sortis de la fenêtre (mode `Footer::Crc`).
    crc: crc32fast::Hasher,
    pub(crate) t0: u64,
    pub(crate) seed: u64,
    pub(crate) version: u8,
    /// Position du flux de commandes (v2 ; défaut en v1).
    pub(crate) jpos: JournalPos,
    /// Condensé du pied (v2, mode `Footer::Crc`), lu après `T_END`.
    pub(crate) footer_digest: Option<u32>,
    zsets: HashMap<u32, ZSet>,
    lists: HashMap<u32, VecDeque<SharedByte>>,
    key: Vec<u8>,
}

impl<R: Source> DumpReader<R> {
    /// Lit l'en-tête et la section ZSet/List.
    pub(crate) fn new(src: R, footer: Footer) -> Result<Self, LoadError> {
        let mut rd = Self {
            src,
            buf: Vec::new(),
            pos: 0,
            end: 0,
            eof: false,
            footer,
            crc: crc32fast::Hasher::new(),
            t0: 0,
            seed: 0,
            version: 0,
            jpos: JournalPos::default(),
            footer_digest: None,
            zsets: HashMap::new(),
            lists: HashMap::new(),
            key: Vec::new(),
        };
        if rd.fixed(4)? != format::MAGIC {
            return Err(DumpError("magic invalide").into());
        }
        rd.version = rd.byte()?;
        if !matches!(rd.version, format::VERSION_1 | format::VERSION_2 | format::VERSION_3 | format::VERSION) {
            return Err(DumpError("version inconnue").into());
        }
        if rd.byte()? != format::TTL_UNIT_SECONDS {
            return Err(DumpError("unité de TTL inconnue").into());
        }
        rd.t0 = rd.u64_le()?;
        rd.seed = rd.u64_le()?;
        if rd.version >= 2 {
            rd.jpos.seq = rd.u64_le()?;
            rd.jpos.replid = rd.fixed(20)?.try_into().unwrap();
            rd.jpos.offset = rd.u64_le()?;
        }

        for _ in 0..rd.varint()? {
            let idx = rd.varint()? as u32;
            let mut z = ZSet::new();
            for _ in 0..rd.varint()? {
                let score = f64::from_bits(rd.u64_le()?);
                if score.is_nan() {
                    return Err(DumpError("score NaN").into());
                }
                let m = rd.bytes()?;
                z.add(encode_score(score), &m).ok_or(DumpError("table des poignées pleine"))?;
            }
            rd.zsets.insert(idx, z);
        }
        for _ in 0..rd.varint()? {
            let idx = rd.varint()? as u32;
            let n = rd.varint()?;
            let l = (0..n).map(|_| rd.bytes()).collect::<Result<_, _>>()?;
            rd.lists.insert(idx, l);
        }
        Ok(rd)
    }

    /// Garantit `n` octets lisibles à `pos`.
    #[inline]
    fn ensure(&mut self, n: usize) -> Result<(), LoadError> {
        if self.end - self.pos >= n { Ok(()) } else { self.refill(n) }
    }

    /// Sort les octets consommés de la fenêtre (crc), ramène le reste au
    /// début, puis lit la source jusqu'à avoir `n` octets.
    #[cold]
    fn refill(&mut self, n: usize) -> Result<(), LoadError> {
        if self.footer == Footer::Crc {
            self.crc.update(&self.buf[..self.pos]);
        }
        self.buf.copy_within(self.pos..self.end, 0);
        self.end -= self.pos;
        self.pos = 0;
        let cap = READ_CHUNK.max(n);
        if self.buf.len() < cap {
            self.buf.resize(cap, 0);
        }
        while self.end < n {
            if self.eof {
                return Err(match self.footer {
                    Footer::Crc => DumpError("fichier tronqué"),
                    Footer::None => TRUNC,
                }
                .into());
            }
            match self.src.read_some(&mut self.buf[self.end..])? {
                0 => self.eof = true,
                k => self.end += k,
            }
        }
        Ok(())
    }

    #[inline]
    fn fixed(&mut self, n: usize) -> Result<&[u8], LoadError> {
        self.ensure(n)?;
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }
    #[inline]
    fn byte(&mut self) -> Result<u8, LoadError> {
        Ok(self.fixed(1)?[0])
    }
    #[inline]
    fn u64_le(&mut self) -> Result<u64, LoadError> {
        Ok(u64::from_le_bytes(self.fixed(8)?.try_into().unwrap()))
    }
    #[inline]
    fn varint(&mut self) -> Result<u64, LoadError> {
        // Chemin rapide : décodage direct dans la fenêtre.
        if let Some(v) = get_varint(&self.buf[..self.end], &mut self.pos) {
            return Ok(v);
        }
        self.varint_slow()
    }
    #[cold]
    fn varint_slow(&mut self) -> Result<u64, LoadError> {
        self.ensure(1)?;
        // Longueur lue dans le premier octet (radixox_lib::varint).
        let extra = self.buf[self.pos].leading_ones() as usize;
        self.ensure(if extra == 8 { 9 } else { 1 + extra })?;
        Ok(get_varint(&self.buf[..self.end], &mut self.pos).expect("octets garantis"))
    }
    #[inline]
    fn len(&mut self) -> Result<usize, LoadError> {
        let n = self.varint()?;
        if n > MAX_LEN {
            return Err(DumpError("longueur aberrante").into());
        }
        Ok(n as usize)
    }
    /// Comme [`bytes`](Self::bytes), emprunté à la fenêtre (valable jusqu'à la lecture
    /// suivante) : pour une donnée recopiée ailleurs, sans `SharedByte` temporaire.
    #[inline]
    fn bytes_ref(&mut self) -> Result<&[u8], LoadError> {
        let mut p = self.pos;
        if let Some(n) = get_bytes(&self.buf[..self.end], &mut p).map(<[u8]>::len) {
            self.pos = p;
            return Ok(&self.buf[p - n..p]);
        }
        let n = self.len()?;
        self.fixed(n)
    }
    #[inline]
    fn bytes(&mut self) -> Result<SharedByte, LoadError> {
        let p0 = self.pos;
        if let Some(b) = get_bytes(&self.buf[..self.end], &mut self.pos) {
            return Ok(SharedByte::from_slice(b));
        }
        self.pos = p0;
        let n = self.len()?;
        Ok(SharedByte::from_slice(self.fixed(n)?))
    }

    /// Enregistrement suivant (`None` au marqueur de fin ; le pied éventuel
    /// et la fin de la source sont vérifiés). Met à jour [`key`](Self::key).
    pub(crate) fn next_head(&mut self) -> Result<Option<Head>, LoadError> {
        let tag = self.byte()?;
        if tag == format::T_END {
            self.finish()?;
            return Ok(None);
        }
        let lcp = self.varint()? as usize;
        let suffix_len = self.len()?;
        let ttl = if tag & format::TTL_BIT != 0 { Some(self.varint()?) } else { None };
        if lcp > self.key.len() {
            return Err(DumpError("lcp plus long que la clé précédente").into());
        }
        self.ensure(suffix_len)?;
        self.key.truncate(lcp);
        self.key.extend_from_slice(&self.buf[self.pos..self.pos + suffix_len]);
        self.pos += suffix_len;
        Ok(Some(Head { ty: tag & format::TYPE_MASK, ttl, lcp }))
    }

    /// Après `T_END` : pied (crc de tout ce qui précède), puis fin de source.
    fn finish(&mut self) -> Result<(), LoadError> {
        if self.footer == Footer::Crc {
            self.crc.update(&self.buf[..self.pos]);
            self.buf.copy_within(self.pos..self.end, 0);
            self.end -= self.pos;
            self.pos = 0;
            let crc = self.crc.clone().finalize();
            // Le pied lui-même n'entre pas dans le crc.
            self.footer = Footer::None;
            let len = format::footer_len(self.version);
            self.ensure(len).map_err(|_| DumpError("fichier tronqué (pied absent)"))?;
            let foot = self.fixed(len)?;
            if foot[len - 4..] != format::FOOTER_MAGIC {
                return Err(DumpError("fichier tronqué (pied absent)").into());
            }
            if crc != u32::from_le_bytes(foot[..4].try_into().unwrap()) {
                return Err(DumpError("crc invalide").into());
            }
            if len == format::FOOTER_LEN {
                self.footer_digest = Some(u32::from_le_bytes(foot[4..8].try_into().unwrap()));
            }
        }
        if self.pos != self.end {
            return Err(DumpError("octets après le marqueur de fin").into());
        }
        while !self.eof {
            let mut probe = [0u8; 1];
            match self.src.read_some(&mut probe)? {
                0 => self.eof = true,
                _ => return Err(DumpError("octets après le marqueur de fin").into()),
            }
        }
        Ok(())
    }

    /// Clé de l'enregistrement courant.
    pub(crate) fn key(&self) -> &[u8] {
        &self.key
    }

    fn zset(&mut self) -> Result<ZSet, LoadError> {
        if self.version >= format::VERSION_3 {
            // v3 : en ligne, dans l'ordre (score, membre) strictement croissant
            let n = self.varint()?;
            if n > crate::zset::small::MAX_MEMBERS as u64 {
                return self.zset_large(n);
            }
            let mut entries: Vec<(u64, SharedByte)> = Vec::new();
            for _ in 0..n {
                let score = f64::from_bits(self.u64_le()?);
                if score.is_nan() {
                    return Err(DumpError("score NaN").into());
                }
                let m = self.bytes()?;
                let s = encode_score(score);
                if entries.last().is_some_and(|(ps, pm)| (*ps, pm.as_slice()) >= (s, m.as_slice())) {
                    return Err(DumpError("ZSet hors d'ordre").into());
                }
                entries.push((s, m));
            }
            return ZSet::from_sorted(entries.iter().map(|(s, m)| (*s, m.as_slice())))
                .ok_or(DumpError("ZSet : membre en double ou table des poignées pleine").into());
        }
        let idx = self.varint()? as u32;
        Ok(self.zsets.remove(&idx).ok_or(DumpError("ZSet absent de la section"))?)
    }
    /// Grande forme (v3, `n` > [`MAX_MEMBERS`](crate::zset::small::MAX_MEMBERS)) : chaque
    /// membre devient sa poignée dès sa lecture, emprunté à la fenêtre, et va droit dans
    /// le B+tree construit au fil de la lecture ; l'index suit, sur les feuilles. Aucune
    /// copie intermédiaire : le pic du rechargement est la taille finale. L'ordre est
    /// vérifié contre le membre précédent, déjà en poignée ; en cas d'erreur, l'arbre
    /// partiel et ses poignées sont rendus.
    fn zset_large(&mut self, n: u64) -> Result<ZSet, LoadError> {
        use crate::zset::btree::BTree;
        let mut err: Option<LoadError> = None;
        let mut prev: Option<(u64, u32)> = None;
        let mut left = n;
        let entries = core::iter::from_fn(|| {
            if left == 0 || err.is_some() {
                return None;
            }
            left -= 1;
            let r = (|| {
                let score = f64::from_bits(self.u64_le()?);
                if score.is_nan() {
                    return Err(DumpError("score NaN").into());
                }
                let s = encode_score(score);
                let m = self.bytes_ref()?;
                if prev.is_some_and(|(ps, ph)| (ps, crate::zset::large::member(ph)) >= (s, m)) {
                    return Err(DumpError("ZSet hors d'ordre").into());
                }
                let h = radixox_lib::handle::new_obj(m).ok_or(DumpError("table des poignées pleine"))?;
                Ok((s, h))
            })();
            match r {
                Ok(e) => {
                    prev = Some(e);
                    Some(e)
                }
                Err(e) => {
                    err = Some(e);
                    None
                }
            }
        });
        let mut tree = BTree::from_sorted(entries);
        if let Some(e) = err {
            for (_, h) in tree.iter() {
                crate::zset::save::free_handle(h);
            }
            tree.free_all();
            return Err(e);
        }
        // from_tree rend lui-même les poignées s'il refuse
        Ok(ZSet::from_tree(tree).ok_or(DumpError("ZSet : membre en double"))?)
    }

    fn list(&mut self) -> Result<VecDeque<SharedByte>, LoadError> {
        if self.version >= format::VERSION {
            // v4 : en ligne, dans l'ordre
            let n = self.varint()?;
            return (0..n).map(|_| self.bytes()).collect::<Result<_, _>>();
        }
        let idx = self.varint()? as u32;
        Ok(self.lists.remove(&idx).ok_or(DumpError("List absente de la section"))?)
    }

    /// Valeur de l'enregistrement courant, en `Value` (tests, outils).
    pub(crate) fn value(&mut self, ty: u8) -> Result<Value, LoadError> {
        Ok(match ty {
            format::T_STRING => Value::String(self.bytes()?),
            format::T_INT => Value::Int(unzigzag(self.varint()?)),
            format::T_HASH => {
                let n = self.varint()?;
                let pairs = (0..n)
                    .map(|_| Ok((self.bytes()?, self.bytes()?)))
                    .collect::<Result<_, LoadError>>()?;
                Value::Hash(pairs)
            }
            format::T_SET => {
                let n = self.varint()?;
                Value::Set((0..n).map(|_| self.bytes()).collect::<Result<_, _>>()?)
            }
            format::T_ZSET => Value::ZSet(self.zset()?),
            format::T_LIST => Value::List(self.list()?),
            _ => return Err(DumpError("type inconnu").into()),
        })
    }

    /// Valeur de l'enregistrement courant, construite directement dans sa
    /// représentation de nœud (Hash/Set insérés dans valart sans `Vec`
    /// intermédiaire).
    fn raw_value(&mut self, ty: u8) -> Result<RawVal, LoadError> {
        Ok(match ty {
            format::T_STRING => RawVal::new(
                Tag::Bytes,
                ValUnion { bytes: ManuallyDrop::new(self.bytes()?) },
            ),
            format::T_INT => RawVal::new(Tag::Int, ValUnion { integer: unzigzag(self.varint()?) }),
            format::T_HASH => {
                let n = self.varint()?;
                if n as usize <= SMALL_MAX {
                    let mut pairs = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        pairs.push([self.bytes()?, self.bytes()?]);
                    }
                    RawVal::new(Tag::HashSmall, ValUnion { ptr: small::from_entries::<2>(pairs.into_iter()) })
                } else {
                    let root = valart().hash_create();
                    // Possédée dès maintenant : libérée si la suite est illisible.
                    let mut raw = RawVal::new(Tag::Hash, ValUnion { cont: Cont { idx: root, card: 0 } });
                    for _ in 0..n {
                        let (field, val) = (self.bytes()?, self.bytes()?);
                        unsafe { raw.val.cont.card += valart().hash_set(root, field, val) as u32 };
                    }
                    raw
                }
            }
            format::T_SET => {
                let n = self.varint()?;
                if n as usize <= SMALL_MAX {
                    let mut members = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        members.push([self.bytes()?]);
                    }
                    RawVal::new(Tag::SetSmall, ValUnion { ptr: small::from_entries::<1>(members.into_iter()) })
                } else {
                    let root = valart().set_create();
                    let mut raw = RawVal::new(Tag::Set, ValUnion { cont: Cont { idx: root, card: 0 } });
                    for _ in 0..n {
                        let member = self.bytes()?;
                        unsafe { raw.val.cont.card += valart().set_add(root, member) as u32 };
                    }
                    raw
                }
            }
            format::T_ZSET => {
                let z = self.zset()?;
                RawVal::new(Tag::ZSet, ValUnion { idx: alloc_zset(z) })
            }
            format::T_LIST => {
                let l = self.list()?;
                RawVal::new(Tag::List, ValUnion { idx: alloc_list(l) })
            }
            _ => return Err(DumpError("type inconnu").into()),
        })
    }
}

// ─── Construction ────────────────────────────────────────────────────────────

/// Bilan d'un reload.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoadStats {
    /// Clés chargées.
    pub keys: u64,
    /// Clés expirées entre la save et le reload, sautées.
    pub expired: u64,
    /// T0 du dump.
    pub t0: u64,
    /// Position du flux de commandes contenue (v2 ; défaut en v1).
    pub pos: JournalPos,
    /// Le condensé du pied (v2) a été vérifié sur l'arbre rechargé.
    pub digest_checked: bool,
}

/// Nœud ouvert : chemin `kept[..end]`, entrées dans `childs[childs_start..]`.
struct Open {
    end: usize,
    val: Option<(RawVal, Option<u64>)>,
    childs_start: usize,
}

/// Entrée construite, en attente de son nœud parent.
type Built = (u8, Entry);

impl OxidArt {
    /// Charge un fichier de dump (flux + pied) depuis n'importe quelle
    /// source [`Read`], par fragments : seule une fenêtre de lecture est en
    /// mémoire, et le crc du pied est vérifié à la fin. Voir
    /// [`load_dump`](Self::load_dump) pour le reste.
    ///
    /// Le fichier peut être brut ou compressé (une frame zstd, reconnue à son
    /// magic) : il est alors décompressé au fil de la lecture.
    #[cfg(feature = "std")]
    pub fn load_file<R: std::io::Read>(&mut self, mut src: R) -> Result<LoadStats, LoadError> {
        use std::io::Read;
        self.check_empty()?;
        let mut head = [0u8; 4];
        let mut got = 0;
        while got < head.len() {
            match src.read(&mut head[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let src = std::io::Cursor::new(head).take(got as u64).chain(src);
        if got == head.len() && head == crate::compress::ZSTD_MAGIC {
            self.load_from(DumpReader::new(crate::compress::ZstdReader::new(src), Footer::Crc)?)
        } else {
            self.load_from(DumpReader::new(src, Footer::Crc)?)
        }
    }

    /// Charge un dump (flux brut, sans pied) dans un arbre vide, en un seul
    /// passage (voir le module). Les clés dont l'expiration (`t0 + ttl`) est
    /// passée à `self.now` sont sautées : appeler `tick()` / `set_now()` avant.
    ///
    /// En cas d'erreur, le contenu de l'arbre est indéterminé : le jeter.
    pub fn load_dump(&mut self, buf: &[u8]) -> Result<LoadStats, LoadError> {
        self.check_empty()?;
        self.load_from(DumpReader::new(buf, Footer::None)?)
    }

    fn check_empty(&self) -> Result<(), DumpError> {
        assert!(!self.is_saving(), "reload pendant une save");
        let root = self.node(self.root_idx);
        if root.has_val() || root.len() > 0 {
            return Err(DumpError("arbre non vide"));
        }
        Ok(())
    }

    fn load_from<R: Source>(&mut self, mut rd: DumpReader<R>) -> Result<LoadStats, LoadError> {
        let now = self.now;
        let mut stats = LoadStats::default();
        // Termes des clés sautées (expirées depuis la save), pour le condensé.
        let mut skipped = Digest::default();

        let mut kept: Vec<u8> = Vec::new();
        let mut stack = vec![Open { end: 0, val: None, childs_start: 0 }];
        let mut childs: Vec<Built> = Vec::new();
        // lcp avec la dernière clé gardée = min des lcp lus depuis.
        let mut lcp = usize::MAX;

        while let Some(head) = rd.next_head()? {
            lcp = lcp.min(head.lcp);
            let exp = match head.ttl {
                Some(ttl) => {
                    let exp = rd.t0.saturating_add(ttl);
                    if exp < now {
                        let v = rd.value(head.ty)?;
                        value_terms(&mut skipped, rd.key(), Some(exp), &v);
                        stats.expired += 1;
                        continue;
                    }
                    Some(exp)
                }
                None => None,
            };
            let val = rd.raw_value(head.ty)?;
            let l = lcp.min(kept.len());
            lcp = usize::MAX;

            self.load_close(&mut stack, &mut childs, &kept, l);
            kept.truncate(l);
            kept.extend_from_slice(&rd.key()[l..]);

            let top = stack.last_mut().expect("racine toujours ouverte");
            if top.end == kept.len() {
                // Racine (clé vide) ou nœud de branchement : impossible pour
                // un autre nœud en ordre DFS (un préfixe précède ses suites).
                if top.val.is_some() {
                    return Err(DumpError("clé en double").into());
                }
                top.val = Some((val, exp));
            } else {
                stack.push(Open { end: kept.len(), val: Some((val, exp)), childs_start: childs.len() });
            }
            stats.keys += 1;
        }
        self.load_close(&mut stack, &mut childs, &kept, 0);

        // La racine existe déjà : on y pose sa valeur et ses entrées.
        let root = stack.pop().expect("racine");
        debug_assert!(stack.is_empty() && root.childs_start == 0);
        let root_idx = self.root_idx;
        if let Some((val, exp)) = root.val {
            let (tag, val) = val.into_parts();
            let node = self.node_mut(root_idx);
            node.tag = tag;
            node.val = val;
            if let Some(exp) = exp {
                node.exp_and_radix.set_exp(exp);
            }
        }
        let kids: Vec<(u8, u32)> = childs
            .iter()
            .filter_map(|(r, e)| match e {
                Entry::Child(c) => Some((*r, *c)),
                Entry::Leaf(_) => None,
            })
            .collect();
        let node = self.node_mut(root_idx);
        for (radix, entry) in childs.drain(..) {
            node.insert(radix, entry);
        }
        for (radix, child) in kids {
            self.set_parent(child, root_idx, radix);
        }
        if self.node(root_idx).has_any_ttl() {
            self.map.tag(root_idx);
        }
        self.hint_containers(root_idx);
        self.key_count += stats.keys as usize;
        stats.t0 = rd.t0;
        stats.pos = rd.jpos;
        // Condensé (DURABILITY_DESIGN.md §6.4) : parcours de l'arbre construit,
        // un autre code que le DFS de la save qui l'a calculé.
        if let Some(want) = rd.footer_digest {
            let mut got = self.content_digest(0);
            got.merge(skipped);
            if got.value() != want {
                return Err(DumpError("condensé du contenu invalide : l'arbre rechargé diffère de l'arbre sauvé").into());
            }
            stats.digest_checked = true;
        }
        Ok(stats)
    }

    /// Ferme les nœuds ouverts au-delà de la profondeur `l` (voir le module).
    fn load_close(&mut self, stack: &mut Vec<Open>, childs: &mut Vec<Built>, kept: &[u8], l: usize) {
        while stack.last().expect("racine").end > l {
            let p = stack.pop().expect("au-dessus de la racine");
            let below = stack.last().expect("racine").end;
            let start = below.max(l);
            let entry = if childs.len() == p.childs_start {
                // aucune clé ne prolonge p : feuille inline de son parent
                let (val, exp) = p.val.expect("nœud ouvert sans valeur ni enfant");
                let (tag, val) = val.into_parts();
                let sfx = &kept[start + 1..p.end];
                Entry::Leaf(Leaf {
                    tag,
                    val,
                    ttl: exp,
                    sfx: (!sfx.is_empty()).then(|| CompactPath::from_slice(sfx)),
                })
            } else {
                Entry::Child(self.load_node(p, start, kept, childs))
            };
            if below < l {
                // `l` coupe p : nœud de branchement, parent de p.
                stack.push(Open { end: l, val: None, childs_start: childs.len() });
            }
            childs.push((kept[start], entry));
        }
    }

    /// Alloue le nœud fermé `p`, qui commence à `start` (son radix est
    /// `kept[start]`), avec ses entrées.
    fn load_node(&mut self, p: Open, start: usize, kept: &[u8], childs: &mut Vec<Built>) -> u32 {
        let radix = kept[start];
        let mut node = Node::new(CompactPath::from_slice(&kept[start + 1..p.end]), u32::MAX, radix);
        if let Some((val, exp)) = p.val {
            let (tag, val) = val.into_parts();
            node.tag = tag;
            node.val = val;
            if let Some(exp) = exp {
                node.exp_and_radix.set_exp(exp);
            }
        }
        debug_assert!(
            node.has_val() || childs.len() - p.childs_start >= 2,
            "nœud sans valeur à une seule entrée"
        );
        let mut kids: radixox_lib::small_vec::SmallVec<16, (u8, u32)> = radixox_lib::small_vec::SmallVec::new();
        for (r, entry) in childs.drain(p.childs_start..) {
            if let Entry::Child(c) = entry {
                kids.push((r, c));
            }
            node.insert(r, entry);
        }
        let idx = if node.has_any_ttl() { self.map.insert_tagged(node) } else { self.map.insert(node) };
        for (r, c) in kids {
            self.set_parent(c, idx, r);
        }
        self.hint_containers(idx);
        idx
    }
}
