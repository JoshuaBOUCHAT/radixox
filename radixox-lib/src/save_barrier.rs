//! Barrière d'écriture pour une save progressive (COW applicatif).
//! Design complet : `SAVE_DESIGN.md` à la racine du dépôt.
//!
//! Le slab est muté en place pendant toute la save, qui parcourt l'arbre
//! (pas le slab) par tranches. Au début, on copie le bitmap de présence du
//! slab (un memcpy) : c'est le bitset `intact`.
//!
//! - `intact[i] == 1` : présent à T0, ni modifié ni encore sérialisé → la
//!   save le lit dans le slab (et baisse le bit).
//! - `intact[i] == 0` et `i` dans `images` : présent à T0, modifié ensuite
//!   → la save lit l'image T0.
//! - sinon : libre à T0, déjà sérialisé, ou inaccessible depuis T0.
//!   Les allocations n'ont donc pas de barrière.
//!
//! Avant de muter ou de libérer un slot, on capture son image si
//! `intact[idx]`, puis on baisse le bit : au plus une capture par slot.
//!
//! Les idx restent valides d'une tranche à l'autre (la vue T0 ne bouge
//! pas) : on peut les garder dans la pile du parcours et les prefetcher.
//!
//! L'image est construite par l'appelant (closure) et appartient à la
//! barrière : elle est droppée quand la save la consomme (au `take`
//! suivant) ou à `finish`. Ce qu'elle référence sans le posséder doit
//! rester vivant jusqu'à `finish` (valart diffère toutes ses libérations
//! pendant une save, SAVE_DESIGN.md §2.B).

use std::collections::BTreeMap;

/// Bitset 1 bit par slot sur `Vec<u64>` (`Vec<bool>` coûterait 1 octet/slot).
/// Même layout que le bitmap de niveau 1 de HiSlab : slot `i` = bit `i % 64`
/// du mot `i / 64`.
#[derive(Default)]
pub struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    /// Nombre de bits couverts.
    #[inline]
    pub fn len(&self) -> usize {
        self.words.len() * 64
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Buffer brut, pour le remplir par memcpy (`HiSlab::copy_occupancy`).
    pub fn words_mut(&mut self) -> &mut Vec<u64> {
        &mut self.words
    }

    /// Hors bornes = 0 (slot inexistant à T0).
    #[inline]
    pub fn get(&self, idx: usize) -> bool {
        self.words
            .get(idx >> 6)
            .is_some_and(|w| w & (1 << (idx & 63)) != 0)
    }

    #[inline]
    pub fn set(&mut self, idx: usize) {
        self.words[idx >> 6] |= 1 << (idx & 63);
    }

    #[inline]
    pub fn clear(&mut self, idx: usize) {
        self.words[idx >> 6] &= !(1 << (idx & 63));
    }

    /// Premier bit à 1 à partir de `from` (inclus).
    #[inline]
    pub fn next_set(&self, from: usize) -> Option<usize> {
        let mut w = from >> 6;
        let mut word = *self.words.get(w)? & (u64::MAX << (from & 63));
        loop {
            if word != 0 {
                return Some((w << 6) | word.trailing_zeros() as usize);
            }
            w += 1;
            word = *self.words.get(w)?;
        }
    }
}

/// Version T0 d'un nœud, pour la save.
pub enum T0<'a, I> {
    /// Intact : lire `slab[idx]` maintenant (garanti occupé). Le slot est
    /// désormais considéré comme sérialisé : une écriture ultérieure ne le
    /// capturera plus, donc tout ce qu'il faut en lire doit l'être avant de
    /// rendre la main.
    Slab,
    /// Modifié depuis T0 : contenu T0. Valide jusqu'au prochain `take`.
    Image(&'a I),
}

pub struct SaveBarrier<I> {
    active: bool,
    /// Présent à T0, ni capturé ni sérialisé. Écrasé à chaque `begin_with`.
    intact: BitSet,
    images: BTreeMap<u32, I>,
    /// Image rendue par le dernier `take` (`images` ne la contient plus).
    last: Option<I>,
}

impl<I> Default for SaveBarrier<I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I> SaveBarrier<I> {
    pub fn new() -> Self {
        Self {
            active: false,
            intact: BitSet::default(),
            images: BTreeMap::new(),
            last: None,
        }
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Nombre d'images T0 pas encore consommées par la save.
    pub fn pending_images(&self) -> usize {
        self.images.len()
    }

    /// Démarre une save. `snapshot` remplit le buffer avec le bitmap de
    /// présence actuel du slab (état T0), typiquement
    /// `|buf| slab.copy_occupancy(buf)`.
    pub fn begin_with(&mut self, snapshot: impl FnOnce(&mut Vec<u64>)) {
        assert!(!self.active, "save déjà en cours");
        debug_assert!(self.images.is_empty() && self.last.is_none());
        snapshot(self.intact.words_mut());
        self.active = true;
    }

    /// À appeler **avant** de muter ou de libérer le slot occupé `idx`.
    /// Inutile avant une allocation : un slot libre à T0 n'est jamais lu.
    /// `capture` construit l'image T0 à partir de l'état courant (encore
    /// intact) du slot ; elle n'est appelée qu'une fois par slot et par save.
    #[inline]
    pub fn before_write(&mut self, idx: u32, capture: impl FnOnce() -> I) {
        if self.active && self.intact.get(idx as usize) {
            self.capture(idx, capture);
        }
    }

    #[cold]
    #[inline(never)]
    fn capture(&mut self, idx: u32, capture: impl FnOnce() -> I) {
        let img = capture();
        self.images.insert(idx, img);
        self.intact.clear(idx as usize);
    }

    /// Version T0 du nœud `idx` sans la consommer.
    pub fn peek(&self, idx: u32) -> Option<T0<'_, I>> {
        if !self.active {
            return None;
        }
        if self.intact.get(idx as usize) {
            return Some(T0::Slab);
        }
        self.images.get(&idx).map(T0::Image)
    }

    /// Indice pour prefetcher : le nœud sera-t-il lu dans le slab ?
    #[inline]
    pub fn is_intact(&self, idx: u32) -> bool {
        self.intact.get(idx as usize)
    }

    /// Consomme la version T0 du nœud `idx`, atteint par le parcours.
    /// `None` si le slot n'existait pas à T0 ou a déjà été sérialisé.
    /// L'image rendue par le `take` précédent est droppée ici.
    pub fn take(&mut self, idx: u32) -> Option<T0<'_, I>> {
        if !self.active {
            return None;
        }
        self.last = None;
        if self.intact.get(idx as usize) {
            self.intact.clear(idx as usize);
            return Some(T0::Slab);
        }
        let img = self.images.remove(&idx)?;
        Some(T0::Image(self.last.insert(img)))
    }

    /// Termine la save (fin normale ou abandon). Les images restantes sont
    /// droppées. `intact` sera écrasé par la prochaine copie.
    pub fn finish(&mut self) {
        self.images.clear();
        self.last = None;
        self.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nœud jouet : `String` et `Vec` pour avoir du vrai heap. Les images
    /// sont des clones (possédées), comme les `NodeImage` d'oxidart.
    #[derive(Clone, Debug, PartialEq)]
    struct N {
        val: String,
        childs: Vec<u32>,
    }

    /// Arbre jouet dans un slab, racine = slot 0. Les UNLINK coupent tout de
    /// suite et libèrent par tranches, mais jamais pendant une save (ils
    /// attendent la fin, comme le waker de la barrière).
    struct Toy {
        slots: Vec<Option<N>>,
        barrier: SaveBarrier<N>,
        jobs: Vec<Vec<u32>>,
    }

    impl Toy {
        fn new(cap: usize) -> Self {
            let mut slots: Vec<Option<N>> = (0..cap).map(|_| None).collect();
            slots[0] = Some(N { val: "root".into(), childs: vec![] });
            Self { slots, barrier: SaveBarrier::new(), jobs: vec![] }
        }

        fn begin(&mut self) {
            let slots = &self.slots;
            self.barrier.begin_with(|buf| {
                buf.clear();
                buf.resize(slots.len().div_ceil(64), 0);
                for (i, s) in slots.iter().enumerate() {
                    if s.is_some() {
                        buf[i >> 6] |= 1 << (i & 63);
                    }
                }
            });
        }

        /// Point de passage unique pour muter un nœud.
        fn mutate(&mut self, idx: u32, f: impl FnOnce(&mut N)) {
            let Self { slots, barrier, .. } = self;
            let cur = slots[idx as usize].as_mut().unwrap();
            barrier.before_write(idx, || cur.clone());
            f(cur);
        }

        /// Point de passage unique pour libérer un nœud.
        fn remove(&mut self, idx: u32) -> N {
            let Self { slots, barrier, .. } = self;
            let cur = slots[idx as usize].as_ref().unwrap();
            barrier.before_write(idx, || cur.clone());
            slots[idx as usize].take().unwrap()
        }

        /// Nœuds accessibles depuis la racine (état courant).
        fn reachable(&self) -> Vec<u32> {
            let mut out = vec![];
            let mut st = vec![0u32];
            while let Some(i) = st.pop() {
                out.push(i);
                st.extend(self.slots[i as usize].as_ref().unwrap().childs.iter().rev());
            }
            out
        }

        fn add_child(&mut self, parent: u32, val: String) -> Option<u32> {
            let idx = self.slots.iter().position(|s| s.is_none())? as u32;
            // Allocation : pas de barrière.
            self.slots[idx as usize] = Some(N { val, childs: vec![] });
            self.mutate(parent, |n| n.childs.push(idx));
            Some(idx)
        }

        /// DEL d'une feuille : libération immédiate (capturée si besoin).
        fn del_leaf(&mut self, parent: u32, pos: usize) {
            let child = self.slots[parent as usize].as_ref().unwrap().childs[pos];
            if !self.slots[child as usize].as_ref().unwrap().childs.is_empty() {
                return;
            }
            self.mutate(parent, |n| {
                n.childs.remove(pos);
            });
            self.remove(child);
        }

        /// UNLINK : coupe immédiate, libération par tranches via `jobs`.
        fn cut(&mut self, parent: u32, pos: usize) {
            let child = self.slots[parent as usize].as_ref().unwrap().childs[pos];
            self.mutate(parent, |n| {
                n.childs.remove(pos);
            });
            self.jobs.push(vec![child]);
        }

        /// Une tranche de libération (au plus `budget` nœuds). En attente
        /// pendant une save.
        fn free_step(&mut self, budget: usize) {
            if self.barrier.is_active() {
                return;
            }
            let Some(mut stack) = self.jobs.pop() else { return };
            for _ in 0..budget {
                let Some(idx) = stack.pop() else { break };
                let n = self.remove(idx);
                stack.extend(n.childs.iter().copied());
            }
            if !stack.is_empty() {
                self.jobs.push(stack);
            }
        }
    }

    /// Parcours DFS de la vue T0 : renvoie les valeurs dans l'ordre.
    /// `between` est appelé entre deux nœuds (mutations).
    fn save_dfs(t: &mut Toy, mut between: impl FnMut(&mut Toy)) -> Vec<String> {
        let mut out = vec![];
        let mut stack = vec![0u32];
        while let Some(idx) = stack.pop() {
            let node = match t.barrier.take(idx) {
                Some(T0::Slab) => t.slots[idx as usize].clone().expect("intact vide"),
                Some(T0::Image(n)) => n.clone(),
                None => panic!("nœud T0 {idx} introuvable"),
            };
            out.push(node.val);
            stack.extend(node.childs.iter().rev());
            between(t);
        }
        out
    }

    fn dfs_vals(t: &Toy) -> Vec<String> {
        t.reachable()
            .into_iter()
            .map(|i| t.slots[i as usize].as_ref().unwrap().val.clone())
            .collect()
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Une mutation aléatoire sur un nœud accessible.
    fn random_op(t: &mut Toy, rng: &mut Rng) {
        let r = t.reachable();
        let idx = r[rng.below(r.len())];
        let k = t.slots[idx as usize].as_ref().unwrap().childs.len();
        match rng.below(12) {
            0..=3 => {
                let v = format!("v{}", rng.next());
                t.mutate(idx, |n| n.val = v);
            }
            4..=7 => {
                t.add_child(idx, format!("c{}", rng.next()));
            }
            8 if k > 0 => t.cut(idx, rng.below(k)),
            9 | 10 if k > 0 => t.del_leaf(idx, rng.below(k)),
            _ => t.free_step(1 + rng.below(4)),
        }
    }

    #[test]
    fn bitset_basics() {
        let mut b = BitSet::default();
        b.words_mut().resize(3, 0);
        assert_eq!(b.len(), 192);
        assert_eq!(b.next_set(0), None);
        for i in [0, 63, 64, 129] {
            assert!(!b.get(i));
            b.set(i);
            assert!(b.get(i));
        }
        assert!(!b.get(10_000)); // hors bornes = 0
        assert_eq!(b.next_set(0), Some(0));
        assert_eq!(b.next_set(1), Some(63));
        assert_eq!(b.next_set(64), Some(64));
        assert_eq!(b.next_set(65), Some(129));
        assert_eq!(b.next_set(130), None);
        assert_eq!(b.next_set(10_000), None);
        b.clear(64);
        assert_eq!(b.next_set(64), Some(129));
    }

    #[test]
    fn serialized_node_is_not_captured_again() {
        let mut t = Toy::new(8);
        let a = t.add_child(0, "a".into()).unwrap();
        t.begin();
        assert!(matches!(t.barrier.take(a), Some(T0::Slab)));
        t.mutate(a, |n| n.val = "a2".into());
        assert_eq!(t.barrier.pending_images(), 0);
        assert!(t.barrier.take(a).is_none());
        t.barrier.finish();
    }

    #[test]
    fn modified_freed_and_new_nodes() {
        let mut t = Toy::new(16);
        let a = t.add_child(0, "a".into()).unwrap();
        let b = t.add_child(0, "b".into()).unwrap();
        let c = t.add_child(a, "c".into()).unwrap();
        t.add_child(b, "d".into()).unwrap();
        let expect = dfs_vals(&t);
        t.begin();
        t.mutate(a, |n| n.val = "a-modifié".into()); // image T0
        t.mutate(a, |n| n.val = "a-modifié2".into()); // déjà capturé
        t.add_child(b, "né après T0".into()); // b capturé, nouveau ignoré
        t.del_leaf(b, 0); // d libéré : lu dans son image
        t.cut(a, 0); // coupe pendant la save : c reste lisible
        t.free_step(10); // en attente : rien n'est libéré
        assert!(t.slots[c as usize].is_some());
        let got = save_dfs(&mut t, |_| {});
        assert_eq!(got, expect);
        t.barrier.finish();
        t.free_step(10);
        assert!(t.slots[c as usize].is_none());
    }

    #[test]
    fn cut_before_t0_waits_and_is_not_dumped() {
        let mut t = Toy::new(64);
        let a = t.add_child(0, "a".into()).unwrap();
        for i in 0..20 {
            t.add_child(a, format!("x{i}")).unwrap();
        }
        t.add_child(0, "b".into()).unwrap();
        t.cut(0, 0); // UNLINK de `a` avant la save, rien n'est libéré
        let expect = dfs_vals(&t);
        t.begin();
        t.free_step(100); // en attente pendant la save
        assert!(t.slots[a as usize].is_some());
        assert_eq!(t.barrier.pending_images(), 0);
        assert_eq!(save_dfs(&mut t, |_| {}), expect);
        t.barrier.finish();
        while !t.jobs.is_empty() {
            t.free_step(3);
        }
        assert!(t.slots[a as usize].is_none());
    }

    /// Mutations aléatoires (dont DEL, UNLINK et libérations par tranches)
    /// entrelacées avec les pas de la save, comparées à l'état T0.
    #[test]
    fn random_interleaving_matches_snapshot() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut t = Toy::new(400);
        for round in 0..150 {
            for _ in 0..200 {
                random_op(&mut t, &mut rng);
            }
            let expect = dfs_vals(&t);
            t.begin();
            let mut ops = Rng(rng.next());
            let got = save_dfs(&mut t, |t| {
                for _ in 0..ops.below(5) {
                    random_op(t, &mut ops);
                }
            });
            assert_eq!(got, expect, "round {round}");
            t.barrier.finish();
            assert_eq!(t.barrier.pending_images(), 0);
        }
    }

    /// Abandon de save en cours de route : l'état reste cohérent et la
    /// save suivante est correcte.
    #[test]
    fn aborted_save_then_full_save() {
        let mut rng = Rng(42);
        let mut t = Toy::new(200);
        for _ in 0..300 {
            random_op(&mut t, &mut rng);
        }
        t.begin();
        for _ in 0..50 {
            random_op(&mut t, &mut rng);
        }
        t.barrier.finish(); // abandon
        let expect = dfs_vals(&t);
        t.begin();
        assert_eq!(save_dfs(&mut t, |_| {}), expect);
        t.barrier.finish();
    }
}
