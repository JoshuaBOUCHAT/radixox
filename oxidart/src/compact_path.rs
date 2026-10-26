// Chemin d'un nœud ou suffixe d'une feuille : 7 octets inline, sinon un objet du pool des
// `SharedByte` (MEMORY_ARCH.md §17), sans compteur de références (propriétaire unique).
use radixox_lib::pool;
use radixox_lib::raw_alloc::external;
use radixox_lib::shared_byte::SharedByte;
use std::ops::Deref;

// Inline : inner[0] = (len << 1) | 1    len ∈ 0..=7
//          inner[1..8] = data
// Tas    : inner = ptr vers les données, en-tête lu à reculons en u16 alignés (adresse
//          paire → bit 0 = 0, ce qui distingue le tas de l'inline) :
//            forme courte (pool, ≤ 16 Kio) : [len u16][données]          bit 15 de len à 0
//            forme longue (`external`)     : [len_hi u16][len_lo u16][données]  bit 15 à 1
union Inner {
    heap: *mut u8,
    inline: [u8; 8],
}

pub struct CompactPath(Inner);

impl CompactPath {
    const INLINE_CAP: usize = 7;
    const SHORT_HEADER: usize = 2;
    const LONG_HEADER: usize = 4;
    const LONG: u16 = 0x8000;
    /// Au-delà, forme longue (même seuil que les `SharedByte`).
    const POOL_MAX_LEN: usize = SharedByte::POOL_MAX_LEN;

    #[inline]
    pub fn new() -> Self {
        Self(Inner {
            inline: [1, 0, 0, 0, 0, 0, 0, 0],
        })
    }

    /// Sur le tas (plus de 7 octets) : se lit sans déréférencer, avant que les
    /// données soient en cache.
    #[inline]
    pub(crate) fn is_heap(&self) -> bool {
        unsafe { self.0.inline[0] & 1 == 0 }
    }

    #[inline]
    fn len_lo(&self) -> u16 {
        unsafe { (self.0.heap as *const u16).sub(1).read() }
    }

    /// Longueur de la forme tas (`None` : inline).
    #[cfg(test)]
    pub(crate) fn heap_len(&self) -> Option<usize> {
        self.is_heap().then(|| self.heap_slice_len())
    }

    #[inline]
    fn heap_slice_len(&self) -> usize {
        let lo = self.len_lo();
        if lo & Self::LONG == 0 {
            lo as usize
        } else {
            let hi = unsafe { (self.0.heap as *const u16).sub(2).read() };
            ((hi as usize) << 15) | (lo & !Self::LONG) as usize
        }
    }

    /// Emplacement d'une forme courte : en-tête + données, arrondi au granule (2 o).
    #[inline]
    fn slot(len: usize) -> usize {
        (Self::SHORT_HEADER + len + 1) & !1
    }

    /// Objet tas de `len` octets, en-tête écrit, données à remplir ; rend le pointeur des
    /// données.
    unsafe fn alloc(len: usize) -> *mut u8 {
        assert!(len <= SharedByte::MAX_LEN);
        unsafe {
            let short = if len > Self::POOL_MAX_LEN { std::ptr::null_mut() } else { pool::short_alloc(Self::slot(len)) };
            if !short.is_null() {
                (short as *mut u16).write(len as u16);
                return short.add(Self::SHORT_HEADER);
            }
            // trop long, ou pool plein (repli)
            let base = external::malloc(Self::LONG_HEADER + len);
            assert!(!base.is_null());
            (base as *mut u16).write((len >> 15) as u16);
            (base as *mut u16).add(1).write(Self::LONG | (len as u16 & !Self::LONG));
            base.add(Self::LONG_HEADER)
        }
    }

    fn heap_of(data: &[u8]) -> Self {
        unsafe {
            let p = Self::alloc(data.len());
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            Self(Inner { heap: p })
        }
    }

    /// Recompression : remplace self par (self ++ radix ++ suffix) en une seule alloc.
    pub fn append_and_replace(&mut self, radix: u8, suffix: &[u8]) {
        let new = {
            let old: &[u8] = self;
            let new_len = old.len() + 1 + suffix.len();
            if new_len <= Self::INLINE_CAP {
                let mut inner = [0u8; 8];
                inner[0] = ((new_len as u8) << 1) | 1;
                inner[1..1 + old.len()].copy_from_slice(old);
                inner[1 + old.len()] = radix;
                inner[2 + old.len()..1 + new_len].copy_from_slice(suffix);
                Self(Inner { inline: inner })
            } else {
                unsafe {
                    let data = Self::alloc(new_len);
                    std::ptr::copy_nonoverlapping(old.as_ptr(), data, old.len());
                    *data.add(old.len()) = radix;
                    std::ptr::copy_nonoverlapping(suffix.as_ptr(), data.add(old.len() + 1), suffix.len());
                    Self(Inner { heap: data })
                }
            }
        }; // emprunt `old` libéré ici → *self = new drop l'ancien en sécurité
        *self = new;
    }

    /// Retire les `k` premiers octets. La forme courte qui reste sur le tas garde son objet
    /// du pool : données décalées, fin rendue par `short_shrink` (un trou de 0 à `k` octets,
    /// pas un objet entier pour le GC : un `split_leaf` par insertion au chargement). Sinon
    /// (≤ 7 octets restants, forme longue) : recopie.
    pub fn drop_front(&mut self, k: usize) {
        if k == 0 {
            return;
        }
        let len = self.len();
        debug_assert!(k <= len);
        let new_len = len - k;
        if !self.is_heap() || new_len <= Self::INLINE_CAP || self.len_lo() & Self::LONG != 0 {
            let new = Self::from_slice(&self[k..]);
            *self = new;
            return;
        }
        unsafe {
            let p = self.0.heap;
            std::ptr::copy(p.add(k), p, new_len);
            (p as *mut u16).sub(1).write(new_len as u16);
            let (old, new) = (Self::slot(len), Self::slot(new_len));
            if new < old {
                pool::short_shrink(p.sub(Self::SHORT_HEADER), old, new);
            }
        }
    }

    pub fn from_slice(data: &[u8]) -> Self {
        if data.len() <= Self::INLINE_CAP {
            let mut inner = [0u8; 8];
            inner[0] = ((data.len() as u8) << 1) | 1;
            inner[1..1 + data.len()].copy_from_slice(data);
            Self(Inner { inline: inner })
        } else {
            Self::heap_of(data)
        }
    }

    /// Emplacement dans le pool (adresse de l'en-tête, taille), `None` hors pool (inline,
    /// forme longue, ou mimalloc sans pool). Vérificateur du GC.
    pub(crate) fn pool_slot(&self) -> Option<(usize, usize)> {
        if !pool::ENABLED || !self.is_heap() || self.len_lo() & Self::LONG != 0 {
            return None;
        }
        let base = unsafe { self.0.heap.sub(Self::SHORT_HEADER) } as usize;
        Some((base, Self::slot(self.len_lo() as usize)))
    }

    /// GC (MEMORY_ARCH.md §17) : un objet d'un bloc marqué est recopié au curseur des
    /// survivants. Propriétaire unique : jamais épinglé. Rend les octets recopiés.
    pub(crate) fn gc_relocate(&mut self) -> usize {
        // L'adresse d'abord, sans lire l'en-tête (un défaut de cache par chemin visité) :
        // une forme longue, hors du pool, n'est jamais marquée.
        if !self.is_heap() || !pool::is_marked(unsafe { self.0.heap.sub(Self::SHORT_HEADER) } as usize) {
            return 0;
        }
        let Some((base, size)) = self.pool_slot() else { return 0 };
        let new = pool::alloc_survivor(size);
        if new.is_null() {
            return 0; // plage pleine : l'objet reste
        }
        unsafe {
            std::ptr::copy_nonoverlapping(base as *const u8, new, size);
            self.0.heap = new.add(Self::SHORT_HEADER);
            pool::short_free(base as *mut u8, size);
        }
        pool::note_evacuated(size);
        size
    }
}

impl CompactPath {
    /// Prefetch de l'objet tas (rien si inline).
    #[inline]
    pub fn prefetch(&self) {
        if self.is_heap() {
            radixox_lib::prefetch::prefetch_l1(unsafe { self.0.heap.sub(Self::SHORT_HEADER) });
        }
    }
}

impl Deref for CompactPath {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        unsafe {
            let tag = self.0.inline[0];
            if tag & 1 == 1 {
                let len = (tag >> 1) as usize;
                // Aide le compilo: len ne peut pas dépasser 7 par construction
                std::hint::assert_unchecked(len <= 7);
                self.0.inline.get_unchecked(1..1 + len)
            } else {
                std::slice::from_raw_parts(self.0.heap, self.heap_slice_len())
            }
        }
    }
}

impl Default for CompactPath {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CompactPath {
    fn drop(&mut self) {
        if !self.is_heap() {
            return;
        }
        unsafe {
            let len = self.heap_slice_len();
            if self.len_lo() & Self::LONG == 0 {
                pool::short_free(self.0.heap.sub(Self::SHORT_HEADER), Self::slot(len));
            } else {
                external::free(self.0.heap.sub(Self::LONG_HEADER), Self::LONG_HEADER + len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_front_keeps_bytes_in_every_form() {
        // tas → tas (sur place), tas → inline, forme longue, inline
        for (len, k) in [(30, 1), (30, 9), (30, 23), (30, 30), (12, 5), (5, 2), (70_000, 3), (70_000, 69_995)] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let mut p = CompactPath::from_slice(&data);
            p.drop_front(k);
            assert_eq!(&*p, &data[k..], "len {len}, k {k}");
            p.drop_front(0);
            assert_eq!(&*p, &data[k..]);
        }
    }

    #[test]
    fn compact_path_lengths_and_forms() {
        for len in [0, 1, 7, 8, 9, 22, 255, 16_383, 16_384, 16_385, 70_000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 13 + 5) as u8).collect();
            let p = CompactPath::from_slice(&data);
            assert_eq!(&*p, &data[..], "len {len}");
            assert_eq!(p.heap_len(), (len > 7).then_some(len));
            assert_eq!(p.pool_slot().is_some(), pool::ENABLED && len > 7 && len <= 16_384, "len {len}");
            let mut q = CompactPath::from_slice(&data[..len.min(3)]);
            q.append_and_replace(b'!', &data);
            assert_eq!(q.len(), len.min(3) + 1 + len);
            assert_eq!(&q[len.min(3) + 1..], &data[..]);
        }
    }
}
