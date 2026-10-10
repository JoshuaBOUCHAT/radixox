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
}

#[cfg(test)]
pub(crate) mod reference {
    //! DFS synchrone de la vue T0, pour les tests (le vrai parcours, par
    //! tranches avec prefetch, viendra aux étapes 5 et 6).

    use radixox_lib::save_barrier::T0;

    use crate::node_childs::ChildAble;
    use crate::value::{Tag, Value, valart};
    use crate::{OxidArt, save::OxidSave};

    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum DumpVal {
        Str(Vec<u8>),
        Hash(Vec<(Vec<u8>, Vec<u8>)>),
        Set(Vec<Vec<u8>>),
        Other(&'static str),
    }

    impl DumpVal {
        /// Même forme que le dump pour une valeur lue par `getn` (String et
        /// Int confondus, comme dans le format).
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
                Value::ZSet(_) => Self::Other("zset"),
                Value::List(_) => Self::Other("list"),
            }
        }
    }

    enum Raw {
        None,
        Str(Vec<u8>),
        Int(i64),
        Hash(u32),
        Set(u32),
        Other(&'static str),
    }

    fn raw(tag: Tag, val: &crate::value::ValUnion) -> Raw {
        unsafe {
            match tag {
                Tag::None => Raw::None,
                Tag::Bytes => Raw::Str(val.bytes.to_vec()),
                Tag::Int => Raw::Int(val.integer),
                Tag::Hash => Raw::Hash(val.idx),
                Tag::Set => Raw::Set(val.idx),
                Tag::ZSet => Raw::Other("zset"),
                Tag::List => Raw::Other("list"),
            }
        }
    }

    impl OxidArt {
        /// Toutes les clés de la vue T0 (non expirées à T0), `between` étant
        /// appelé entre deux nœuds pour muter l'arbre.
        pub(crate) fn dump_t0(
            &mut self,
            mut between: impl FnMut(&mut OxidArt),
        ) -> Vec<(Vec<u8>, DumpVal)> {
            let mut out = Vec::new();
            let mut path: Vec<u8> = Vec::new();
            let mut stack: Vec<(u32, usize, Option<u8>)> = vec![(self.root_idx, 0, None)];
            let mut childs: Vec<(u8, u32)> = Vec::new();
            while let Some((idx, depth, radix)) = stack.pop() {
                path.truncate(depth);
                path.extend(radix);
                childs.clear();
                let t0 = self.save.t0;
                let (value, exp) = {
                    let OxidArt { save, map, overflow_arena, .. } = self;
                    let OxidSave { barrier, childs: img_childs, .. } = save;
                    match barrier.take(idx) {
                        Some(T0::Slab) => {
                            let n = map.get(idx).expect("intact ⇒ occupé");
                            path.extend_from_slice(&n.compression);
                            childs.extend(n.childs.iter());
                            if let Some(oi) = n.get_overflow_idx()
                                && let Some(ov) = overflow_arena.get(oi)
                            {
                                ov.for_each_child(|r, i| childs.push((r, i)));
                            }
                            (raw(n.tag, &n.val), n.exp_and_radix.exp())
                        }
                        Some(T0::Image(img)) => {
                            path.extend_from_slice(&img.compression);
                            let s = img.childs_start as usize;
                            childs.extend_from_slice(&img_childs[s..s + img.childs_len as usize]);
                            (raw(img.tag, &img.val), img.exp_and_radix.exp())
                        }
                        None => panic!("nœud {idx} absent de la vue T0"),
                    }
                };
                for &(r, c) in childs.iter() {
                    stack.push((c, path.len(), Some(r)));
                }
                let expired = exp.is_some_and(|e| e < t0);
                if !expired {
                    let v = match value {
                        Raw::None => None,
                        Raw::Str(b) => Some(DumpVal::Str(b)),
                        Raw::Int(n) => Some(DumpVal::Str(n.to_string().into_bytes())),
                        Raw::Hash(root) => {
                            let mut h = Vec::new();
                            valart().hash_for_each_t0(root, |k, v| h.push((k.to_vec(), v.to_vec())));
                            h.sort();
                            Some(DumpVal::Hash(h))
                        }
                        Raw::Set(root) => {
                            let mut s = Vec::new();
                            valart().set_for_each_t0(root, |m| s.push(m.to_vec()));
                            s.sort();
                            Some(DumpVal::Set(s))
                        }
                        Raw::Other(t) => Some(DumpVal::Other(t)),
                    };
                    if let Some(v) = v {
                        out.push((path.clone(), v));
                    }
                }
                between(self);
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }

        /// Snapshot de l'état courant, même forme que `dump_t0`.
        pub(crate) fn snapshot(&self) -> Vec<(Vec<u8>, DumpVal)> {
            let mut out: Vec<_> = self
                .getn(radixox_lib::shared_byte::SharedByte::from_str(""))
                .iter()
                .map(|(k, v)| (k.to_vec(), DumpVal::from_value(v)))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
    }
}
