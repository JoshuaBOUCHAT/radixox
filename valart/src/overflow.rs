use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::ptr::NonNull;

use radixox_lib::prefetch::PreFetch;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::prefetch::PreFetchPtr;

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

// --- SetOverflow : une seule valeur (key) par entrée ------------------------
//
// Bloc alloué (align 8), pointeur tagué (3 bits bas = classe de capacité) :
// `[len: u8][bitmap: (cap+7)/8 bytes][radix: cap bytes][pad][packed data]`
//
// Zone data, par paire d'entrées (2i, 2i+1) : 24 bytes, zéro padding puisque
// key (8B, align 8) alterne avec deux idx (4+4=8B) :
// [key_2i(8) | idx_2i(4) | idx_2i+1(4) | key_2i+1(8)]

const SET_STRIDE: usize = 24;

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
        let pair_base = unsafe { self.data_ptr().add((i / 2) * SET_STRIDE) };
        let off = if i % 2 == 0 { 0 } else { 16 };
        unsafe { pair_base.add(off) as *mut SharedByte }
    }

    fn idx_ptr(&self, i: usize) -> *mut u32 {
        let pair_base = unsafe { self.data_ptr().add((i / 2) * SET_STRIDE) };
        let off = if i % 2 == 0 { 8 } else { 12 };
        unsafe { pair_base.add(off) as *mut u32 }
    }

    pub fn key(&self, i: usize) -> &SharedByte {
        debug_assert!(i < self.len());
        unsafe { &*self.key_ptr(i) }
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
        data_start(self.capacity()) + (self.capacity() / 2) * SET_STRIDE
    }

    fn alloc_block(class: u8) -> Self {
        let capacity = CAPACITIES[class as usize];
        let size = data_start(capacity) + (capacity / 2) * SET_STRIDE;
        let layout = Layout::from_size_align(size, 8).unwrap();
        unsafe {
            let raw = alloc(layout);
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
        assert!(class < 6, "SetOverflow: max capacity (256) reached");
        let old_cap = self.capacity();
        let mut new = Self::alloc_block(class + 1);
        unsafe {
            std::ptr::copy_nonoverlapping(self.bitmap_ptr(), new.bitmap_ptr(), bitmap_len(old_cap));
            std::ptr::copy_nonoverlapping(self.radix_ptr(), new.radix_ptr(), old_cap);
            std::ptr::copy_nonoverlapping(
                self.data_ptr(),
                new.data_ptr(),
                (old_cap / 2) * SET_STRIDE,
            );
        }
        new.set_len(self.len());
        unsafe {
            dealloc(
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
    pub fn add_value(&mut self, radix: u8, key: SharedByte, idx: u32, has_ttl: bool) {
        if self.is_full() {
            self.grow();
        }
        let i = self.len();
        self.set_radix(i, radix);
        unsafe { self.key_ptr(i).write(key) };
        unsafe { self.idx_ptr(i).write(idx) };
        self.set_ttl(i, has_ttl);
        self.set_len(i + 1);
    }

    /// Scan linéaire sur `radix` (pas de dédup à l'insertion donc au plus une
    /// entrée matche en usage normal ; la première trouvée est retournée).
    /// Version lecture seule : le TTL est un simple `bool`, pas de ref vers
    /// le bitmap (impossible d'avoir `&mut bool` sur un bit tassé).
    pub fn get_value(&self, radix: u8) -> Option<(&SharedByte, &u8, bool)> {
        (0..self.len())
            .find(|&i| self.radix(i) == radix)
            .map(|i| unsafe {
                (
                    &*self.key_ptr(i),
                    &*self.radix_ptr().add(i),
                    self.has_ttl(i),
                )
            })
    }

    /// Version lecture/écriture : retourne l'index `i` au lieu du TTL — pas
    /// de `&mut bool` possible sur le bitmap, l'appelant modifie le TTL via
    /// `has_ttl(i)` / `set_ttl(i, ..)` avec cet index.
    pub fn get_value_mut(&mut self, radix: u8) -> Option<(&mut SharedByte, &u8, usize)> {
        let i = (0..self.len()).find(|&i| self.radix(i) == radix)?;
        let key_ptr = self.key_ptr(i);
        let radix_ptr = unsafe { self.radix_ptr().add(i) };
        unsafe { Some((&mut *key_ptr, &*radix_ptr, i)) }
    }

    /// Scan linéaire sur `radix` : retourne juste `(index, has_ttl)`, sans
    /// emprunter la clé — utilisé quand l'appelant a seulement besoin de
    /// savoir où est l'entrée et si elle porte un TTL (ex: décider s'il faut
    /// aller vérifier l'expiration avant de renvoyer `Overflow { idx }`).
    pub fn position_and_ttl(&self, radix: u8) -> Option<(usize, bool)> {
        let i = (0..self.len()).find(|&i| self.radix(i) == radix)?;
        Some((i, self.has_ttl(i)))
    }

    /// Retire l'entrée `i` en la remplaçant par la dernière (ordre non préservé).
    pub fn swap_remove(&mut self, i: usize) -> (SharedByte, u32) {
        debug_assert!(i < self.len());
        let last = self.len() - 1;
        unsafe {
            let removed_key = self.key_ptr(i).read();
            let removed_idx = self.idx_ptr(i).read();
            if i != last {
                let last_key = self.key_ptr(last).read();
                let last_idx = self.idx_ptr(last).read();
                let last_ttl = self.has_ttl(last);
                self.key_ptr(i).write(last_key);
                self.idx_ptr(i).write(last_idx);
                self.set_radix(i, self.radix(last));
                self.set_ttl(i, last_ttl);
            }
            self.set_len(last);
            (removed_key, removed_idx)
        }
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
            unsafe { std::ptr::drop_in_place(self.key_ptr(i)) };
        }
        unsafe {
            dealloc(
                self.base_ptr(),
                Layout::from_size_align(self.total_size(), 8).unwrap(),
            )
        };
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
impl PreFetch for HashOverflow{
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
            let raw = alloc(layout);
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
            dealloc(
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

    pub fn get_value(&mut self,radix:u8,now:u64)->Option<SharedByte>

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
            dealloc(
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
        for i in 0..20u32 {
            o.add_value(
                i as u8,
                SharedByte::from_slice(i.to_le_bytes()),
                i * 10,
                i % 3 == 0,
            );
        }
        assert_eq!(o.len(), 20);
        assert!(o.capacity() >= 20);
        for i in 0..20u32 {
            assert_eq!(o.radix(i as usize), i as u8);
            assert_eq!(o.idx(i as usize), i * 10);
            assert_eq!(o.has_ttl(i as usize), i % 3 == 0);
            assert_eq!(o.key(i as usize).as_slice(), &i.to_le_bytes());
        }
    }

    #[test]
    fn set_drop_releases_shared_byte_rc() {
        let shared = SharedByte::from_slice(b"hello");
        {
            let mut o = SetOverflow::new();
            o.add_value(0, shared.clone(), 0, false);
            o.add_value(1, shared.clone(), 0, false);
            assert_eq!(shared.rc(), 3);
        }
        assert_eq!(shared.rc(), 1);
    }

    #[test]
    fn set_swap_remove() {
        let mut o = SetOverflow::new();
        for i in 0..4u32 {
            o.add_value(i as u8, SharedByte::from_slice(i.to_le_bytes()), i, false);
        }
        let (k, idx) = o.swap_remove(0);
        assert_eq!(k.as_slice(), &0u32.to_le_bytes());
        assert_eq!(idx, 0);
        assert_eq!(o.len(), 3);
        // le dernier élément (i=3) a été déplacé dans le slot 0
        assert_eq!(o.key(0).as_slice(), &3u32.to_le_bytes());
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
