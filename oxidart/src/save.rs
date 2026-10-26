//! Save progressive, côté oxidart : images T0 des nœuds (SAVE_DESIGN.md §2.A),
//! DFS qui sérialise directement le format binaire (§4, §5) et lecture du
//! dump.
//!
//! # Pipeline d'écriture (SAVE_DESIGN.md §4bis)
//!
//! Un enregistrement est visité (`save_node` : `take` T0, prefetch de la
//! valeur, enfants, en-tête encodé) puis mis en attente dans
//! `SaveCursor::pending` ; il est écrit (`emit_oldest`) `PIPE_DEPTH`
//! enregistrements plus tard, quand sa valeur est en cache. Pour un Hash/Set,
//! la visite intermédiaire prefetch les entrées de la racine (`warm`).
//!
//! Invariants (détail et justification dans SAVE_DESIGN.md §4bis) :
//! - **I1 ordre** : FIFO + conteneur bloquant ⇒ écriture dans l'ordre de
//!   visite ; le lcp est calculé à la visite.
//! - **I2** : aucune mutation de la base pendant une tranche (`save_step`).
//! - **I3** : `PVal::Bytes { owned: false }` (copie sans refcount d'une
//!   valeur du slab) ne vit que pendant la tranche.
//! - **I4** : valeur lue dans une image ⇒ clone immédiat (image droppée au
//!   `take` suivant).
//! - **I5** : `own_pending()` avant **chaque** `return false` de `save_step`.
//! - **I6** : conteneur en attente = idx de racine ; `peek` jusqu'à l'écriture,
//!   `take` seulement dans `*_ser_step`.
//! - **I7** : les prefetch ne lisent que la vue T0 (`peek`).
//! - **I8** : `T_END` seulement si pile, file et conteneur sont vides.
//! - **I9** : jamais de pause au milieu d'un nœud ou d'un enregistrement.
//! - **I10** : dropper un `SaveCursor` entre deux tranches est sûr.

use std::mem::ManuallyDrop;

use radixox_lib::save_barrier::SaveBarrier;

use crate::compact_path::CompactPath;
use crate::node::{EntryRef, Node};
use crate::value::{Tag, ValUnion};

/// Copie possédée d'une valeur pour une image : String clonée (rc+1), le
/// reste copié tel quel (Int, ou idx d'un conteneur qui n'est pas détruit
/// pendant la save, SAVE_DESIGN.md §2.B).
fn image_val(tag: Tag, val: &ValUnion) -> ValUnion {
    match tag {
        Tag::Bytes => ValUnion {
            bytes: ManuallyDrop::new(unsafe { (*val.bytes).clone() }),
        },
        // petit conteneur : le bloc appartient au nœud, copie profonde
        Tag::HashSmall => ValUnion {
            ptr: unsafe { crate::small::deep_clone::<2>(val.ptr) },
        },
        Tag::SetSmall => ValUnion {
            ptr: unsafe { crate::small::deep_clone::<1>(val.ptr) },
        },
        // SAFETY: pas de ressource possédée hors Bytes : copie bit à bit.
        _ => unsafe { std::ptr::read(val) },
    }
}

fn drop_image_val(tag: Tag, val: &mut ValUnion) {
    unsafe {
        match tag {
            Tag::Bytes => ManuallyDrop::drop(&mut val.bytes),
            Tag::HashSmall => crate::small::free::<2>(val.ptr),
            Tag::SetSmall => crate::small::free::<1>(val.ptr),
            _ => {}
        }
    }
}

/// Feuille d'une image (suffixe copié, valeur possédée).
pub(crate) struct ImgLeaf {
    pub(crate) radix: u8,
    pub(crate) tag: Tag,
    pub(crate) val: ValUnion,
    pub(crate) exp: Option<u64>,
    pub(crate) sfx: CompactPath,
}

impl Drop for ImgLeaf {
    fn drop(&mut self) {
        drop_image_val(self.tag, &mut self.val);
    }
}

/// Copie T0 d'un nœud oxidart, autonome :
/// - préfixe copié en profondeur ;
/// - self-val possédée (voir `image_val`) ;
/// - enfants recopiés dans [`OxidSave::childs`], feuilles (suffixe, valeur
///   possédée) dans [`OxidSave::leaves`].
///
/// Jamais converti en `Node` : le `Drop` de `Node` détruirait les conteneurs.
pub(crate) struct NodeImage {
    pub(crate) prefix: CompactPath,
    pub(crate) tag: Tag,
    pub(crate) val: ValUnion,
    pub(crate) exp: Option<u64>,
    pub(crate) childs: (u32, u32),
    pub(crate) leaves: (u32, u32),
}

impl NodeImage {
    pub(crate) fn capture(node: &Node, childs: &mut Vec<(u8, u32)>, leaves: &mut Vec<ImgLeaf>) -> Self {
        let (c0, l0) = (childs.len(), leaves.len());
        for (radix, e) in node.iter() {
            match e {
                EntryRef::Child(idx) => childs.push((radix, idx)),
                EntryRef::Leaf { tag, val, ttl, sfx } => leaves.push(ImgLeaf {
                    radix,
                    tag,
                    val: image_val(tag, val),
                    exp: ttl,
                    sfx: sfx.map_or_else(CompactPath::new, |p| CompactPath::from_slice(p)),
                }),
            }
        }
        Self {
            prefix: CompactPath::from_slice(&node.prefix),
            tag: node.tag,
            val: image_val(node.tag, &node.val),
            exp: node.exp_and_radix.exp(),
            childs: (c0 as u32, (childs.len() - c0) as u32),
            leaves: (l0 as u32, (leaves.len() - l0) as u32),
        }
    }
}

impl Drop for NodeImage {
    fn drop(&mut self) {
        drop_image_val(self.tag, &mut self.val);
    }
}

/// État de save d'un `OxidArt`.
#[derive(Default)]
pub(crate) struct OxidSave {
    pub(crate) barrier: SaveBarrier<NodeImage>,
    /// Enfants des images, aplatis. Vidé à `finish` (capacité gardée).
    pub(crate) childs: Vec<(u8, u32)>,
    /// Feuilles des images, aplaties. Déplacées dans `leftover` à `end_save`.
    pub(crate) leaves: Vec<ImgLeaf>,
    /// Feuilles de la save terminée, libérées par tranches (`drain_deferred`) :
    /// chacune tient une copie de valeur ; les libérer d'un bloc prenait ~9 ms
    /// pour ~480 000 feuilles (1 M clés sous écriture).
    pub(crate) leftover: Vec<ImgLeaf>,
    /// `now` au begin : une clé expirée à T0 n'est pas écrite.
    pub(crate) t0: u64,
}

impl OxidSave {
    /// Fin de save : les enfants (sans destructeur) sont vidés tout de suite,
    /// les feuilles mises de côté pour une libération par tranches.
    pub(crate) fn clear_images(&mut self) {
        self.childs.clear();
        if self.leftover.is_empty() {
            std::mem::swap(&mut self.leaves, &mut self.leftover);
        } else {
            self.leftover.append(&mut self.leaves);
        }
    }

    /// Libère au plus `budget` feuilles mises de côté ; rend le budget restant.
    pub(crate) fn drain_leftover(&mut self, budget: u32) -> u32 {
        let n = (budget as usize).min(self.leftover.len());
        self.leftover.truncate(self.leftover.len() - n);
        budget - n as u32
    }
}

#[cfg(test)]
pub(crate) mod reference {
    //! Outils de test : dump complet décodé, comparé à l'état attendu.

    use radixox_lib::shared_byte::SharedByte;

    use crate::value::Value;
    use crate::{OxidArt, TtlResult, parse_dump};

    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum DumpVal {
        Str(Vec<u8>),
        Hash(Vec<(Vec<u8>, Vec<u8>)>),
        Set(Vec<Vec<u8>>),
        ZSet(Vec<(u64, Vec<u8>)>),
        List(Vec<Vec<u8>>),
    }

    impl DumpVal {
        /// Forme canonique (String et Int confondus, Hash/Set triés).
        pub(crate) fn from_value(v: &Value) -> Self {
            match v {
                Value::String(b) => Self::Str(b.to_vec()),
                Value::Int(n) => Self::Str(n.to_string().into_bytes()),
                Value::Hash(h) => {
                    let mut h: Vec<_> = h.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
                    h.sort();
                    Self::Hash(h)
                }
                Value::Set(s) => {
                    let mut s: Vec<_> = s.iter().map(|m| m.to_vec()).collect();
                    s.sort();
                    Self::Set(s)
                }
                Value::ZSet(z) => Self::ZSet(
                    z.iter()
                        .map(|(s, m)| (crate::zset::decode_score(s).to_bits(), m.to_vec()))
                        .collect(),
                ),
                Value::List(l) => Self::List(l.iter().map(|i| i.to_vec()).collect()),
            }
        }
    }

    pub(crate) type Entries = Vec<(Vec<u8>, DumpVal, Option<u64>)>;

    /// État courant (clé, valeur, TTL restant), trié par clé.
    pub(crate) fn expected(art: &OxidArt) -> Entries {
        let mut out: Vec<_> = art
            .getn(SharedByte::from_str(""))
            .iter()
            .map(|(k, v)| {
                let ttl = match art.get_ttl(k.clone()) {
                    TtlResult::KeyWithTtl(s) => Some(s),
                    _ => None,
                };
                (k.to_vec(), DumpVal::from_value(v), ttl)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Dump décodé, trié par clé.
    pub(crate) fn decoded(buf: &[u8]) -> Entries {
        let dump = parse_dump(buf).expect("dump illisible");
        let mut out: Vec<_> = dump
            .entries
            .iter()
            .map(|e| (e.key.clone(), DumpVal::from_value(&e.value), e.ttl))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Save complète par tranches de `budget`, `between` appelé entre deux
    /// tranches ; renvoie le dump décodé.
    pub(crate) fn save_all(
        art: &mut OxidArt,
        mut budget: impl FnMut() -> u32,
        mut between: impl FnMut(&mut OxidArt),
    ) -> Entries {
        let mut out = Vec::new();
        let mut cur = art.save_start(&mut out);
        while !art.save_step(&mut cur, &mut out, budget()) {
            between(art);
        }
        art.finish_save();
        decoded(&out)
    }
}

// ─── Format (SAVE_DESIGN.md §5) ──────────────────────────────────────────────

/// Position du flux de commandes que contient un dump (DURABILITY_DESIGN.md
/// §6.3) : opaque pour oxidart, écrite dans l'en-tête v2 et rendue au reload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalPos {
    /// Le dump contient exactement les blocs `≤ seq` (0 : aucun flux).
    pub seq: u64,
    /// Octets du flux jusqu'à la fin du bloc `seq`.
    pub offset: u64,
    /// Identifiant d'histoire.
    pub replid: [u8; 20],
}

/// Constantes du format de dump.
///
/// ```text
/// en-tête  : "RXDB" | u8 version | u8 unité TTL | u64 LE t0 | u64 LE seed valart
///            | v2 : u64 LE seq | [u8; 20] replid | u64 LE offset (`JournalPos`)
/// section  : varint nb_zsets | (varint idx | varint n | (f64 LE | bytes membre)*)*
///            varint nb_lists | (varint idx | varint n | bytes élément*)*
/// records  : u8 tag | varint lcp | varint len_suffixe | [varint ttl] | suffixe | valeur
///            tag : bits 0-3 = type, bit 7 = TTL présent
///            String → bytes ; Int → varint zigzag ;
///            Hash → varint nb | (bytes champ | bytes valeur)* ;
///            Set → varint nb | bytes membre* ; ZSet/List → varint idx (section)
/// fin      : u8 T_END
/// pied     : u32 LE crc32 (IEEE) de tout ce qui précède | v2 : u32 LE condensé
///            (`digest.rs`) | "RXDE"   (fichier seulement)
/// ```
/// `bytes` = varint len | octets. `lcp` = préfixe commun avec la clé
/// précédente (le DFS écrit chaque sous-arbre d'un bloc).
pub mod format {
    pub const MAGIC: [u8; 4] = *b"RXDB";
    /// Écrite : 2 (position du journal dans l'en-tête, condensé dans le pied,
    /// DURABILITY_DESIGN.md §6.3). Lues : 1 et 2.
    pub const VERSION: u8 = 2;
    pub const VERSION_1: u8 = 1;
    pub const TTL_UNIT_SECONDS: u8 = 0;
    pub const T_STRING: u8 = 1;
    pub const T_INT: u8 = 2;
    pub const T_HASH: u8 = 3;
    pub const T_SET: u8 = 4;
    pub const T_ZSET: u8 = 5;
    pub const T_LIST: u8 = 6;
    pub const T_END: u8 = 0x0F;
    pub const TYPE_MASK: u8 = 0x0F;
    pub const TTL_BIT: u8 = 0x80;
    /// Pied du fichier, ajouté par l'écrivain après `T_END` : détecte un
    /// fichier tronqué ou corrompu (`check_footer`).
    pub const FOOTER_MAGIC: [u8; 4] = *b"RXDE";
    /// v2 : `u32 crc | u32 condensé | "RXDE"`.
    pub const FOOTER_LEN: usize = 12;
    /// v1 : `u32 crc | "RXDE"`.
    pub const FOOTER_LEN_V1: usize = 8;

    pub fn footer_len(version: u8) -> usize {
        if version == VERSION_1 { FOOTER_LEN_V1 } else { FOOTER_LEN }
    }

    /// `crc` : de tout ce qui précède ; `digest` : [`SaveCursor::digest`](super::SaveCursor::digest).
    pub fn footer(crc: u32, digest: u32) -> [u8; FOOTER_LEN] {
        let mut f = [0u8; FOOTER_LEN];
        f[..4].copy_from_slice(&crc.to_le_bytes());
        f[4..8].copy_from_slice(&digest.to_le_bytes());
        f[8..].copy_from_slice(&FOOTER_MAGIC);
        f
    }
}

use std::collections::VecDeque;

use radixox_lib::prefetch::prefetch_l1;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::save_barrier::T0;
use radixox_lib::varint::{put_bytes, put_varint, zigzag};
use valart::ValSer;

use std::collections::HashMap;

use crate::OxidArt;
use crate::digest::{self, Digest};
use crate::value::{for_each_list, for_each_zset, valart};

const ROOT_RADIX: u16 = u16::MAX;

/// Nœud oxidart à visiter : `depth` = longueur du chemin au parent (après
/// sa compression), `radix` = octet qui y mène (`ROOT_RADIX` pour la racine).
#[derive(Clone, Copy)]
struct Frame {
    idx: u32,
    depth: u32,
    radix: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    None,
    Hash,
    Set,
}

/// Curseur d'une save en cours : un seul DFS, reprenable d'une tranche à
/// l'autre. Entre deux tranches il ne contient que des idx (vue T0 stable
/// grâce à la barrière) et le chemin courant.
pub struct SaveCursor {
    stack: Vec<Frame>,
    /// Clé du nœud courant.
    path: Vec<u8>,
    /// Longueur de la dernière clé écrite.
    prev_len: usize,
    /// Plus petite profondeur de troncature du chemin depuis la dernière clé
    /// écrite : `lcp = min(min_trunc, prev_len)`, exact en O(1) (deux frères
    /// ont des radix différents).
    min_trunc: usize,
    container: Container,
    ser: ValSer,
    /// Enregistrements visités pas encore écrits (au plus `PIPE_DEPTH + 1`),
    /// dans l'ordre du DFS. Voir `PIPE_DEPTH`.
    pending: VecDeque<Pending>,
    /// Buffers d'en-tête recyclés.
    spare: Vec<Vec<u8>>,
    done: bool,
    /// Condensé du contenu écrit (DURABILITY_DESIGN.md §6.4).
    digest: Digest,
    /// Préfixe des termes du conteneur valart en cours.
    ctr_prefix: Option<crc32fast::Hasher>,
    /// Sommes des éléments des ZSet / List de la section, par idx.
    zset_sums: HashMap<u32, (u64, u64)>,
    list_sums: HashMap<u32, (u64, u64)>,
}

impl SaveCursor {
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Condensé de ce qui a été écrit ; complet quand la save est finie.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// Début d'un conteneur valart : terme de tête (cardinal, lu dans `card`,
    /// écrit par `*_ser_begin`), préfixe gardé pour ses entrées.
    fn begin_container_digest(&mut self, key: Option<(crc32fast::Hasher, Vec<u8>, Option<u64>)>, card: &[u8], ty: u8) {
        let (pre, key, exp) = key.expect("clé d'un conteneur valart");
        let mut pos = 0;
        let card = radixox_lib::varint::get_varint(card, &mut pos).expect("cardinal écrit");
        self.digest.add(digest::head_term(&key, ty, exp, card));
        self.ctr_prefix = Some(pre);
    }

    /// Termes des entrées d'un conteneur valart que la dernière étape du
    /// sérialiseur a écrites dans `out[from..]` (des entrées entières).
    fn digest_entries(&mut self, out: &[u8], hash: bool) {
        use radixox_lib::varint::get_bytes;
        let Some(p) = &self.ctr_prefix else { return };
        let mut pos = 0;
        while pos < out.len() {
            let mut h = p.clone();
            let f = get_bytes(out, &mut pos).expect("entrée écrite par le sérialiseur");
            digest::part(&mut h, f);
            if hash {
                let v = get_bytes(out, &mut pos).expect("entrée écrite par le sérialiseur");
                digest::part(&mut h, v);
            }
            self.digest.add(h);
        }
    }

    /// Avant de rendre la main : plus aucun pointeur emprunté au slab dans
    /// les enregistrements en attente.
    fn own_pending(&mut self) {
        for p in self.pending.iter_mut() {
            p.val.own();
        }
    }
}

/// Profondeur du pipeline d'écriture : un enregistrement est écrit
/// `PIPE_DEPTH` enregistrements après sa visite.
/// - visite k : prefetch de la valeur (bloc `SharedByte`, ou racine valart) ;
/// - visite k+1 : conteneur → racine en cache, prefetch de ses entrées ;
/// - visite k+2 : écriture (tout est en cache).
///
/// L'en-tête (tag, lcp, suffixe, TTL) est encodé à la visite : l'ordre des
/// enregistrements reste celui du DFS.
const PIPE_DEPTH: usize = 2;

/// Valeur d'un enregistrement en attente.
enum PVal {
    /// `owned == false` : copie bit à bit d'une `SharedByte` du slab, sans
    /// refcount, valide tant que la tranche n'est pas finie (aucune mutation
    /// pendant une tranche). Rendue possédée (clone) avant une pause.
    Bytes { sb: ManuallyDrop<SharedByte>, owned: bool },
    Int(i64),
    Hash(u32),
    Set(u32),
    /// ZSet/List : idx dans la section.
    Idx(u32),
    /// Valeur déjà encodée (petit Hash/Set : sérialisé à la visite, aucun
    /// pointeur vers le bloc ne survit à la tranche).
    Encoded(Vec<u8>),
}

impl PVal {
    fn own(&mut self) {
        if let PVal::Bytes { sb, owned } = self
            && !*owned
        {
            *sb = ManuallyDrop::new((**sb).clone());
            *owned = true;
        }
    }
}

impl Drop for PVal {
    fn drop(&mut self) {
        if let PVal::Bytes { sb, owned: true } = self {
            unsafe { ManuallyDrop::drop(sb) };
        }
    }
}

struct Pending {
    head: Vec<u8>,
    val: PVal,
    /// Hash / Set valart : clé, type et échéance des termes de ses entrées.
    key: Option<(crc32fast::Hasher, Vec<u8>, Option<u64>)>,
}

/// Visite k+1 d'un conteneur : sa racine est en cache, prefetch des entrées.
#[inline(always)]
fn warm(p: &Pending) {
    match p.val {
        PVal::Hash(root) | PVal::Set(root) => valart().prefetch_root_entries(root),
        _ => {}
    }
}

/// Prefetch de la valeur d'un nœud dès sa lecture : le traitement de ses
/// enfants masque la latence. String : bloc `SharedByte` ; Hash/Set : racine
/// valart.
#[inline(always)]
pub(crate) fn prefetch_value(tag: Tag, val: &ValUnion) {
    unsafe {
        match tag {
            Tag::Bytes => val.bytes.prefetch(),
            Tag::Hash | Tag::Set => valart().prefetch_root(val.idx),
            Tag::HashSmall | Tag::SetSmall => prefetch_l1(val.ptr),
            _ => {}
        }
    }
}

#[inline(always)]
fn sub(budget: &mut u32, n: u32) {
    *budget = budget.saturating_sub(n);
}

impl OxidArt {
    /// Démarre une save : fige la vue T0, écrit l'en-tête et la section
    /// ZSet/List (bloquant, v1) dans `out`. Avancer ensuite avec
    /// [`OxidArt::save_step`] puis terminer par [`OxidArt::finish_save`].
    pub fn save_start(&mut self, out: &mut Vec<u8>) -> SaveCursor {
        self.save_start_at(out, &JournalPos::default())
    }

    /// [`save_start`](Self::save_start), le dump contenant le flux de
    /// commandes jusqu'à `pos` (écrit dans l'en-tête).
    pub fn save_start_at(&mut self, out: &mut Vec<u8>, pos: &JournalPos) -> SaveCursor {
        self.begin_save();
        out.extend_from_slice(&format::MAGIC);
        out.push(format::VERSION);
        out.push(format::TTL_UNIT_SECONDS);
        out.extend_from_slice(&self.save.t0.to_le_bytes());
        out.extend_from_slice(&valart().seed().to_le_bytes());
        out.extend_from_slice(&pos.seq.to_le_bytes());
        out.extend_from_slice(&pos.replid);
        out.extend_from_slice(&pos.offset.to_le_bytes());

        let mut zset_sums = HashMap::new();
        let mut n = 0u64;
        for_each_zset(|_, _| n += 1);
        put_varint(out, n);
        for_each_zset(|idx, z| {
            put_varint(out, idx as u64);
            put_varint(out, z.len() as u64);
            for (score, member) in z.iter() {
                out.extend_from_slice(&crate::zset::decode_score(score).to_le_bytes());
                put_bytes(out, member);
            }
            zset_sums.insert(idx, digest::zset_inner(z.iter().map(|(s, m)| (crate::zset::decode_score(s), m))));
        });
        let mut list_sums = HashMap::new();
        let mut n = 0u64;
        for_each_list(|_, _| n += 1);
        put_varint(out, n);
        for_each_list(|idx, l| {
            put_varint(out, idx as u64);
            put_varint(out, l.len() as u64);
            for item in l {
                put_bytes(out, item);
            }
            list_sums.insert(idx, digest::list_inner(l.iter().map(|b| b.as_slice())));
        });

        SaveCursor {
            stack: vec![Frame {
                idx: self.root_idx,
                depth: 0,
                radix: ROOT_RADIX,
            }],
            path: Vec::new(),
            prev_len: 0,
            min_trunc: usize::MAX,
            container: Container::None,
            ser: ValSer::default(),
            pending: VecDeque::with_capacity(PIPE_DEPTH + 1),
            spare: Vec::new(),
            done: false,
            digest: Digest::default(),
            ctr_prefix: None,
            zset_sums,
            list_sums,
        }
    }

    /// Une tranche de save : écrit dans `out` jusqu'à épuiser `budget`
    /// (1 par nœud, 1 par valeur ou entrée, +1 par 4 KB ; un nœud est
    /// toujours traité en entier). Retourne `true` quand le dump est complet
    /// (marqueur de fin écrit).
    pub fn save_step(&mut self, cur: &mut SaveCursor, out: &mut Vec<u8>, mut budget: u32) -> bool {
        if cur.done {
            return true;
        }
        loop {
            let mark = out.len();
            let finished = match cur.container {
                Container::None => true,
                Container::Hash => valart().hash_ser_step(&mut cur.ser, out, &mut budget),
                Container::Set => valart().set_ser_step(&mut cur.ser, out, &mut budget),
            };
            if cur.container != Container::None {
                cur.digest_entries(&out[mark..], cur.container == Container::Hash);
            }
            if !finished {
                cur.own_pending(); // I5
                return false;
            }
            cur.container = Container::None;
            if budget == 0 {
                cur.own_pending(); // I5
                return false;
            }
            if cur.pending.len() > PIPE_DEPTH {
                if let Some(next) = cur.pending.get(1) {
                    warm(next);
                }
                emit_oldest(cur, out, &mut budget);
                continue;
            }
            match cur.stack.pop() {
                Some(frame) => self.save_node(cur, frame, &mut budget),
                None => {
                    if cur.pending.is_empty() {
                        out.push(format::T_END);
                        cur.done = true;
                        return true;
                    }
                    if let Some(next) = cur.pending.get(1) {
                        warm(next);
                    }
                    emit_oldest(cur, out, &mut budget);
                }
            }
        }
    }

    /// Visite d'un nœud : `take` T0, chemin, puis un enregistrement pour sa
    /// self-val et pour chacune de ses feuilles (valeurs préchargées), enfants
    /// empilés, prefetch du nouveau haut de pile. Le nœud est traité en
    /// entier (I9).
    fn save_node(&mut self, cur: &mut SaveCursor, f: Frame, budget: &mut u32) {
        let depth = f.depth as usize;
        cur.path.truncate(depth);
        cur.min_trunc = cur.min_trunc.min(depth);
        if f.radix != ROOT_RADIX {
            cur.path.push(f.radix as u8);
        }
        sub(budget, 1);

        let t0 = self.save.t0;
        let OxidArt { map, save, .. } = self;
        let OxidSave { barrier, childs: img_childs, leaves: img_leaves, .. } = save;
        match barrier.take(f.idx) {
            Some(T0::Slab) => {
                let n = map.get(f.idx).expect("intact à T0 ⇒ occupé");
                cur.path.extend_from_slice(&n.prefix);
                let node_len = cur.path.len();
                if n.has_val() {
                    prefetch_value(n.tag, &n.val);
                    cur.record(n.tag, &n.val, n.exp_and_radix.exp(), false, t0);
                }
                for (radix, e) in n.iter() {
                    match e {
                        EntryRef::Child(idx) => {
                            cur.stack.push(Frame { idx, depth: node_len as u32, radix: radix as u16 })
                        }
                        EntryRef::Leaf { tag, val, ttl, sfx } => {
                            prefetch_value(tag, val);
                            cur.path.push(radix);
                            if let Some(sfx) = sfx {
                                cur.path.extend_from_slice(sfx);
                            }
                            sub(budget, 1);
                            cur.record(tag, val, ttl, false, t0);
                            cur.path.truncate(node_len);
                            cur.min_trunc = cur.min_trunc.min(node_len);
                        }
                    }
                }
            }
            Some(T0::Image(img)) => {
                cur.path.extend_from_slice(&img.prefix);
                let node_len = cur.path.len();
                if img.tag != Tag::None {
                    prefetch_value(img.tag, &img.val);
                    cur.record(img.tag, &img.val, img.exp, true, t0);
                }
                let (s, n) = (img.childs.0 as usize, img.childs.1 as usize);
                for &(radix, idx) in &img_childs[s..s + n] {
                    cur.stack.push(Frame { idx, depth: node_len as u32, radix: radix as u16 });
                }
                let (s, n) = (img.leaves.0 as usize, img.leaves.1 as usize);
                for l in &img_leaves[s..s + n] {
                    cur.path.push(l.radix);
                    cur.path.extend_from_slice(&l.sfx);
                    sub(budget, 1);
                    cur.record(l.tag, &l.val, l.exp, true, t0);
                    cur.path.truncate(node_len);
                    cur.min_trunc = cur.min_trunc.min(node_len);
                }
            }
            None => panic!("nœud {} absent de la vue T0", f.idx),
        }
        if let Some(top) = cur.stack.last()
            && let Some(next) = map.get(top.idx)
        {
            prefetch_l1(next as *const Node as *const u8);
            next.prefetch_body();
        }
    }
}

impl SaveCursor {
    /// Met en attente l'enregistrement de la clé `self.path` (valeur `tag` /
    /// `val`), sauf si elle est expirée à T0. `from_image` : valeur lue dans
    /// une image, clonée tout de suite (I4).
    fn record(&mut self, tag: Tag, val: &ValUnion, exp: Option<u64>, from_image: bool, t0: u64) {
        if exp.is_some_and(|e| e < t0) {
            return;
        }
        let ty = match tag {
            Tag::Bytes => format::T_STRING,
            Tag::Int => format::T_INT,
            Tag::Hash | Tag::HashSmall => format::T_HASH,
            Tag::Set | Tag::SetSmall => format::T_SET,
            Tag::ZSet => format::T_ZSET,
            Tag::List => format::T_LIST,
            Tag::None => unreachable!(),
        };
        // I1 : lcp calculé à la visite, valide à l'écriture (ordre FIFO).
        let lcp = self.min_trunc.min(self.prev_len);
        let ttl = exp.map(|e| e - t0);
        let mut head = self.spare.pop().unwrap_or_default();
        head.clear();
        head.push(ty | if ttl.is_some() { format::TTL_BIT } else { 0 });
        put_varint(&mut head, lcp as u64);
        put_varint(&mut head, (self.path.len() - lcp) as u64);
        if let Some(ttl) = ttl {
            put_varint(&mut head, ttl);
        }
        head.extend_from_slice(&self.path[lcp..]);
        // Condensé : termes de la clé tant que son chemin est là (§6.4) ;
        // ceux des entrées d'un conteneur valart, à son écriture.
        let mut key = None;
        unsafe {
            let p = || digest::prefix(&self.path, ty, exp);
            match tag {
                Tag::Bytes => {
                    let mut h = p();
                    digest::part(&mut h, &val.bytes);
                    self.digest.add(h);
                }
                Tag::Int => {
                    let mut h = p();
                    digest::part(&mut h, &val.integer.to_le_bytes());
                    self.digest.add(h);
                }
                Tag::HashSmall => {
                    let pairs = crate::small::entries::<2>(val.ptr);
                    let pre = p();
                    for [f, v] in pairs {
                        let mut h = pre.clone();
                        digest::part(&mut h, f);
                        digest::part(&mut h, v);
                        self.digest.add(h);
                    }
                    self.digest.add(digest::head_term(&self.path, ty, exp, pairs.len() as u64));
                }
                Tag::SetSmall => {
                    let members = crate::small::entries::<1>(val.ptr);
                    let pre = p();
                    for [m] in members {
                        let mut h = pre.clone();
                        digest::part(&mut h, m);
                        self.digest.add(h);
                    }
                    self.digest.add(digest::head_term(&self.path, ty, exp, members.len() as u64));
                }
                Tag::Hash | Tag::Set => key = Some((p(), self.path.clone(), exp)),
                Tag::ZSet => {
                    let sums = self.zset_sums.get(&val.idx).copied().unwrap_or_default();
                    self.digest.add(digest::seq_term(&self.path, ty, exp, sums));
                }
                Tag::List => {
                    let sums = self.list_sums.get(&val.idx).copied().unwrap_or_default();
                    self.digest.add(digest::seq_term(&self.path, ty, exp, sums));
                }
                Tag::None => unreachable!(),
            }
        }
        let val = unsafe {
            match tag {
                // I4 : l'image est droppée au prochain `take` : clone possédé.
                Tag::Bytes if from_image => PVal::Bytes {
                    sb: ManuallyDrop::new((*val.bytes).clone()),
                    owned: true,
                },
                // I3 : copie sans refcount, valide jusqu'à la fin de la tranche.
                Tag::Bytes => PVal::Bytes {
                    sb: ManuallyDrop::new(std::ptr::read(&*val.bytes)),
                    owned: false,
                },
                Tag::Int => PVal::Int(val.integer),
                Tag::Hash => PVal::Hash(val.idx),
                Tag::Set => PVal::Set(val.idx),
                Tag::HashSmall => {
                    let pairs = crate::small::entries::<2>(val.ptr);
                    let mut buf = Vec::new();
                    put_varint(&mut buf, pairs.len() as u64);
                    for [f, v] in pairs {
                        put_bytes(&mut buf, f);
                        put_bytes(&mut buf, v);
                    }
                    PVal::Encoded(buf)
                }
                Tag::SetSmall => {
                    let members = crate::small::entries::<1>(val.ptr);
                    let mut buf = Vec::new();
                    put_varint(&mut buf, members.len() as u64);
                    for [m] in members {
                        put_bytes(&mut buf, m);
                    }
                    PVal::Encoded(buf)
                }
                Tag::ZSet | Tag::List => PVal::Idx(val.idx),
                Tag::None => unreachable!(),
            }
        };
        self.pending.push_back(Pending { head, val, key });
        self.prev_len = self.path.len();
        self.min_trunc = usize::MAX;
    }
}

/// Écrit l'enregistrement en attente le plus ancien. Un conteneur démarre son
/// sérialiseur ; `save_step` le termine avant toute autre écriture.
fn emit_oldest(cur: &mut SaveCursor, out: &mut Vec<u8>, budget: &mut u32) {
    let mut p = cur.pending.pop_front().expect("enregistrement en attente");
    out.extend_from_slice(&p.head);
    match &p.val {
        PVal::Bytes { sb, .. } => {
            put_bytes(out, sb);
            sub(budget, 1 + (sb.len() >> 12) as u32);
        }
        PVal::Int(n) => put_varint(out, zigzag(*n)),
        PVal::Hash(root) => {
            let mark = out.len();
            valart().hash_ser_begin(*root, &mut cur.ser, out);
            cur.begin_container_digest(p.key.take(), &out[mark..], format::T_HASH);
            cur.container = Container::Hash;
        }
        PVal::Set(root) => {
            let mark = out.len();
            valart().set_ser_begin(*root, &mut cur.ser, out);
            cur.begin_container_digest(p.key.take(), &out[mark..], format::T_SET);
            cur.container = Container::Set;
        }
        PVal::Idx(idx) => put_varint(out, *idx as u64),
        PVal::Encoded(buf) => {
            out.extend_from_slice(buf);
            sub(budget, 1 + (buf.len() >> 12) as u32);
        }
    }
    cur.spare.push(std::mem::take(&mut p.head));
}

// ─── Lecture d'un dump ───────────────────────────────────────────────────────

/// Une clé lue dans un dump.
#[derive(Debug, Clone, PartialEq)]
pub struct DumpEntry {
    pub key: Vec<u8>,
    /// Secondes restantes au T0 du dump.
    pub ttl: Option<u64>,
    pub value: crate::Value,
}

#[derive(Debug)]
pub struct Dump {
    pub t0: u64,
    pub seed: u64,
    pub pos: JournalPos,
    pub entries: Vec<DumpEntry>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DumpError(pub &'static str);

/// Vérifie le pied d'un fichier de dump et renvoie le flux qui le précède,
/// à passer à `parse_dump`.
pub fn check_footer(file: &[u8]) -> Result<&[u8], DumpError> {
    let len = format::footer_len(file.get(4).copied().unwrap_or(format::VERSION));
    let Some(body_len) = file.len().checked_sub(len) else {
        return Err(DumpError("fichier tronqué (pas de pied)"));
    };
    let (body, foot) = file.split_at(body_len);
    if foot[len - 4..] != format::FOOTER_MAGIC {
        return Err(DumpError("fichier tronqué (pied absent)"));
    }
    if crc32fast::hash(body) != u32::from_le_bytes(foot[..4].try_into().unwrap()) {
        return Err(DumpError("crc invalide"));
    }
    Ok(body)
}

/// Décode un dump complet (en-tête, section ZSet/List, enregistrements).
///
/// Sert aux tests et aux outils ; le reload du serveur passe par
/// [`OxidArt::load_dump`](crate::OxidArt::load_dump), qui construit l'arbre
/// sans passer par des `DumpEntry`.
pub fn parse_dump(buf: &[u8]) -> Result<Dump, DumpError> {
    use crate::load::{DumpReader, Footer, LoadError};
    let parse = || -> Result<Dump, LoadError> {
        let mut rd = DumpReader::new(buf, Footer::None)?;
        let mut entries = Vec::new();
        while let Some(head) = rd.next_head()? {
            let value = rd.value(head.ty)?;
            entries.push(DumpEntry { key: rd.key().to_vec(), ttl: head.ttl, value });
        }
        Ok(Dump { t0: rd.t0, seed: rd.seed, pos: rd.jpos, entries })
    };
    parse().map_err(|e| match e {
        LoadError::Dump(e) => e,
        LoadError::Io(_) => unreachable!("lecture d'une tranche en mémoire"),
    })
}
