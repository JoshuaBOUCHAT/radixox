use std::alloc::{Layout, handle_alloc_error};

// Blocs overflow : pool des structures, alignés sur 8 (MEMORY_ARCH.md §17.4) ; ce qui
// suffit au tag de 3 bits. Taille arrondie à 8 (les curseurs du pool n'allouent que des
// multiples de 8), à l'allocation comme à la libération. Le pool ne rend pas de mémoire
// à zéro : remise à zéro ici.
// En test, allocateur système pour qu'AddressSanitizer voie les blocs.
#[cfg(not(test))]
unsafe fn block_zalloc(layout: Layout) -> *mut u8 {
    let size = align_up8(layout.size());
    unsafe {
        let p = radixox_lib::pool::struct_alloc(size);
        if !p.is_null() {
            std::ptr::write_bytes(p, 0, size);
        }
        p
    }
}
#[cfg(not(test))]
unsafe fn block_free(ptr: *mut u8, layout: Layout) {
    unsafe { radixox_lib::pool::struct_free(ptr, align_up8(layout.size())) }
}
#[cfg(test)]
unsafe fn block_zalloc(layout: Layout) -> *mut u8 {
    unsafe { std::alloc::alloc_zeroed(layout) }
}
#[cfg(test)]
unsafe fn block_free(ptr: *mut u8, layout: Layout) {
    unsafe { std::alloc::dealloc(ptr, layout) }
}
use std::ptr::NonNull;

use radixox_lib::prefetch::PreFetch;
use radixox_lib::prefetch::PreFetchPtr;
use radixox_lib::shared_byte::SharedByte;

/// Capacités possibles pour un bloc overflow. L'indice dans ce tableau (0..=6)
/// est la "classe" de capacité, stockée dans les 3 bits bas du pointeur.
const CAPACITIES: [usize; 7] = [4, 8, 16, 32, 64, 128, 256];
const TAG_MASK: usize = 0b111;

/// Le radix space fait 256 valeurs (`u8`) et l'inline du node parent en
/// occupe toujours au moins une : l'overflow ne dépasse jamais 255 entrées,
/// `len` et `nb_childs` tiennent dans un `u8`.
#[inline]
const fn bitmap_len(capacity: usize) -> usize {
    capacity.div_ceil(8)
}

#[inline]
const fn align_up8(n: usize) -> usize {
    (n + 7) & !7
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

    /// Clé en place : seulement pour la réécrire vers les mêmes octets (GC).
    #[inline]
    pub fn key_mut(&mut self, i: usize) -> &mut SharedByte {
        debug_assert!(i < self.len());
        unsafe { &mut *self.keys_ptr().add(i) }
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
            let raw = block_zalloc(layout);
            if raw.is_null() {
                handle_alloc_error(layout);
            }
            debug_assert!(raw as usize & TAG_MASK == 0);
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

    /// Taille du bloc alloué (octets demandés au pool).
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
            block_free(self.base_ptr(), self.block_layout());
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

impl SetOverflow {
    /// Copie bit à bit du bloc pour une image de save : les entrées ne sont
    /// pas clonées (pas de refcount). Libérer avec [`Self::free_shallow`],
    /// jamais dropper.
    pub(crate) fn shallow_copy(&self) -> std::mem::ManuallyDrop<Self> {
        let layout = self.block_layout();
        unsafe {
            let raw = block_zalloc(layout);
            if raw.is_null() {
                handle_alloc_error(layout);
            }
            std::ptr::copy_nonoverlapping(self.base_ptr(), raw, layout.size());
            let tagged = (raw as usize | self.class() as usize) as *mut u8;
            std::mem::ManuallyDrop::new(Self {
                ptr: NonNull::new_unchecked(tagged),
            })
        }
    }

    /// Libère le bloc d'une copie superficielle sans dropper ses entrées.
    pub(crate) fn free_shallow(copy: std::mem::ManuallyDrop<Self>) {
        unsafe { block_free(copy.base_ptr(), copy.block_layout()) };
    }
}

impl Default for SetOverflow {
    fn default() -> Self {
        Self::new()
    }
}

impl SetOverflow {
    /// GC (MEMORY_ARCH.md §17.4) : donne le bloc (sans tag, taille arrondie à 8) au
    /// visiteur, qui peut le déplacer, puis remet le tag sur la nouvelle adresse.
    pub(crate) fn gc_block(&mut self, g: &mut impl FnMut(&mut *mut u8, usize)) {
        let class = self.class() as usize;
        let mut base = self.base_ptr();
        g(&mut base, align_up8(self.block_bytes()));
        // SAFETY: `base` est non nul (bloc vivant, éventuellement recopié).
        self.ptr = unsafe { NonNull::new_unchecked((base as usize | class) as *mut u8) };
    }
}

impl Drop for SetOverflow {
    fn drop(&mut self) {
        for i in 0..self.len() {
            unsafe { std::ptr::drop_in_place(self.keys_ptr().add(i)) };
        }
        unsafe { block_free(self.base_ptr(), self.block_layout()) };
    }
}

// --- HashOverflow : clé + valeur par entrée, enfants packés -----------------
//
// Même schéma que `SetOverflow`, avec une colonne de valeurs en plus :
//
// [len: u8][nb_childs: u8][child_cap: u16]
// [radix: cap][ctrl: cap][has_child: cap/8 bits][pad → 8]
// [keys: cap × 8][vals: cap × 8][childs: child_cap × 4]

const HASH_HEADER: usize = 4;

#[inline]
const fn hash_keys_start(cap: usize) -> usize {
    align_up8(HASH_HEADER + 2 * cap + bitmap_len(cap))
}

#[inline]
const fn hash_block_size(cap: usize, child_cap: usize) -> usize {
    hash_keys_start(cap) + cap * 16 + child_cap * 4
}

#[repr(transparent)]
pub struct HashOverflow {
    ptr: NonNull<u8>,
}

impl PreFetch for SetOverflow {
    fn prefetch(&self) {
        self.ptr.ptr_prefetch();
    }
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
        unsafe { self.base_ptr().add(HASH_HEADER) }
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
        unsafe { self.base_ptr().add(hash_keys_start(self.capacity())) as *mut SharedByte }
    }

    #[inline]
    fn vals_ptr(&self) -> *mut SharedByte {
        unsafe { self.keys_ptr().add(self.capacity()) }
    }

    #[inline]
    fn childs_ptr(&self) -> *mut u32 {
        unsafe { self.vals_ptr().add(self.capacity()) as *mut u32 }
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
    pub fn val(&self, i: usize) -> &SharedByte {
        debug_assert!(i < self.len());
        unsafe { &*self.vals_ptr().add(i) }
    }

    #[inline]
    pub fn val_mut(&mut self, i: usize) -> &mut SharedByte {
        debug_assert!(i < self.len());
        unsafe { &mut *self.vals_ptr().add(i) }
    }

    /// Clé en place : seulement pour la réécrire vers les mêmes octets (GC).
    #[inline]
    pub fn key_mut(&mut self, i: usize) -> &mut SharedByte {
        debug_assert!(i < self.len());
        unsafe { &mut *self.keys_ptr().add(i) }
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

    /// Nombre d'entrées avec enfant parmi `0..i` (voir `SetOverflow::rank`).
    #[inline]
    fn rank(&self, i: usize) -> usize {
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

    /// Remplace la clé, la valeur et le ctrl de l'entrée `i` (radix et enfant
    /// inchangés), retourne l'ancien couple `(clé, valeur)`.
    pub fn replace_entry(
        &mut self,
        i: usize,
        key: SharedByte,
        val: SharedByte,
        ctrl: u8,
    ) -> (SharedByte, SharedByte) {
        debug_assert!(i < self.len());
        unsafe {
            *self.ctrl_ptr().add(i) = ctrl;
            (
                std::ptr::replace(self.keys_ptr().add(i), key),
                std::ptr::replace(self.vals_ptr().add(i), val),
            )
        }
    }

    fn alloc_block(class: u8, child_cap: usize) -> Self {
        let size = hash_block_size(CAPACITIES[class as usize], child_cap);
        let layout = Layout::from_size_align(size, 8).unwrap();
        unsafe {
            // Zéro : voir `SetOverflow::alloc_block`.
            let raw = block_zalloc(layout);
            if raw.is_null() {
                handle_alloc_error(layout);
            }
            debug_assert!(raw as usize & TAG_MASK == 0);
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

    /// Taille du bloc alloué (octets demandés au pool).
    pub fn block_bytes(&self) -> usize {
        hash_block_size(self.capacity(), self.child_capacity())
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
            // Bits ≥ len toujours à 0 : `bitmap_len(len)` suffit.
            std::ptr::copy_nonoverlapping(self.bitmap_ptr(), new.bitmap_ptr(), bitmap_len(len));
            std::ptr::copy_nonoverlapping(self.keys_ptr(), new.keys_ptr(), len);
            std::ptr::copy_nonoverlapping(self.vals_ptr(), new.vals_ptr(), len);
            std::ptr::copy_nonoverlapping(self.childs_ptr(), new.childs_ptr(), nb_childs);
            block_free(self.base_ptr(), self.block_layout());
        }
        // Voir `SetOverflow::realloc` : on adopte le pointeur, sans Drop.
        self.ptr = new.ptr;
        std::mem::forget(new);
    }

    /// Ajoute une entrée en fin de tableau, sans dédup (à l'appelant de
    /// garantir que `radix` est absent).
    pub fn add_value(&mut self, radix: u8, ctrl: u8, key: SharedByte, val: SharedByte, child: u32) {
        if self.is_full() {
            let class = self.class();
            assert!(class < 6, "HashOverflow: max capacity (256) reached");
            self.realloc(class + 1, self.child_capacity());
        }
        let i = self.len();
        unsafe {
            *self.radix_ptr().add(i) = radix;
            *self.ctrl_ptr().add(i) = ctrl;
            self.keys_ptr().add(i).write(key);
            self.vals_ptr().add(i).write(val);
        }
        self.set_len(i + 1);
        self.set_child(i, child);
    }

    /// Position de l'entrée de radix `radix` (unique dans le node), SWAR.
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
    /// préservé). Retourne `(radix, ctrl, key, val, child)`.
    pub fn swap_remove(&mut self, i: usize) -> (u8, u8, SharedByte, SharedByte, u32) {
        debug_assert!(i < self.len());
        let last = self.len() - 1;
        let child = self.child(i);
        self.set_child_no_shrink(i, NO_CHILD);
        let removed = unsafe {
            (
                *self.radix_ptr().add(i),
                *self.ctrl_ptr().add(i),
                self.keys_ptr().add(i).read(),
                self.vals_ptr().add(i).read(),
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
                self.vals_ptr().add(i).write(self.vals_ptr().add(last).read());
            }
            self.set_child_no_shrink(i, last_child);
        }
        self.set_len(last);
        self.shrink_if_sparse();
        removed
    }

    /// Voir `SetOverflow::shrink_if_sparse`.
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

    /// Retire la dernière entrée. Retourne `(radix, ctrl, key, val, child)`.
    pub fn pop(&mut self) -> Option<(u8, u8, SharedByte, SharedByte, u32)> {
        let last = self.len().checked_sub(1)?;
        Some(self.swap_remove(last))
    }
}

impl HashOverflow {
    /// Copie bit à bit du bloc pour une image de save : les entrées ne sont
    /// pas clonées (pas de refcount). Libérer avec [`Self::free_shallow`],
    /// jamais dropper.
    pub(crate) fn shallow_copy(&self) -> std::mem::ManuallyDrop<Self> {
        let layout = self.block_layout();
        unsafe {
            let raw = block_zalloc(layout);
            if raw.is_null() {
                handle_alloc_error(layout);
            }
            std::ptr::copy_nonoverlapping(self.base_ptr(), raw, layout.size());
            let tagged = (raw as usize | self.class() as usize) as *mut u8;
            std::mem::ManuallyDrop::new(Self {
                ptr: NonNull::new_unchecked(tagged),
            })
        }
    }

    /// Libère le bloc d'une copie superficielle sans dropper ses entrées.
    pub(crate) fn free_shallow(copy: std::mem::ManuallyDrop<Self>) {
        unsafe { block_free(copy.base_ptr(), copy.block_layout()) };
    }
}

impl Default for HashOverflow {
    fn default() -> Self {
        Self::new()
    }
}

impl HashOverflow {
    /// GC (MEMORY_ARCH.md §17.4) : donne le bloc (sans tag, taille arrondie à 8) au
    /// visiteur, qui peut le déplacer, puis remet le tag sur la nouvelle adresse.
    pub(crate) fn gc_block(&mut self, g: &mut impl FnMut(&mut *mut u8, usize)) {
        let class = self.class() as usize;
        let mut base = self.base_ptr();
        g(&mut base, align_up8(self.block_bytes()));
        // SAFETY: `base` est non nul (bloc vivant, éventuellement recopié).
        self.ptr = unsafe { NonNull::new_unchecked((base as usize | class) as *mut u8) };
    }
}

impl Drop for HashOverflow {
    fn drop(&mut self) {
        for i in 0..self.len() {
            unsafe {
                std::ptr::drop_in_place(self.keys_ptr().add(i));
                std::ptr::drop_in_place(self.vals_ptr().add(i));
            }
        }
        unsafe { block_free(self.base_ptr(), self.block_layout()) };
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
    fn hash_push_get_grow_shrink() {
        let mut o = HashOverflow::new();
        for i in 0..200u32 {
            let child = if i % 3 == 0 { i * 10 } else { NO_CHILD };
            o.add_value(
                i as u8,
                (i * 7) as u8,
                SharedByte::from_slice(i.to_le_bytes()),
                SharedByte::from_slice((i * 2).to_le_bytes()),
                child,
            );
        }
        assert_eq!((o.len(), o.childs_len()), (200, 67));
        for i in 0..200u32 {
            let child = if i % 3 == 0 { i * 10 } else { NO_CHILD };
            assert_eq!(o.radix(i as usize), i as u8);
            assert_eq!(o.ctrl(i as usize), (i * 7) as u8);
            assert_eq!(o.child(i as usize), child);
            assert_eq!(o.key(i as usize).as_slice(), &i.to_le_bytes());
            assert_eq!(o.val(i as usize).as_slice(), &(i * 2).to_le_bytes());
            assert_eq!(o.find(i as u8), Some(i as usize));
        }
        while o.len() > 2 {
            let (radix, _, key, val, child) = o.swap_remove(0);
            let r = radix as u32;
            assert_eq!(key.as_slice(), &r.to_le_bytes());
            assert_eq!(val.as_slice(), &(r * 2).to_le_bytes());
            assert_eq!(child, if r % 3 == 0 { r * 10 } else { NO_CHILD });
            for i in 0..o.len() {
                let r = o.radix(i) as u32;
                assert_eq!(o.val(i).as_slice(), &(r * 2).to_le_bytes());
                assert_eq!(o.child(i), if r % 3 == 0 { r * 10 } else { NO_CHILD });
            }
        }
        assert_eq!(o.capacity(), 4);
    }

    #[test]
    fn hash_drop_releases_both_shared_bytes() {
        let key = SharedByte::from_slice(b"k");
        let val = SharedByte::from_slice(b"v");
        {
            let mut o = HashOverflow::new();
            o.add_value(0, 0, key.clone(), val.clone(), 3);
            assert_eq!(key.rc(), 2);
            assert_eq!(val.rc(), 2);
            let (old_k, old_v) =
                o.replace_entry(0, SharedByte::from_slice(b"k2"), val.clone(), 1);
            assert_eq!((old_k.rc(), old_v.rc()), (2, 3));
        }
        assert_eq!(key.rc(), 1);
        assert_eq!(val.rc(), 1);
    }
}
