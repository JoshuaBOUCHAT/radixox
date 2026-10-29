use crate::raw_alloc::external;
use core::borrow::Borrow;

use core::ops::Deref;
use core::ptr::NonNull;

use crate::prefetch::PreFetch;

/// `ptr` pointe sur les **données** ; l'en-tête est lu à reculons, en `u16` alignés
/// (l'adresse des données est toujours paire). MEMORY_ARCH.md §16.2 :
///
/// ```text
/// forme courte : [len_lo u16][rc u8][lfu u8][données…]              len ≤ POOL_MAX_LEN
/// forme longue : [len_hi u16][len_lo u16][rc u8][lfu u8][données…]  bit 15 de len_lo = EXTERNE
/// ```
///
/// `rc` (MEMORY_ARCH.md §10.2 ter) : `0xxxxxxx` = compte de 0 à 127 ; `1iiiiiii` = case
/// `i` de la table de débordement ([`RcOverflow`]), qui tient le compte en `u32`. `lfu`
/// est réservé au compteur d'éviction (§10.2 bis), à 0 tant que rien ne l'écrit.
///
/// La forme longue est allouée par `external` (allocateur des grands objets, MEMORY_ARCH §18) ; la courte par le pool
/// ([`crate::pool`]) par défaut, par la libc sous `--cfg radixox_nopool` ou
/// `--cfg radixox_sysalloc` (ASan ne voit pas les objets du pool).
#[repr(transparent)]
pub struct SharedByte {
    ptr: NonNull<u8>,
}

impl SharedByte {
    const SHORT_HEADER: usize = 4;
    const LONG_HEADER: usize = 6;
    const EXTERNAL: u16 = 0x8000;
    /// G : au-delà, toute valeur est externe.
    pub const POOL_MAX_LEN: usize = 16 * 1024;
    /// 2³¹ − 1 : `len_hi` porte 16 bits, `len_lo` 15.
    pub const MAX_LEN: usize = (1 << 31) - 1;

    /// Octet `rc` (le premier du `u16` qui précède les données ; `lfu` est le second).
    #[inline]
    fn rc_ptr(&self) -> *mut u8 {
        unsafe { self.ptr.as_ptr().sub(2) }
    }

    #[inline]
    fn len_lo(&self) -> u16 {
        unsafe { (self.ptr.as_ptr() as *const u16).sub(2).read() }
    }

    #[inline]
    fn data_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Alloue un objet de `len` octets, en-tête écrit et `rc == 1`, données non
    /// initialisées. `external` force la forme longue.
    ///
    /// # Safety
    /// L'appelant écrit les `len` octets de données avant toute lecture.
    unsafe fn alloc(len: usize, external: bool) -> Self {
        assert!(len <= Self::MAX_LEN);
        unsafe {
            let short = if external || len > Self::POOL_MAX_LEN {
                core::ptr::null_mut()
            } else {
                crate::pool::short_alloc(Self::slot(len))
            };
            let data = if !short.is_null() {
                (short as *mut u16).write(len as u16);
                short.add(Self::SHORT_HEADER)
            } else {
                // Forme longue : demandée, trop grande, ou pool plein (repli).
                let base = external::malloc(Self::LONG_HEADER + len);
                assert!(!base.is_null());
                (base as *mut u16).write((len >> 15) as u16);
                (base as *mut u16).add(1).write(Self::EXTERNAL | (len as u16 & 0x7FFF));
                base.add(Self::LONG_HEADER)
            };
            data.sub(2).write(1); // rc
            data.sub(1).write(LFU_INIT); // lfu
            Self { ptr: NonNull::new_unchecked(data) }
        }
    }

    /// Emplacement d'un objet en forme courte : en-tête + données, arrondi au granule (2 o).
    #[inline]
    fn slot(len: usize) -> usize {
        (Self::SHORT_HEADER + len + 1) & !1
    }

    fn copy_of(data: &[u8], external: bool) -> Self {
        unsafe {
            let sb = Self::alloc(data.len(), external);
            core::ptr::copy_nonoverlapping(data.as_ptr(), sb.ptr.as_ptr(), data.len());
            sb
        }
    }

    pub fn from_byte(data: impl Deref<Target = [u8]>) -> Self {
        Self::copy_of(&data, false)
    }
    #[inline]
    pub fn from_slice(data: impl AsRef<[u8]>) -> Self {
        Self::copy_of(data.as_ref(), false)
    }

    /// Toujours la forme longue, quelle que soit la taille : pour les `SharedByte` qui
    /// vivent longtemps hors de l'arbre, que le GC ne voit pas (MEMORY_ARCH.md §16.9).
    pub fn from_slice_external(data: impl AsRef<[u8]>) -> Self {
        Self::copy_of(data.as_ref(), true)
    }

    /// `self` en forme longue : copie si l'objet est en forme courte, sinon rien.
    pub fn into_external(self) -> Self {
        if self.is_external() { self } else { Self::copy_of(&self, true) }
    }

    /// Écrase les données en place (MEMORY_ARCH.md §16.5) : seulement si l'objet n'est
    /// partagé avec personne (`rc == 1`), en forme courte, et si `src` tient dans son
    /// emplacement. Faux : rien n'a changé, à l'appelant de remplacer l'objet.
    ///
    /// `rc == 1` ne protège que des copies qui clonent : une structure dont la capture
    /// de save copie les `SharedByte` bit à bit (valart) doit refuser elle-même pendant
    /// une save.
    pub fn overwrite_from(&mut self, src: &[u8]) -> bool {
        if self.rc() != 1 || self.is_external() || src.len() > Self::POOL_MAX_LEN {
            return false;
        }
        let (old, new) = (Self::slot(self.len()), Self::slot(src.len()));
        if new > old {
            return false;
        }
        unsafe {
            core::ptr::copy(src.as_ptr(), self.ptr.as_ptr(), src.len());
            (self.ptr.as_ptr() as *mut u16).sub(2).write(src.len() as u16);
            if new < old {
                crate::pool::short_shrink(self.ptr.as_ptr().sub(Self::SHORT_HEADER), old, new);
            }
        }
        true
    }

    /// GC (MEMORY_ARCH.md §16.6) : un objet d'un bloc marqué est recopié au curseur des
    /// survivants et l'ancien emplacement rendu. Tenu ailleurs (`rc > 1`) : épinglé,
    /// laissé en place. À n'appeler que sur un emplacement que l'arbre est seul à voir
    /// (énumérateurs du GC) : `self` est réécrit vers les mêmes octets.
    /// Rend les octets recopiés (0 : rien à faire, épinglé, ou plage pleine).
    pub fn gc_relocate(&mut self) -> usize {
        #[cfg(not(any(radixox_nopool, radixox_sysalloc)))]
        {
            // L'adresse d'abord : la plupart des objets ne sont pas dans un bloc marqué,
            // et lire l'en-tête serait un défaut de cache par valeur visitée. Une forme
            // longue (`external`) est hors de la plage du pool : jamais marquée.
            let base = self.header_ptr() as *mut u8;
            if !crate::pool::is_marked(base as usize) || self.is_external() {
                return 0;
            }
            if self.rc() != 1 {
                crate::pool::note_pinned();
                return 0;
            }
            let size = Self::slot(self.len());
            let new = crate::pool::alloc_survivor(size);
            if new.is_null() {
                return 0; // plage pleine : l'objet reste
            }
            unsafe {
                core::ptr::copy_nonoverlapping(base, new, size);
                let moved = Self { ptr: NonNull::new_unchecked(new.add(Self::SHORT_HEADER)) };
                // rc 1 → 0 : l'ancien emplacement est rendu par le chemin normal.
                drop(core::mem::replace(self, moved));
            }
            crate::pool::note_evacuated(size);
            size
        }
        #[cfg(any(radixox_nopool, radixox_sysalloc))]
        0
    }

    /// Emplacement dans le pool (adresse de l'en-tête, taille), `None` hors pool
    /// (forme longue, ou forme courte dans la libc). Vérificateur du GC.
    pub fn pool_slot(&self) -> Option<(usize, usize)> {
        (crate::pool::ENABLED && !self.is_external())
            .then(|| (self.header_ptr() as usize, Self::slot(self.len())))
    }

    #[inline]
    pub fn is_external(&self) -> bool {
        self.len_lo() & Self::EXTERNAL != 0
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        Self::from_slice(s.as_bytes())
    }

    #[inline]
    pub fn len(&self) -> usize {
        let lo = self.len_lo();
        if lo & Self::EXTERNAL == 0 {
            lo as usize
        } else {
            let hi = unsafe { (self.ptr.as_ptr() as *const u16).sub(3).read() };
            ((hi as usize) << 15) | (lo & 0x7FFF) as usize
        }
    }

    /// Prefetch de l'en-tête et du début des données.
    #[inline]
    pub fn prefetch(&self) {
        crate::prefetch::prefetch_l1(self.header_ptr());
    }

    /// Début de l'en-tête (des deux formes, la ligne de cache de `len_lo` et `rc`).
    #[inline]
    fn header_ptr(&self) -> *const u8 {
        unsafe { self.ptr.as_ptr().sub(Self::SHORT_HEADER) }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.data_ptr(), self.len()) }
    }

    pub fn as_str(&self) -> Result<&str, core::str::Utf8Error> {
        core::str::from_utf8(self.as_slice())
    }

    /// Nombre de détenteurs, table de débordement comprise.
    pub fn rc(&self) -> u32 {
        let b = unsafe { self.rc_ptr().read() };
        if b & RC_TABLE == 0 { b as u32 } else { unsafe { RC_OVERFLOW.counts[(b & !RC_TABLE) as usize] } }
    }

    /// `clone` au-delà de 127 détenteurs : entrée dans la table, ou case déjà prise.
    /// Table pleine : copie profonde, un objet neuf à `rc == 1` (les octets d'un objet
    /// partagé ne changent jamais : `overwrite_from` et `to_uppercase` exigent `rc == 1`).
    #[cold]
    #[inline(never)]
    fn clone_overflow(&self) -> Self {
        unsafe {
            let rc = self.rc_ptr();
            let t = &mut *core::ptr::addr_of_mut!(RC_OVERFLOW);
            let b = *rc;
            if b & RC_TABLE != 0 {
                // 2³² détenteurs = 32 Gio de pointeurs : pas atteignable.
                t.counts[(b & !RC_TABLE) as usize] += 1;
            } else {
                let Some(i) = t.counts.iter().position(|&c| c == 0) else {
                    t.copies += 1;
                    return Self::copy_of(self, self.is_external());
                };
                t.counts[i] = RC_INLINE_MAX as u32 + 1;
                t.used += 1;
                *rc = RC_TABLE | i as u8;
            }
            Self { ptr: self.ptr }
        }
    }

    /// `drop` d'un objet de la table : à 127 détenteurs, la case est rendue et le compte
    /// revient dans l'en-tête (même seuil qu'à l'entrée).
    #[cold]
    #[inline(never)]
    fn drop_overflow(&self) {
        unsafe {
            let rc = self.rc_ptr();
            let t = &mut *core::ptr::addr_of_mut!(RC_OVERFLOW);
            let i = (*rc & !RC_TABLE) as usize;
            t.counts[i] -= 1;
            if t.counts[i] == RC_INLINE_MAX as u32 {
                t.counts[i] = 0;
                t.used -= 1;
                *rc = RC_INLINE_MAX;
            }
        }
    }

    /// Uppercases the bytes in-place if `rc == 1`, otherwise allocates a new
    /// buffer (same form), copies uppercase in one pass, and decrements rc on the old allocation.
    ///
    /// After this call, `as_slice()` is guaranteed to be ASCII-uppercase.
    pub fn to_uppercase(&mut self) {
        let len = self.len();
        if self.rc() == 1 {
            unsafe {
                let data = self.ptr.as_ptr();
                for i in 0..len {
                    *data.add(i) = (*data.add(i)).to_ascii_uppercase();
                }
            }
        } else {
            unsafe {
                let new = Self::alloc(len, self.is_external());
                let src = self.data_ptr();
                let dst = new.ptr.as_ptr();
                for i in 0..len {
                    *dst.add(i) = (*src.add(i)).to_ascii_uppercase();
                }
                // Assigner `*self` drop l'ancienne valeur → décrémente son rc.
                *self = new;
            }
        }
    }

    /// Dernier `drop` : rend l'objet à son allocateur, aiguillé par le bit EXTERNE.
    #[cold]
    unsafe fn dealloc(&self) {
        unsafe {
            if self.is_external() {
                let size = Self::LONG_HEADER + self.len();
                external::free(self.ptr.as_ptr().sub(Self::LONG_HEADER), size);
            } else {
                crate::pool::short_free(self.ptr.as_ptr().sub(Self::SHORT_HEADER), Self::slot(self.len()));
            }
        }
    }
}

impl Clone for SharedByte {
    #[inline]
    fn clone(&self) -> Self {
        unsafe {
            let rc = self.rc_ptr();
            if *rc < RC_INLINE_MAX {
                *rc += 1;
                return Self { ptr: self.ptr };
            }
        }
        self.clone_overflow()
    }
}

impl Drop for SharedByte {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let rc = self.rc_ptr();
            if *rc & RC_TABLE != 0 {
                return self.drop_overflow();
            }
            *rc -= 1;
            if *rc == 0 {
                self.dealloc();
            }
        }
    }
}

/// Valeur initiale du compteur d'accès (comme Redis) : en LFU, une valeur neuve survit à
/// quelques passages de l'aiguille ; en LRU (CLOCK), « lue depuis le dernier passage »
/// veut dire au-dessus. 0 sans la feature `eviction` (rien ne le lit).
pub const LFU_INIT: u8 = if cfg!(feature = "eviction") { 5 } else { 0 };

impl SharedByte {
    /// Compteur d'accès (octet `lfu` de l'en-tête, MEMORY_ARCH.md §10.2 bis).
    #[inline]
    pub fn lfu(&self) -> u8 {
        unsafe { self.ptr.as_ptr().sub(1).read() }
    }

    /// Écrit le compteur (aiguille de l'éviction : décroissance, seconde chance).
    #[inline]
    pub fn set_lfu(&self, v: u8) {
        unsafe { self.ptr.as_ptr().sub(1).write(v) }
    }

    /// Un accès en lecture (feature `eviction`) : compteur de Morris, incrémenté avec une
    /// probabilité 1 / ((c − LFU_INIT) × 10 + 1) (le `lfu-log-factor` 10 de Redis), au moins
    /// 1 après un accès (seconde chance du CLOCK).
    #[cfg(feature = "eviction")]
    #[inline]
    pub fn touch(&self) {
        let c = self.lfu();
        if c == u8::MAX {
            return;
        }
        let base = c.saturating_sub(LFU_INIT) as u32;
        if c == 0 || base == 0 || morris_rand() % (base * 10 + 1) == 0 {
            self.set_lfu(c + 1);
        }
    }
}

/// xorshift32, mono-thread comme le reste du serveur ; la qualité suffit à un compteur
/// probabiliste.
#[cfg(feature = "eviction")]
#[inline]
fn morris_rand() -> u32 {
    static mut STATE: u32 = 0x9E37_79B9;
    unsafe {
        let mut x = STATE;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        STATE = x;
        x
    }
}

/// Bit de l'octet `rc` : les 7 autres sont l'indice d'une case de la table.
const RC_TABLE: u8 = 0x80;
/// Plus grand compte tenu dans l'en-tête.
const RC_INLINE_MAX: u8 = 127;

/// Table de débordement des compteurs (MEMORY_ARCH.md §10.2 ter) : 128 cases, une par
/// objet tenu plus de 127 fois. La remplir demande 128 × 128 détentions simultanées ;
/// pleine, `clone` copie au lieu de partager — jamais d'échec, jamais de fuite.
/// Mono-thread comme le pool : `SharedByte` n'est pas `Send`.
struct RcOverflow {
    /// 0 = case libre.
    counts: [u32; 128],
    used: u32,
    copies: u64,
}

static mut RC_OVERFLOW: RcOverflow = RcOverflow { counts: [0; 128], used: 0, copies: 0 };

/// `INFO` : (cases occupées, copies faites faute de case).
pub fn rc_overflow_stats() -> (u32, u64) {
    unsafe {
        let t = &*core::ptr::addr_of!(RC_OVERFLOW);
        (t.used, t.copies)
    }
}

impl core::ops::Deref for SharedByte {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for SharedByte {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl core::fmt::Debug for SharedByte {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // octets échappés : une clé binaire (non UTF-8) reste lisible
        write!(f, "SharedByte(\"{}\", rc={})", self.as_slice().escape_ascii(), self.rc())
    }
}

impl PartialEq for SharedByte {
    fn eq(&self, other: &Self) -> bool {
        // ptr identity fast path
        if self.ptr == other.ptr {
            return true;
        }
        self.as_slice() == other.as_slice()
    }
}
impl Eq for SharedByte {}

impl core::hash::Hash for SharedByte {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}
impl PartialOrd for SharedByte {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SharedByte {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}
impl Borrow<[u8]> for SharedByte {
    fn borrow(&self) -> &[u8] {
        self
    }
}
impl PreFetch for SharedByte {
    fn prefetch(&self)
    where
        Self: Sized,
    {
        crate::prefetch::prefetch_l1(self.header_ptr());
    }
}

/// Un `SharedByte` à un seul propriétaire (rc == 1). Pas `Send` : l'allocateur des
/// `SharedByte` est mono-thread (MEMORY_ARCH.md §15, worker de persistance).

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct OwnedByte(SharedByte);
impl Borrow<[u8]> for OwnedByte {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}
impl OwnedByte {
    pub fn from_slice(data: impl AsRef<[u8]>) -> Self {
        Self(SharedByte::from_slice(data.as_ref()))
    }
    #[inline]
    pub fn into_shared(self) -> SharedByte {
        self.0
    }
}
impl core::ops::Deref for OwnedByte {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}
impl Clone for OwnedByte {
    fn clone(&self) -> Self {
        Self(SharedByte::from_slice(self.0.as_slice()))
    }
}
impl Into<SharedByte> for OwnedByte {
    fn into(self) -> SharedByte {
        self.0
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::prelude::*;
    use super::*;

    #[test]
    fn to_uppercase_shared_releases_old_rc() {
        let a = SharedByte::from_slice(b"hello");
        let mut b = a.clone();
        assert_eq!(a.rc(), 2);
        b.to_uppercase();
        assert_eq!(b.as_slice(), b"HELLO");
        assert_eq!(b.rc(), 1);
        assert_eq!(a.as_slice(), b"hello");
        assert_eq!(a.rc(), 1);
    }

    #[test]
    fn to_uppercase_unique_in_place() {
        let mut a = SharedByte::from_slice(b"MiXeD");
        a.to_uppercase();
        assert_eq!(a.as_slice(), b"MIXED");
        assert_eq!(a.rc(), 1);
    }
}
