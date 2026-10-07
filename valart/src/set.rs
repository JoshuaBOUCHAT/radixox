//! Set de valart : trie sur le hash des membres, un octet de hash par niveau.
//!
//! # Structure
//!
//! - Hash 64 bits seedé (`foldhash::quality`, seed globale à tous les Set).
//!   Octets `h[0..7]` = dispatch des niveaux 0..=6, octet `h[7]` = octet de
//!   contrôle (`ctrl`) stocké à côté de chaque clé.
//! - Un slot = `(radix, ctrl, clé, enfant)`. Une entrée s'arrête au **premier
//!   slot libre** de son chemin : la profondeur suit `log256(n)`, pas 7.
//!   Pas de compression de chemin, donc jamais de split : une entrée posée ne
//!   bouge plus (sauf remontée à la suppression).
//! - Lookup : au niveau `d`, slot de radix `h[d]` ; on ne déréférence la clé
//!   que si `ctrl` correspond (1 faux positif sur 256), sinon on descend.
//! - Niveau 7 (octets de dispatch épuisés) : l'enfant du slot n'est pas un
//!   node mais un bucket de [`CollisionBuckets`], hors de l'union, scanné
//!   linéairement. Pour y arriver il faut 8 clés empilées sur le même chemin
//!   de 56 bits : jamais en pratique avec une seed aléatoire.
//!
//! # Invariants
//!
//! 1. Un radix apparaît au plus une fois par node (inline ∪ overflow).
//! 2. Tout slot porte une clé (pas de slot « pointeur seul ») : à la
//!    suppression d'une clé dont le slot a un enfant, on remonte une feuille du
//!    sous-arbre. Une insertion = une lookup ratée + écriture là où elle s'est
//!    arrêtée.
//! 3. Un node non-racine n'est jamais vide (libéré dès qu'il se vide).
//! 4. `overflow.is_some()` ⇒ inline plein et overflow non vide.
//! 5. Racine : `parent_or_card` = cardinal du Set. Autres nodes :
//!    `parent_or_card` = idx du parent, `parent_radix` = radix du slot parent
//!    (back-pointer pour la compaction).

use std::hash::{BuildHasher, Hasher};
use std::mem::{ManuallyDrop, MaybeUninit};

use radixox_lib::prefetch::PreFetch;
use radixox_lib::shared_byte::SharedByte;

use crate::overflow::{NO_CHILD, swar_find};
use crate::{NodeType, SetOverflow, ValNode, ValTrees};

/// Entrées inline max (nœud sans enfant).
pub(crate) const INLINE_SET_LEN: usize = 11;
/// Zone partagée clés/enfants, en mots de 4 B : une clé = 2 mots, un enfant = 1.
const POOL_WORDS: usize = 2 * INLINE_SET_LEN;
/// Nombre de niveaux de dispatch (octets `h[0..7]`).
pub(crate) const DISPATCH_LEVELS: usize = 7;

#[derive(Clone, Copy)]
struct MemberHash(u64);

impl MemberHash {
    #[inline(always)]
    fn radix(self, depth: usize) -> u8 {
        debug_assert!(depth < DISPATCH_LEVELS);
        (self.0 >> (8 * depth)) as u8
    }

    #[inline(always)]
    fn ctrl(self) -> u8 {
        (self.0 >> 56) as u8
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Inline(usize),
    Overflow(usize),
}

/// Nœud de Set, 128 B.
///
/// `pool` est partagé entre les clés et les enfants : les clés occupent les
/// mots `[0, 2·count)` depuis le début, les idx d'enfants sont packés à la
/// fin, `[POOL_WORDS - K, POOL_WORDS)`, dans l'ordre des slots (rang). Seuls
/// les slots qui ont un enfant (bit dans `has_child`) paient ses 4 B.
/// Contrainte : `2·count + K ≤ 22`, donc 11 entrées sans enfant, 10 avec 1-2
/// enfants, 7 quand toutes ont un enfant (nœud interne plein).
#[repr(C, align(128))]
pub struct SetNode {
    pool: [MaybeUninit<u64>; INLINE_SET_LEN],
    prefix: [u8; INLINE_SET_LEN],
    ctrl: [u8; INLINE_SET_LEN],
    has_child: u16,
    overflow: Option<SetOverflow>,
    count: u8,
    parent_radix: u8,
    _pad: u8,
    // Même offset que dans `HashNode` (voir NODE_TYPE_OFFSET) : nécessaire
    // pour lire le tag depuis l'union sans connaître la variante active.
    pub(crate) node_type: NodeType,
    parent_or_card: u32,
}

impl PreFetch for SetNode {}

impl SetNode {
    fn new(parent_or_card: u32, parent_radix: u8) -> Self {
        Self {
            pool: [MaybeUninit::uninit(); INLINE_SET_LEN],
            prefix: [0; INLINE_SET_LEN],
            ctrl: [0; INLINE_SET_LEN],
            has_child: 0,
            overflow: None,
            count: 0,
            parent_radix,
            _pad: 0,
            node_type: NodeType::Set,
            parent_or_card,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.count as usize + self.overflow.as_ref().map_or(0, SetOverflow::len)
    }

    /// Valide grâce à l'invariant 4 : inline vide ⇒ overflow vide.
    #[inline]
    fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    fn slot_at(&self, n: usize) -> Slot {
        let count = self.count as usize;
        if n < count {
            Slot::Inline(n)
        } else {
            Slot::Overflow(n - count)
        }
    }

    // --- pool ---------------------------------------------------------------

    #[inline]
    fn inline_childs(&self) -> usize {
        self.has_child.count_ones() as usize
    }

    /// Mots libres dans le pool.
    #[inline]
    fn room(&self) -> usize {
        POOL_WORDS - 2 * self.count as usize - self.inline_childs()
    }

    #[inline]
    fn key_ptr(&self, i: usize) -> *const SharedByte {
        debug_assert!(i < INLINE_SET_LEN);
        unsafe { self.pool.as_ptr().add(i) as *const SharedByte }
    }

    #[inline]
    fn key_ptr_mut(&mut self, i: usize) -> *mut SharedByte {
        debug_assert!(i < INLINE_SET_LEN);
        unsafe { self.pool.as_mut_ptr().add(i) as *mut SharedByte }
    }

    #[inline]
    fn words(&self) -> *const u32 {
        self.pool.as_ptr() as *const u32
    }

    #[inline]
    fn words_mut(&mut self) -> *mut u32 {
        self.pool.as_mut_ptr() as *mut u32
    }

    #[inline]
    fn inline_has_child(&self, i: usize) -> bool {
        (self.has_child >> i) & 1 != 0
    }

    #[inline]
    fn inline_rank(&self, i: usize) -> usize {
        (self.has_child & ((1u16 << i) - 1)).count_ones() as usize
    }

    /// Insère un idx au rang `r` (le bit `has_child` est posé par l'appelant,
    /// après). Le bloc d'enfants grandit vers le bas.
    fn inline_child_insert(&mut self, r: usize, child: u32) {
        let k = self.inline_childs();
        debug_assert!(self.room() >= 1);
        let base = POOL_WORDS - k;
        unsafe {
            let words = self.words_mut();
            std::ptr::copy(words.add(base), words.add(base - 1), r);
            words.add(base - 1 + r).write(child);
        }
    }

    /// Retire l'idx au rang `r` (le bit est effacé par l'appelant, après).
    fn inline_child_remove(&mut self, r: usize) -> u32 {
        let k = self.inline_childs();
        debug_assert!(r < k);
        let base = POOL_WORDS - k;
        unsafe {
            let words = self.words_mut();
            let child = words.add(base + r).read();
            std::ptr::copy(words.add(base), words.add(base + 1), r);
            child
        }
    }

    // --- accès par slot -----------------------------------------------------

    #[inline]
    fn find(&self, radix: u8) -> Option<Slot> {
        let count = self.count as usize;
        // Octets au-delà de `count` périmés mais toujours à des positions plus
        // hautes que les valides : le premier match SWAR est valide ssi < count.
        let word = u64::from_le_bytes(self.prefix[..8].try_into().unwrap());
        match swar_find(word, radix) {
            Some(i) if i < count => return Some(Slot::Inline(i)),
            Some(_) => {}
            None => {
                if let Some(i) = (8..count).find(|&i| self.prefix[i] == radix) {
                    return Some(Slot::Inline(i));
                }
            }
        }
        self.overflow.as_ref()?.find(radix).map(Slot::Overflow)
    }

    #[inline]
    fn key(&self, slot: Slot) -> &SharedByte {
        match slot {
            Slot::Inline(i) => {
                debug_assert!(i < self.count as usize);
                unsafe { &*self.key_ptr(i) }
            }
            Slot::Overflow(i) => self.overflow_ref().key(i),
        }
    }

    #[inline]
    fn radix_of(&self, slot: Slot) -> u8 {
        match slot {
            Slot::Inline(i) => self.prefix[i],
            Slot::Overflow(i) => self.overflow_ref().radix(i),
        }
    }

    #[inline]
    fn ctrl_of(&self, slot: Slot) -> u8 {
        match slot {
            Slot::Inline(i) => self.ctrl[i],
            Slot::Overflow(i) => self.overflow_ref().ctrl(i),
        }
    }

    #[inline]
    fn child(&self, slot: Slot) -> u32 {
        match slot {
            Slot::Inline(i) => {
                if !self.inline_has_child(i) {
                    return NO_CHILD;
                }
                let base = POOL_WORDS - self.inline_childs();
                unsafe { self.words().add(base + self.inline_rank(i)).read() }
            }
            Slot::Overflow(i) => self.overflow_ref().child(i),
        }
    }

    /// Pose ou efface l'enfant d'un slot.
    ///
    /// ⚠ Poser un enfant sur un slot inline alors que le pool est plein
    /// déplace l'entrée dans l'overflow : `slot` n'est plus valide ensuite.
    /// Effacer un enfant libère de la place et peut rapatrier des entrées
    /// d'overflow : les `Slot::Overflow` ne sont plus valides ensuite.
    fn set_child(&mut self, slot: Slot, child: u32) {
        let i = match slot {
            Slot::Overflow(i) => {
                self.overflow_mut().set_child(i, child);
                // L'entrée devient peut-être rapatriable (2 mots suffisent).
                if child == NO_CHILD {
                    self.refill();
                }
                return;
            }
            Slot::Inline(i) => i,
        };
        match (self.inline_has_child(i), child != NO_CHILD) {
            (true, true) => {
                let base = POOL_WORDS - self.inline_childs();
                let r = self.inline_rank(i);
                unsafe { self.words_mut().add(base + r).write(child) }
            }
            (true, false) => {
                self.inline_child_remove(self.inline_rank(i));
                self.has_child &= !(1 << i);
                self.refill();
            }
            (false, true) if self.room() >= 1 => {
                self.inline_child_insert(self.inline_rank(i), child);
                self.has_child |= 1 << i;
            }
            (false, true) => {
                let radix = self.prefix[i];
                let (key, ctrl, _) = self.remove(slot);
                self.overflow
                    .get_or_insert_with(SetOverflow::new)
                    .add_value(radix, ctrl, key, child);
            }
            (false, false) => {}
        }
    }

    #[inline]
    fn overflow_ref(&self) -> &SetOverflow {
        // SAFETY: un `Slot::Overflow` n'est produit que si l'overflow existe.
        unsafe { self.overflow.as_ref().unwrap_unchecked() }
    }

    #[inline]
    fn overflow_mut(&mut self) -> &mut SetOverflow {
        // SAFETY: idem `overflow_ref`.
        unsafe { self.overflow.as_mut().unwrap_unchecked() }
    }

    /// Remplace la clé d'un slot (radix et enfant inchangés), retourne l'ancienne.
    fn replace_key(&mut self, slot: Slot, key: SharedByte, ctrl: u8) -> SharedByte {
        match slot {
            Slot::Inline(i) => {
                self.ctrl[i] = ctrl;
                unsafe { std::ptr::replace(self.key_ptr_mut(i), key) }
            }
            Slot::Overflow(i) => self.overflow_mut().replace_key(i, key, ctrl),
        }
    }

    /// Ajoute un slot. L'appelant garantit que `radix` est absent du node.
    fn push(&mut self, radix: u8, ctrl: u8, key: SharedByte, child: u32) {
        let need = 2 + (child != NO_CHILD) as usize;
        if self.room() >= need {
            self.push_inline(radix, ctrl, key, child);
        } else {
            self.overflow
                .get_or_insert_with(SetOverflow::new)
                .add_value(radix, ctrl, key, child);
        }
    }

    /// Ajoute en fin d'inline. L'appelant a vérifié la place.
    fn push_inline(&mut self, radix: u8, ctrl: u8, key: SharedByte, child: u32) {
        let i = self.count as usize;
        self.prefix[i] = radix;
        self.ctrl[i] = ctrl;
        unsafe { self.key_ptr_mut(i).write(key) };
        self.count += 1;
        if child != NO_CHILD {
            // Dernier slot ⇒ rang le plus haut.
            self.inline_child_insert(self.inline_childs(), child);
            self.has_child |= 1 << i;
        }
    }

    /// Peut-on rapatrier une entrée d'overflow dans l'inline ?
    fn refill_candidate(&self) -> Option<usize> {
        let overflow = self.overflow.as_ref()?;
        match self.room() {
            0 | 1 => None,
            2 => overflow.find_childless(),
            _ => Some(overflow.len() - 1),
        }
    }

    /// Invariant 4 : rapatrie des entrées d'overflow tant qu'il y a la place.
    fn refill(&mut self) {
        while let Some(j) = self.refill_candidate() {
            let overflow = self.overflow_mut();
            let (radix, ctrl, key, child) = overflow.swap_remove(j);
            if overflow.is_empty() {
                self.overflow = None;
            }
            self.push_inline(radix, ctrl, key, child);
        }
    }

    /// Retire un slot, retourne `(clé, ctrl, enfant)`. Maintient l'invariant
    /// 4 en rapatriant des entrées d'overflow dans la place libérée.
    fn remove(&mut self, slot: Slot) -> (SharedByte, u8, u32) {
        match slot {
            Slot::Overflow(i) => {
                let overflow = self.overflow_mut();
                let (_, ctrl, key, child) = overflow.swap_remove(i);
                if overflow.is_empty() {
                    self.overflow = None;
                }
                (key, ctrl, child)
            }
            Slot::Inline(i) => {
                let last = self.count as usize - 1;
                debug_assert!(i <= last);
                let child = self.child(slot);
                if child != NO_CHILD {
                    self.inline_child_remove(self.inline_rank(i));
                    self.has_child &= !(1 << i);
                }
                let key = unsafe { self.key_ptr(i).read() };
                let removed = (key, self.ctrl[i], child);
                if i != last {
                    self.prefix[i] = self.prefix[last];
                    self.ctrl[i] = self.ctrl[last];
                    unsafe {
                        let moved = self.key_ptr(last).read();
                        self.key_ptr_mut(i).write(moved);
                    }
                    // L'enfant du dernier slot change de rang.
                    if self.inline_has_child(last) {
                        let c = self.inline_child_remove(self.inline_rank(last));
                        self.has_child &= !(1 << last);
                        self.inline_child_insert(self.inline_rank(i), c);
                        self.has_child |= 1 << i;
                    }
                }
                self.count -= 1;
                self.refill();
                removed
            }
        }
    }
}

impl Drop for SetNode {
    fn drop(&mut self) {
        for i in 0..self.count as usize {
            unsafe { std::ptr::drop_in_place(self.key_ptr_mut(i)) };
        }
    }
}

/// Buckets de collision : atteints quand les 7 octets de dispatch sont
/// épuisés. Hors de l'union `ValNode` (cas quasi inexistant, aucune
/// optimisation de layout).
#[derive(Default)]
pub(crate) struct CollisionBuckets {
    buckets: Vec<Vec<SharedByte>>,
    free: Vec<u32>,
}

impl CollisionBuckets {
    fn alloc(&mut self, first: SharedByte) -> u32 {
        match self.free.pop() {
            Some(idx) => {
                self.buckets[idx as usize].push(first);
                idx
            }
            None => {
                self.buckets.push(vec![first]);
                (self.buckets.len() - 1) as u32
            }
        }
    }

    fn get(&self, idx: u32) -> &Vec<SharedByte> {
        &self.buckets[idx as usize]
    }

    fn get_mut(&mut self, idx: u32) -> &mut Vec<SharedByte> {
        &mut self.buckets[idx as usize]
    }

    fn release(&mut self, idx: u32) {
        self.buckets[idx as usize] = Vec::new();
        self.free.push(idx);
    }
}

impl ValTrees {
    #[inline(always)]
    fn member_hash(&self, member: &[u8]) -> MemberHash {
        let mut hasher = self.hasher.build_hasher();
        hasher.write(member);
        let h = hasher.finish();
        #[cfg(test)]
        let h = h & self.hash_mask;
        MemberHash(h)
    }

    #[inline(always)]
    fn set_node(&self, idx: u32) -> &SetNode {
        debug_assert!(self.nodes.is_occupied(idx));
        let node = unsafe { self.nodes.get_unchecked(idx) };
        debug_assert_eq!(node.tag(), NodeType::Set);
        unsafe { &node.set }
    }

    #[inline(always)]
    fn set_node_mut(&mut self, idx: u32) -> &mut SetNode {
        debug_assert!(self.nodes.is_occupied(idx));
        let node = unsafe { self.nodes.get_unchecked_mut(idx) };
        debug_assert_eq!(node.tag(), NodeType::Set);
        unsafe { &mut node.set }
    }

    fn alloc_set_node(&self, parent_or_card: u32, parent_radix: u8) -> u32 {
        self.nodes.insert(ValNode {
            set: ManuallyDrop::new(SetNode::new(parent_or_card, parent_radix)),
        })
    }

    fn free_set_node(&self, idx: u32) {
        drop(self.nodes.remove(idx));
    }

    #[inline]
    fn set_card_add(&mut self, root: u32, delta: i32) {
        let card = &mut self.set_node_mut(root).parent_or_card;
        *card = card.wrapping_add_signed(delta);
    }

    /// Crée un Set vide, retourne l'idx de sa racine (à stocker côté oxidart).
    pub fn set_create(&self) -> u32 {
        self.alloc_set_node(0, 0)
    }

    /// SCARD, O(1).
    pub fn set_card(&self, root: u32) -> u32 {
        self.set_node(root).parent_or_card
    }

    /// SISMEMBER.
    pub fn set_contains(&self, root: u32, member: &[u8]) -> bool {
        let h = self.member_hash(member);
        let ctrl = h.ctrl();
        let mut idx = root;
        for depth in 0..DISPATCH_LEVELS {
            let node = self.set_node(idx);
            let Some(slot) = node.find(h.radix(depth)) else {
                return false;
            };
            let child = node.child(slot);
            if child != NO_CHILD && depth + 1 < DISPATCH_LEVELS {
                self.set_node(child).prefetch();
            }
            if node.ctrl_of(slot) == ctrl && node.key(slot).as_slice() == member {
                return true;
            }
            if child == NO_CHILD {
                return false;
            }
            idx = child;
        }
        self.set_collisions
            .get(idx)
            .iter()
            .any(|k| k.as_slice() == member)
    }

    /// SADD d'un membre. Retourne `true` si le membre a été ajouté, `false`
    /// s'il était déjà présent (`member` est alors simplement droppé).
    pub fn set_add(&mut self, root: u32, member: SharedByte) -> bool {
        let h = self.member_hash(&member);
        let ctrl = h.ctrl();
        let mut idx = root;
        for depth in 0..DISPATCH_LEVELS {
            let radix = h.radix(depth);
            let node = self.set_node(idx);
            let Some(slot) = node.find(radix) else {
                self.set_node_mut(idx).push(radix, ctrl, member, NO_CHILD);
                self.set_card_add(root, 1);
                return true;
            };
            if node.ctrl_of(slot) == ctrl && node.key(slot).as_slice() == member.as_slice() {
                return false;
            }
            let child = node.child(slot);
            if child != NO_CHILD {
                idx = child;
                continue;
            }
            // Slot occupé par une autre clé, sans enfant : on crée le niveau
            // suivant sous lui. Rien ne bouge au-dessus.
            let new_child = if depth + 1 < DISPATCH_LEVELS {
                let c = self.alloc_set_node(idx, radix);
                self.set_node_mut(c)
                    .push(h.radix(depth + 1), ctrl, member, NO_CHILD);
                c
            } else {
                self.set_collisions.alloc(member)
            };
            self.set_node_mut(idx).set_child(slot, new_child);
            self.set_card_add(root, 1);
            return true;
        }
        let bucket = self.set_collisions.get_mut(idx);
        if bucket.iter().any(|k| k.as_slice() == member.as_slice()) {
            return false;
        }
        bucket.push(member);
        self.set_card_add(root, 1);
        true
    }

    /// SREM d'un membre. Retourne `true` s'il était présent.
    ///
    /// Si le Set devient vide, la racine reste allouée (`set_card == 0`) :
    /// à l'appelant de supprimer la clé et d'appeler [`Self::set_drop`].
    pub fn set_remove(&mut self, root: u32, member: &[u8]) -> bool {
        let h = self.member_hash(member);
        let ctrl = h.ctrl();
        let mut idx = root;
        let mut parent: Option<(u32, Slot)> = None;
        for depth in 0..DISPATCH_LEVELS {
            let node = self.set_node(idx);
            let Some(slot) = node.find(h.radix(depth)) else {
                return false;
            };
            let child = node.child(slot);
            if node.ctrl_of(slot) != ctrl || node.key(slot).as_slice() != member {
                if child == NO_CHILD {
                    return false;
                }
                parent = Some((idx, slot));
                idx = child;
                continue;
            }

            if child == NO_CHILD {
                drop(self.remove_leaf(idx, slot, parent));
            } else {
                // Invariant 2 : pas de trou, on remonte une feuille du
                // sous-arbre (toutes partagent h[0..=depth] avec ce slot).
                let (key, key_ctrl, child_emptied) = self.take_leaf(child, depth + 1);
                let node = self.set_node_mut(idx);
                drop(node.replace_key(slot, key, key_ctrl));
                if child_emptied {
                    node.set_child(slot, NO_CHILD);
                }
            }
            self.set_card_add(root, -1);
            return true;
        }

        let bucket = self.set_collisions.get_mut(idx);
        let Some(pos) = bucket.iter().position(|k| k.as_slice() == member) else {
            return false;
        };
        drop(bucket.swap_remove(pos));
        if bucket.is_empty() {
            self.release_bucket(idx, parent);
        }
        self.set_card_add(root, -1);
        true
    }

    /// SPOP : retire un membre pseudo-aléatoire, choisi par une descente
    /// aléatoire jusqu'à une feuille (`rnd` fourni par l'appelant). Ne
    /// déclenche jamais de remontée. Biaisé vers les feuilles, quasi uniforme
    /// quand le Set est grand (l'essentiel des entrées sont des feuilles).
    pub fn set_pop(&mut self, root: u32, rnd: u64) -> Option<SharedByte> {
        if self.set_card(root) == 0 {
            return None;
        }
        let mut rnd = rnd;
        let mut idx = root;
        let mut parent: Option<(u32, Slot)> = None;
        for _ in 0..DISPATCH_LEVELS {
            let node = self.set_node(idx);
            rnd = mix64(rnd);
            let slot = node.slot_at((rnd % node.len() as u64) as usize);
            let child = node.child(slot);
            if child == NO_CHILD {
                let key = self.remove_leaf(idx, slot, parent);
                self.set_card_add(root, -1);
                return Some(key);
            }
            parent = Some((idx, slot));
            idx = child;
        }
        let bucket = self.set_collisions.get_mut(idx);
        let key = bucket.pop().expect("bucket de collision vide");
        if bucket.is_empty() {
            self.release_bucket(idx, parent);
        }
        self.set_card_add(root, -1);
        Some(key)
    }

    /// SMEMBERS : parcours complet. Ordre = ordre du trie, sans signification.
    pub fn set_for_each(&self, root: u32, mut f: impl FnMut(&SharedByte)) {
        let mut stack = vec![(root, 0usize)];
        while let Some((idx, depth)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                self.set_collisions.get(idx).iter().for_each(&mut f);
                continue;
            }
            let node = self.set_node(idx);
            for n in 0..node.len() {
                let slot = node.slot_at(n);
                f(node.key(slot));
                let child = node.child(slot);
                if child != NO_CHILD {
                    stack.push((child, depth + 1));
                }
            }
        }
    }

    /// Octets des blocs overflow du Set. Ils sont alloués par MiMalloc en
    /// direct : invisibles pour l'allocateur global, comme le slab
    /// ([`Self::slab_bytes`]). Parcours complet, pour les benchs.
    pub fn set_overflow_bytes(&self, root: u32) -> usize {
        let mut total = 0;
        let mut stack = vec![(root, 0usize)];
        while let Some((idx, depth)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                continue;
            }
            let node = self.set_node(idx);
            total += node.overflow.as_ref().map_or(0, SetOverflow::block_bytes);
            for n in 0..node.len() {
                let child = node.child(node.slot_at(n));
                if child != NO_CHILD {
                    stack.push((child, depth + 1));
                }
            }
        }
        total
    }

    /// Libère tout le Set, racine comprise. `root` n'est plus valide après.
    pub fn set_drop(&mut self, root: u32) {
        let mut stack = vec![(root, 0usize)];
        while let Some((idx, depth)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                self.set_collisions.release(idx);
                continue;
            }
            let node = self.set_node(idx);
            for n in 0..node.len() {
                let child = node.child(node.slot_at(n));
                if child != NO_CHILD {
                    stack.push((child, depth + 1));
                }
            }
            self.free_set_node(idx);
        }
    }

    /// Retire un slot sans enfant. Libère le node s'il se vide (sauf racine)
    /// et efface le pointeur du parent.
    fn remove_leaf(&mut self, idx: u32, slot: Slot, parent: Option<(u32, Slot)>) -> SharedByte {
        let node = self.set_node_mut(idx);
        let (key, _, child) = node.remove(slot);
        debug_assert_eq!(child, NO_CHILD);
        if node.is_empty()
            && let Some((parent_idx, parent_slot)) = parent
        {
            self.free_set_node(idx);
            self.set_node_mut(parent_idx).set_child(parent_slot, NO_CHILD);
        }
        key
    }

    fn release_bucket(&mut self, idx: u32, parent: Option<(u32, Slot)>) {
        self.set_collisions.release(idx);
        let (parent_idx, parent_slot) = parent.expect("un bucket a toujours un parent");
        self.set_node_mut(parent_idx).set_child(parent_slot, NO_CHILD);
    }

    /// Détache une feuille du sous-arbre `idx` (niveau `depth`) et retourne
    /// `(clé, ctrl, sous-arbre_vidé)`. Si le sous-arbre est vidé, son node (ou
    /// bucket) est déjà libéré : l'appelant doit effacer son pointeur.
    fn take_leaf(&mut self, idx: u32, depth: usize) -> (SharedByte, u8, bool) {
        if depth == DISPATCH_LEVELS {
            let bucket = self.set_collisions.get_mut(idx);
            let key = bucket.pop().expect("bucket de collision vide");
            let emptied = bucket.is_empty();
            if emptied {
                self.set_collisions.release(idx);
            }
            let ctrl = self.member_hash(&key).ctrl();
            return (key, ctrl, emptied);
        }
        let node = self.set_node(idx);
        // Dernier slot : retrait sans rapatriement depuis l'overflow.
        let slot = node.slot_at(node.len() - 1);
        let child = node.child(slot);
        if child == NO_CHILD {
            let node = self.set_node_mut(idx);
            let (key, ctrl, _) = node.remove(slot);
            let emptied = node.is_empty();
            if emptied {
                self.free_set_node(idx);
            }
            (key, ctrl, emptied)
        } else {
            let (key, ctrl, child_emptied) = self.take_leaf(child, depth + 1);
            if child_emptied {
                self.set_node_mut(idx).set_child(slot, NO_CHILD);
            }
            (key, ctrl, false)
        }
    }

    /// Vérifie tous les invariants du Set et retourne son cardinal recompté.
    #[cfg(test)]
    pub(crate) fn set_check(&self, root: u32) -> usize {
        let mut total = 0;
        let mut stack = vec![(root, 0usize, None::<(u32, u8)>)];
        while let Some((idx, depth, parent)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                let bucket = self.set_collisions.get(idx);
                assert!(!bucket.is_empty(), "bucket vide encore référencé");
                total += bucket.len();
                continue;
            }
            let node = self.set_node(idx);
            match parent {
                None => {}
                Some((parent_idx, parent_radix)) => {
                    assert!(!node.is_empty(), "node non-racine vide");
                    assert_eq!(node.parent_or_card, parent_idx, "back-pointer parent");
                    assert_eq!(node.parent_radix, parent_radix, "back-pointer radix");
                }
            }
            assert!(2 * node.count as usize + node.inline_childs() <= POOL_WORDS);
            assert_eq!(node.has_child >> node.count, 0, "bit has_child hors des slots");
            if let Some(overflow) = &node.overflow {
                assert!(!overflow.is_empty(), "overflow vide alloué");
                assert!(node.refill_candidate().is_none(), "place inline non utilisée");
                assert_eq!(
                    overflow.childs_len(),
                    (0..overflow.len()).filter(|&i| overflow.has_child(i)).count()
                );
            }
            let mut seen = [false; 256];
            for n in 0..node.len() {
                let slot = node.slot_at(n);
                let radix = node.radix_of(slot);
                assert!(!seen[radix as usize], "radix dupliqué dans un node");
                seen[radix as usize] = true;
                let h = self.member_hash(node.key(slot));
                assert_eq!(h.radix(depth), radix, "clé mal placée");
                assert_eq!(h.ctrl(), node.ctrl_of(slot), "ctrl incohérent");
                total += 1;
                let child = node.child(slot);
                if child != NO_CHILD {
                    stack.push((child, depth + 1, Some((idx, radix))));
                }
            }
        }
        assert_eq!(total, self.set_card(root) as usize, "cardinal incohérent");
        total
    }
}

#[inline(always)]
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn trees() -> ValTrees {
        ValTrees::with_seed(0xDEAD_BEEF, 1 << 22)
    }

    fn sb(s: impl AsRef<[u8]>) -> SharedByte {
        SharedByte::from_slice(s)
    }

    #[test]
    fn layout() {
        assert_eq!(size_of::<SetNode>(), 128);
        assert_eq!(std::mem::offset_of!(SetNode, node_type), crate::NODE_TYPE_OFFSET);
    }

    /// Nœud seul : modèle de référence `radix → (clé, enfant)`, enfants posés,
    /// effacés et slots retirés dans le désordre. Exerce la relocation vers
    /// l'overflow (pool plein) et le rapatriement (place libérée).
    #[test]
    fn node_child_packing() {
        use std::collections::HashMap;
        let mut node = SetNode::new(0, 0);
        let mut model: HashMap<u8, (Vec<u8>, u32)> = HashMap::new();
        for r in 0..40u8 {
            node.push(r, r, sb([r]), NO_CHILD);
            model.insert(r, (vec![r], NO_CHILD));
        }
        assert_eq!(node.count as usize, INLINE_SET_LEN);
        let check = |node: &SetNode, model: &HashMap<u8, (Vec<u8>, u32)>| {
            assert_eq!(node.len(), model.len());
            assert!(2 * node.count as usize + node.inline_childs() <= POOL_WORDS);
            if node.overflow.is_some() {
                assert!(node.refill_candidate().is_none());
            }
            for (&r, (key, child)) in model {
                let slot = node.find(r).expect("radix perdu");
                assert_eq!(node.key(slot).as_slice(), key.as_slice());
                assert_eq!(node.ctrl_of(slot), r);
                assert_eq!(node.child(slot), *child, "radix {r}");
            }
        };
        let mut rnd = 1u64;
        for step in 0..5_000u32 {
            rnd = mix64(rnd);
            let r = (rnd % 40) as u8;
            match (rnd >> 40) % 3 {
                0 | 1 if model.contains_key(&r) => {
                    let child = if (rnd >> 50) % 3 == 0 { NO_CHILD } else { step };
                    node.set_child(node.find(r).unwrap(), child);
                    model.get_mut(&r).unwrap().1 = child;
                }
                _ => match model.remove(&r) {
                    Some((key, child)) => {
                        let (k, ctrl, c) = node.remove(node.find(r).unwrap());
                        assert_eq!((k.as_slice(), ctrl, c), (key.as_slice(), r, child));
                    }
                    None => {
                        node.push(r, r, sb([r]), NO_CHILD);
                        model.insert(r, (vec![r], NO_CHILD));
                    }
                },
            }
            check(&node, &model);
        }
    }

    #[test]
    fn add_contains_remove_basic() {
        let mut t = trees();
        let root = t.set_create();
        assert!(t.set_add(root, sb("a")));
        assert!(!t.set_add(root, sb("a")));
        assert!(t.set_add(root, sb("b")));
        assert_eq!(t.set_card(root), 2);
        assert!(t.set_contains(root, b"a"));
        assert!(t.set_contains(root, b"b"));
        assert!(!t.set_contains(root, b"c"));
        assert!(t.set_remove(root, b"a"));
        assert!(!t.set_remove(root, b"a"));
        assert!(!t.set_contains(root, b"a"));
        assert_eq!(t.set_card(root), 1);
        t.set_check(root);
        t.set_drop(root);
    }

    /// Les cas qui cassaient le dispatch par octets de clé.
    #[test]
    fn prefix_keys() {
        let mut t = trees();
        let root = t.set_create();
        let keys = ["10", "1", "user:1", "user", "", "field0", "field1", "field10"];
        for k in keys {
            assert!(t.set_add(root, sb(k)), "{k}");
        }
        for k in keys {
            assert!(t.set_contains(root, k.as_bytes()), "{k}");
        }
        t.set_check(root);
    }

    #[test]
    fn large_set_matches_reference() {
        let mut t = trees();
        let root = t.set_create();
        let mut reference = HashSet::new();
        for i in 0..100_000u32 {
            let k = format!("member:{i}");
            assert!(t.set_add(root, sb(&k)));
            reference.insert(k);
        }
        t.set_check(root);
        for i in (0..100_000u32).step_by(3) {
            let k = format!("member:{i}");
            assert!(t.set_remove(root, k.as_bytes()));
            reference.remove(&k);
        }
        t.set_check(root);
        for i in 0..100_000u32 {
            let k = format!("member:{i}");
            assert_eq!(t.set_contains(root, k.as_bytes()), reference.contains(&k), "{k}");
        }
        let mut seen = HashSet::new();
        t.set_for_each(root, |k| assert!(seen.insert(k.as_str().unwrap().to_owned())));
        assert_eq!(seen, reference);
    }

    /// Opérations aléatoires mêlées, invariants vérifiés régulièrement.
    #[test]
    fn random_ops_against_reference() {
        let mut t = trees();
        let root = t.set_create();
        let mut reference = HashSet::new();
        let mut rnd = 42u64;
        for step in 0..200_000u32 {
            rnd = mix64(rnd);
            let k = format!("k{}", rnd % 5_000);
            match (rnd >> 32) % 4 {
                0 | 1 => assert_eq!(t.set_add(root, sb(&k)), reference.insert(k.clone())),
                2 => assert_eq!(t.set_remove(root, k.as_bytes()), reference.remove(&k)),
                _ => {
                    if let Some(popped) = t.set_pop(root, rnd) {
                        assert!(reference.remove(popped.as_str().unwrap()));
                    } else {
                        assert!(reference.is_empty());
                    }
                }
            }
            if step % 10_000 == 0 {
                assert_eq!(t.set_check(root), reference.len());
            }
        }
        assert_eq!(t.set_check(root), reference.len());
        while let Some(k) = t.set_pop(root, rnd) {
            rnd = mix64(rnd);
            assert!(reference.remove(k.as_str().unwrap()));
        }
        assert!(reference.is_empty());
        assert_eq!(t.set_card(root), 0);
        t.set_check(root);
    }

    /// Hash dégradé à 0 : tout le monde collisionne. Exerce la chaîne des 7
    /// niveaux, les buckets de collision et la remontée depuis un bucket.
    #[test]
    fn full_collisions() {
        let mut t = trees();
        t.hash_mask = 0;
        let root = t.set_create();
        for i in 0..50u32 {
            assert!(t.set_add(root, sb(i.to_string())));
        }
        assert!(!t.set_add(root, sb("7")));
        t.set_check(root);
        for i in 0..50u32 {
            assert!(t.set_contains(root, i.to_string().as_bytes()));
        }
        // Retrait par le haut de la chaîne : remontées successives.
        for i in 0..50u32 {
            assert!(t.set_remove(root, i.to_string().as_bytes()), "{i}");
            t.set_check(root);
            assert!(!t.set_contains(root, i.to_string().as_bytes()));
        }
        assert_eq!(t.set_card(root), 0);
        assert!(t.set_collisions.free.len() >= 1);
    }

    /// Hash dégradé à 12 bits : beaucoup de collisions partielles, nodes
    /// profonds, overflow plein aux premiers niveaux.
    #[test]
    fn partial_collisions() {
        let mut t = trees();
        t.hash_mask = 0x0F_FF;
        let root = t.set_create();
        let mut reference = HashSet::new();
        let mut rnd = 7u64;
        for _ in 0..30_000 {
            rnd = mix64(rnd);
            let k = format!("{}", rnd % 3_000);
            if rnd >> 63 == 0 {
                assert_eq!(t.set_add(root, sb(&k)), reference.insert(k));
            } else {
                assert_eq!(t.set_remove(root, k.as_bytes()), reference.remove(&k));
            }
        }
        assert_eq!(t.set_check(root), reference.len());
    }

    #[test]
    fn drop_releases_members() {
        let mut t = trees();
        let shared = sb("shared");
        let root = t.set_create();
        assert!(t.set_add(root, shared.clone()));
        for i in 0..1_000u32 {
            t.set_add(root, sb(i.to_le_bytes()));
        }
        assert_eq!(shared.rc(), 2);
        t.set_drop(root);
        assert_eq!(shared.rc(), 1);
    }

    #[test]
    fn independent_sets() {
        let mut t = trees();
        let a = t.set_create();
        let b = t.set_create();
        t.set_add(a, sb("x"));
        t.set_add(b, sb("y"));
        assert!(t.set_contains(a, b"x") && !t.set_contains(a, b"y"));
        assert!(t.set_contains(b, b"y") && !t.set_contains(b, b"x"));
    }
}
