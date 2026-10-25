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

use crate::krange::{OwnedVal, RangeVal, str_val};
use crate::node::{EntryRef, Probe};
use crate::save::prefetch_value;
use crate::value::{RawVal, Value, value_from_raw_ref};
use crate::counter::CounterError;
use crate::{Loc, OxidArt, Slot, TtlResult, eq_fast, expired as is_expired, starts_with_fast};

/// Nombre maximal de clés suivies ensemble.
pub const PREFETCH_BATCH: usize = 64;

/// Emplacement d'une valeur trouvée par la descente entrelacée, encodé sur
/// 64 bits : `nœud << 9 | 0x100 | radix` pour une feuille, `nœud << 9` pour
/// une self-val.
const NOT_FOUND: u64 = u64::MAX;

#[inline]
fn encode(loc: Loc) -> u64 {
    (loc.node as u64) << 9
        | match loc.slot {
            Slot::SelfVal => 0,
            Slot::Leaf(r) => 0x100 | r as u64,
        }
}

#[inline]
fn decode(x: u64) -> Loc {
    Loc {
        node: (x >> 9) as u32,
        slot: if x & 0x100 == 0 { Slot::SelfVal } else { Slot::Leaf(x as u8) },
    }
}

/// Étape d'une clé dans la descente : chacune ne lit que de la mémoire
/// préchargée au tour précédent.
#[derive(Clone, Copy)]
enum Step {
    /// Nœud courant préchargé (deux lignes) : comparer son préfixe, chercher
    /// le radix suivant.
    Enter,
    /// En-tête de la page préchargé : y chercher le radix.
    PageSearch,
    /// Cellule de l'entrée de ce rang préchargée (dans la page).
    Entry(u16),
    /// Suffixe sur le tas préchargé : comparer la feuille de ce rang.
    LeafSfx(u16),
    Done,
}

#[inline(always)]
fn prefetch_node(art: &OxidArt, idx: u32) {
    let node = art.node(idx);
    prefetch_l1(node as *const _ as *const u8);
    node.prefetch_body();
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
    /// l'emplacement de la valeur de `keys[i]` ([`NOT_FOUND`] sinon), valeur
    /// préchargée. Elle peut être expirée : à l'appelant de vérifier.
    fn descend_batch(&self, keys: &[&[u8]], found: &mut [u64]) {
        debug_assert!(keys.len() <= PREFETCH_BATCH && found.len() == keys.len());
        let n = keys.len();
        if n == 1 {
            // Rien à entrelacer : descente directe, sans initialiser les états
            // des 64 voies (~1 Ko de pile, +20 % sur un MGET d'une clé).
            found[0] = self.locate(keys[0]).map_or(NOT_FOUND, encode);
            return;
        }
        let mut idx = [self.root_idx; PREFETCH_BATCH];
        let mut cursor = [0usize; PREFETCH_BATCH];
        let mut step = [Step::Enter; PREFETCH_BATCH];
        found[..n].fill(NOT_FOUND);
        let mut active = n;
        while active > 0 {
            for i in 0..n {
                let next = match step[i] {
                    Step::Done => continue,
                    Step::Enter => self.batch_enter(keys[i], &mut idx[i], &mut cursor[i], &mut found[i]),
                    Step::PageSearch => self.batch_search(keys[i], &mut idx[i], &mut cursor[i], &mut found[i]),
                    Step::Entry(e) => self.batch_entry(keys[i], &mut idx[i], &mut cursor[i], e as usize, &mut found[i]),
                    Step::LeafSfx(e) => {
                        self.batch_leaf(keys[i], idx[i], cursor[i], e as usize, &mut found[i]);
                        Step::Done
                    }
                };
                if matches!(next, Step::Done) {
                    active -= 1;
                }
                step[i] = next;
            }
        }
    }

    /// Nœud en cache : préfixe, puis self-val (clé finie) ou recherche du radix.
    #[inline(always)]
    fn batch_enter(&self, key: &[u8], idx: &mut u32, cursor: &mut usize, found: &mut u64) -> Step {
        let node = self.node(*idx);
        // sans test : sans page, le pointeur vise une ligne poubelle chaude
        node.prefetch_page();
        let p: &[u8] = &node.prefix;
        if !starts_with_fast(&key[*cursor..], p) {
            return Step::Done;
        }
        *cursor += p.len();
        let Some(&radix) = key.get(*cursor) else {
            if node.has_val() {
                prefetch_value(node.tag, &node.val);
                *found = encode(Loc { node: *idx, slot: Slot::SelfVal });
            }
            return Step::Done;
        };
        if node.search_needs_page(radix) {
            return Step::PageSearch;
        }
        self.batch_search(key, idx, cursor, found)
    }

    /// Recherche du radix (nœud et, s'il le faut, en-tête de page en cache).
    /// Entrée dans le nœud : traitée tout de suite ; dans la page : sa cellule
    /// est préchargée pour le tour suivant.
    #[inline(always)]
    fn batch_search(&self, key: &[u8], idx: &mut u32, cursor: &mut usize, found: &mut u64) -> Step {
        match self.node(*idx).probe(key[*cursor]) {
            Probe::Miss => Step::Done,
            Probe::Page(e, addr) => {
                prefetch_l1(addr);
                Step::Entry(e as u16)
            }
            Probe::Here(entry) => self.batch_entry_ref(key, idx, cursor, entry, found),
        }
    }

    /// Entrée de la page (cellule préchargée au tour précédent).
    #[inline(always)]
    fn batch_entry(&self, key: &[u8], idx: &mut u32, cursor: &mut usize, e: usize, found: &mut u64) -> Step {
        let entry = self.node(*idx).entry_ref(e);
        self.batch_entry_ref(key, idx, cursor, entry, found)
    }

    /// Entrée en cache : enfant → on le précharge ; feuille → comparaison du
    /// suffixe (après préchargement s'il est sur le tas).
    #[inline(always)]
    fn batch_entry_ref(&self, key: &[u8], idx: &mut u32, cursor: &mut usize, entry: EntryRef<'_>, found: &mut u64) -> Step {
        match entry {
            EntryRef::Child(c) => {
                prefetch_node(self, c);
                *idx = c;
                *cursor += 1;
                Step::Enter
            }
            EntryRef::Leaf { sfx: Some(sfx), .. } if sfx.len() > 7 => {
                sfx.prefetch();
                let e = self.node(*idx).find(key[*cursor]).expect("feuille trouvée");
                Step::LeafSfx(e as u16)
            }
            EntryRef::Leaf { tag, val, sfx, .. } => {
                let s: &[u8] = sfx.map_or(&[], |p| p);
                if eq_fast(s, &key[*cursor + 1..]) {
                    prefetch_value(tag, val);
                    *found = encode(Loc { node: *idx, slot: Slot::Leaf(key[*cursor]) });
                }
                Step::Done
            }
        }
    }

    /// Feuille de rang `e` dont le suffixe (sur le tas) est en cache.
    #[inline(always)]
    fn batch_leaf(&self, key: &[u8], idx: u32, cursor: usize, e: usize, found: &mut u64) {
        if let EntryRef::Leaf { tag, val, sfx, .. } = self.node(idx).entry_ref(e) {
            let s: &[u8] = sfx.map_or(&[], |p| p);
            if eq_fast(s, &key[cursor + 1..]) {
                prefetch_value(tag, val);
                *found = encode(Loc { node: idx, slot: Slot::Leaf(key[cursor]) });
            }
        }
    }
}

// ── Moteur AMAC (prototype, `RADIXOX_AMAC`) ──────────────────────────────────
//
// Deux phases par fenêtre, sans jamais refaire une descente à cause d'une autre
// commande :
// 1. `amac_descend` : toutes les descentes, en lecture pure (au plus `lanes` en
//    vol, une voie libérée prend la clé suivante) → un indice par clé ;
// 2. entre `begin_window` et `end_window`, les commandes s'exécutent dans
//    l'ordre avec leur indice. Aucun nœud n'est libéré ni fusionné, et l'arbre
//    reste canonique : une suppression qui garde la forme (racine, ou nœud qui
//    garde au moins deux choses) est faite tout de suite ; sinon la valeur
//    reçoit une échéance passée (expirée = absente, DET2) et son emplacement
//    attend `end_window`, qui ne retire que ce qui est encore expiré. Un indice
//    ne vaut plus que pour deux raisons, propres à sa clé : la clé a été
//    ajoutée ou retirée plus tôt dans la fenêtre (`touched`), ou un
//    `split_leaf` a déplacé sa feuille (`moved`) ; seule cette commande
//    redescend.

/// Emplacement de la valeur d'une clé (ou son absence) trouvé par
/// [`OxidArt::amac_descend`], valable dans la fenêtre qui suit.
#[derive(Clone, Copy, Debug)]
pub struct Hint {
    found: u64,
}

impl Hint {
    pub const NONE: Hint = Hint { found: NOT_FOUND };
}

/// Compteurs du moteur : commandes servies par indice, et commandes
/// redescendues (clé touchée plus tôt dans la fenêtre, feuille déplacée).
#[derive(Clone, Copy, Debug, Default)]
pub struct AmacStats {
    pub cmds: u64,
    pub redescents: u64,
}

/// État d'une fenêtre ouverte (voir l'en-tête du moteur).
#[derive(Default)]
pub(crate) struct WindowState {
    /// Valeurs mortes (expirées) dont le retrait changerait la forme.
    dead: Vec<Loc>,
    /// Empreintes des clés ajoutées ou retirées dans la fenêtre.
    touched: Vec<u64>,
    /// Nœuds dont une feuille est partie dans un nouveau nœud (`split_leaf`).
    moved: Vec<u32>,
}

impl WindowState {
    /// `split_leaf` a déplacé la feuille `from` en `to` : seul déplacement de
    /// valeur possible dans une fenêtre.
    pub(crate) fn leaf_moved(&mut self, from: Loc, to: Loc) {
        self.moved.push(from.node);
        for d in &mut self.dead {
            if *d == from {
                *d = to;
            }
        }
    }
}

/// Empreinte d'une clé : une collision ne coûte qu'une redescente.
#[inline]
fn key_tag(key: &[u8]) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64 ^ key.len() as u64;
    for c in key.chunks(8) {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(b)).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(29);
    }
    h
}

impl OxidArt {
    /// Phase 1 : descentes de `keys` en AMAC, lecture pure. `hints[i]` reçoit
    /// l'emplacement de la valeur de `keys[i]` (préchargée), expirée ou non.
    pub fn amac_descend(&self, keys: &[&[u8]], lanes: usize, hints: &mut [Hint]) {
        let n = keys.len();
        debug_assert_eq!(hints.len(), n);
        if n == 0 {
            return;
        }
        let k = lanes.clamp(1, PREFETCH_BATCH).min(n);
        let root = self.root_idx;
        let mut idx = [root; PREFETCH_BATCH];
        let mut cursor = [0usize; PREFETCH_BATCH];
        let mut step = [Step::Enter; PREFETCH_BATCH];
        let mut found = [NOT_FOUND; PREFETCH_BATCH];
        let mut key_of = [0usize; PREFETCH_BATCH];
        for (s, slot) in key_of[..k].iter_mut().enumerate() {
            *slot = s;
        }
        let (mut issued, mut active) = (k, k);
        while active > 0 {
            for s in 0..k {
                let key = keys[key_of[s]];
                let next = match step[s] {
                    Step::Done => continue,
                    Step::Enter => self.batch_enter(key, &mut idx[s], &mut cursor[s], &mut found[s]),
                    Step::PageSearch => self.batch_search(key, &mut idx[s], &mut cursor[s], &mut found[s]),
                    Step::Entry(e) => self.batch_entry(key, &mut idx[s], &mut cursor[s], e as usize, &mut found[s]),
                    Step::LeafSfx(e) => {
                        self.batch_leaf(key, idx[s], cursor[s], e as usize, &mut found[s]);
                        Step::Done
                    }
                };
                if !matches!(next, Step::Done) {
                    step[s] = next;
                    continue;
                }
                hints[key_of[s]] = Hint { found: found[s] };
                if issued < n {
                    key_of[s] = issued;
                    (idx[s], cursor[s], step[s], found[s]) = (root, 0, Step::Enter, NOT_FOUND);
                    issued += 1;
                } else {
                    step[s] = Step::Done;
                    active -= 1;
                }
            }
        }
    }

    /// Phase 2, en avance sur l'exécution : précharge le nœud de l'indice
    /// (les premiers de la fenêtre ont pu quitter la L1 pendant les autres
    /// descentes).
    #[inline]
    pub fn prefetch_hint_node(&self, hint: Hint) {
        if hint.found != NOT_FOUND {
            prefetch_node(self, decode(hint.found).node);
        }
    }

    /// Comme [`prefetch_hint_node`](Self::prefetch_hint_node), pour la valeur
    /// (le nœud doit déjà être en cache : une recherche du radix).
    #[inline]
    pub fn prefetch_hint_value(&self, hint: Hint) {
        if hint.found == NOT_FOUND {
            return;
        }
        let loc = decode(hint.found);
        let node = self.node(loc.node);
        match loc.slot {
            Slot::SelfVal if node.has_val() => prefetch_value(node.tag, &node.val),
            // une feuille déplacée par un split : rien à précharger
            Slot::Leaf(r) => {
                if let Some(EntryRef::Leaf { tag, val, .. }) = node.get(r) {
                    prefetch_value(tag, val);
                }
            }
            Slot::SelfVal => {}
        }
    }

    /// Phase 2 : ouvre la fenêtre (voir l'en-tête du moteur).
    pub fn begin_window(&mut self) {
        debug_assert!(self.window.is_none());
        self.window = Some(WindowState::default());
    }

    /// Une fenêtre est ouverte.
    #[inline]
    pub fn in_window(&self) -> bool {
        self.window.is_some()
    }

    /// Ferme la fenêtre : retire les valeurs mortes encore expirées (une
    /// valeur recréée entre-temps est vivante : sautée). Par nœud, toutes ses
    /// valeurs mortes puis une seule normalisation (ce qui remonte n'est pas
    /// en attente) ; les ancêtres d'abord : une normalisation ne déplace que
    /// vers le haut, et la fusion avec un enfant garde l'idx de l'enfant, donc
    /// aucune valeur en attente ne bouge. Le garde-fou `is_occupied` ne devrait
    /// jamais servir ; on ne retire de toute façon qu'une valeur expirée.
    pub fn end_window(&mut self) {
        let Some(mut w) = self.window.take() else { return };
        if w.dead.is_empty() {
            return;
        }
        let depth = |art: &OxidArt, mut x: u32| {
            let mut d = 0u32;
            while x != art.root_idx {
                x = art.node(x).parent_idx;
                d += 1;
            }
            d
        };
        let mut order: Vec<(u32, u32)> = w.dead.iter().map(|l| (depth(self, l.node), l.node)).collect();
        order.sort_unstable();
        order.dedup();
        for (_, x) in order {
            if !self.map.is_occupied(x) {
                continue;
            }
            let mut took = false;
            for loc in w.dead.iter_mut().filter(|l| l.node == x) {
                let node = self.node(x);
                let exp = match loc.slot {
                    Slot::SelfVal if node.has_val() => node.exp_and_radix.exp(),
                    Slot::Leaf(r) => match node.get(r) {
                        Some(EntryRef::Leaf { ttl, .. }) => ttl,
                        _ => continue,
                    },
                    Slot::SelfVal => continue,
                };
                if is_expired(exp, self.now) {
                    drop(self.take_loc(*loc));
                    took = true;
                }
            }
            if took {
                self.normalize(x);
            }
        }
    }

    /// Retire la valeur à `loc` si la forme de l'arbre n'en dépend pas ;
    /// sinon la laisse (expirée) pour [`end_window`](Self::end_window).
    fn remove_in_window(&mut self, loc: Loc) {
        let node = self.node(loc.node);
        if loc.node == self.root_idx || node.len() + node.has_val() as usize > 2 {
            // `normalize` n'aura rien à faire
            drop(self.remove_loc(loc));
            return;
        }
        if !is_expired(self.loc_value(loc).2, self.now) {
            self.set_loc_exp(loc, Some(0));
        }
        self.window.as_mut().expect("fenêtre ouverte").dead.push(loc);
    }

    /// Note que la présence de `key` a changé (ajout, suppression) ou que son
    /// indice n'a pas servi.
    #[inline]
    fn touch(&mut self, key: &[u8]) {
        if let Some(w) = &mut self.window {
            w.touched.push(key_tag(key));
        }
    }

    /// `Some(emplacement ou absence)` si l'indice est encore valable.
    fn resolve(&mut self, key: &[u8], hint: Hint) -> Option<Option<Loc>> {
        let w = self.window.as_ref().expect("indice AMAC hors fenêtre");
        let touched = !w.touched.is_empty() && w.touched.contains(&key_tag(key));
        let loc = (hint.found != NOT_FOUND).then(|| decode(hint.found));
        let moved = matches!(loc, Some(Loc { node, slot: Slot::Leaf(_) }) if w.moved.contains(&node));
        if touched || moved {
            self.amac_stats.redescents += 1;
            return None;
        }
        self.amac_stats.cmds += 1;
        debug_assert_eq!(loc, self.locate(key), "indice AMAC faux pour {}", key.escape_ascii());
        Some(loc)
    }

    /// Emplacement de `key`, expirée ou non : l'indice s'il vaut encore,
    /// sinon une descente.
    fn hinted_loc(&mut self, key: &[u8], hint: Hint) -> Option<Loc> {
        match self.resolve(key, hint) {
            Some(loc) => loc,
            None => self.locate(key),
        }
    }

    /// `live_loc` dans la fenêtre : une valeur expirée est retirée par
    /// `remove_in_window` (sauf pendant une save) et vue absente.
    fn live_hinted(&mut self, key: &[u8], hint: Hint) -> Option<Loc> {
        let loc = self.hinted_loc(key, hint)?;
        if is_expired(self.loc_value(loc).2, self.now) {
            if !self.is_saving() {
                self.remove_in_window(loc);
                self.touch(key);
            }
            return None;
        }
        Some(loc)
    }

    // Les commandes avec l'indice de `amac_descend`, dans une fenêtre ouverte :
    // mêmes réponses et mêmes effets que leur version sans indice.

    /// [`get`](Self::get).
    pub fn get_hinted(&mut self, key: &[u8], hint: Hint) -> Option<Value> {
        let loc = self.live_hinted(key, hint)?;
        let (tag, val, _) = self.loc_value(loc);
        // SAFETY: tag != None (valeur trouvée).
        Some(unsafe { value_from_raw_ref(tag, val) })
    }

    /// EXISTS à une clé.
    pub fn exists_hinted(&mut self, key: &[u8], hint: Hint) -> bool {
        self.live_hinted(key, hint).is_some()
    }

    /// [`get_ttl`](Self::get_ttl) (une expirée reste en place, comme sans indice).
    pub fn ttl_hinted(&mut self, key: &[u8], hint: Hint) -> TtlResult {
        let Some(loc) = self.hinted_loc(key, hint) else {
            return TtlResult::KeyNotExist;
        };
        match self.loc_value(loc).2 {
            Some(exp) if exp < self.now => TtlResult::KeyNotExist,
            Some(exp) => TtlResult::KeyWithTtl(exp - self.now),
            None => TtlResult::KeyWithoutTtl,
        }
    }

    /// [`expire`](Self::expire) : l'échéance change sur place.
    pub fn expire_hinted(&mut self, key: &[u8], ttl: std::time::Duration, hint: Hint) -> bool {
        let Some(loc) = self.hinted_loc(key, hint) else {
            return false;
        };
        if is_expired(self.loc_value(loc).2, self.now) {
            return false;
        }
        self.set_loc_exp(loc, Some(self.now.saturating_add(ttl.as_secs())));
        true
    }

    /// [`persist`](Self::persist).
    pub fn persist_hinted(&mut self, key: &[u8], hint: Hint) -> bool {
        let Some(loc) = self.hinted_loc(key, hint) else {
            return false;
        };
        let exp = self.loc_value(loc).2;
        if exp.is_none() || is_expired(exp, self.now) {
            return false;
        }
        self.set_loc_exp(loc, None);
        true
    }

    /// [`incrby`](Self::incrby) : une clé présente change sur place.
    pub fn incrby_hinted(&mut self, key: &[u8], delta: i64, hint: Hint) -> Result<i64, CounterError> {
        if let Some(loc) = self.live_hinted(key, hint) {
            return Ok(self.val_mut_at(loc).incr(delta)?);
        }
        self.touch(key);
        self.set_raw(key, RawVal::from_value(Value::Int(delta)), None, None);
        Ok(delta)
    }

    /// [`del`](Self::del).
    pub fn del_hinted(&mut self, key: &[u8], hint: Hint) -> bool {
        let Some(loc) = self.hinted_loc(key, hint) else {
            return false;
        };
        let live = !is_expired(self.loc_value(loc).2, self.now);
        self.remove_in_window(loc);
        self.touch(key);
        live
    }

    /// [`set_cond`](Self::set_cond) : une clé présente est écrasée sur place,
    /// sans descente.
    pub fn set_cond_hinted(
        &mut self,
        key: &[u8],
        ttl: Option<std::time::Duration>,
        val: Value,
        if_exists: Option<bool>,
        hint: Hint,
    ) -> bool {
        let exp = ttl.map(|d| self.now.saturating_add(d.as_secs()));
        let raw = RawVal::from_value(val);
        let Some(Some(loc)) = self.resolve(key, hint) else {
            // absente (insertion) ou indice inutilisable : descente normale
            self.touch(key);
            return self.set_raw(key, raw, exp, if_exists);
        };
        let live = !is_expired(self.loc_value(loc).2, self.now);
        if !if_exists.is_none_or(|want| want == live) {
            return false;
        }
        self.write_at(loc, raw, exp);
        true
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
            for (i, &x) in found.iter().enumerate() {
                if x == NOT_FOUND {
                    out.push(None);
                    continue;
                }
                let (tag, val, exp) = self.loc_value(decode(x));
                if is_expired(exp, self.now) {
                    expired_push(expired, c, i);
                }
                out.push(str_val(tag, val, exp, self.now));
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
            let _ = self.live_loc(keys[pos as usize]);
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
            for (i, &x) in found.iter().enumerate() {
                if x == NOT_FOUND {
                    continue;
                }
                if is_expired(self.loc_value(decode(x)).2, self.now) {
                    expired_push(expired, c, i);
                } else {
                    n += 1;
                }
            }
        }
        n
    }
}

#[inline]
fn expired_push(expired: &mut Vec<u32>, chunk: usize, i: usize) {
    expired.push((chunk * PREFETCH_BATCH + i) as u32);
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
        // mget_owned a supprimé les 50 valeurs expirées croisées (souvent des
        // feuilles inline : le nombre de nœuds ne baisse pas forcément)
        assert!(art.node_count() <= nodes_before);
        art.check();
        let mut expired3 = Vec::new();
        art.mget_ref(&refs, &mut expired3);
        assert!(expired3.is_empty(), "plus aucune expirée : {expired3:?}");
        assert_eq!(art.mget_owned(&refs).len(), refs.len());
    }

    /// AMAC contre l'arbre ordinaire : GET / SET (avec ou sans TTL) / SET NX /
    /// DEL tirés sur des clés à préfixes partagés (splits, nouvelles clés,
    /// suppressions et expirations en pleine fenêtre), 1 à 64 voies. Les
    /// indices utilisés sont vérifiés dans `resolve` ; l'arbre est canonique
    /// après chaque fenêtre.
    #[test]
    fn amac_matches_plain_tree() {
        use rand::{Rng, SeedableRng, rngs::StdRng};
        use radixox_lib::shared_byte::SharedByte;
        use std::time::Duration;
        // chaque commande avec indice, et sa version sans indice
        fn hinted(art: &mut OxidArt, op: u8, k: &[u8], v: Value, h: super::Hint) -> String {
            let ttl = Some(Duration::from_secs(1));
            match op {
                0..=2 => format!("{:?}", art.get_hinted(k, h)),
                3..=4 => format!("{}", art.set_cond_hinted(k, None, v, None, h)),
                5 => format!("{}", art.set_cond_hinted(k, ttl, v, None, h)),
                6 => format!("{}", art.set_cond_hinted(k, None, v, Some(false), h)),
                7 => format!("{}", art.set_cond_hinted(k, None, v, Some(true), h)),
                8..=9 => format!("{}", art.del_hinted(k, h)),
                10 => format!("{}", art.exists_hinted(k, h)),
                11 => format!("{:?}", art.ttl_hinted(k, h)),
                12 => format!("{}", art.expire_hinted(k, Duration::from_secs(2), h)),
                13 => format!("{}", art.persist_hinted(k, h)),
                _ => format!("{:?}", art.incrby_hinted(k, 3, h)),
            }
        }
        fn plain(art: &mut OxidArt, op: u8, k: &[u8], v: Value) -> String {
            let ttl = Some(Duration::from_secs(1));
            let sb = SharedByte::from_slice(k);
            match op {
                0..=2 => format!("{:?}", art.get(k)),
                3..=4 => format!("{}", art.set_cond(k, None, v, None)),
                5 => format!("{}", art.set_cond(k, ttl, v, None)),
                6 => format!("{}", art.set_cond(k, None, v, Some(false))),
                7 => format!("{}", art.set_cond(k, None, v, Some(true))),
                8..=9 => format!("{}", art.del(k)),
                10 => format!("{}", art.get(k).is_some()),
                11 => format!("{:?}", art.get_ttl(sb)),
                12 => format!("{}", art.expire(sb, Duration::from_secs(2))),
                13 => format!("{}", art.persist(sb)),
                _ => format!("{:?}", art.incrby(sb, 3)),
            }
        }
        for (seed, lanes) in [(1u64, 1usize), (2, 2), (3, 8), (4, 16), (5, 64)] {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut a = OxidArt::new();
            let mut b = OxidArt::new();
            // préfixes partagés courts (splits fréquents), et nœuds larges à
            // profondeurs variées (étapes de page)
            let key = |rng: &mut StdRng| -> Vec<u8> {
                if rng.gen_bool(0.5) {
                    let len = rng.gen_range(0..6);
                    let mut k = b"user:".to_vec();
                    k.extend((0..len).map(|_| b"abc"[rng.gen_range(0..3)]));
                    k
                } else {
                    let len = rng.gen_range(1..9);
                    let mut k = b"k:".to_vec();
                    k.extend((0..len).map(|i| b"abcdefghijklmnopqrst"[rng.gen_range(0..if i < 2 { 20 } else { 3 })]));
                    k
                }
            };
            for round in 0..1000u64 {
                a.set_now(1000 + round);
                b.set_now(1000 + round);
                let n = rng.gen_range(1..=64);
                let ops: Vec<(u8, Vec<u8>)> = (0..n).map(|_| (rng.gen_range(0..15), key(&mut rng))).collect();
                // valeurs : entiers (INCRBY) ou texte
                let vals: Vec<String> =
                    (0..n).map(|i| if rng.gen_bool(0.3) { format!("{i}") } else { format!("{round}:{i}") }).collect();
                let keys: Vec<&[u8]> = ops.iter().map(|(_, k)| k.as_slice()).collect();
                let mut hints = vec![super::Hint::NONE; n];
                a.amac_descend(&keys, lanes, &mut hints);
                a.begin_window();
                let got: Vec<String> =
                    ops.iter().zip(&hints).zip(&vals).map(|(((op, k), &h), v)| hinted(&mut a, *op, k, val(v), h)).collect();
                a.end_window();
                let want: Vec<String> = ops.iter().zip(&vals).map(|((op, k), v)| plain(&mut b, *op, k, val(v))).collect();
                assert_eq!(got, want, "seed {seed} lanes {lanes} round {round}");
                assert_eq!(a.len(), b.len());
                a.assert_canonical();
            }
            let st = a.amac_stats;
            // peu de clés : une même clé revient souvent dans la fenêtre
            assert!(st.cmds > st.redescents && st.redescents > 0, "{st:?}");
        }
    }
}

#[cfg(test)]
mod purge_tests {
    use std::collections::BTreeMap;

    use radixox_lib::shared_byte::SharedByte;

    use crate::OxidArt;
    use crate::value::Value;

    impl OxidArt {
        /// Forme canonique : voir [`OxidArt::check`].
        pub(crate) fn assert_canonical(&self) -> usize {
            self.check()
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
    #[should_panic(expected = "non nécessaire")]
    fn assert_canonical_detects_uncompressed_node() {
        let mut art = OxidArt::new();
        art.set(SharedByte::from_str("ab"), Value::String(SharedByte::from_str("1")));
        art.set(SharedByte::from_str("abcd"), Value::String(SharedByte::from_str("2")));
        art.assert_canonical();
        // self-val de « ab » retirée sans normalisation : nœud à une seule entrée
        let loc = art.locate(b"ab").unwrap();
        let node = art.node_mut(loc.node);
        unsafe { crate::value::drop_raw(node.tag, &mut node.val) };
        node.tag = crate::value::Tag::None;
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
            // avec les feuilles inline, une clé purgée n'est souvent qu'une cellule :
            // on compte les clés, pas les nœuds
            assert!(nodes_before >= n_fresh, "round {round} : nœuds {nodes_before} → {n_fresh}");
            assert!(all.len() > live.len() + 100, "round {round} : purge non exercée");
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
        let (tag, val, exp) = self.loc_value(self.locate(key)?);
        if is_expired(exp, self.now) {
            return None;
        }
        let v = unsafe { crate::value::value_from_raw_ref(tag, val) };
        v.as_bytes().map(|b| b.as_slice().to_vec())
    }
    fn exists_for_test(&self, key: &[u8]) -> bool {
        self.locate(key).is_some_and(|loc| !is_expired(self.loc_value(loc).2, self.now))
    }
}
