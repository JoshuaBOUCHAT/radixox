use std::ptr::NonNull;

use radixox_lib::raw_alloc;

pub(crate) const CHILDS_SIZE: usize = 6;
const EXA_DIGIT_COUNT: usize = 16;
const LIGHT_OVERFLOW_SIZE: usize = EXA_DIGIT_COUNT - CHILDS_SIZE; // 10

// ── SWAR ─────────────────────────────────────────────────────────────────────

/// 0x80 exactement dans les lanes de `w` égales à `b` (sans faux positif, contrairement
/// au `haszero` classique : les radix périmés au-delà de `len` ne contaminent rien).
#[inline(always)]
fn eq_mask(w: u64, b: u8) -> u64 {
    const LO7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
    let x = w ^ 0x0101_0101_0101_0101u64.wrapping_mul(b as u64);
    !(((x & LO7).wrapping_add(LO7)) | x | LO7)
}

/// Masque des `n` premières lanes (saturé à 8).
#[inline(always)]
fn lanes(n: u32) -> u64 {
    if n >= 8 { u64::MAX } else { (1u64 << (n * 8)) - 1 }
}

/// Lit 8 octets à `off` dans `*base`.
/// SAFETY: `[off, off+8)` doit être dans `T` et entièrement initialisé (pas de padding).
#[inline(always)]
unsafe fn load_u64<T>(base: &T, off: usize) -> u64 {
    unsafe {
        let p = (base as *const T as *const u8).add(off);
        u64::from_le(p.cast::<u64>().read_unaligned())
    }
}

// ── Childs ───────────────────────────────────────────────────────────────────

#[repr(C, packed)]
pub(crate) struct Childs {
    idxs: [u32; CHILDS_SIZE],
    radixs: [u8; CHILDS_SIZE],
    len: u8,
}

pub(crate) trait ChildAble {
    fn find(&self, radix: u8) -> Option<u32>;
    fn push(&mut self, radix: u8, idx: u32);
    fn remove(&mut self, radix: u8) -> Option<u32>;
    fn is_empty(&self) -> bool;
    fn iter(&self) -> impl Iterator<Item = (u8, u32)>;
}

impl Default for Childs {
    fn default() -> Self {
        Self {
            idxs: [u32::MAX; CHILDS_SIZE],
            radixs: [0; CHILDS_SIZE],
            len: 0,
        }
    }
}

// Le mot lu par `position` = octets [23..31) : [idxs[5] haut][radixs 0..6][len].
const CHILDS_WORD_OFF: usize = 4 * CHILDS_SIZE - 1;
const _: () = {
    assert!(size_of::<Childs>() == 31);
    assert!(std::mem::offset_of!(Childs, radixs) == CHILDS_WORD_OFF + 1);
    assert!(std::mem::offset_of!(Childs, len) == CHILDS_WORD_OFF + 1 + CHILDS_SIZE);
};

impl ChildAble for Childs {
    #[inline]
    fn find(&self, radix: u8) -> Option<u32> {
        let i = self.position(radix)?;
        Some(self.idxs[i])
    }

    fn push(&mut self, radix: u8, idx: u32) {
        assert!(!self.is_full() && radix != 0);
        let len = self.len();
        self.idxs[len] = idx;
        self.radixs[len] = radix;
        self.len += 1;
    }

    fn remove(&mut self, radix: u8) -> Option<u32> {
        let pos = self.position(radix)?;
        let last_idx = self.len() - 1;
        self.radixs[pos] = self.radixs[last_idx];
        let cpy = self.idxs[pos];
        self.idxs[pos] = self.idxs[last_idx];
        self.len -= 1;
        Some(cpy)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn iter(&self) -> impl Iterator<Item = (u8, u32)> {
        let len = self.len();
        (0..len).map(|i| (self.radixs[i], self.idxs[i]))
    }
}

impl Childs {
    pub(crate) fn is_full(&self) -> bool {
        self.len == CHILDS_SIZE as u8
    }

    pub(crate) fn old_get_single_child(&self) -> Option<(u8, u32)> {
        if self.len() == 1 {
            Some((self.radixs[0], self.idxs[0]))
        } else {
            None
        }
    }

    fn len(&self) -> usize {
        self.len as usize
    }

    /// Index de `radix` parmi les enfants inline : un seul load de 8 octets, sans boucle.
    #[inline(always)]
    fn position(&self, radix: u8) -> Option<usize> {
        // SAFETY: [23..31) est dans Childs (31 o, packed) et entièrement initialisé.
        let w = unsafe { load_u64(self, CHILDS_WORD_OFF) } >> 8;
        let len = (w >> 48) as u32 & 0xFF; // lane 6 = len
        let m = eq_mask(w, radix) & lanes(len);
        (m != 0).then(|| (m.trailing_zeros() / 8) as usize)
    }
}

// ── Huge (bloc mimalloc derrière un pointeur fin) ────────────────────────────
//
// Au-delà des 16 enfants inline + légers, deux formats :
// - groupes `[[u8; 8][u32; 8]]` par paliers de 1/3/6/14 groupes (8/24/48/112
//   enfants, 40/120/240/560 o) : recherche SWAR par groupe ;
// - dense `[u32; 256]` indexé par radix (1 Ko) au-delà de 112 : accès direct.
// Rétrécissement avec hystérésis : groupes quand on retombe `SHRINK_MARGIN` sous le
// palier inférieur, dense → groupes sous `DENSE_SHRINK_LEN`.

const GROUP_TIERS: [usize; 4] = [1, 3, 6, 14];
const GROUPS_MAX_LEN: usize = 8 * 14; // 112
const DENSE_SHRINK_LEN: usize = 96;
const SHRINK_MARGIN: usize = 4;
const DENSE_EMPTY: u32 = u32::MAX;

#[derive(Clone, Copy)]
#[repr(C)]
struct Group {
    radixs: [u8; 8],
    idxs: [u32; 8],
}

type Dense = [u32; 256];

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HugeKind {
    None = 0,
    Groups = 1,
    Dense = 2,
    /// Slot libre de l'`OverflowArena` ; `idxs[0]` = slot libre suivant.
    Free = 3,
}

/// Plus petit palier de groupes qui contient `len` enfants.
#[inline]
fn tier_groups(len: usize) -> usize {
    debug_assert!(len <= GROUPS_MAX_LEN);
    GROUP_TIERS.into_iter().find(|&g| g * 8 >= len).unwrap_or(14)
}

fn alloc_groups(n: usize) -> NonNull<Group> {
    // SAFETY: libéré par `free_huge` ; mimalloc aligne sur 8 (Group demande 4).
    // Mis à zéro : la lecture SWAR d'un groupe partiellement rempli lit ses 8 lanes,
    // y compris celles pas encore écrites (masquées ensuite, mais elles doivent être init).
    let p = unsafe { raw_alloc::zalloc(n * size_of::<Group>()) } as *mut Group;
    NonNull::new(p).expect("mimalloc: out of memory")
}

fn alloc_dense() -> NonNull<Dense> {
    // SAFETY: idem ; rempli avant toute lecture.
    let p = unsafe { raw_alloc::malloc(size_of::<Dense>()) } as *mut Dense;
    let mut p = NonNull::new(p).expect("mimalloc: out of memory");
    unsafe { p.as_mut() }.fill(DENSE_EMPTY);
    p
}

/// Position du premier `radix` parmi les `len` premières entrées des groupes.
#[inline]
fn groups_position(groups: &[Group], len: usize, radix: u8) -> Option<usize> {
    for (g, grp) in groups[..len.div_ceil(8)].iter().enumerate() {
        let m = eq_mask(u64::from_le_bytes(grp.radixs), radix)
            & lanes((len - 8 * g) as u32);
        if m != 0 {
            return Some(8 * g + (m.trailing_zeros() / 8) as usize);
        }
    }
    None
}

// ── Overflow ──────────────────────────────────────────────────────────────────
#[repr(C)]
pub(crate) struct Overflow {
    idxs: [u32; LIGHT_OVERFLOW_SIZE],
    radixs: [u8; LIGHT_OVERFLOW_SIZE],
    len: u8,
    huge_len: u8,
    huge_kind: HugeKind,
    /// Groupes alloués (mode Groups) : 1, 3, 6 ou 14.
    huge_groups: u8,
    huge: *mut u8,
}

impl Default for Overflow {
    fn default() -> Self {
        Self {
            idxs: [u32::MAX; LIGHT_OVERFLOW_SIZE],
            radixs: [0; LIGHT_OVERFLOW_SIZE],
            len: 0,
            huge_len: 0,
            huge_kind: HugeKind::None,
            huge_groups: 0,
            huge: std::ptr::null_mut(),
        }
    }
}

impl Drop for Overflow {
    fn drop(&mut self) {
        self.free_huge();
    }
}

// Mots lus par `light_position` : A = [40..48) radixs 0..8,
// B = [43..51) radixs 3..10 + len (lane 7). Seuls des champs initialisés sont lus.
const OVF_RADIX_OFF: usize = 4 * LIGHT_OVERFLOW_SIZE;
const _: () = {
    assert!(LIGHT_OVERFLOW_SIZE == 10);
    assert!(std::mem::offset_of!(Overflow, radixs) == OVF_RADIX_OFF);
    assert!(std::mem::offset_of!(Overflow, len) == OVF_RADIX_OFF + LIGHT_OVERFLOW_SIZE);
    assert!(size_of::<Overflow>() == 64);
    assert!(size_of::<Group>() == 40);
};

impl ChildAble for Overflow {
    #[inline]
    fn find(&self, radix: u8) -> Option<u32> {
        if let Some(i) = self.light_position(radix) {
            return Some(self.idxs[i]);
        }
        self.huge_find(radix)
    }

    fn push(&mut self, radix: u8, idx: u32) {
        if (self.len as usize) < LIGHT_OVERFLOW_SIZE {
            let len = self.len as usize;
            self.radixs[len] = radix;
            self.idxs[len] = idx;
            self.len += 1;
        } else {
            self.huge_push(radix, idx);
        }
    }

    fn remove(&mut self, radix: u8) -> Option<u32> {
        if let Some(i) = self.light_position(radix) {
            let last = self.len as usize - 1;
            let val = self.idxs[i];
            self.radixs[i] = self.radixs[last];
            self.idxs[i] = self.idxs[last];
            self.len -= 1;
            return Some(val);
        }
        self.huge_remove(radix)
    }

    fn is_empty(&self) -> bool {
        self.len == 0 && self.huge_kind == HugeKind::None
    }

    fn iter(&self) -> impl Iterator<Item = (u8, u32)> {
        let light = (0..self.len as usize).map(|i| (self.radixs[i], self.idxs[i]));
        let groups = (self.huge_kind == HugeKind::Groups).then(|| {
            self.groups()
                .iter()
                .flat_map(|g| g.radixs.into_iter().zip(g.idxs))
                .take(self.huge_len as usize)
        });
        let dense = (self.huge_kind == HugeKind::Dense).then(|| {
            self.dense()
                .iter()
                .enumerate()
                .filter(|&(_, &v)| v != DENSE_EMPTY)
                .map(|(r, &v)| (r as u8, v))
        });
        light
            .chain(groups.into_iter().flatten())
            .chain(dense.into_iter().flatten())
    }
}

impl Overflow {
    /// Index de `radix` parmi les 10 enfants légers : deux loads de 8 octets qui se chevauchent.
    #[inline(always)]
    fn light_position(&self, radix: u8) -> Option<usize> {
        // SAFETY: [40..48) et [43..51) sont dans Overflow (repr(C)) et initialisés.
        let (a, b) = unsafe { (load_u64(self, OVF_RADIX_OFF), load_u64(self, OVF_RADIX_OFF + 3)) };
        let len = (b >> 56) as u32;
        let ma = eq_mask(a, radix) & lanes(len);
        if ma != 0 {
            return Some((ma.trailing_zeros() / 8) as usize);
        }
        // dans b, seules les lanes 5,6 (= radixs[8], radixs[9]) sont nouvelles
        let mb = eq_mask(b, radix) & lanes(len.saturating_sub(3)) & 0x00FF_FF00_0000_0000;
        (mb != 0).then(|| (mb.trailing_zeros() / 8) as usize + 3)
    }

    /// Parcourt tous les enfants (léger puis huge) sans allouer.
    pub(crate) fn for_each_child(&self, mut f: impl FnMut(u8, u32)) {
        for (r, i) in self.iter() {
            f(r, i);
        }
    }

    // ── huge ──

    /// Groupes alloués (palier courant). Vide si le huge n'est pas en mode groupes.
    #[inline]
    fn groups(&self) -> &[Group] {
        if self.huge_kind != HugeKind::Groups {
            return &[];
        }
        let n = self.huge_len.div_ceil(8) as usize;
        // SAFETY: en mode Groups, `huge` pointe `huge_groups` groupes dont les `huge_len`
        // premières entrées sont initialisées ; n ≤ huge_groups.
        unsafe { std::slice::from_raw_parts(self.huge as *const Group, n) }
    }

    /// Tout le bloc alloué (y compris les entrées pas encore écrites).
    #[inline]
    fn groups_mut(&mut self) -> &mut [Group] {
        debug_assert_eq!(self.huge_kind, HugeKind::Groups);
        // SAFETY: le bloc fait `huge_groups` groupes ; on n'y lit que des entrées < huge_len
        // (les autres sont écrites avant d'être comptées). Group est Copy sans invariant.
        unsafe { std::slice::from_raw_parts_mut(self.huge as *mut Group, self.huge_groups as usize) }
    }

    #[inline]
    fn dense(&self) -> &Dense {
        debug_assert_eq!(self.huge_kind, HugeKind::Dense);
        // SAFETY: en mode Dense, `huge` pointe un `[u32; 256]` initialisé.
        unsafe { &*(self.huge as *const Dense) }
    }

    #[inline]
    fn dense_mut(&mut self) -> &mut Dense {
        debug_assert_eq!(self.huge_kind, HugeKind::Dense);
        // SAFETY: idem, accès exclusif via &mut self.
        unsafe { &mut *(self.huge as *mut Dense) }
    }

    #[inline]
    fn huge_find(&self, radix: u8) -> Option<u32> {
        match self.huge_kind {
            HugeKind::None | HugeKind::Free => None,
            HugeKind::Groups => {
                let len = self.huge_len as usize;
                let groups = self.groups();
                let i = groups_position(groups, len, radix)?;
                Some(groups[i / 8].idxs[i % 8])
            }
            HugeKind::Dense => {
                let v = self.dense()[radix as usize];
                (v != DENSE_EMPTY).then_some(v)
            }
        }
    }

    fn huge_push(&mut self, radix: u8, idx: u32) {
        let len = self.huge_len as usize;
        match self.huge_kind {
            HugeKind::None => {
                self.huge = alloc_groups(GROUP_TIERS[0]).as_ptr() as *mut u8;
                self.huge_kind = HugeKind::Groups;
                self.huge_groups = GROUP_TIERS[0] as u8;
            }
            HugeKind::Groups if len == GROUPS_MAX_LEN => self.groups_to_dense(),
            HugeKind::Groups if len == self.huge_groups as usize * 8 => {
                self.regroup(tier_groups(len + 1));
            }
            _ => {}
        }
        match self.huge_kind {
            HugeKind::Groups => {
                let g = &mut self.groups_mut()[len / 8];
                g.radixs[len % 8] = radix;
                g.idxs[len % 8] = idx;
            }
            HugeKind::Dense => self.dense_mut()[radix as usize] = idx,
            HugeKind::None | HugeKind::Free => unreachable!(),
        }
        self.huge_len += 1;
    }

    fn huge_remove(&mut self, radix: u8) -> Option<u32> {
        let len = self.huge_len as usize;
        let val = match self.huge_kind {
            HugeKind::None | HugeKind::Free => return None,
            HugeKind::Groups => {
                let i = groups_position(self.groups(), len, radix)?;
                let last = len - 1;
                let groups = self.groups_mut();
                let val = groups[i / 8].idxs[i % 8];
                let (lr, li) = (groups[last / 8].radixs[last % 8], groups[last / 8].idxs[last % 8]);
                groups[i / 8].radixs[i % 8] = lr;
                groups[i / 8].idxs[i % 8] = li;
                val
            }
            HugeKind::Dense => {
                let slot = &mut self.dense_mut()[radix as usize];
                if *slot == DENSE_EMPTY {
                    return None;
                }
                std::mem::replace(slot, DENSE_EMPTY)
            }
        };
        self.huge_len -= 1;
        let len = len - 1;
        match self.huge_kind {
            _ if len == 0 => self.free_huge(),
            HugeKind::Dense if len <= DENSE_SHRINK_LEN => self.dense_to_groups(),
            HugeKind::Groups => {
                let target = tier_groups((len + SHRINK_MARGIN).min(GROUPS_MAX_LEN));
                if target < self.huge_groups as usize {
                    self.regroup(target);
                }
            }
            _ => {}
        }
        Some(val)
    }

    /// Réalloue les groupes vers un palier de `n` groupes (croissance ou rétrécissement) en
    /// copiant les groupes qui portent les `huge_len` entrées courantes.
    fn regroup(&mut self, n: usize) {
        let used = (self.huge_len as usize).div_ceil(8);
        debug_assert!(used <= n);
        let new = alloc_groups(n);
        // SAFETY: blocs distincts ; `used` ≤ huge_groups côté source et ≤ n côté destination.
        unsafe { std::ptr::copy_nonoverlapping(self.huge as *const Group, new.as_ptr(), used) };
        unsafe { raw_alloc::free(self.huge) };
        self.huge = new.as_ptr() as *mut u8;
        self.huge_groups = n as u8;
    }

    fn groups_to_dense(&mut self) {
        let len = self.huge_len as usize;
        let mut dense = alloc_dense();
        // SAFETY: bloc neuf, distinct de `huge`.
        let d = unsafe { dense.as_mut() };
        for (r, i) in self.groups().iter().flat_map(|g| g.radixs.into_iter().zip(g.idxs)).take(len) {
            d[r as usize] = i;
        }
        unsafe { raw_alloc::free(self.huge) };
        self.huge = dense.as_ptr() as *mut u8;
        self.huge_kind = HugeKind::Dense;
        self.huge_groups = 0;
    }

    fn dense_to_groups(&mut self) {
        let len = self.huge_len as usize;
        let n = tier_groups(len);
        let new = alloc_groups(n);
        // SAFETY: bloc neuf de n groupes, rempli sur les `len` premières entrées.
        let groups = unsafe { std::slice::from_raw_parts_mut(new.as_ptr(), n) };
        let mut k = 0;
        for (r, &v) in self.dense().iter().enumerate() {
            if v != DENSE_EMPTY {
                groups[k / 8].radixs[k % 8] = r as u8;
                groups[k / 8].idxs[k % 8] = v;
                k += 1;
            }
        }
        debug_assert_eq!(k, len);
        unsafe { raw_alloc::free(self.huge) };
        self.huge = new.as_ptr() as *mut u8;
        self.huge_kind = HugeKind::Groups;
        self.huge_groups = n as u8;
    }

    fn free_huge(&mut self) {
        if matches!(self.huge_kind, HugeKind::Groups | HugeKind::Dense) {
            // SAFETY: `huge` vient de raw_alloc::malloc et n'est libéré qu'ici ou en réallocation.
            unsafe { raw_alloc::free(self.huge) };
            self.huge = std::ptr::null_mut();
            self.huge_kind = HugeKind::None;
            self.huge_groups = 0;
            self.huge_len = 0;
        }
    }
}

// ── OverflowArena ─────────────────────────────────────────────────────────────

/// Un slot = un `Overflow` (64 o, aligné ligne de cache). Libre : `huge_kind == Free`
/// et `idxs[0]` chaîne la freelist (l'enum `Item | NextFree` ne profitait pas de la
/// niche de `HugeKind` et passait à 128 o).
#[repr(align(64))]
struct OverflowSlot(Overflow);

const _: () = assert!(size_of::<OverflowSlot>() == 64);

pub(crate) struct OverflowArena {
    slots: Vec<OverflowSlot>,
    free_head: u32, // u32::MAX = no free slots
    count: usize,
}

impl OverflowArena {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_head: u32::MAX,
            count: 0,
        }
    }

    pub(crate) fn alloc(&mut self) -> u32 {
        self.count += 1;
        if self.free_head != u32::MAX {
            let idx = self.free_head;
            let slot = &mut self.slots[idx as usize].0;
            assert!(slot.huge_kind == HugeKind::Free, "free_head points to occupied slot");
            self.free_head = slot.idxs[0];
            *slot = Overflow::default();
            idx
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(OverflowSlot(Overflow::default()));
            idx
        }
    }

    /// Frees the slot and its huge block if any.
    pub(crate) fn free(&mut self, idx: u32) {
        let slot = &mut self.slots[idx as usize].0;
        debug_assert!(slot.huge_kind != HugeKind::Free, "double free");
        slot.free_huge();
        slot.len = 0;
        slot.huge_kind = HugeKind::Free;
        slot.idxs[0] = self.free_head;
        self.free_head = idx;
        self.count -= 1;
    }

    /// Précharge le bloc `idx` (sans le lire).
    #[inline(always)]
    pub(crate) fn prefetch(&self, idx: u32) {
        if let Some(slot) = self.slots.get(idx as usize) {
            radixox_lib::prefetch::prefetch_l1(slot as *const OverflowSlot as *const u8);
        }
    }

    pub(crate) fn get(&self, idx: u32) -> Option<&Overflow> {
        let o = &self.slots[idx as usize].0;
        (o.huge_kind != HugeKind::Free).then_some(o)
    }

    pub(crate) fn get_mut(&mut self, idx: u32) -> Option<&mut Overflow> {
        let o = &mut self.slots[idx as usize].0;
        (o.huge_kind != HugeKind::Free).then_some(o)
    }

    pub(crate) fn count(&self) -> usize {
        self.count
    }
}

#[cfg(test)]
mod huge_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn check(o: &Overflow, model: &BTreeMap<u8, u32>) {
        for r in 0..=255u8 {
            assert_eq!(o.find(r), model.get(&r).copied(), "find({r})");
        }
        let mut got: Vec<_> = o.iter().collect();
        got.sort_unstable();
        let want: Vec<_> = model.iter().map(|(&r, &i)| (r, i)).collect();
        assert_eq!(got, want);
        assert_eq!(o.is_empty(), model.is_empty());
        if o.huge_kind == HugeKind::Groups {
            assert!(o.huge_len as usize <= o.huge_groups as usize * 8);
        }
    }

    /// Montée jusqu'à 256 enfants (light → groupes 8/24/48/112 → dense), puis descente
    /// complète, dans un ordre pseudo-aléatoire ; vérifie find/iter à chaque palier.
    #[test]
    fn huge_tiers_roundtrip() {
        let mut seed = 0x9E37_79B9u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        // Miri : un tour, vérifs espacées (les transitions de palier restent toutes traversées).
        let (rounds, every) = if cfg!(miri) { (1, 23) } else { (4, 1) };
        for round in 0..rounds {
            let mut radixs: Vec<u8> = (0..=255u8).collect();
            for i in (1..radixs.len()).rev() {
                radixs.swap(i, next() as usize % (i + 1));
            }
            let mut o = Overflow::default();
            let mut model = BTreeMap::new();
            for (k, &r) in radixs.iter().enumerate() {
                o.push(r, k as u32 + round);
                model.insert(r, k as u32 + round);
                if (k % 7 == 0 || k > 100 && k < 130) && k % every == 0 {
                    check(&o, &model);
                }
            }
            check(&o, &model);
            assert_eq!(o.huge_kind, HugeKind::Dense);
            // retraits dans un autre ordre, avec quelques ré-insertions (oscillation autour des seuils)
            for i in (1..radixs.len()).rev() {
                radixs.swap(i, next() as usize % (i + 1));
            }
            for (k, &r) in radixs.iter().enumerate() {
                assert_eq!(o.remove(r), model.remove(&r));
                assert_eq!(o.remove(r), None);
                if k % 5 == 0 {
                    o.push(r, 7);
                    model.insert(r, 7);
                    assert_eq!(o.remove(r), model.remove(&r));
                }
                if (k % 7 == 0 || k > 120) && k % every == 0 {
                    check(&o, &model);
                }
            }
            assert!(o.is_empty());
            assert_eq!(o.huge_kind, HugeKind::None);
        }
    }
}

#[cfg(test)]
mod swar_tests {
    use super::*;

    /// Compare `position`/`light_position` à la recherche linéaire, y compris avec
    /// des radix périmés au-delà de `len` (swap-remove) et des octets ≥ 0x80.
    #[test]
    fn swar_matches_linear() {
        let probes = [0u8, 1, 2, 7, b'a', b'z', 0x7F, 0x80, 0xFE, 0xFF];
        for len in 0..=CHILDS_SIZE {
            for &stale in &probes {
                let mut c = Childs::default();
                c.radixs = [stale; CHILDS_SIZE];
                for i in 0..len {
                    c.radixs[i] = probes[(i * 3 + 1) % probes.len()];
                }
                c.len = len as u8;
                let radixs = c.radixs;
                for &b in &probes {
                    let want = radixs[..len].iter().position(|&r| r == b);
                    assert_eq!(c.position(b), want, "Childs len={len} stale={stale} b={b}");
                }
            }
        }
        for len in 0..=LIGHT_OVERFLOW_SIZE {
            for &stale in &probes {
                let mut o = Overflow::default();
                o.radixs = [stale; LIGHT_OVERFLOW_SIZE];
                for i in 0..len {
                    o.radixs[i] = probes[(i * 7 + 2) % probes.len()];
                }
                o.len = len as u8;
                for &b in &probes {
                    let want = o.radixs[..len].iter().position(|&r| r == b);
                    assert_eq!(o.light_position(b), want, "Overflow len={len} stale={stale} b={b}");
                }
            }
        }
    }
}
