//! Hash de valart : même structure que le Set ([`crate::set`]), chaque entrée
//! porte un couple `(champ, valeur)`.
//!
//! - Trie sur le hash du champ, un octet par niveau (`h[0..7]`), `h[7]` en
//!   octet de contrôle. Une entrée s'arrête au premier slot libre ; pas de
//!   split, remontée d'une feuille à la suppression.
//! - Au-delà des 7 niveaux : bucket de collision hors de l'union.
//! - Mêmes invariants que le Set (voir `set.rs`). Racine : `parent_or_card`
//!   = nombre de champs (HLEN).

use std::mem::{ManuallyDrop, MaybeUninit};

use radixox_lib::prefetch::PreFetch;
use radixox_lib::shared_byte::SharedByte;

use crate::common::{DISPATCH_LEVELS, Slot, mix64};
use crate::overflow::{NO_CHILD, swar_find};
use crate::{HashOverflow, NodeType, ValNode, ValNodeImage, ValSer, ValTrees, prefetch_slot};
use radixox_lib::save_barrier::T0;
use radixox_lib::varint::{put_bytes, put_varint};

/// Entrées inline max (nœud sans enfant).
pub(crate) const INLINE_HASH_LEN: usize = 6;
/// Zone partagée entrées/enfants, en mots de 4 B : une entrée (clé + valeur)
/// = 4 mots, un enfant = 1.
const POOL_WORDS: usize = 4 * INLINE_HASH_LEN;

/// Nœud de Hash, 128 B.
///
/// `pool` est partagé entre les entrées et les enfants : l'entrée `i` occupe
/// `pool[2i]` (champ) et `pool[2i+1]` (valeur), les idx d'enfants sont
/// packés à la fin, `[POOL_WORDS - K, POOL_WORDS)`, par rang. Contrainte :
/// `4·count + K ≤ 24`, donc 6 entrées sans enfant, 5 avec 1 à 4 enfants, 4
/// quand toutes en ont un.
#[repr(C, align(128))]
pub struct HashNode {
    pool: [MaybeUninit<u64>; 2 * INLINE_HASH_LEN],
    prefix: [u8; INLINE_HASH_LEN],
    ctrl: [u8; INLINE_HASH_LEN],
    has_child: u8,
    overflow: Option<HashOverflow>,
    count: u8,
    parent_radix: u8,
    _pad: u8,
    // Offset commun à toutes les variantes de `ValNode` (NODE_TYPE_OFFSET).
    pub(crate) node_type: NodeType,
    parent_or_card: u32,
}

impl PreFetch for HashNode {}

impl HashNode {
    /// Image T0 pour la save (SAVE_DESIGN.md §2.A) : copie bit à bit, bloc
    /// overflow copié. Les entrées ne sont pas clonées : valart diffère
    /// toutes ses libérations pendant une save. Libérer avec `free_image`.
    pub(crate) fn image_copy(&self) -> ManuallyDrop<Self> {
        // SAFETY: copie bit à bit jamais droppée ; son overflow est remplacé
        // par une copie à elle avant toute utilisation.
        let mut copy = ManuallyDrop::new(unsafe { std::ptr::read(self) });
        if let Some(ov) = &self.overflow {
            let ov = ManuallyDrop::into_inner(ov.shallow_copy());
            unsafe { std::ptr::write(&mut copy.overflow, Some(ov)) };
        }
        copy
    }

    /// Prefetch des blocs des 8 premières entrées (save).
    pub(crate) fn prefetch_entries(&self) {
        for n in 0..self.len().min(8) {
            let slot = self.slot_at(n);
            self.key(slot).prefetch();
            self.val(slot).prefetch();
        }
    }

    /// Libère le bloc overflow d'une image, sans toucher aux entrées.
    pub(crate) fn free_image(img: &mut ManuallyDrop<Self>) {
        if let Some(ov) = img.overflow.take() {
            HashOverflow::free_shallow(ManuallyDrop::new(ov));
        }
    }
}

impl HashNode {
    fn new(parent_or_card: u32, parent_radix: u8) -> Self {
        Self {
            pool: [MaybeUninit::uninit(); 2 * INLINE_HASH_LEN],
            prefix: [0; INLINE_HASH_LEN],
            ctrl: [0; INLINE_HASH_LEN],
            has_child: 0,
            overflow: None,
            count: 0,
            parent_radix,
            _pad: 0,
            node_type: NodeType::Hash,
            parent_or_card,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.count as usize + self.overflow.as_ref().map_or(0, HashOverflow::len)
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
        POOL_WORDS - 4 * self.count as usize - self.inline_childs()
    }

    #[inline]
    fn key_ptr(&self, i: usize) -> *const SharedByte {
        debug_assert!(i < INLINE_HASH_LEN);
        unsafe { self.pool.as_ptr().add(2 * i) as *const SharedByte }
    }

    #[inline]
    fn key_ptr_mut(&mut self, i: usize) -> *mut SharedByte {
        debug_assert!(i < INLINE_HASH_LEN);
        unsafe { self.pool.as_mut_ptr().add(2 * i) as *mut SharedByte }
    }

    #[inline]
    fn val_ptr(&self, i: usize) -> *const SharedByte {
        debug_assert!(i < INLINE_HASH_LEN);
        unsafe { self.pool.as_ptr().add(2 * i + 1) as *const SharedByte }
    }

    #[inline]
    fn val_ptr_mut(&mut self, i: usize) -> *mut SharedByte {
        debug_assert!(i < INLINE_HASH_LEN);
        unsafe { self.pool.as_mut_ptr().add(2 * i + 1) as *mut SharedByte }
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
        (self.has_child & ((1u8 << i) - 1)).count_ones() as usize
    }

    /// Insère un idx au rang `r` (bit `has_child` posé par l'appelant, après).
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

    /// Retire l'idx au rang `r` (bit effacé par l'appelant, après).
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
        // Octets 6 et 7 à zéro, octets ≥ count périmés : toujours au-dessus
        // des positions valides, donc le premier match est valide ssi < count.
        let mut bytes = [0u8; 8];
        bytes[..INLINE_HASH_LEN].copy_from_slice(&self.prefix);
        if let Some(i) = swar_find(u64::from_le_bytes(bytes), radix) {
            if i < self.count as usize {
                return Some(Slot::Inline(i));
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
    fn val(&self, slot: Slot) -> &SharedByte {
        match slot {
            Slot::Inline(i) => {
                debug_assert!(i < self.count as usize);
                unsafe { &*self.val_ptr(i) }
            }
            Slot::Overflow(i) => self.overflow_ref().val(i),
        }
    }

    #[inline]
    fn val_mut(&mut self, slot: Slot) -> &mut SharedByte {
        match slot {
            Slot::Inline(i) => {
                debug_assert!(i < self.count as usize);
                unsafe { &mut *self.val_ptr_mut(i) }
            }
            Slot::Overflow(i) => self.overflow_mut().val_mut(i),
        }
    }

    #[cfg(test)]
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

    /// Pose ou efface l'enfant d'un slot. Mêmes mises en garde que
    /// `SetNode::set_child` : poser peut déplacer l'entrée dans l'overflow,
    /// effacer peut rapatrier des entrées d'overflow.
    fn set_child(&mut self, slot: Slot, child: u32) {
        let i = match slot {
            Slot::Overflow(i) => {
                self.overflow_mut().set_child(i, child);
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
                let (key, val, ctrl, _) = self.remove(slot);
                self.overflow
                    .get_or_insert_with(HashOverflow::new)
                    .add_value(radix, ctrl, key, val, child);
            }
            (false, false) => {}
        }
    }

    #[inline]
    fn overflow_ref(&self) -> &HashOverflow {
        // SAFETY: un `Slot::Overflow` n'est produit que si l'overflow existe.
        unsafe { self.overflow.as_ref().unwrap_unchecked() }
    }

    #[inline]
    fn overflow_mut(&mut self) -> &mut HashOverflow {
        // SAFETY: idem `overflow_ref`.
        unsafe { self.overflow.as_mut().unwrap_unchecked() }
    }

    /// Remplace le champ, la valeur et le ctrl d'un slot (radix et enfant
    /// inchangés), retourne l'ancien couple.
    fn replace_entry(
        &mut self,
        slot: Slot,
        key: SharedByte,
        val: SharedByte,
        ctrl: u8,
    ) -> (SharedByte, SharedByte) {
        match slot {
            Slot::Inline(i) => {
                self.ctrl[i] = ctrl;
                unsafe {
                    (
                        std::ptr::replace(self.key_ptr_mut(i), key),
                        std::ptr::replace(self.val_ptr_mut(i), val),
                    )
                }
            }
            Slot::Overflow(i) => self.overflow_mut().replace_entry(i, key, val, ctrl),
        }
    }

    /// Ajoute un slot. L'appelant garantit que `radix` est absent du node.
    fn push(&mut self, radix: u8, ctrl: u8, key: SharedByte, val: SharedByte, child: u32) {
        let need = 4 + (child != NO_CHILD) as usize;
        if self.room() >= need {
            self.push_inline(radix, ctrl, key, val, child);
        } else {
            self.overflow
                .get_or_insert_with(HashOverflow::new)
                .add_value(radix, ctrl, key, val, child);
        }
    }

    /// Ajoute en fin d'inline. L'appelant a vérifié la place.
    fn push_inline(&mut self, radix: u8, ctrl: u8, key: SharedByte, val: SharedByte, child: u32) {
        let i = self.count as usize;
        self.prefix[i] = radix;
        self.ctrl[i] = ctrl;
        unsafe {
            self.key_ptr_mut(i).write(key);
            self.val_ptr_mut(i).write(val);
        }
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
            0..=3 => None,
            4 => overflow.find_childless(),
            _ => Some(overflow.len() - 1),
        }
    }

    /// Invariant 4 : rapatrie des entrées d'overflow tant qu'il y a la place.
    fn refill(&mut self) {
        while let Some(j) = self.refill_candidate() {
            let overflow = self.overflow_mut();
            let (radix, ctrl, key, val, child) = overflow.swap_remove(j);
            if overflow.is_empty() {
                self.overflow = None;
            }
            self.push_inline(radix, ctrl, key, val, child);
        }
    }

    /// Retire un slot, retourne `(champ, valeur, ctrl, enfant)`.
    fn remove(&mut self, slot: Slot) -> (SharedByte, SharedByte, u8, u32) {
        match slot {
            Slot::Overflow(i) => {
                let overflow = self.overflow_mut();
                let (_, ctrl, key, val, child) = overflow.swap_remove(i);
                if overflow.is_empty() {
                    self.overflow = None;
                }
                (key, val, ctrl, child)
            }
            Slot::Inline(i) => {
                let last = self.count as usize - 1;
                debug_assert!(i <= last);
                let child = self.child(slot);
                if child != NO_CHILD {
                    self.inline_child_remove(self.inline_rank(i));
                    self.has_child &= !(1 << i);
                }
                let (key, val) = unsafe { (self.key_ptr(i).read(), self.val_ptr(i).read()) };
                let removed = (key, val, self.ctrl[i], child);
                if i != last {
                    self.prefix[i] = self.prefix[last];
                    self.ctrl[i] = self.ctrl[last];
                    unsafe {
                        let (k, v) = (self.key_ptr(last).read(), self.val_ptr(last).read());
                        self.key_ptr_mut(i).write(k);
                        self.val_ptr_mut(i).write(v);
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

impl Drop for HashNode {
    fn drop(&mut self) {
        for i in 0..self.count as usize {
            unsafe {
                std::ptr::drop_in_place(self.key_ptr_mut(i));
                std::ptr::drop_in_place(self.val_ptr_mut(i));
            }
        }
    }
}

/// Emplacement d'un champ trouvé.
#[derive(Clone, Copy)]
enum Loc {
    Node(u32, Slot),
    Bucket(u32, usize),
}

impl ValTrees {
    #[inline(always)]
    fn hash_node(&self, idx: u32) -> &HashNode {
        debug_assert!(self.nodes.is_occupied(idx));
        let node = unsafe { self.nodes.get_unchecked(idx) };
        debug_assert_eq!(node.tag(), NodeType::Hash);
        unsafe { &node.hash }
    }

    #[inline(always)]
    fn hash_node_mut(&mut self, idx: u32) -> &mut HashNode {
        debug_assert!(self.nodes.is_occupied(idx));
        self.before_write(idx);
        let node = unsafe { self.nodes.get_unchecked_mut(idx) };
        debug_assert_eq!(node.tag(), NodeType::Hash);
        unsafe { &mut node.hash }
    }

    fn alloc_hash_node(&self, parent_or_card: u32, parent_radix: u8) -> u32 {
        self.nodes.insert(ValNode {
            hash: ManuallyDrop::new(HashNode::new(parent_or_card, parent_radix)),
        })
    }

    /// Hors `hash_drop`, un nœud libéré est toujours vide (ni entrée ni
    /// overflow) : rien à différer.
    fn free_hash_node(&mut self, idx: u32) {
        self.before_write(idx);
        drop(self.nodes.remove(idx));
    }

    #[inline]
    fn hash_len_add(&mut self, root: u32, delta: i32) {
        let len = &mut self.hash_node_mut(root).parent_or_card;
        *len = len.wrapping_add_signed(delta);
    }

    /// Crée un Hash vide, retourne l'idx de sa racine (à stocker côté oxidart).
    pub fn hash_create(&self) -> u32 {
        self.alloc_hash_node(0, 0)
    }

    /// HLEN, O(1).
    pub fn hash_len(&self, root: u32) -> u32 {
        self.hash_node(root).parent_or_card
    }

    fn hash_locate(&self, root: u32, field: &[u8]) -> Option<Loc> {
        let h = self.member_hash(field);
        let ctrl = h.ctrl();
        let mut idx = root;
        for depth in 0..DISPATCH_LEVELS {
            let node = self.hash_node(idx);
            let slot = node.find(h.radix(depth))?;
            let child = node.child(slot);
            if child != NO_CHILD && depth + 1 < DISPATCH_LEVELS {
                self.hash_node(child).prefetch();
            }
            if node.ctrl_of(slot) == ctrl && node.key(slot).as_slice() == field {
                return Some(Loc::Node(idx, slot));
            }
            if child == NO_CHILD {
                return None;
            }
            idx = child;
        }
        self.hash_collisions
            .get(idx)
            .iter()
            .position(|(k, _)| k.as_slice() == field)
            .map(|i| Loc::Bucket(idx, i))
    }

    /// HGET.
    pub fn hash_get(&self, root: u32, field: &[u8]) -> Option<&SharedByte> {
        match self.hash_locate(root, field)? {
            Loc::Node(idx, slot) => Some(self.hash_node(idx).val(slot)),
            Loc::Bucket(b, i) => Some(&self.hash_collisions.get(b)[i].1),
        }
    }

    /// HEXISTS.
    pub fn hash_contains(&self, root: u32, field: &[u8]) -> bool {
        self.hash_locate(root, field).is_some()
    }

    /// HSET d'un champ. Retourne `true` si le champ est nouveau (à compter
    /// dans la réponse de HSET). L'ancienne valeur n'est pas ramenée : lire
    /// avec [`Self::hash_get`] avant si besoin.
    pub fn hash_set(&mut self, root: u32, field: SharedByte, val: SharedByte) -> bool {
        let h = self.member_hash(&field);
        let ctrl = h.ctrl();
        let mut idx = root;
        for depth in 0..DISPATCH_LEVELS {
            let radix = h.radix(depth);
            let node = self.hash_node(idx);
            let Some(slot) = node.find(radix) else {
                self.hash_node_mut(idx).push(radix, ctrl, field, val, NO_CHILD);
                self.hash_len_add(root, 1);
                return true;
            };
            if node.ctrl_of(slot) == ctrl && node.key(slot).as_slice() == field.as_slice() {
                let old = std::mem::replace(self.hash_node_mut(idx).val_mut(slot), val);
                self.dispose(idx, old);
                return false;
            }
            let child = node.child(slot);
            if child != NO_CHILD {
                idx = child;
                continue;
            }
            let new_child = if depth + 1 < DISPATCH_LEVELS {
                let c = self.alloc_hash_node(idx, radix);
                self.hash_node_mut(c)
                    .push(h.radix(depth + 1), ctrl, field, val, NO_CHILD);
                c
            } else {
                self.hash_collisions.alloc((field, val))
            };
            self.hash_node_mut(idx).set_child(slot, new_child);
            self.hash_len_add(root, 1);
            return true;
        }
        let bucket = self.hash_collisions.get_mut(idx);
        if let Some(entry) = bucket.iter_mut().find(|(k, _)| k.as_slice() == field.as_slice()) {
            entry.1 = val;
            return false;
        }
        bucket.push((field, val));
        self.hash_len_add(root, 1);
        true
    }

    /// HDEL d'un champ. Retourne `true` s'il était présent (la valeur n'est
    /// pas ramenée).
    ///
    /// Si le Hash devient vide, la racine reste allouée (`hash_len == 0`) :
    /// à l'appelant de supprimer la clé et d'appeler [`Self::hash_drop`].
    pub fn hash_remove(&mut self, root: u32, field: &[u8]) -> bool {
        let h = self.member_hash(field);
        let ctrl = h.ctrl();
        let mut idx = root;
        let mut parent: Option<(u32, Slot)> = None;
        for depth in 0..DISPATCH_LEVELS {
            let node = self.hash_node(idx);
            let Some(slot) = node.find(h.radix(depth)) else {
                return false;
            };
            let child = node.child(slot);
            if node.ctrl_of(slot) != ctrl || node.key(slot).as_slice() != field {
                if child == NO_CHILD {
                    return false;
                }
                parent = Some((idx, slot));
                idx = child;
                continue;
            }

            let (old_key, old_val) = if child == NO_CHILD {
                self.hash_remove_leaf(idx, slot, parent)
            } else {
                // Invariant 2 : on remonte une feuille du sous-arbre.
                let (key, val, key_ctrl, child_emptied) = self.hash_take_leaf(child, depth + 1);
                let node = self.hash_node_mut(idx);
                let old = node.replace_entry(slot, key, val, key_ctrl);
                if child_emptied {
                    node.set_child(slot, NO_CHILD);
                }
                old
            };
            self.dispose(idx, old_key);
            self.dispose(idx, old_val);
            self.hash_len_add(root, -1);
            return true;
        }

        let bucket = self.hash_collisions.get_mut(idx);
        let Some(pos) = bucket.iter().position(|(k, _)| k.as_slice() == field) else {
            return false;
        };
        drop(bucket.swap_remove(pos));
        if bucket.is_empty() {
            self.hash_release_bucket(idx, parent);
        }
        self.hash_len_add(root, -1);
        true
    }

    /// HRANDFIELD : un champ pseudo-aléatoire (descente aléatoire jusqu'à une
    /// feuille, mêmes biais que `set_pop`), sans le retirer.
    pub fn hash_random(&self, root: u32, rnd: u64) -> Option<(&SharedByte, &SharedByte)> {
        if self.hash_len(root) == 0 {
            return None;
        }
        let mut rnd = rnd;
        let mut idx = root;
        for _ in 0..DISPATCH_LEVELS {
            let node = self.hash_node(idx);
            rnd = mix64(rnd);
            let slot = node.slot_at((rnd % node.len() as u64) as usize);
            let child = node.child(slot);
            if child == NO_CHILD {
                return Some((node.key(slot), node.val(slot)));
            }
            idx = child;
        }
        let bucket = self.hash_collisions.get(idx);
        let (k, v) = &bucket[(mix64(rnd) % bucket.len() as u64) as usize];
        Some((k, v))
    }

    /// Démarre la sérialisation du conteneur `root` pendant une save : écrit
    /// sa cardinalité T0 et prépare `ser`. Format : `varint nb | (varint len | champ | varint len | valeur)*`.
    pub fn hash_ser_begin(&mut self, root: u32, ser: &mut ValSer, out: &mut Vec<u8>) {
        let card = match self.save.peek(root) {
            Some(T0::Slab) => self.hash_node(root).parent_or_card,
            Some(T0::Image(ValNodeImage::Hash(n))) => n.parent_or_card,
            _ => panic!("racine valart {root} absente de la vue T0"),
        };
        put_varint(out, card as u64);
        ser.stack.clear();
        ser.stack.push((root, 0));
    }

    /// Sérialise des nœuds du conteneur jusqu'à épuiser `budget` (1 par
    /// nœud + 1 par entrée ; un nœud est toujours traité en entier).
    /// Retourne `true` quand le conteneur est fini.
    ///
    /// Par nœud : prefetch des entrées, empilement des enfants, prefetch du
    /// nouveau haut de pile, puis écriture des entrées (en cache).
    pub fn hash_ser_step(&mut self, ser: &mut ValSer, out: &mut Vec<u8>, budget: &mut u32) -> bool {
        let Self { nodes, save, hash_collisions: collisions, .. } = self;
        while let Some((idx, depth)) = ser.stack.pop() {
            if depth as usize == DISPATCH_LEVELS {
                let bucket = collisions.get_t0(idx);
                for (k, v) in bucket {
                    put_bytes(out, k);
                    put_bytes(out, v);
                }
                *budget = budget.saturating_sub(1 + bucket.len() as u32);
            } else {
                let node: &HashNode = match save.take(idx) {
                    Some(T0::Slab) => unsafe { &nodes.get_unchecked(idx).hash },
                    Some(T0::Image(ValNodeImage::Hash(n))) => n,
                    _ => panic!("nœud valart {idx} absent de la vue T0"),
                };
                let len = node.len();
                for n in 0..len {
                    let slot = node.slot_at(n);
                    node.key(slot).prefetch();
                    node.val(slot).prefetch();
                }
                for n in 0..len {
                    let child = node.child(node.slot_at(n));
                    if child != NO_CHILD {
                        ser.stack.push((child, depth + 1));
                    }
                }
                if let Some(&(top, d)) = ser.stack.last()
                    && (d as usize) < DISPATCH_LEVELS
                {
                    prefetch_slot(nodes, top);
                }
                for n in 0..len {
                    let slot = node.slot_at(n);
                    put_bytes(out, node.key(slot));
                    put_bytes(out, node.val(slot));
                }
                *budget = budget.saturating_sub(1 + len as u32);
            }
            if *budget == 0 {
                return ser.stack.is_empty();
            }
        }
        true
    }

    /// HGETALL / HKEYS / HVALS : parcours complet, ordre sans signification.
    pub fn hash_for_each(&self, root: u32, mut f: impl FnMut(&SharedByte, &SharedByte)) {
        let mut stack = vec![(root, 0usize)];
        while let Some((idx, depth)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                for (k, v) in self.hash_collisions.get(idx) {
                    f(k, v);
                }
                continue;
            }
            let node = self.hash_node(idx);
            for n in 0..node.len() {
                let slot = node.slot_at(n);
                f(node.key(slot), node.val(slot));
                let child = node.child(slot);
                if child != NO_CHILD {
                    stack.push((child, depth + 1));
                }
            }
        }
    }

    /// Octets des blocs overflow du Hash (MiMalloc en direct, invisibles pour
    /// l'allocateur global). Parcours complet, pour les benchs.
    pub fn hash_overflow_bytes(&self, root: u32) -> usize {
        let mut total = 0;
        let mut stack = vec![(root, 0usize)];
        while let Some((idx, depth)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                continue;
            }
            let node = self.hash_node(idx);
            total += node.overflow.as_ref().map_or(0, HashOverflow::block_bytes);
            for n in 0..node.len() {
                let child = node.child(node.slot_at(n));
                if child != NO_CHILD {
                    stack.push((child, depth + 1));
                }
            }
        }
        total
    }

    /// Libère tout le Hash, racine comprise. `root` n'est plus valide après.
    ///
    /// Pendant une save, la destruction est différée à `finish_save` (le
    /// parcours peut encore lire ce conteneur dans la vue T0).
    pub fn hash_drop(&mut self, root: u32) {
        if self.save.is_active() {
            self.deferred_drops.push((NodeType::Hash, root));
            return;
        }
        self.hash_drop_now(root);
    }

    pub(crate) fn hash_drop_now(&mut self, root: u32) {
        let mut stack = vec![(root, 0usize)];
        while let Some((idx, depth)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                self.hash_collisions.release(idx);
                continue;
            }
            let node = self.hash_node(idx);
            for n in 0..node.len() {
                let child = node.child(node.slot_at(n));
                if child != NO_CHILD {
                    stack.push((child, depth + 1));
                }
            }
            self.free_hash_node(idx);
        }
    }

    /// Retire un slot sans enfant. Libère le node s'il se vide (sauf racine)
    /// et efface le pointeur du parent.
    fn hash_remove_leaf(
        &mut self,
        idx: u32,
        slot: Slot,
        parent: Option<(u32, Slot)>,
    ) -> (SharedByte, SharedByte) {
        let node = self.hash_node_mut(idx);
        let (key, val, _, child) = node.remove(slot);
        debug_assert_eq!(child, NO_CHILD);
        if node.is_empty()
            && let Some((parent_idx, parent_slot)) = parent
        {
            self.free_hash_node(idx);
            self.hash_node_mut(parent_idx).set_child(parent_slot, NO_CHILD);
        }
        (key, val)
    }

    fn hash_release_bucket(&mut self, idx: u32, parent: Option<(u32, Slot)>) {
        self.hash_collisions.release(idx);
        let (parent_idx, parent_slot) = parent.expect("un bucket a toujours un parent");
        self.hash_node_mut(parent_idx).set_child(parent_slot, NO_CHILD);
    }

    /// Détache une feuille du sous-arbre `idx` (niveau `depth`) et retourne
    /// `(champ, valeur, ctrl, sous-arbre_vidé)`. Voir `set.rs::take_leaf`.
    fn hash_take_leaf(&mut self, idx: u32, depth: usize) -> (SharedByte, SharedByte, u8, bool) {
        if depth == DISPATCH_LEVELS {
            let bucket = self.hash_collisions.get_mut(idx);
            let (key, val) = bucket.pop().expect("bucket de collision vide");
            let emptied = bucket.is_empty();
            if emptied {
                self.hash_collisions.release(idx);
            }
            let ctrl = self.member_hash(&key).ctrl();
            return (key, val, ctrl, emptied);
        }
        let node = self.hash_node(idx);
        // Dernier slot : retrait sans rapatriement depuis l'overflow.
        let slot = node.slot_at(node.len() - 1);
        let child = node.child(slot);
        if child == NO_CHILD {
            let node = self.hash_node_mut(idx);
            let (key, val, ctrl, _) = node.remove(slot);
            let emptied = node.is_empty();
            if emptied {
                self.free_hash_node(idx);
            }
            (key, val, ctrl, emptied)
        } else {
            let (key, val, ctrl, child_emptied) = self.hash_take_leaf(child, depth + 1);
            if child_emptied {
                self.hash_node_mut(idx).set_child(slot, NO_CHILD);
            }
            (key, val, ctrl, false)
        }
    }

    /// Vérifie tous les invariants du Hash et retourne son nombre de champs
    /// recompté.
    #[cfg(test)]
    pub(crate) fn hash_check(&self, root: u32) -> usize {
        let mut total = 0;
        let mut stack = vec![(root, 0usize, None::<(u32, u8)>)];
        while let Some((idx, depth, parent)) = stack.pop() {
            if depth == DISPATCH_LEVELS {
                let bucket = self.hash_collisions.get(idx);
                assert!(!bucket.is_empty(), "bucket vide encore référencé");
                total += bucket.len();
                continue;
            }
            let node = self.hash_node(idx);
            if let Some((parent_idx, parent_radix)) = parent {
                assert!(!node.is_empty(), "node non-racine vide");
                assert_eq!(node.parent_or_card, parent_idx, "back-pointer parent");
                assert_eq!(node.parent_radix, parent_radix, "back-pointer radix");
            }
            assert!(4 * node.count as usize + node.inline_childs() <= POOL_WORDS);
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
                assert_eq!(h.radix(depth), radix, "champ mal placé");
                assert_eq!(h.ctrl(), node.ctrl_of(slot), "ctrl incohérent");
                total += 1;
                let child = node.child(slot);
                if child != NO_CHILD {
                    stack.push((child, depth + 1, Some((idx, radix))));
                }
            }
        }
        assert_eq!(total, self.hash_len(root) as usize, "HLEN incohérent");
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn trees() -> ValTrees {
        ValTrees::with_seed(0xDEAD_BEEF, 1 << 22)
    }

    fn sb(s: impl AsRef<[u8]>) -> SharedByte {
        SharedByte::from_slice(s)
    }

    fn get(t: &ValTrees, root: u32, field: &str) -> Option<String> {
        t.hash_get(root, field.as_bytes())
            .map(|v| v.as_str().unwrap().to_owned())
    }

    /// HSET qui renvoie l'ancienne valeur (lue avant), et vérifie le booléen.
    fn set_old(t: &mut ValTrees, root: u32, field: &str, val: &str) -> Option<String> {
        let old = get(t, root, field);
        assert_eq!(t.hash_set(root, sb(field), sb(val)), old.is_none(), "{field}");
        old
    }

    /// HDEL qui renvoie la valeur retirée (lue avant), et vérifie le booléen.
    fn remove_old(t: &mut ValTrees, root: u32, field: &str) -> Option<String> {
        let old = get(t, root, field);
        assert_eq!(t.hash_remove(root, field.as_bytes()), old.is_some(), "{field}");
        old
    }

    /// Sérialise le Hash `root` pendant une save, tranche par tranche
    /// (`budget` par tranche, `between` appelé entre deux tranches), et
    /// décode le résultat.
    fn ser_all(
        t: &mut ValTrees,
        root: u32,
        budget: u32,
        mut between: impl FnMut(&mut ValTrees),
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        use radixox_lib::varint::{get_bytes, get_varint};
        let (mut out, mut ser) = (Vec::new(), crate::ValSer::default());
        t.hash_ser_begin(root, &mut ser, &mut out);
        loop {
            let mut b = budget;
            if t.hash_ser_step(&mut ser, &mut out, &mut b) {
                break;
            }
            between(t);
        }
        let mut pos = 0;
        let n = get_varint(&out, &mut pos).unwrap();
        let mut got: Vec<_> = (0..n)
            .map(|_| {
                let k = get_bytes(&out, &mut pos).unwrap().to_vec();
                (k, get_bytes(&out, &mut pos).unwrap().to_vec())
            })
            .collect();
        assert_eq!(pos, out.len());
        got.sort();
        got
    }

    /// Un conteneur sérialisé sur plusieurs tranches : la racine est déjà
    /// écrite quand HDEL fait remonter des entrées des feuilles vers elle,
    /// puis les supprime depuis la racine (non capturée). Les images des
    /// feuilles référencent encore ces entrées : elles doivent survivre
    /// jusqu'à `finish_save` (sous ASan, l'ancienne règle « on ne diffère
    /// que ce qui sort d'un nœud capturé » fait un use-after-free ici).
    #[test]
    fn save_entries_moved_up_then_removed() {
        let mut t = trees();
        t.hash_mask = 0; // chaîne : une entrée par niveau
        let root = t.hash_create();
        for i in 0..6 {
            t.hash_set(root, sb(format!("f{i}")), sb(format!("valeur-longue-{i}")));
        }
        let mut expect = Vec::new();
        t.hash_for_each(root, |k, v| expect.push((k.to_vec(), v.to_vec())));
        expect.sort();

        t.begin_save();
        // Budget 1 : la racine est écrite seule dans la première tranche.
        let mut first = true;
        let got = ser_all(&mut t, root, 1, |t| {
            if std::mem::take(&mut first) {
                for i in 0..6 {
                    assert!(t.hash_remove(root, format!("f{i}").as_bytes()));
                }
            }
        });
        t.finish_save();
        assert_eq!(got, expect);
        assert_eq!(t.hash_len(root), 0);
    }

    /// Buckets de collision modifiés pendant la save : le parcours T0 lit
    /// la copie faite au `begin_save`.
    #[test]
    fn save_collision_buckets_t0() {
        let mut t = trees();
        t.hash_mask = 0;
        let root = t.hash_create();
        for i in 0..40 {
            t.hash_set(root, sb(format!("f{i}")), sb(format!("valeur-longue-{i}")));
        }
        let mut expect = Vec::new();
        t.hash_for_each(root, |k, v| expect.push((k.to_vec(), v.to_vec())));
        expect.sort();

        t.begin_save();
        let mut first = true;
        let got = ser_all(&mut t, root, 1, |t| {
            if std::mem::take(&mut first) {
                for i in (0..40).step_by(2) {
                    assert!(t.hash_remove(root, format!("f{i}").as_bytes()));
                }
                for i in 40..60 {
                    t.hash_set(root, sb(format!("f{i}")), sb("nouveau"));
                }
                assert!(!t.hash_set(root, sb("f39"), sb("écrasé")));
            }
        });
        t.finish_save();
        assert_eq!(got, expect);
        t.hash_check(root);
    }

    #[test]
    fn layout() {
        assert_eq!(size_of::<HashNode>(), 128);
        assert_eq!(std::mem::offset_of!(HashNode, node_type), crate::NODE_TYPE_OFFSET);
    }

    #[test]
    fn set_get_overwrite_remove() {
        let mut t = trees();
        let root = t.hash_create();
        assert!(t.hash_set(root, sb("a"), sb("1")));
        assert!(t.hash_set(root, sb("b"), sb("2")));
        assert_eq!(set_old(&mut t, root, "a", "3").as_deref(), Some("1"));
        assert_eq!(t.hash_len(root), 2);
        assert_eq!(get(&t, root, "a").as_deref(), Some("3"));
        assert_eq!(get(&t, root, "c"), None);
        assert!(!t.hash_set(root, sb("b"), sb("20")));
        assert_eq!(get(&t, root, "b").as_deref(), Some("20"));
        assert_eq!(remove_old(&mut t, root, "a").as_deref(), Some("3"));
        assert!(!t.hash_remove(root, b"a"));
        assert!(!t.hash_contains(root, b"a"));
        assert_eq!(t.hash_len(root), 1);
        t.hash_check(root);
        t.hash_drop(root);
    }

    #[test]
    fn prefix_fields() {
        let mut t = trees();
        let root = t.hash_create();
        let fields = ["10", "1", "user:1", "user", "", "field0", "field1", "field10"];
        for (i, f) in fields.iter().enumerate() {
            assert!(t.hash_set(root, sb(f), sb(i.to_string())), "{f}");
        }
        for (i, f) in fields.iter().enumerate() {
            assert_eq!(get(&t, root, f), Some(i.to_string()), "{f}");
        }
        t.hash_check(root);
    }

    #[test]
    fn large_hash_matches_reference() {
        let mut t = trees();
        let root = t.hash_create();
        let mut reference = HashMap::new();
        for i in 0..100_000u32 {
            let (f, v) = (format!("field:{i}"), format!("v{i}"));
            assert!(t.hash_set(root, sb(&f), sb(&v)));
            reference.insert(f, v);
        }
        t.hash_check(root);
        for i in (0..100_000u32).step_by(3) {
            let f = format!("field:{i}");
            assert_eq!(remove_old(&mut t, root, &f), reference.remove(&f));
        }
        t.hash_check(root);
        for i in 0..100_000u32 {
            let f = format!("field:{i}");
            assert_eq!(get(&t, root, &f).as_ref(), reference.get(&f), "{f}");
        }
        let mut seen = HashMap::new();
        t.hash_for_each(root, |k, v| {
            let prev = seen.insert(
                k.as_str().unwrap().to_owned(),
                v.as_str().unwrap().to_owned(),
            );
            assert!(prev.is_none());
        });
        assert_eq!(seen, reference);
    }

    /// Opérations aléatoires mêlées contre un `HashMap`, invariants vérifiés
    /// régulièrement.
    #[test]
    fn random_ops_against_reference() {
        let mut t = trees();
        let root = t.hash_create();
        let mut reference: HashMap<String, String> = HashMap::new();
        let mut rnd = 42u64;
        for step in 0..200_000u32 {
            rnd = mix64(rnd);
            let f = format!("f{}", rnd % 5_000);
            match (rnd >> 32) % 4 {
                0 | 1 => {
                    let v = step.to_string();
                    let old = set_old(&mut t, root, &f, &v);
                    assert_eq!(old, reference.insert(f, v));
                }
                2 => assert_eq!(remove_old(&mut t, root, &f), reference.remove(&f)),
                _ => assert_eq!(get(&t, root, &f).as_ref(), reference.get(&f)),
            }
            if step % 10_000 == 0 {
                assert_eq!(t.hash_check(root), reference.len());
            }
        }
        assert_eq!(t.hash_check(root), reference.len());
        let fields: Vec<String> = reference.keys().cloned().collect();
        for f in fields {
            assert!(t.hash_remove(root, f.as_bytes()));
        }
        assert_eq!(t.hash_len(root), 0);
        t.hash_check(root);
    }

    /// Hash dégradé à 0 : chaîne des 7 niveaux, buckets, remontée depuis un
    /// bucket.
    #[test]
    fn full_collisions() {
        let mut t = trees();
        t.hash_mask = 0;
        let root = t.hash_create();
        for i in 0..50u32 {
            assert!(t.hash_set(root, sb(i.to_string()), sb(format!("v{i}"))));
        }
        assert_eq!(set_old(&mut t, root, "7", "x").as_deref(), Some("v7"));
        assert_eq!(set_old(&mut t, root, "49", "y").as_deref(), Some("v49"));
        t.hash_check(root);
        for i in 0..50u32 {
            let expected = match i {
                7 => "x".to_owned(),
                49 => "y".to_owned(),
                _ => format!("v{i}"),
            };
            assert_eq!(get(&t, root, &i.to_string()), Some(expected));
        }
        for i in 0..50u32 {
            assert!(t.hash_remove(root, i.to_string().as_bytes()), "{i}");
            t.hash_check(root);
            assert!(!t.hash_contains(root, i.to_string().as_bytes()));
        }
        assert_eq!(t.hash_len(root), 0);
        assert!(!t.hash_collisions.free.is_empty());
    }

    /// Hash dégradé à 12 bits : collisions partielles, nœuds profonds.
    #[test]
    fn partial_collisions() {
        let mut t = trees();
        t.hash_mask = 0x0F_FF;
        let root = t.hash_create();
        let mut reference: HashMap<String, String> = HashMap::new();
        let mut rnd = 7u64;
        for step in 0..30_000u32 {
            rnd = mix64(rnd);
            let f = format!("{}", rnd % 3_000);
            if rnd >> 63 == 0 {
                let v = step.to_string();
                let old = set_old(&mut t, root, &f, &v);
                assert_eq!(old, reference.insert(f, v));
            } else {
                assert_eq!(remove_old(&mut t, root, &f), reference.remove(&f));
            }
        }
        assert_eq!(t.hash_check(root), reference.len());
    }

    /// Nœud seul contre un modèle `radix → (champ, valeur, enfant)` : exerce
    /// la relocation vers l'overflow et le rapatriement.
    #[test]
    fn node_child_packing() {
        let mut node = HashNode::new(0, 0);
        let mut model: HashMap<u8, (u8, u32)> = HashMap::new();
        for r in 0..40u8 {
            node.push(r, r, sb([r]), sb([r, r]), NO_CHILD);
            model.insert(r, (r, NO_CHILD));
        }
        assert_eq!(node.count as usize, INLINE_HASH_LEN);
        let check = |node: &HashNode, model: &HashMap<u8, (u8, u32)>| {
            assert_eq!(node.len(), model.len());
            assert!(4 * node.count as usize + node.inline_childs() <= POOL_WORDS);
            if node.overflow.is_some() {
                assert!(node.refill_candidate().is_none());
            }
            for (&r, &(v, child)) in model {
                let slot = node.find(r).expect("radix perdu");
                assert_eq!(node.key(slot).as_slice(), &[r]);
                assert_eq!(node.val(slot).as_slice(), &[v, r]);
                assert_eq!(node.ctrl_of(slot), r);
                assert_eq!(node.child(slot), child, "radix {r}");
            }
        };
        let mut rnd = 1u64;
        for step in 0..5_000u32 {
            rnd = mix64(rnd);
            let r = (rnd % 40) as u8;
            match (rnd >> 40) % 4 {
                0 | 1 if model.contains_key(&r) => {
                    let child = if (rnd >> 50) % 3 == 0 { NO_CHILD } else { step };
                    node.set_child(node.find(r).unwrap(), child);
                    model.get_mut(&r).unwrap().1 = child;
                }
                2 if model.contains_key(&r) => {
                    let v = (step % 256) as u8;
                    *node.val_mut(node.find(r).unwrap()) = sb([v, r]);
                    model.get_mut(&r).unwrap().0 = v;
                }
                _ => match model.remove(&r) {
                    Some((v, child)) => {
                        let (k, val, ctrl, c) = node.remove(node.find(r).unwrap());
                        assert_eq!((k.as_slice(), val.as_slice(), ctrl, c), (&[r][..], &[v, r][..], r, child));
                    }
                    None => {
                        node.push(r, r, sb([r]), sb([r, r]), NO_CHILD);
                        model.insert(r, (r, NO_CHILD));
                    }
                },
            }
            check(&node, &model);
        }
    }

    #[test]
    fn random_field_is_member() {
        let mut t = trees();
        let root = t.hash_create();
        assert!(t.hash_random(root, 1).is_none());
        for i in 0..10_000u32 {
            t.hash_set(root, sb(format!("f{i}")), sb(format!("v{i}")));
        }
        let mut rnd = 3u64;
        for _ in 0..1_000 {
            rnd = mix64(rnd);
            let (k, v) = t.hash_random(root, rnd).unwrap();
            assert_eq!(get(&t, root, k.as_str().unwrap()).unwrap(), v.as_str().unwrap());
        }
    }

    #[test]
    fn drop_releases_fields_and_values() {
        let mut t = trees();
        let (field, val) = (sb("shared"), sb("value"));
        let root = t.hash_create();
        t.hash_set(root, field.clone(), val.clone());
        for i in 0..1_000u32 {
            t.hash_set(root, sb(i.to_le_bytes()), val.clone());
        }
        assert_eq!((field.rc(), val.rc()), (2, 1_002));
        t.hash_drop(root);
        assert_eq!((field.rc(), val.rc()), (1, 1));
    }

    #[test]
    fn hash_and_set_share_slab() {
        let mut t = trees();
        let h = t.hash_create();
        let s = t.set_create();
        t.hash_set(h, sb("x"), sb("1"));
        t.set_add(s, sb("x"));
        assert!(t.hash_contains(h, b"x") && t.set_contains(s, b"x"));
        t.hash_drop(h);
        assert!(t.set_contains(s, b"x"));
    }
}
