use std::alloc::{GlobalAlloc, Layout, handle_alloc_error};

// Blocs overflow : MiMalloc en direct (pools par taille, comme `SharedByte`).
// En test, allocateur système pour qu'AddressSanitizer voie les blocs.
#[cfg(test)]
use std::alloc::System as BlockAlloc;
#[cfg(not(test))]
use mimalloc::MiMalloc as BlockAlloc;
use std::ptr::NonNull;

use radixox_lib::prefetch::PreFetch;
use radixox_lib::prefetch::PreFetchPtr;
use radixox_lib::shared_byte::SharedByte;

/// Capacités possibles pour un bloc overflow. L'indice dans ce tableau (0..=6)
/// est la "classe" de capacité, stockée dans les 3 bits bas du pointeur.
const CAPACITIES: [usize; 7] = [4, 8, 16, 32, 64, 128, 256];
const TAG_MASK: usize = 0b111;

/// Le radix space fait 256 valeurs (`u8`) et l'inline du node parent en
/// occupe déjà au moins une : l'overflow ne peut donc structurellement
/// jamais dépasser 255 entrées, même à la classe de capacité 256. `len`
/// tient toujours dans un `u8`.
const HEADER_LEN_SIZE: usize = size_of::<u8>();

#[inline]
const fn bitmap_len(capacity: usize) -> usize {
    capacity.div_ceil(8)
}

#[inline]
const fn align_up8(n: usize) -> usize {
    (n + 7) & !7
}

/// Offset du début de la zone de données packées (key/val/idx), depuis le
/// début du bloc alloué. Header `len` (1B) + bitmap TTL + radix, arrondi à 8
/// pour que le premier `SharedByte` de la zone data soit correctement aligné.
#[inline]
const fn data_start(capacity: usize) -> usize {
    align_up8(HEADER_LEN_SIZE + bitmap_len(capacity) + capacity)
}

// --- SetOverflow : une clé par entrée, enfants packés -----------------------
//
// Bloc alloué (align 8, mis à zéro), pointeur tagué (3 bits bas = classe de
// capacité des entrées) :
//
// [len: u8][nb_childs: u8][child_cap: u16]
// [radix: cap][ctrl: cap][has_child: cap/8 bits][pad → 8]
// [keys: cap × 8][childs: child_cap × 4]
//
// `radix` = octet de dispatch du niveau courant (unique dans le node), `ctrl`
// = octet de contrôle (h[7]) de la clé. Seules les entrées qui ont un enfant
// paient ses 4 B : l'idx de l'enfant de l'entrée `i` est
// `childs[rank(i)]`, `rank(i)` = nombre de bits `has_child` avant `i`.
// `child_cap` grandit à la demande, indépendamment de `cap`.

/// Sentinelle « pas d'enfant ».
pub(crate) const NO_CHILD: u32 = u32::MAX;

const SET_HEADER: usize = 4;

#[inline]
const fn set_keys_start(cap: usize) -> usize {
    align_up8(SET_HEADER + 2 * cap + bitmap_len(cap))
}

#[inline]
const fn set_block_size(cap: usize, child_cap: usize) -> usize {
    set_keys_start(cap) + cap * 8 + child_cap * 4
}

#[repr(transparent)]
pub struct SetOverflow {
    ptr: NonNull<u8>,
}

impl SetOverflow {
    #[inline]
    fn class(&self) -> u8 {
        (self.ptr.as_ptr() as usize & TAG_MASK) as u8
    }

    #[inline]
    fn base_ptr(&self) -> *mut u8 {
        (self.ptr.as_ptr() as usize & !TAG_MASK) as *mut u8
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        CAPACITIES[self.class() as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        unsafe { self.base_ptr().read() as usize }
    }

    #[inline]
    fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity() && len <= u8::MAX as usize);
        unsafe { self.base_ptr().write(len as u8) }
    }

    /// Nombre d'entrées qui ont un enfant.
    #[inline]
    pub fn childs_len(&self) -> usize {
        unsafe { self.base_ptr().add(1).read() as usize }
    }

    #[inline]
    fn set_childs_len(&mut self, n: usize) {
        unsafe { self.base_ptr().add(1).write(n as u8) }
    }

    #[inline]
    pub fn child_capacity(&self) -> usize {
        unsafe { (self.base_ptr().add(2) as *const u16).read() as usize }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    #[inline]
    fn radix_ptr(&self) -> *mut u8 {
        unsafe { self.base_ptr().add(SET_HEADER) }
    }

    #[inline]
    fn ctrl_ptr(&self) -> *mut u8 {
        unsafe { self.radix_ptr().add(self.capacity()) }
    }

    #[inline]
    fn bitmap_ptr(&self) -> *mut u8 {
        unsafe { self.ctrl_ptr().add(self.capacity()) }
    }

    #[inline]
    fn keys_ptr(&self) -> *mut SharedByte {
        unsafe { self.base_ptr().add(set_keys_start(self.capacity())) as *mut SharedByte }
    }

    #[inline]
    fn childs_ptr(&self) -> *mut u32 {
        unsafe { self.keys_ptr().add(self.capacity()) as *mut u32 }
    }

    #[inline]
    pub fn radix(&self, i: usize) -> u8 {
        debug_assert!(i < self.len());
        unsafe { *self.radix_ptr().add(i) }
    }

    #[inline]
    pub fn ctrl(&self, i: usize) -> u8 {
        debug_assert!(i < self.len());
        unsafe { *self.ctrl_ptr().add(i) }
    }

    #[inline]
    pub fn key(&self, i: usize) -> &SharedByte {
        debug_assert!(i < self.len());
        unsafe { &*self.keys_ptr().add(i) }
    }

    #[inline]
    pub fn has_child(&self, i: usize) -> bool {
        unsafe { (*self.bitmap_ptr().add(i / 8) >> (i % 8)) & 1 != 0 }
    }

    #[inline]
    fn set_has_child(&mut self, i: usize, val: bool) {
        unsafe {
            let byte = self.bitmap_ptr().add(i / 8);
            if val {
                *byte |= 1 << (i % 8);
            } else {
                *byte &= !(1 << (i % 8));
            }
        }
    }

    /// Nombre d'entrées avec enfant parmi `0..i`.
    #[inline]
    fn rank(&self, i: usize) -> usize {
        // Lectures u64 non alignées : la zone keys suit le bitmap, le mot
        // partiel déborde donc toujours dans le bloc, et le bloc est mis à
        // zéro à l'allocation (pas d'octet non initialisé). Bits ≥ i masqués.
        let bm = self.bitmap_ptr();
        let mut rank = 0;
        let mut w = 0;
        while (w + 1) * 64 <= i {
            rank += unsafe { (bm.add(w * 8) as *const u64).read_unaligned() }.count_ones();
            w += 1;
        }
        let rem = i - w * 64;
        if rem > 0 {
            let word = unsafe { (bm.add(w * 8) as *const u64).read_unaligned() };
            rank += (word & ((1u64 << rem) - 1)).count_ones();
        }
        rank as usize
    }

    #[inline]
    pub fn child(&self, i: usize) -> u32 {
        debug_assert!(i < self.len());
        if !self.has_child(i) {
            return NO_CHILD;
        }
        unsafe { self.childs_ptr().add(self.rank(i)).read() }
    }

    /// Pose ou efface l'enfant de l'entrée `i`. Effacer peut réduire le
    /// tableau d'enfants (voir [`Self::shrink_if_sparse`]).
    pub fn set_child(&mut self, i: usize, child: u32) {
        self.set_child_no_shrink(i, child);
        if child == NO_CHILD {
            self.shrink_if_sparse();
        }
    }

    /// `set_child` sans réduction : pour les opérations composées, qui
    /// réduisent une seule fois à la fin.
    fn set_child_no_shrink(&mut self, i: usize, child: u32) {
        debug_assert!(i < self.len());
        match (self.has_child(i), child != NO_CHILD) {
            (true, true) => unsafe { self.childs_ptr().add(self.rank(i)).write(child) },
            (true, false) => {
                self.child_remove(self.rank(i));
                self.set_has_child(i, false);
            }
            (false, true) => {
                self.child_insert(self.rank(i), child);
                self.set_has_child(i, true);
            }
            (false, false) => {}
        }
    }

    /// Insère un idx au rang `r` du tableau d'enfants (décale la suite).
    fn child_insert(&mut self, r: usize, child: u32) {
        let n = self.childs_len();
        if n == self.child_capacity() {
            let new_child_cap = (n * 2).clamp(4, self.capacity());
            self.realloc(self.class(), new_child_cap);
        }
        unsafe {
            let childs = self.childs_ptr();
            std::ptr::copy(childs.add(r), childs.add(r + 1), n - r);
            childs.add(r).write(child);
        }
        self.set_childs_len(n + 1);
    }

    /// Retire l'idx au rang `r` du tableau d'enfants (décale la suite).
    fn child_remove(&mut self, r: usize) -> u32 {
        let n = self.childs_len();
        debug_assert!(r < n);
        unsafe {
            let childs = self.childs_ptr();
            let child = childs.add(r).read();
            std::ptr::copy(childs.add(r + 1), childs.add(r), n - r - 1);
            self.set_childs_len(n - 1);
            child
        }
    }

    /// Remplace la clé (et son ctrl) de l'entrée `i`, retourne l'ancienne.
    /// Le radix et l'enfant ne bougent pas.
    pub fn replace_key(&mut self, i: usize, key: SharedByte, ctrl: u8) -> SharedByte {
        debug_assert!(i < self.len());
        unsafe {
            *self.ctrl_ptr().add(i) = ctrl;
            std::ptr::replace(self.keys_ptr().add(i), key)
        }
    }

    fn alloc_block(class: u8, child_cap: usize) -> Self {
        let size = set_block_size(CAPACITIES[class as usize], child_cap);
        let layout = Layout::from_size_align(size, 8).unwrap();
        unsafe {
            // Zéro : len = nb_childs = 0, bitmap vide, et `rank` peut lire
            // des mots entiers sans toucher d'octet non initialisé.
            let raw = BlockAlloc.alloc_zeroed(layout);
            if raw.is_null() {
                handle_alloc_error(layout);
            }
            (raw.add(2) as *mut u16).write(child_cap as u16);
            let tagged = (raw as usize | class as usize) as *mut u8;
            Self {
                ptr: NonNull::new_unchecked(tagged),
            }
        }
    }

    pub fn new() -> Self {
        Self::alloc_block(0, 0)
    }

    fn block_layout(&self) -> Layout {
        Layout::from_size_align(self.block_bytes(), 8).unwrap()
    }

    /// Taille du bloc alloué (octets demandés à MiMalloc).
    pub fn block_bytes(&self) -> usize {
        set_block_size(self.capacity(), self.child_capacity())
    }

    /// Nouveau bloc (classe et capacité d'enfants données), contenu recopié.
    fn realloc(&mut self, class: u8, child_cap: usize) {
        let len = self.len();
        let nb_childs = self.childs_len();
        debug_assert!(len <= CAPACITIES[class as usize] && nb_childs <= child_cap);
        let new = Self::alloc_block(class, child_cap);
        unsafe {
            new.base_ptr().write(len as u8);
            new.base_ptr().add(1).write(nb_childs as u8);
            std::ptr::copy_nonoverlapping(self.radix_ptr(), new.radix_ptr(), len);
            std::ptr::copy_nonoverlapping(self.ctrl_ptr(), new.ctrl_ptr(), len);
            // Les bits ≥ len sont toujours à 0 : copier `bitmap_len(len)`
            // suffit et reste dans les bornes quand on réduit la classe.
            std::ptr::copy_nonoverlapping(self.bitmap_ptr(), new.bitmap_ptr(), bitmap_len(len));
            std::ptr::copy_nonoverlapping(self.keys_ptr(), new.keys_ptr(), len);
            std::ptr::copy_nonoverlapping(self.childs_ptr(), new.childs_ptr(), nb_childs);
            BlockAlloc.dealloc(self.base_ptr(), self.block_layout());
        }
        // Pas `*self = new` : ça déclencherait le `Drop` de l'ancienne valeur
        // (déjà désallouée, clés déplacées). On adopte le pointeur et on
        // `forget` `new`.
        self.ptr = new.ptr;
        std::mem::forget(new);
    }

    /// Ajoute une entrée en fin de tableau, sans vérifier si `radix` est déjà
    /// présent (pur append, pas de dédup — à l'appelant de garantir l'unicité,
    /// ex: vérifier `find(radix).is_none()` avant).
    pub fn add_value(&mut self, radix: u8, ctrl: u8, key: SharedByte, child: u32) {
        if self.is_full() {
            let class = self.class();
            assert!(class < 6, "SetOverflow: max capacity (256) reached");
            self.realloc(class + 1, self.child_capacity());
        }
        let i = self.len();
        unsafe {
            *self.radix_ptr().add(i) = radix;
            *self.ctrl_ptr().add(i) = ctrl;
            self.keys_ptr().add(i).write(key);
        }
        self.set_len(i + 1);
        self.set_child(i, child);
    }

    /// Position de l'entrée de radix `radix`. Les radix sont uniques dans un
    /// node, donc au plus une entrée matche. Scan SWAR 8 octets à la fois.
    #[inline]
    pub fn find(&self, radix: u8) -> Option<usize> {
        let len = self.len();
        let base = self.radix_ptr();
        let mut i = 0;
        while i + 8 <= len {
            let word = unsafe { (base.add(i) as *const u64).read_unaligned() };
            if let Some(j) = swar_find(word, radix) {
                return Some(i + j);
            }
            i += 8;
        }
        (i..len).find(|&j| unsafe { *base.add(j) } == radix)
    }

    /// Une entrée sans enfant, s'il y en a.
    pub fn find_childless(&self) -> Option<usize> {
        (0..self.len()).rev().find(|&i| !self.has_child(i))
    }

    /// Retire l'entrée `i` en la remplaçant par la dernière (ordre non
    /// préservé). Retourne `(radix, ctrl, key, child)`.
    pub fn swap_remove(&mut self, i: usize) -> (u8, u8, SharedByte, u32) {
        debug_assert!(i < self.len());
        let last = self.len() - 1;
        let child = self.child(i);
        self.set_child_no_shrink(i, NO_CHILD);
        let removed = unsafe {
            (
                *self.radix_ptr().add(i),
                *self.ctrl_ptr().add(i),
                self.keys_ptr().add(i).read(),
                child,
            )
        };
        if i != last {
            let last_child = self.child(last);
            self.set_child_no_shrink(last, NO_CHILD);
            unsafe {
                *self.radix_ptr().add(i) = *self.radix_ptr().add(last);
                *self.ctrl_ptr().add(i) = *self.ctrl_ptr().add(last);
                self.keys_ptr().add(i).write(self.keys_ptr().add(last).read());
            }
            self.set_child_no_shrink(i, last_child);
        }
        self.set_len(last);
        self.shrink_if_sparse();
        removed
    }

    /// Redescend de classe quand les entrées n'occupent plus qu'un quart de la
    /// capacité, et réduit le tableau d'enfants de la même façon (à 0 s'il
    /// n'y a plus d'enfant). Le seuil à 1/4 (et non 1/2) évite d'osciller
    /// entre deux classes sur un insert/remove répété.
    fn shrink_if_sparse(&mut self) {
        let len = self.len();
        let mut class = self.class();
        while class > 0 && len <= CAPACITIES[class as usize] / 4 {
            class -= 1;
        }
        let nb_childs = self.childs_len();
        let child_cap = self.child_capacity();
        let new_child_cap = if nb_childs == 0 {
            0
        } else if nb_childs <= child_cap / 4 {
            (nb_childs * 2).max(4)
        } else {
            child_cap
        };
        if class != self.class() || new_child_cap != child_cap {
            self.realloc(class, new_child_cap);
        }
    }

    /// Retire la dernière entrée. Retourne `(radix, ctrl, key, child)`.
    pub fn pop(&mut self) -> Option<(u8, u8, SharedByte, u32)> {
        let last = self.len().checked_sub(1)?;
        Some(self.swap_remove(last))
    }
}

/// Index du premier octet de `word` égal à `byte`, ou `None`.
///
/// Détection classique "has zero byte" sur `word ^ broadcast(byte)`. Les faux
/// positifs de cette formule ne peuvent apparaître qu'au-dessus d'un vrai
/// zéro (propagation de la retenue), donc le bit le plus bas est toujours un
/// vrai match.
#[inline(always)]
pub(crate) fn swar_find(word: u64, byte: u8) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let x = word ^ (LO * byte as u64);
    let mask = x.wrapping_sub(LO) & !x & HI;
    if mask == 0 {
        None
    } else {
        Some((mask.trailing_zeros() / 8) as usize)
    }
}

impl Default for SetOverflow {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SetOverflow {
    fn drop(&mut self) {
        for i in 0..self.len() {
            unsafe { std::ptr::drop_in_place(self.keys_ptr().add(i)) };
        }
        unsafe { BlockAlloc.dealloc(self.base_ptr(), self.block_layout()) };
    }
}

// --- HashOverflow : key + val par entrée -------------------------------------
//
// Même schéma de bloc que `SetOverflow`. Zone data, par paire d'entrées :
// 40 bytes :
// [key_2i(8) | val_2i(8) | idx_2i(4) | idx_2i+1(4) | key_2i+1(8) | val_2i+1(8)]

const HASH_STRIDE: usize = 40;

#[repr(transparent)]
pub struct HashOverflow {
    ptr: NonNull<u8>,
}
impl PreFetch for HashOverflow {
    fn prefetch(&self) {
        self.ptr.ptr_prefetch();
    }
}

impl HashOverflow {
    #[inline]
    fn class(&self) -> u8 {
        (self.ptr.as_ptr() as usize & TAG_MASK) as u8
    }

    #[inline]
    fn base_ptr(&self) -> *mut u8 {
        (self.ptr.as_ptr() as usize & !TAG_MASK) as *mut u8
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        CAPACITIES[self.class() as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        unsafe { self.base_ptr().read() as usize }
    }

    #[inline]
    fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity() && len <= u8::MAX as usize);
        unsafe { self.base_ptr().write(len as u8) }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    #[inline]
    fn bitmap_ptr(&self) -> *mut u8 {
        unsafe { self.base_ptr().add(HEADER_LEN_SIZE) }
    }

    #[inline]
    pub fn has_ttl(&self, i: usize) -> bool {
        debug_assert!(i < self.len());
        unsafe { (*self.bitmap_ptr().add(i / 8) & (1 << (i % 8))) != 0 }
    }

    #[inline]
    pub fn set_ttl(&mut self, i: usize, val: bool) {
        unsafe {
            let byte = self.bitmap_ptr().add(i / 8);
            let bit = 1u8 << (i % 8);
            if val {
                *byte |= bit;
            } else {
                *byte &= !bit;
            }
        }
    }

    #[inline]
    fn radix_ptr(&self) -> *mut u8 {
        unsafe { self.bitmap_ptr().add(bitmap_len(self.capacity())) }
    }

    #[inline]
    pub fn radix(&self, i: usize) -> u8 {
        debug_assert!(i < self.len());
        unsafe { *self.radix_ptr().add(i) }
    }

    #[inline]
    fn set_radix(&mut self, i: usize, r: u8) {
        unsafe { *self.radix_ptr().add(i) = r }
    }

    #[inline]
    fn data_ptr(&self) -> *mut u8 {
        unsafe { self.base_ptr().add(data_start(self.capacity())) }
    }

    fn key_ptr(&self, i: usize) -> *mut SharedByte {
        let pair_base = unsafe { self.data_ptr().add((i / 2) * HASH_STRIDE) };
        let off = if i % 2 == 0 { 0 } else { 24 };
        unsafe { pair_base.add(off) as *mut SharedByte }
    }

    fn val_ptr(&self, i: usize) -> *mut SharedByte {
        let pair_base = unsafe { self.data_ptr().add((i / 2) * HASH_STRIDE) };
        let off = if i % 2 == 0 { 8 } else { 32 };
        unsafe { pair_base.add(off) as *mut SharedByte }
    }

    fn idx_ptr(&self, i: usize) -> *mut u32 {
        let pair_base = unsafe { self.data_ptr().add((i / 2) * HASH_STRIDE) };
        let off = if i % 2 == 0 { 16 } else { 20 };
        unsafe { pair_base.add(off) as *mut u32 }
    }

    pub fn key(&self, i: usize) -> &SharedByte {
        debug_assert!(i < self.len());
        unsafe { &*self.key_ptr(i) }
    }

    pub fn val(&self, i: usize) -> &SharedByte {
        debug_assert!(i < self.len());
        unsafe { &*self.val_ptr(i) }
    }

    pub fn val_mut(&mut self, i: usize) -> &mut SharedByte {
        debug_assert!(i < self.len());
        unsafe { &mut *self.val_ptr(i) }
    }

    pub fn idx(&self, i: usize) -> u32 {
        debug_assert!(i < self.len());
        unsafe { self.idx_ptr(i).read() }
    }

    pub fn set_idx(&mut self, i: usize, idx: u32) {
        debug_assert!(i < self.len());
        unsafe { self.idx_ptr(i).write(idx) }
    }

    fn total_size(&self) -> usize {
        data_start(self.capacity()) + (self.capacity() / 2) * HASH_STRIDE
    }

    fn alloc_block(class: u8) -> Self {
        let capacity = CAPACITIES[class as usize];
        let size = data_start(capacity) + (capacity / 2) * HASH_STRIDE;
        let layout = Layout::from_size_align(size, 8).unwrap();
        unsafe {
            let raw = BlockAlloc.alloc(layout);
            if raw.is_null() {
                handle_alloc_error(layout);
            }
            raw.write(0); // len = 0
            let tagged = (raw as usize | class as usize) as *mut u8;
            Self {
                ptr: NonNull::new_unchecked(tagged),
            }
        }
    }

    pub fn new() -> Self {
        Self::alloc_block(0)
    }

    fn grow(&mut self) {
        let class = self.class();
        assert!(class < 6, "HashOverflow: max capacity (256) reached");
        let old_cap = self.capacity();
        let mut new = Self::alloc_block(class + 1);
        unsafe {
            std::ptr::copy_nonoverlapping(self.bitmap_ptr(), new.bitmap_ptr(), bitmap_len(old_cap));
            std::ptr::copy_nonoverlapping(self.radix_ptr(), new.radix_ptr(), old_cap);
            std::ptr::copy_nonoverlapping(
                self.data_ptr(),
                new.data_ptr(),
                (old_cap / 2) * HASH_STRIDE,
            );
        }
        new.set_len(self.len());
        unsafe {
            BlockAlloc.dealloc(
                self.base_ptr(),
                Layout::from_size_align(self.total_size(), 8).unwrap(),
            )
        };
        // Pas `*self = new` : ça déclencherait le `Drop` de l'ancienne valeur
        // (double free, déjà désallouée juste au-dessus). On ne remplace que
        // le pointeur, puis on `forget` `new` pour ne pas désallouer deux fois
        // le bloc qu'on vient d'adopter.
        self.ptr = new.ptr;
        std::mem::forget(new);
    }

    /// Ajoute une entrée en fin de tableau, sans vérifier si `radix` est déjà
    /// présent (pur append, pas de dédup — à l'appelant de garantir l'unicité
    /// si besoin, ex: vérifier `get_value(radix).is_none()` avant).
    pub fn add_value(
        &mut self,
        radix: u8,
        key: SharedByte,
        val: SharedByte,
        idx: u32,
        has_ttl: bool,
    ) {
        if self.is_full() {
            self.grow();
        }
        let i = self.len();
        self.set_radix(i, radix);
        unsafe { self.key_ptr(i).write(key) };
        unsafe { self.val_ptr(i).write(val) };
        unsafe { self.idx_ptr(i).write(idx) };
        self.set_ttl(i, has_ttl);
        self.set_len(i + 1);
    }

    /// Scan linéaire sur `radix`. Version lecture seule : le TTL est un
    /// simple `bool`, pas de ref vers le bitmap (impossible d'avoir
    /// `&mut bool` sur un bit tassé).
    pub fn get_values(&self, radix: u8) -> Option<(&SharedByte, &SharedByte, &u8, bool)> {
        (0..self.len())
            .find(|&i| self.radix(i) == radix)
            .map(|i| unsafe {
                (
                    &*self.val_ptr(i),
                    &*self.key_ptr(i),
                    &*self.radix_ptr().add(i),
                    self.has_ttl(i),
                )
            })
    }

    //pub fn get_value(&mut self,radix:u8,now:u64)->Option<SharedByte>

    /// Version lecture/écriture : retourne l'index `i` au lieu du TTL — pas
    /// de `&mut bool` possible sur le bitmap, l'appelant modifie le TTL via
    /// `has_ttl(i)` / `set_ttl(i, ..)` avec cet index.
    pub fn get_value_mut(
        &mut self,
        radix: u8,
    ) -> Option<(&mut SharedByte, &SharedByte, &u8, usize)> {
        let i = (0..self.len()).find(|&i| self.radix(i) == radix)?;
        let val_ptr = self.val_ptr(i);
        let key_ptr = self.key_ptr(i);
        let radix_ptr = unsafe { self.radix_ptr().add(i) };
        unsafe { Some((&mut *val_ptr, &*key_ptr, &*radix_ptr, i)) }
    }

    /// Retire l'entrée `i` en la remplaçant par la dernière (ordre non préservé).
    pub fn swap_remove(&mut self, i: usize) -> (SharedByte, SharedByte, u32) {
        debug_assert!(i < self.len());
        let last = self.len() - 1;
        unsafe {
            let removed_key = self.key_ptr(i).read();
            let removed_val = self.val_ptr(i).read();
            let removed_idx = self.idx_ptr(i).read();
            if i != last {
                let last_key = self.key_ptr(last).read();
                let last_val = self.val_ptr(last).read();
                let last_idx = self.idx_ptr(last).read();
                let last_ttl = self.has_ttl(last);
                self.key_ptr(i).write(last_key);
                self.val_ptr(i).write(last_val);
                self.idx_ptr(i).write(last_idx);
                self.set_radix(i, self.radix(last));
                self.set_ttl(i, last_ttl);
            }
            self.set_len(last);
            (removed_key, removed_val, removed_idx)
        }
    }
}

impl Default for HashOverflow {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HashOverflow {
    fn drop(&mut self) {
        for i in 0..self.len() {
            unsafe {
                std::ptr::drop_in_place(self.key_ptr(i));
                std::ptr::drop_in_place(self.val_ptr(i));
            }
        }
        unsafe {
            BlockAlloc.dealloc(
                self.base_ptr(),
                Layout::from_size_align(self.total_size(), 8).unwrap(),
            )
        };
    }
}

// Un seul pointeur, capacité taguée dans les 3 bits bas, niche-optimisé sous
// `Option` (comme `ThinVec`) : pas de coût par rapport à un slab-index u32.
const _: () = assert!(size_of::<SetOverflow>() == 8);
const _: () = assert!(size_of::<HashOverflow>() == 8);
const _: () = assert!(size_of::<Option<SetOverflow>>() == 8);
const _: () = assert!(size_of::<Option<HashOverflow>>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_push_get_grow_across_classes() {
        let mut o = SetOverflow::new();
        assert_eq!(o.capacity(), 4);
        for i in 0..200u32 {
            // un enfant une entrée sur trois
            let child = if i % 3 == 0 { i * 10 } else { NO_CHILD };
            o.add_value(i as u8, (i * 7) as u8, SharedByte::from_slice(i.to_le_bytes()), child);
        }
        assert_eq!(o.len(), 200);
        assert!(o.capacity() >= 200);
        assert_eq!(o.childs_len(), 67);
        assert!(o.child_capacity() < 200, "enfants non réservés pour toutes les entrées");
        for i in 0..200u32 {
            let child = if i % 3 == 0 { i * 10 } else { NO_CHILD };
            assert_eq!(o.radix(i as usize), i as u8);
            assert_eq!(o.ctrl(i as usize), (i * 7) as u8);
            assert_eq!(o.child(i as usize), child);
            assert_eq!(o.key(i as usize).as_slice(), &i.to_le_bytes());
            assert_eq!(o.find(i as u8), Some(i as usize));
        }
        assert_eq!(o.find(250), None);
    }

    #[test]
    fn set_child_rank_updates() {
        let mut o = SetOverflow::new();
        for i in 0..100u32 {
            o.add_value(i as u8, 0, SharedByte::from_slice(i.to_le_bytes()), NO_CHILD);
        }
        // enfants posés dans le désordre : l'ordre par rang doit tenir
        for i in (0..100u32).rev().step_by(7) {
            o.set_child(i as usize, i + 1000);
        }
        for i in 0..100u32 {
            let expected = if (99 - i) % 7 == 0 { i + 1000 } else { NO_CHILD };
            assert_eq!(o.child(i as usize), expected, "{i}");
        }
        for i in (0..100u32).rev().step_by(7) {
            o.set_child(i as usize, NO_CHILD);
        }
        assert_eq!(o.childs_len(), 0);
        assert!((0..100).all(|i| o.child(i) == NO_CHILD));
    }

    #[test]
    fn set_shrinks_classes_and_childs() {
        let mut o = SetOverflow::new();
        for i in 0..200u32 {
            o.add_value(i as u8, i as u8, SharedByte::from_slice(i.to_le_bytes()), i);
        }
        assert_eq!((o.capacity(), o.child_capacity()), (256, 256));
        // retire depuis le début : chaque swap_remove déplace la dernière
        // entrée (et son enfant) vers l'avant
        while o.len() > 2 {
            let (radix, ctrl, key, child) = o.swap_remove(0);
            assert_eq!(radix as u32, child);
            assert_eq!(ctrl as u32, child);
            assert_eq!(key.as_slice(), &child.to_le_bytes());
            for i in 0..o.len() {
                let r = o.radix(i) as u32;
                assert_eq!(o.child(i), r);
                assert_eq!(o.key(i).as_slice(), &r.to_le_bytes());
                assert_eq!(o.find(r as u8), Some(i));
            }
        }
        assert_eq!(o.capacity(), 4);
        assert!(o.child_capacity() <= 8);
        for i in 0..o.len() {
            o.set_child(i, NO_CHILD);
        }
        assert_eq!((o.childs_len(), o.child_capacity()), (0, 0));
        assert!(o.block_bytes() <= 48);
    }

    #[test]
    fn swar_find_matches_scalar() {
        for word in [0u64, u64::MAX, 0x0001_0203_0405_0607, 0x0100_0000_0000_0001] {
            for byte in 0..=255u8 {
                let expected = word.to_le_bytes().iter().position(|&b| b == byte);
                assert_eq!(swar_find(word, byte), expected, "{word:#x} {byte}");
            }
        }
    }

    #[test]
    fn set_drop_releases_shared_byte_rc() {
        let shared = SharedByte::from_slice(b"hello");
        {
            let mut o = SetOverflow::new();
            o.add_value(0, 0, shared.clone(), NO_CHILD);
            o.add_value(1, 0, shared.clone(), 5);
            assert_eq!(shared.rc(), 3);
        }
        assert_eq!(shared.rc(), 1);
    }

    #[test]
    fn set_swap_remove() {
        let mut o = SetOverflow::new();
        for i in 0..4u32 {
            let child = if i % 2 == 1 { i } else { NO_CHILD };
            o.add_value(i as u8, i as u8 + 100, SharedByte::from_slice(i.to_le_bytes()), child);
        }
        let (radix, ctrl, k, child) = o.swap_remove(0);
        assert_eq!((radix, ctrl, child), (0, 100, NO_CHILD));
        assert_eq!(k.as_slice(), &0u32.to_le_bytes());
        assert_eq!(o.len(), 3);
        // le dernier élément (i=3, enfant 3) a été déplacé dans le slot 0
        assert_eq!(o.key(0).as_slice(), &3u32.to_le_bytes());
        assert_eq!((o.radix(0), o.ctrl(0), o.child(0)), (3, 103, 3));
        assert_eq!(o.child(1), 1);
        assert_eq!(o.child(2), NO_CHILD);
        assert_eq!(o.find(3), Some(0));
        let (_, _, _, child) = o.swap_remove(1);
        assert_eq!(child, 1);
        assert_eq!(o.childs_len(), 1);
    }

    #[test]
    fn hash_push_get_grow_across_classes() {
        let mut o = HashOverflow::new();
        for i in 0..20u32 {
            o.add_value(
                i as u8,
                SharedByte::from_slice(i.to_le_bytes()),
                SharedByte::from_slice((i * 2).to_le_bytes()),
                i * 10,
                i % 2 == 0,
            );
        }
        assert_eq!(o.len(), 20);
        for i in 0..20u32 {
            assert_eq!(o.key(i as usize).as_slice(), &i.to_le_bytes());
            assert_eq!(o.val(i as usize).as_slice(), &(i * 2).to_le_bytes());
            assert_eq!(o.idx(i as usize), i * 10);
            assert_eq!(o.has_ttl(i as usize), i % 2 == 0);
        }
    }

    #[test]
    fn hash_drop_releases_both_shared_bytes() {
        let key = SharedByte::from_slice(b"k");
        let val = SharedByte::from_slice(b"v");
        {
            let mut o = HashOverflow::new();
            o.add_value(0, key.clone(), val.clone(), 0, false);
            assert_eq!(key.rc(), 2);
            assert_eq!(val.rc(), 2);
        }
        assert_eq!(key.rc(), 1);
        assert_eq!(val.rc(), 1);
    }
}
