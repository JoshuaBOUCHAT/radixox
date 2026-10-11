//! Préchargement par lots (expérimental) : superposer les miss de K descentes.
//!
//! Une descente suit une chaîne de lectures dépendantes (nœud → enfant →
//! overflow → valeur) : seule, elle attend chaque miss l'un après l'autre.
//! `prefetch_keys` fait avancer K clés d'un étage par tour : chaque clé lit
//! son nœud courant, préchargé au tour précédent, et précharge le suivant.
//! Les K miss d'un tour sont donc en vol en même temps.
//!
//! Rien n'est gardé ni écrit (pas de TTL, pas de suppression d'expiré) : les
//! commandes refont ensuite leur descente normale, dans un cache chaud. Un
//! résultat faux (clé créée ou supprimée entre-temps) ne coûte qu'un miss.

use radixox_lib::prefetch::prefetch_l1;

use crate::node_childs::ChildAble;
use crate::save::prefetch_value;
use crate::{CompResult, OxidArt};

/// Nombre maximal de clés suivies ensemble.
pub const PREFETCH_BATCH: usize = 64;

#[derive(Clone, Copy)]
enum Step {
    /// Nœud courant en cache : chercher l'enfant de `key[cursor]`.
    Find,
    /// Bloc d'overflow préchargé : y chercher l'enfant.
    Overflow(u32),
    /// Nœud courant préchargé : comparer sa compression.
    Cmp,
    Done,
}

impl OxidArt {
    /// Précharge les chemins de `keys` (au plus [`PREFETCH_BATCH`], le reste
    /// est ignoré) et, pour les clés trouvées, le début de leur valeur.
    pub fn prefetch_keys(&self, keys: &[&[u8]]) {
        let n = keys.len().min(PREFETCH_BATCH);
        let mut idx = [self.root_idx; PREFETCH_BATCH];
        let mut cursor = [0usize; PREFETCH_BATCH];
        let mut step = [Step::Find; PREFETCH_BATCH];
        let mut active = 0;
        for i in 0..n {
            if keys[i].is_empty() {
                step[i] = Step::Done;
            } else {
                active += 1;
            }
        }
        while active > 0 {
            for i in 0..n {
                let key = keys[i];
                let next = match step[i] {
                    Step::Done => continue,
                    Step::Find => self.prefetch_child(key, &mut idx[i], &mut cursor[i]),
                    Step::Overflow(o) => match self
                        .overflow_arena
                        .get(o)
                        .and_then(|ov| ov.find(key[cursor[i]]))
                    {
                        Some(child) => self.goto_child(child, &mut idx[i], &mut cursor[i]),
                        None => Step::Done,
                    },
                    Step::Cmp => {
                        let node = self.child_node(idx[i]);
                        match node.compare_compression_key(&key[cursor[i]..]) {
                            CompResult::Final => {
                                prefetch_value(node.tag, &node.val);
                                Step::Done
                            }
                            CompResult::Partial(_) => Step::Done,
                            CompResult::Path => {
                                cursor[i] += node.compression.len();
                                // Nœud déjà en cache : on enchaîne sur l'enfant.
                                self.prefetch_child(key, &mut idx[i], &mut cursor[i])
                            }
                        }
                    }
                };
                if matches!(next, Step::Done) {
                    active -= 1;
                }
                step[i] = next;
            }
        }
    }

    /// Cherche l'enfant de `key[*cursor]` dans le nœud `*idx` (en cache) :
    /// inline → précharge l'enfant ; sinon précharge le bloc d'overflow.
    #[inline(always)]
    fn prefetch_child(&self, key: &[u8], idx: &mut u32, cursor: &mut usize) -> Step {
        let Some(&radix) = key.get(*cursor) else {
            return Step::Done;
        };
        let node = self.child_node(*idx);
        if let Some(child) = node.childs.find(radix) {
            return self.goto_child(child, idx, cursor);
        }
        match node.get_overflow_idx() {
            Some(o) => {
                self.overflow_arena.prefetch(o);
                Step::Overflow(o)
            }
            None => Step::Done,
        }
    }

    #[inline(always)]
    fn goto_child(&self, child: u32, idx: &mut u32, cursor: &mut usize) -> Step {
        *idx = child;
        *cursor += 1;
        prefetch_l1(self.child_node(child) as *const _ as *const u8);
        Step::Cmp
    }
}

#[cfg(test)]
mod tests {
    use radixox_lib::shared_byte::SharedByte;

    use crate::OxidArt;
    use crate::value::Value;

    fn val(s: &str) -> Value {
        Value::String(SharedByte::from_str(s))
    }

    /// Formes variées (overflow léger et huge, compressions longues sur le
    /// tas, préfixes, clé vide, absentes) : pas de panique, arbre inchangé.
    #[test]
    fn prefetch_keys_shapes_leave_tree_intact() {
        let mut art = OxidArt::new();
        let mut keys: Vec<String> = Vec::new();
        for i in 0..2000 {
            keys.push(format!("user:{i}"));
        }
        for c in 33u8..127 {
            keys.push(format!("k{}", c as char));
        }
        keys.push("a-very-long-compressed-path-well-beyond-seven-bytes:1".into());
        keys.push("a-very-long-compressed-path-well-beyond-seven-bytes:2".into());
        keys.push("ab".into());
        for k in &keys {
            art.set(SharedByte::from_str(k), val(k));
        }
        let mut probes: Vec<String> = keys.clone();
        probes.extend(["", "missing", "user:", "user:99999", "a-very-long", "a-very-long-compressed-path-x"].map(String::from));
        for chunk in probes.chunks(crate::PREFETCH_BATCH + 3) {
            let refs: Vec<&[u8]> = chunk.iter().map(|s| s.as_bytes()).collect();
            art.prefetch_keys(&refs);
        }
        for k in &keys {
            assert_eq!(art.get(k.as_bytes()).and_then(|v| v.as_bytes()).as_deref(), Some(k.as_bytes()));
        }
        assert!(art.get(b"missing").is_none());
    }
}
