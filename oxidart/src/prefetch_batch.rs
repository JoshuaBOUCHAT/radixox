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
//!
//! Les commandes à plusieurs clés en lecture (MGET, EXISTS) utilisent
//! directement le nœud trouvé par la descente entrelacée (`descend_batch`) :
//! pas de seconde descente.

use radixox_lib::prefetch::prefetch_l1;

use crate::krange::{OwnedVal, RangeVal, str_ref};
use crate::node_childs::ChildAble;
use crate::save::prefetch_value;
use crate::value::Tag;
use crate::{CompResult, OxidArt};

/// Nombre maximal de clés suivies ensemble.
pub const PREFETCH_BATCH: usize = 64;

const NOT_FOUND: u32 = u32::MAX;

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
        let mut found = [NOT_FOUND; PREFETCH_BATCH];
        let n = keys.len().min(PREFETCH_BATCH);
        self.descend_batch(&keys[..n], &mut found[..n]);
    }

    /// Descente entrelacée de `keys` (≤ [`PREFETCH_BATCH`]) : `found[i]` reçoit
    /// le nœud de `keys[i]` ([`NOT_FOUND`] sinon), dont la valeur est préchargée.
    /// Le nœud peut être expiré ou sans valeur : à l'appelant de vérifier.
    fn descend_batch(&self, keys: &[&[u8]], found: &mut [u32]) {
        debug_assert!(keys.len() <= PREFETCH_BATCH && found.len() == keys.len());
        let n = keys.len();
        if n == 1 {
            // Rien à entrelacer : descente directe, sans initialiser les états
            // des 64 voies (~1 Ko de pile, +20 % sur un MGET d'une clé).
            let key = keys[0];
            found[0] = if key.is_empty() { Some(self.root_idx) } else { self.traverse_to_key(key) }
                .unwrap_or(NOT_FOUND);
            return;
        }
        let mut idx = [self.root_idx; PREFETCH_BATCH];
        let mut cursor = [0usize; PREFETCH_BATCH];
        let mut step = [Step::Find; PREFETCH_BATCH];
        let mut active = 0;
        for i in 0..n {
            found[i] = NOT_FOUND;
            if keys[i].is_empty() {
                found[i] = self.root_idx;
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
                                found[i] = idx[i];
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

// ── Lectures à plusieurs clés ────────────────────────────────────────────────

const _: () = {
    assert!(size_of::<Option<RangeVal<'static>>>() == size_of::<Option<OwnedVal>>());
    assert!(align_of::<Option<RangeVal<'static>>>() == align_of::<Option<OwnedVal>>());
};

impl OxidArt {
    /// MGET par références : la valeur string de chaque clé, dans l'ordre
    /// (`None` : absente, expirée ou pas une string, comme Redis).
    ///
    /// Descentes entrelacées par paquets de [`PREFETCH_BATCH`] (les miss des
    /// clés se chevauchent), valeurs préchargées, aucune seconde descente.
    /// Lecture pure : rien n'est cloné (pas de refcount) ni supprimé. Une clé
    /// expirée est vue absente et sa position ajoutée à `expired` : après avoir
    /// relâché les références, [`purge_expired`](Self::purge_expired) la
    /// supprime (suppression à la volée, comme GET).
    pub fn mget_ref<'a>(&'a self, keys: &[&[u8]], expired: &mut Vec<u32>) -> Vec<Option<RangeVal<'a>>> {
        let mut out = Vec::with_capacity(keys.len());
        let mut found = [NOT_FOUND; PREFETCH_BATCH];
        for (c, chunk) in keys.chunks(PREFETCH_BATCH).enumerate() {
            let found = &mut found[..chunk.len()];
            self.descend_batch(chunk, found);
            for (i, &idx) in found.iter().enumerate() {
                if idx == NOT_FOUND {
                    out.push(None);
                    continue;
                }
                let node = self.child_node(idx);
                if node.tag != Tag::None && node.is_expired(self.now) {
                    expired.push((c * PREFETCH_BATCH + i) as u32);
                }
                out.push(str_ref(node, self.now));
            }
        }
        out
    }

    /// Supprime les clés expirées repérées par [`mget_ref`](Self::mget_ref) ou
    /// [`exists_count`](Self::exists_count) (`expired` = positions dans `keys`).
    /// Revérifie chaque clé (elle a pu être réécrite entre-temps) ; pendant une
    /// save, ne supprime rien (comme GET).
    pub fn purge_expired(&mut self, keys: &[&[u8]], expired: &[u32]) {
        for &pos in expired {
            let _ = self.get_idx(keys[pos as usize]);
        }
    }

    /// Comme [`mget_ref`](Self::mget_ref), en valeurs possédées (refcount +1)
    /// qui survivent à l'emprunt de l'arbre. Conversion sur place, sans
    /// nouvelle allocation ; les clés expirées croisées sont supprimées.
    pub fn mget_owned(&mut self, keys: &[&[u8]]) -> Vec<Option<OwnedVal>> {
        let mut expired = Vec::new();
        let owned = Self::to_owned_vals(self.mget_ref(keys, &mut expired));
        if !expired.is_empty() {
            self.purge_expired(keys, &expired);
        }
        owned
    }

    fn to_owned_vals(v: Vec<Option<RangeVal<'_>>>) -> Vec<Option<OwnedVal>> {
        let v = std::mem::ManuallyDrop::new(v);
        let (ptr, len, cap) = (v.as_ptr() as *mut Option<RangeVal<'_>>, v.len(), v.capacity());
        let dst = ptr as *mut Option<OwnedVal>;
        for i in 0..len {
            // SAFETY: même schéma que `RangeOut::into_owned` : case i lue en
            // ancien type puis réécrite en nouveau type de même taille/alignement
            // (const assert) ; clone = refcount +1, sans panique possible.
            unsafe {
                let owned = match ptr.add(i).read() {
                    None => None,
                    Some(RangeVal::Bytes(b)) => Some(OwnedVal::Bytes(b.clone())),
                    Some(RangeVal::Int(n)) => Some(OwnedVal::Int(n)),
                };
                dst.add(i).write(owned);
            }
        }
        // SAFETY: même allocation, `len` cases converties.
        unsafe { Vec::from_raw_parts(dst, len, cap) }
    }

    /// EXISTS : nombre de `keys` présentes et non expirées (tout type ; une clé
    /// répétée compte autant de fois). Descentes entrelacées, lecture pure ;
    /// les positions des clés expirées vont dans `expired` (voir
    /// [`purge_expired`](Self::purge_expired)).
    pub fn exists_count(&self, keys: &[&[u8]], expired: &mut Vec<u32>) -> usize {
        let mut found = [NOT_FOUND; PREFETCH_BATCH];
        let mut n = 0;
        for (c, chunk) in keys.chunks(PREFETCH_BATCH).enumerate() {
            let found = &mut found[..chunk.len()];
            self.descend_batch(chunk, found);
            for (i, &idx) in found.iter().enumerate() {
                if idx == NOT_FOUND {
                    continue;
                }
                let node = self.child_node(idx);
                if node.tag != Tag::None && node.is_expired(self.now) {
                    expired.push((c * PREFETCH_BATCH + i) as u32);
                } else if node.get_value_ref(self.now).is_some() {
                    n += 1;
                }
            }
        }
        n
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

    /// mget_ref / mget_owned / exists_count == get clé par clé : absentes,
    /// expirées, hash (non string), Int, clé vide, doublons, > PREFETCH_BATCH.
    #[test]
    fn multi_key_reads_match_get() {
        use crate::krange::{OwnedVal, RangeVal};
        let mut art = OxidArt::new();
        art.set_now(1_000);
        for i in 0..300 {
            let k = format!("user:{i}");
            match i % 6 {
                0 => art.set(SharedByte::from_str(&k), Value::Int(i)),
                1 => {
                    let _ = art.cmd_hset(k.as_bytes(), &[(SharedByte::from_str("f"), SharedByte::from_str("v"))], None);
                }
                2 => art.set_ttl(SharedByte::from_str(&k), std::time::Duration::from_secs(5), val(&k)),
                _ => art.set(SharedByte::from_str(&k), val(&k)),
            }
        }
        art.set(SharedByte::from_str(""), val("root"));
        art.set_now(1_000 + 10_000); // les clés à TTL de 5 s sont expirées
        let mut probes: Vec<String> = (0..330).map(|i| format!("user:{i}")).collect();
        probes.extend(["", "user:", "user:7", "nope"].map(String::from));
        let refs: Vec<&[u8]> = probes.iter().map(|s| s.as_bytes()).collect();

        let want: Vec<Option<Vec<u8>>> = refs
            .iter()
            .map(|k| art.clone_get_for_test(k))
            .collect();
        assert_eq!(want[2], None, "user:2 (TTL) doit être expirée");
        assert_eq!(want[1], None, "user:1 est un hash");
        assert_eq!(want[3].as_deref(), Some(&b"user:3"[..]));
        assert_eq!(want[0].as_deref(), Some(&b"0"[..]));
        assert_eq!(want[330].as_deref(), Some(&b"root"[..]));
        let mut expired = Vec::new();
        let got: Vec<Option<Vec<u8>>> = art
            .mget_ref(&refs, &mut expired)
            .into_iter()
            .map(|v| v.map(|v| match v {
                RangeVal::Bytes(b) => b.as_slice().to_vec(),
                RangeVal::Int(n) => n.to_string().into_bytes(),
            }))
            .collect();
        assert_eq!(got, want);
        let want_expired: Vec<u32> = (0..330).filter(|i| i % 6 == 2 && *i < 300).collect();
        assert_eq!(expired, want_expired);
        let mut expired2 = Vec::new();
        let exists = refs.iter().filter(|k| art.exists_for_test(k)).count();
        assert_eq!(art.exists_count(&refs, &mut expired2), exists);
        assert_eq!(expired2, want_expired);
        let nodes_before = art.node_count();
        let owned: Vec<Option<Vec<u8>>> = art
            .mget_owned(&refs)
            .into_iter()
            .map(|v| v.map(|v| match v {
                OwnedVal::Bytes(b) => b.as_slice().to_vec(),
                OwnedVal::Int(n) => n.to_string().into_bytes(),
            }))
            .collect();
        assert_eq!(owned, want);
        // mget_owned a supprimé les 50 valeurs expirées croisées (les nœuds
        // préfixes d'autres clés restent, sans valeur)
        assert!(art.node_count() < nodes_before);
        let mut expired3 = Vec::new();
        art.mget_ref(&refs, &mut expired3);
        assert!(expired3.is_empty(), "plus aucune expirée : {expired3:?}");
        assert_eq!(art.mget_owned(&refs).len(), refs.len());
    }
}

#[cfg(test)]
mod purge_tests {
    use std::collections::BTreeMap;

    use radixox_lib::shared_byte::SharedByte;

    use crate::OxidArt;
    use crate::value::{Tag, Value};

    impl OxidArt {
        /// Forme canonique : hors racine, aucun nœud sans valeur à 0 enfant
        /// (aurait dû être supprimé) ou à 1 enfant (aurait dû être absorbé) ;
        /// chaque enfant pointe vers son parent avec le bon radix.
        pub(crate) fn assert_canonical(&self) -> usize {
            let mut stack = vec![self.root_idx];
            let mut seen = 0;
            while let Some(idx) = stack.pop() {
                seen += 1;
                let node = self.get_node(idx);
                let mut kids = Vec::new();
                self.iter_all_children(idx, |r, c| kids.push((r, c)));
                if idx != self.root_idx && node.tag == Tag::None {
                    assert!(kids.len() >= 2, "nœud {idx} sans valeur à {} enfant(s) : non recompressé", kids.len());
                }
                for (r, c) in kids {
                    let child = self.get_node(c);
                    assert_eq!(child.parent_idx, idx, "parent_idx de {c}");
                    assert_eq!(child.exp_and_radix.parent_radix(), r, "parent_radix de {c}");
                    stack.push(c);
                }
            }
            assert_eq!(seen, self.node_count(), "nœuds orphelins (alloués mais inaccessibles)");
            seen
        }
    }

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut x = seed;
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        }
    }

    fn listing(art: &OxidArt) -> Vec<(Vec<u8>, Vec<u8>)> {
        art.krange(b"", b"", usize::MAX, false)
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    crate::krange::RangeVal::Bytes(b) => b.as_slice().to_vec(),
                    crate::krange::RangeVal::Int(n) => n.to_string().into_bytes(),
                };
                (k.to_vec(), v)
            })
            .collect()
    }

    /// Le vérificateur détecte un nœud sans valeur à un seul enfant : c'est
    /// l'état intermédiaire pendant une save (suppression sans recompression).
    #[test]
    #[should_panic(expected = "non recompressé")]
    fn assert_canonical_detects_uncompressed_node() {
        let mut art = OxidArt::new();
        art.set(SharedByte::from_str("ab"), Value::String(SharedByte::from_str("1")));
        art.set(SharedByte::from_str("abcd"), Value::String(SharedByte::from_str("2")));
        art.assert_canonical();
        // valeur retirée sans recompression : « ab » reste avec un seul enfant
        let idx = art.traverse_to_key(b"ab").unwrap();
        art.node_mut(idx).clear_val();
        art.assert_canonical();
    }

    /// Clés imbriquées (user:1 / user:10 / user:100 / user:1:x…) dont une
    /// partie expire au même instant ; purge par MGET / EXISTS en paquets
    /// mélangés avec doublons. L'arbre doit finir identique (valeurs, ordre,
    /// nombre de nœuds) à un arbre neuf construit avec les seules survivantes.
    #[test]
    fn purge_nested_expired_recompresses() {
        for round in 0..12u64 {
            let mut rnd = rng(0x51F1_5EED ^ (round * 0x9E37_79B9));
            let mut art = OxidArt::new();
            art.set_now(1_000);
            let mut live: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
            let mut all: Vec<Vec<u8>> = Vec::new();
            let alphabet = b"01:x";
            // familles entièrement expirées (sous-arbres qui disparaissent)
            for fam in ["fam:1", "fam:10", "fam:100", "fam:1:a", "fam:1:ab", "fam:11"] {
                all.push(fam.as_bytes().to_vec());
            }
            for _ in 0..1500 {
                let len = 1 + rnd() as usize % 5;
                let mut k = b"user:".to_vec();
                k.extend((0..len).map(|_| alphabet[rnd() as usize % alphabet.len()]));
                all.push(k);
            }
            all.sort();
            all.dedup();
            for k in &all {
                let expiring = k.starts_with(b"fam:") || rnd() % 2 == 0;
                let val = if rnd() % 5 == 0 {
                    Value::Int(rnd() as i64 % 1000)
                } else {
                    Value::String(SharedByte::from_slice(k))
                };
                if expiring {
                    art.set_ttl(SharedByte::from_slice(k), std::time::Duration::from_secs(5), val);
                } else {
                    art.set(SharedByte::from_slice(k), val.clone());
                    live.insert(k.clone(), val);
                }
            }
            art.set(SharedByte::from_str("fam:2"), Value::String(SharedByte::from_str("reste")));
            live.insert(b"fam:2".to_vec(), Value::String(SharedByte::from_str("reste")));
            art.set_now(1_000 + 60_000); // toutes les clés à TTL expirent ensemble

            let nodes_before = art.node_count();
            // purge : ordre mélangé, paquets de 1..=150, doublons
            let mut order: Vec<Vec<u8>> = all.clone();
            order.extend(all.iter().step_by(7).cloned());
            for i in (1..order.len()).rev() {
                order.swap(i, rnd() as usize % (i + 1));
            }
            let mut pos = 0;
            let mut by_mget = true;
            while pos < order.len() {
                let n = (1 + rnd() as usize % 150).min(order.len() - pos);
                let refs: Vec<&[u8]> = order[pos..pos + n].iter().map(Vec::as_slice).collect();
                if by_mget {
                    let got = art.mget_owned(&refs);
                    assert_eq!(got.len(), n);
                } else {
                    let mut expired = Vec::new();
                    art.exists_count(&refs, &mut expired);
                    art.purge_expired(&refs, &expired);
                }
                by_mget = !by_mget;
                pos += n;
            }

            // arbre neuf avec les seules survivantes, ordre d'insertion mélangé
            let mut fresh = OxidArt::new();
            let mut live_keys: Vec<&Vec<u8>> = live.keys().collect();
            for i in (1..live_keys.len()).rev() {
                live_keys.swap(i, rnd() as usize % (i + 1));
            }
            for k in live_keys {
                fresh.set(SharedByte::from_slice(k), live[k].clone());
            }

            let n_art = art.assert_canonical();
            let n_fresh = fresh.assert_canonical();
            assert_eq!(listing(&art), listing(&fresh), "round {round} : contenu");
            assert_eq!(n_art, n_fresh, "round {round} : nœuds après purge vs arbre neuf");
            assert!(nodes_before > n_fresh + 100, "round {round} : purge non exercée ({nodes_before} vs {n_fresh})");
            let mut expired = Vec::new();
            let refs: Vec<&[u8]> = all.iter().map(Vec::as_slice).collect();
            assert_eq!(art.exists_count(&refs, &mut expired), live.len() - 1, "round {round}");
            assert!(expired.is_empty(), "round {round} : expirées restantes {expired:?}");
        }
    }
}

#[cfg(test)]
impl OxidArt {
    /// Valeur string via la lecture pure (sans suppression d'expirée), format MGET.
    fn clone_get_for_test(&self, key: &[u8]) -> Option<Vec<u8>> {
        let idx = if key.is_empty() { Some(self.root_idx) } else { self.traverse_to_key(key) }?;
        let v = self.child_node(idx).get_value(self.now)?;
        v.as_bytes().map(|b| b.as_slice().to_vec())
    }
    fn exists_for_test(&self, key: &[u8]) -> bool {
        let idx = if key.is_empty() { Some(self.root_idx) } else { self.traverse_to_key(key) };
        idx.is_some_and(|i| self.child_node(i).get_value(self.now).is_some())
    }
}
