//! Save progressive, côté oxidart : images T0 des nœuds (SAVE_DESIGN.md §2.A).

use std::mem::ManuallyDrop;

use radixox_lib::save_barrier::SaveBarrier;

use crate::compact_str::CompactStr;
use crate::node_childs::{ChildAble, OverflowArena};
use crate::value::{Tag, ValUnion};
use crate::{ExpAndRadix, Node};

/// Copie T0 d'un nœud oxidart, autonome :
/// - compression copiée en profondeur ;
/// - String : clone de la valeur (rc+1), droppé avec l'image ;
/// - Int : copié ; Hash/Set/ZSet/List : idx copié tel quel (le conteneur
///   n'est pas détruit pendant la save, voir SAVE_DESIGN.md §2.B) ;
/// - enfants (inline + overflow + huge) recopiés dans [`OxidSave::childs`],
///   à `[childs_start, childs_start + childs_len)`.
///
/// Jamais converti en `Node` : le `Drop` de `Node` détruirait le conteneur.
pub(crate) struct NodeImage {
    pub(crate) compression: CompactStr,
    pub(crate) tag: Tag,
    pub(crate) val: ValUnion,
    pub(crate) exp_and_radix: ExpAndRadix,
    pub(crate) childs_start: u32,
    pub(crate) childs_len: u32,
}

impl NodeImage {
    pub(crate) fn capture(node: &Node, overflow: &OverflowArena, childs: &mut Vec<(u8, u32)>) -> Self {
        let start = childs.len();
        childs.extend(node.childs.iter());
        if let Some(oi) = node.get_overflow_idx()
            && let Some(ov) = overflow.get(oi)
        {
            ov.for_each_child(|radix, idx| childs.push((radix, idx)));
        }
        let val = match node.tag {
            Tag::Bytes => ValUnion {
                bytes: ManuallyDrop::new(unsafe { (*node.val.bytes).clone() }),
            },
            // SAFETY: pas de ressource possédée hors Bytes : copie bit à bit.
            _ => unsafe { std::ptr::read(&node.val) },
        };
        Self {
            compression: CompactStr::from_slice(&node.compression),
            tag: node.tag,
            val,
            exp_and_radix: node.exp_and_radix,
            childs_start: start as u32,
            childs_len: (childs.len() - start) as u32,
        }
    }
}

impl Drop for NodeImage {
    fn drop(&mut self) {
        if self.tag == Tag::Bytes {
            unsafe { ManuallyDrop::drop(&mut self.val.bytes) };
        }
    }
}

/// État de save d'un `OxidArt`.
#[derive(Default)]
pub(crate) struct OxidSave {
    pub(crate) barrier: SaveBarrier<NodeImage>,
    /// Enfants des images, aplatis. Vidé à `finish` (capacité gardée).
    pub(crate) childs: Vec<(u8, u32)>,
    /// `now` au begin : une clé expirée à T0 n'est pas écrite.
    pub(crate) t0: u64,
    /// Tâches (UNLINK) en attente de la fin de la save.
    pub(crate) waiters: Vec<std::task::Waker>,
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
                        .map(|(s, m)| (s.into_inner().to_bits(), m.to_vec()))
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

/// Constantes du format de dump.
///
/// ```text
/// en-tête  : "RXDB" | u8 version | u8 unité TTL | u64 LE t0 | u64 LE seed valart
/// section  : varint nb_zsets | (varint idx | varint n | (f64 LE | bytes membre)*)*
///            varint nb_lists | (varint idx | varint n | bytes élément*)*
/// records  : u8 tag | varint lcp | varint len_suffixe | [varint ttl] | suffixe | valeur
///            tag : bits 0-3 = type, bit 7 = TTL présent
///            String → bytes ; Int → varint zigzag ;
///            Hash → varint nb | (bytes champ | bytes valeur)* ;
///            Set → varint nb | bytes membre* ; ZSet/List → varint idx (section)
/// fin      : u8 T_END
/// ```
/// `bytes` = varint len | octets. `lcp` = préfixe commun avec la clé
/// précédente (le DFS écrit chaque sous-arbre d'un bloc).
pub mod format {
    pub const MAGIC: [u8; 4] = *b"RXDB";
    pub const VERSION: u8 = 1;
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
}

use radixox_lib::prefetch::prefetch_l1;
use radixox_lib::save_barrier::T0;
use radixox_lib::varint::{put_bytes, put_varint, zigzag};
use valart::ValSer;

use crate::OxidArt;
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
    done: bool,
}

impl SaveCursor {
    pub fn is_done(&self) -> bool {
        self.done
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
        self.begin_save();
        out.extend_from_slice(&format::MAGIC);
        out.push(format::VERSION);
        out.push(format::TTL_UNIT_SECONDS);
        out.extend_from_slice(&self.save.t0.to_le_bytes());
        out.extend_from_slice(&valart().seed().to_le_bytes());

        let mut n = 0u64;
        for_each_zset(|_, _| n += 1);
        put_varint(out, n);
        for_each_zset(|idx, z| {
            put_varint(out, idx as u64);
            put_varint(out, z.len() as u64);
            for (score, member) in z.iter() {
                out.extend_from_slice(&score.into_inner().to_le_bytes());
                put_bytes(out, member);
            }
        });
        let mut n = 0u64;
        for_each_list(|_, _| n += 1);
        put_varint(out, n);
        for_each_list(|idx, l| {
            put_varint(out, idx as u64);
            put_varint(out, l.len() as u64);
            for item in l {
                put_bytes(out, item);
            }
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
            done: false,
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
            let finished = match cur.container {
                Container::None => true,
                Container::Hash => valart().hash_ser_step(&mut cur.ser, out, &mut budget),
                Container::Set => valart().set_ser_step(&mut cur.ser, out, &mut budget),
            };
            if !finished {
                return false;
            }
            cur.container = Container::None;
            if budget == 0 {
                return false;
            }
            let Some(frame) = cur.stack.pop() else {
                out.push(format::T_END);
                cur.done = true;
                return true;
            };
            self.save_node(cur, frame, out, &mut budget);
        }
    }

    /// Un nœud : `take` T0, prefetch de la valeur, chemin, enfants empilés,
    /// prefetch du nouveau haut de pile, puis écriture de l'enregistrement.
    fn save_node(&mut self, cur: &mut SaveCursor, f: Frame, out: &mut Vec<u8>, budget: &mut u32) {
        let depth = f.depth as usize;
        cur.path.truncate(depth);
        cur.min_trunc = cur.min_trunc.min(depth);
        if f.radix != ROOT_RADIX {
            cur.path.push(f.radix as u8);
        }
        sub(budget, 1);

        let t0 = self.save.t0;
        let OxidArt { map, overflow_arena, save, .. } = self;
        let OxidSave { barrier, childs: img_childs, .. } = save;
        let (tag, val, exp) = match barrier.take(f.idx) {
            Some(T0::Slab) => {
                let n = map.get(f.idx).expect("intact à T0 ⇒ occupé");
                if n.tag == Tag::Bytes {
                    unsafe { n.val.bytes.prefetch() };
                }
                cur.path.extend_from_slice(&n.compression);
                let depth = cur.path.len() as u32;
                for (radix, idx) in n.childs.iter() {
                    cur.stack.push(Frame { idx, depth, radix: radix as u16 });
                }
                if let Some(oi) = n.get_overflow_idx()
                    && let Some(ov) = overflow_arena.get(oi)
                {
                    sub(budget, 1);
                    ov.for_each_child(|radix, idx| {
                        cur.stack.push(Frame { idx, depth, radix: radix as u16 })
                    });
                }
                (n.tag, &n.val, n.exp_and_radix.exp())
            }
            Some(T0::Image(img)) => {
                if img.tag == Tag::Bytes {
                    unsafe { img.val.bytes.prefetch() };
                }
                cur.path.extend_from_slice(&img.compression);
                let depth = cur.path.len() as u32;
                let s = img.childs_start as usize;
                for &(radix, idx) in &img_childs[s..s + img.childs_len as usize] {
                    cur.stack.push(Frame { idx, depth, radix: radix as u16 });
                }
                (img.tag, &img.val, img.exp_and_radix.exp())
            }
            None => panic!("nœud {} absent de la vue T0", f.idx),
        };
        if let Some(top) = cur.stack.last()
            && let Some(next) = map.get(top.idx)
        {
            prefetch_l1(next as *const Node as *const u8);
        }

        if tag == Tag::None || exp.is_some_and(|e| e < t0) {
            return;
        }
        let ty = match tag {
            Tag::Bytes => format::T_STRING,
            Tag::Int => format::T_INT,
            Tag::Hash => format::T_HASH,
            Tag::Set => format::T_SET,
            Tag::ZSet => format::T_ZSET,
            Tag::List => format::T_LIST,
            Tag::None => unreachable!(),
        };
        let lcp = cur.min_trunc.min(cur.prev_len);
        let ttl = exp.map(|e| e - t0);
        out.push(ty | if ttl.is_some() { format::TTL_BIT } else { 0 });
        put_varint(out, lcp as u64);
        put_varint(out, (cur.path.len() - lcp) as u64);
        if let Some(ttl) = ttl {
            put_varint(out, ttl);
        }
        out.extend_from_slice(&cur.path[lcp..]);
        unsafe {
            match tag {
                Tag::Bytes => {
                    let b: &[u8] = &val.bytes;
                    put_bytes(out, b);
                    sub(budget, 1 + (b.len() >> 12) as u32);
                }
                Tag::Int => put_varint(out, zigzag(val.integer)),
                Tag::Hash => {
                    valart().hash_ser_begin(val.idx, &mut cur.ser, out);
                    cur.container = Container::Hash;
                }
                Tag::Set => {
                    valart().set_ser_begin(val.idx, &mut cur.ser, out);
                    cur.container = Container::Set;
                }
                Tag::ZSet | Tag::List => put_varint(out, val.idx as u64),
                Tag::None => unreachable!(),
            }
        }
        cur.prev_len = cur.path.len();
        cur.min_trunc = usize::MAX;
    }
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
    pub entries: Vec<DumpEntry>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DumpError(pub &'static str);

/// Décode un dump complet (en-tête, section ZSet/List, enregistrements).
pub fn parse_dump(buf: &[u8]) -> Result<Dump, DumpError> {
    use std::collections::{HashMap, VecDeque};

    use radixox_lib::shared_byte::SharedByte;
    use radixox_lib::varint::{get_bytes, get_varint, unzigzag};

    use crate::Value;
    use crate::zcommand::InnerZCommand;

    const TRUNC: DumpError = DumpError("dump tronqué");
    let mut pos = 0usize;
    let fixed = |pos: &mut usize, n: usize| -> Result<&[u8], DumpError> {
        let b = buf.get(*pos..*pos + n).ok_or(TRUNC)?;
        *pos += n;
        Ok(b)
    };
    let varint = |pos: &mut usize| get_varint(buf, pos).ok_or(TRUNC);
    let bytes = |pos: &mut usize| get_bytes(buf, pos).map(SharedByte::from_slice).ok_or(TRUNC);

    if fixed(&mut pos, 4)? != format::MAGIC {
        return Err(DumpError("magic invalide"));
    }
    if fixed(&mut pos, 1)?[0] != format::VERSION {
        return Err(DumpError("version inconnue"));
    }
    if fixed(&mut pos, 1)?[0] != format::TTL_UNIT_SECONDS {
        return Err(DumpError("unité de TTL inconnue"));
    }
    let t0 = u64::from_le_bytes(fixed(&mut pos, 8)?.try_into().unwrap());
    let seed = u64::from_le_bytes(fixed(&mut pos, 8)?.try_into().unwrap());

    let mut zsets: HashMap<u32, InnerZCommand> = HashMap::new();
    for _ in 0..varint(&mut pos)? {
        let idx = varint(&mut pos)? as u32;
        let mut z = InnerZCommand::new();
        for _ in 0..varint(&mut pos)? {
            let score = f64::from_le_bytes(fixed(&mut pos, 8)?.try_into().unwrap());
            z.insert(score, bytes(&mut pos)?);
        }
        zsets.insert(idx, z);
    }
    let mut lists: HashMap<u32, VecDeque<SharedByte>> = HashMap::new();
    for _ in 0..varint(&mut pos)? {
        let idx = varint(&mut pos)? as u32;
        let n = varint(&mut pos)?;
        let l = (0..n).map(|_| bytes(&mut pos)).collect::<Result<_, _>>()?;
        lists.insert(idx, l);
    }

    let mut entries = Vec::new();
    let mut key: Vec<u8> = Vec::new();
    loop {
        let tag = fixed(&mut pos, 1)?[0];
        if tag == format::T_END {
            break;
        }
        let lcp = varint(&mut pos)? as usize;
        let suffix_len = varint(&mut pos)? as usize;
        let ttl = if tag & format::TTL_BIT != 0 { Some(varint(&mut pos)?) } else { None };
        if lcp > key.len() {
            return Err(DumpError("lcp plus long que la clé précédente"));
        }
        key.truncate(lcp);
        key.extend_from_slice(fixed(&mut pos, suffix_len)?);
        let value = match tag & format::TYPE_MASK {
            format::T_STRING => Value::String(bytes(&mut pos)?),
            format::T_INT => Value::Int(unzigzag(varint(&mut pos)?)),
            format::T_HASH => {
                let n = varint(&mut pos)?;
                let pairs = (0..n)
                    .map(|_| Ok((bytes(&mut pos)?, bytes(&mut pos)?)))
                    .collect::<Result<_, DumpError>>()?;
                Value::Hash(pairs)
            }
            format::T_SET => {
                let n = varint(&mut pos)?;
                Value::Set((0..n).map(|_| bytes(&mut pos)).collect::<Result<_, _>>()?)
            }
            format::T_ZSET => {
                let idx = varint(&mut pos)? as u32;
                Value::ZSet(zsets.remove(&idx).ok_or(DumpError("ZSet absent de la section"))?)
            }
            format::T_LIST => {
                let idx = varint(&mut pos)? as u32;
                Value::List(lists.remove(&idx).ok_or(DumpError("List absente de la section"))?)
            }
            _ => return Err(DumpError("type inconnu")),
        };
        entries.push(DumpEntry { key: key.clone(), ttl, value });
    }
    if pos != buf.len() {
        return Err(DumpError("octets après le marqueur de fin"));
    }
    Ok(Dump { t0, seed, entries })
}
