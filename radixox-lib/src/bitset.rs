//! Bitset natif, forme clairsemée : B+tree des offsets des bits à 1 (BITSET_DESIGN.md).
//!
//! - Nœuds de 64 o (`#[repr(C)]` : en-tête `u32`, 7 clés `u32`, 8 enfants `NodeIdx`) dans un
//!   **slab global** commun à tous les bitsets : blocs de 1 Kio (16 nœuds), une table `ptrs`,
//!   un masque d'occupation `u16` par bloc, allocation dans le bloc **le plus plein**.
//! - **B+tree** : les offsets sont dans les feuilles ; les nœuds internes portent des
//!   séparateurs (l'enfant `i` contient les clés `k` avec `keys[i-1] <= k < keys[i]`).
//!   Un séparateur peut survivre à la suppression de la clé qui l'a créé : il reste une
//!   borne valide.
//! - Coupe préventive à la descente d'insertion, réparation préventive (emprunt ou fusion)
//!   à la descente de suppression : jamais de remontée.
//! - Plus de bit `is_leaf` : la hauteur est dans la table des arbres, l'en-tête porte
//!   `len` (3 bits) et l'`id` de l'arbre (29 bits, pour le GC à venir).
//! - Table globale `trees[id] = (racine, hauteur, nb de bits à 1)` ; la valeur oxidart ne
//!   portera que le [`BitsetId`].
//!
//! Pas encore : GC par évacuation (§5), feuilles denses (§6), insertion SIMD (§7),
//! libération par tranches d'un gros bitset (DEL).
//!
//! Mono-thread : état global en `static mut` (ARCHITECTURE.md §1, `RUST_TEST_THREADS=1`).

use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use alloc::vec::Vec;
use core::ptr;

// ─── Nœud ─────────────────────────────────────────────────────────────────────

/// Clés par nœud (impair : médiane nette pour la coupe des nœuds internes).
const CAP: usize = 7;
/// Minimum de clés d'un nœud non racine.
const MIN: usize = 3;
const LEN_BITS: u32 = 3;
const LEN_MASK: u32 = (1 << LEN_BITS) - 1;
/// `id` d'arbre sur les 29 bits hauts de l'en-tête.
const MAX_TREES: usize = 1 << (32 - LEN_BITS);

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Node {
    /// bits 0..3 : `len` (≤ 7) ; bits 3..32 : `id` de l'arbre.
    header: u32,
    /// Triées, valides sur `0..len`.
    keys: [u32; CAP],
    /// Valides sur `0..=len` dans un nœud interne, inutilisés dans une feuille.
    child: [NodeIdx; CAP + 1],
}
const _: () = assert!(size_of::<Node>() == 64);

impl Node {
    #[inline]
    fn len(&self) -> usize {
        (self.header & LEN_MASK) as usize
    }
    #[inline]
    fn set_len(&mut self, len: usize) {
        debug_assert!(len <= CAP);
        self.header = (self.header & !LEN_MASK) | len as u32;
    }
    #[inline]
    fn tree_id(&self) -> u32 {
        self.header >> LEN_BITS
    }
    /// Nombre de clés `<= x` : l'enfant à suivre dans un nœud interne. Sans branche
    /// dépendante des données (7 comparaisons, `len` masque les cases mortes).
    #[inline]
    fn route(&self, x: u32) -> usize {
        let len = self.len();
        let mut n = 0;
        for (j, &k) in self.keys.iter().enumerate() {
            n += (j < len && k <= x) as usize;
        }
        n
    }
    /// Nombre de clés `< x` : position de `x` dans une feuille.
    #[inline]
    fn lower_bound(&self, x: u32) -> usize {
        let len = self.len();
        let mut n = 0;
        for (j, &k) in self.keys.iter().enumerate() {
            n += (j < len && k < x) as usize;
        }
        n
    }
    /// Feuille : insère `k` en `i`.
    #[inline]
    fn leaf_insert(&mut self, i: usize, k: u32) {
        let len = self.len();
        debug_assert!(len < CAP && i <= len);
        self.keys.copy_within(i..len, i + 1);
        self.keys[i] = k;
        self.set_len(len + 1);
    }
    /// Feuille : retire la clé `i`.
    #[inline]
    fn leaf_remove(&mut self, i: usize) {
        let len = self.len();
        debug_assert!(i < len);
        self.keys.copy_within(i + 1..len, i);
        self.set_len(len - 1);
    }
    /// Interne : insère le séparateur `sep` en `i` et son enfant de droite en `i + 1`.
    #[inline]
    fn insert_sep(&mut self, i: usize, sep: u32, right: NodeIdx) {
        let len = self.len();
        debug_assert!(len < CAP && i <= len);
        self.keys.copy_within(i..len, i + 1);
        self.keys[i] = sep;
        self.child.copy_within(i + 1..len + 1, i + 2);
        self.child[i + 1] = right;
        self.set_len(len + 1);
    }
    /// Interne : retire le séparateur `i` et son enfant de droite `i + 1`.
    #[inline]
    fn remove_sep(&mut self, i: usize) {
        let len = self.len();
        debug_assert!(i < len);
        self.keys.copy_within(i + 1..len, i);
        self.child.copy_within(i + 2..len + 1, i + 1);
        self.set_len(len - 1);
    }
}

// ─── Index ────────────────────────────────────────────────────────────────────

/// 28 bits de n° de bloc + 4 bits de case dans le bloc. `NONE` (`u32::MAX`) n'est jamais
/// produit : le dernier n° de bloc est exclu.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct NodeIdx(u32);

const SLOTS: usize = 16;
const SLOT_BITS: u32 = 4;
/// N° de bloc max exclu : `(1 << 28) - 1`, pour que `NodeIdx` n'atteigne jamais `NONE`.
const MAX_BLOCKS: usize = (1 << (32 - SLOT_BITS)) - 1;
const NONE: u32 = u32::MAX;

impl NodeIdx {
    const NONE: Self = Self(NONE);
    #[inline]
    fn new(block: usize, slot: usize) -> Self {
        debug_assert!(slot < SLOTS && block < MAX_BLOCKS);
        Self(((block as u32) << SLOT_BITS) | slot as u32)
    }
    #[inline]
    fn block(self) -> usize {
        (self.0 >> SLOT_BITS) as usize
    }
    #[inline]
    fn slot(self) -> usize {
        (self.0 & (SLOTS as u32 - 1)) as usize
    }
}

// ─── Slab global ──────────────────────────────────────────────────────────────

#[repr(C, align(64))]
struct Block([Node; SLOTS]);
const BLOCK_LAYOUT: Layout = Layout::new::<Block>();
const _: () = assert!(size_of::<Block>() == 1024);

/// Blocs de 16 nœuds. Les blocs partiellement remplis sont rangés dans 15 listes
/// doublement chaînées (une par nombre de cases vivantes) : l'allocation prend la tête de
/// la liste la plus pleine en O(1) ; un bloc vide est rendu à l'allocateur.
struct Slab {
    /// Nul : bloc rendu (n° dans `free_blocks`).
    ptrs: Vec<*mut Block>,
    masks: Vec<u16>,
    next: Vec<u32>,
    prev: Vec<u32>,
    /// `heads[c]` : premier bloc à `c` cases vivantes (1..=15).
    heads: [u32; SLOTS],
    /// Bit `c` : `heads[c]` non vide.
    nonempty: u16,
    free_blocks: Vec<u32>,
    live_nodes: usize,
}

impl Slab {
    const fn new() -> Self {
        Self {
            ptrs: Vec::new(),
            masks: Vec::new(),
            next: Vec::new(),
            prev: Vec::new(),
            heads: [NONE; SLOTS],
            nonempty: 0,
            free_blocks: Vec::new(),
            live_nodes: 0,
        }
    }

    #[inline]
    fn node(&self, i: NodeIdx) -> *mut Node {
        debug_assert!(self.masks[i.block()] & (1 << i.slot()) != 0, "nœud mort {i:?}");
        let b: *mut Block = self.ptrs[i.block()];
        unsafe { b.cast::<Node>().add(i.slot()) }
    }

    fn link(&mut self, b: u32, c: usize) {
        let h = self.heads[c];
        self.next[b as usize] = h;
        self.prev[b as usize] = NONE;
        if h != NONE {
            self.prev[h as usize] = b;
        }
        self.heads[c] = b;
        self.nonempty |= 1 << c;
    }

    fn unlink(&mut self, b: u32, c: usize) {
        let (p, n) = (self.prev[b as usize], self.next[b as usize]);
        if p == NONE {
            self.heads[c] = n;
            if n == NONE {
                self.nonempty &= !(1 << c);
            }
        } else {
            self.next[p as usize] = n;
        }
        if n != NONE {
            self.prev[n as usize] = p;
        }
    }

    #[cold]
    fn new_block(&mut self) -> u32 {
        let p = unsafe { alloc(BLOCK_LAYOUT) } as *mut Block;
        if p.is_null() {
            handle_alloc_error(BLOCK_LAYOUT);
        }
        if let Some(b) = self.free_blocks.pop() {
            self.ptrs[b as usize] = p;
            return b;
        }
        let b = self.ptrs.len();
        assert!(b < MAX_BLOCKS, "bitset : slab plein ({MAX_BLOCKS} blocs)");
        self.ptrs.push(p);
        self.masks.push(0);
        self.next.push(NONE);
        self.prev.push(NONE);
        b as u32
    }

    /// Nouveau nœud vide (`len = 0`, enfants à `NONE`) de l'arbre `tree`.
    fn alloc(&mut self, tree: u32) -> NodeIdx {
        let (b, c) = if self.nonempty != 0 {
            let c = 15 - self.nonempty.leading_zeros() as usize;
            let b = self.heads[c];
            self.unlink(b, c);
            (b, c)
        } else {
            (self.new_block(), 0)
        };
        let m = &mut self.masks[b as usize];
        let slot = (!*m).trailing_zeros() as usize;
        *m |= 1 << slot;
        if c + 1 < SLOTS {
            self.link(b, c + 1);
        }
        self.live_nodes += 1;
        let idx = NodeIdx::new(b as usize, slot);
        unsafe {
            self.node(idx).write(Node {
                header: tree << LEN_BITS,
                keys: [0; CAP],
                child: [NodeIdx::NONE; CAP + 1],
            })
        };
        idx
    }

    fn free(&mut self, i: NodeIdx) {
        let b = i.block() as u32;
        let m = self.masks[b as usize];
        debug_assert!(m & (1 << i.slot()) != 0, "double libération {i:?}");
        let c = m.count_ones() as usize;
        if c < SLOTS {
            self.unlink(b, c);
        }
        let m = m & !(1 << i.slot());
        self.masks[b as usize] = m;
        self.live_nodes -= 1;
        if m == 0 {
            unsafe { dealloc(self.ptrs[b as usize] as *mut u8, BLOCK_LAYOUT) };
            self.ptrs[b as usize] = ptr::null_mut();
            self.free_blocks.push(b);
        } else {
            self.link(b, c - 1);
        }
    }

    fn live_blocks(&self) -> usize {
        self.ptrs.len() - self.free_blocks.len()
    }
}

// ─── Table des arbres ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Tree {
    root: NodeIdx,
    /// Nombre de niveaux : 1 = la racine est une feuille. 0 : `id` libre.
    height: u32,
    /// Bits à 1.
    count: u64,
}

struct State {
    slab: Slab,
    trees: Vec<Tree>,
    free_ids: Vec<u32>,
    /// Arbres détruits par [`BitsetId::destroy_lazy`], rendus par [`free_step`].
    doomed: Vec<Doomed>,
}

/// Arbre en cours de libération : pile `(nœud, niveau, prochain enfant)`.
struct Doomed {
    id: u32,
    stack: Vec<(NodeIdx, u32, usize)>,
}

static mut STATE: State = State { slab: Slab::new(), trees: Vec::new(), free_ids: Vec::new(), doomed: Vec::new() };

/// Une tranche de la libération des arbres détruits par [`BitsetId::destroy_lazy`] : au
/// plus `budget` nœuds rendus. Vrai s'il en reste.
pub fn free_step(mut budget: u32) -> bool {
    let s = st();
    while budget > 0
        && let Some(d) = s.doomed.last_mut()
    {
        while budget > 0
            && let Some(&mut (n, level, ref mut next)) = d.stack.last_mut()
        {
            let nn = unsafe { &*s.slab.node(n) };
            if level > 1 && *next <= nn.len() {
                let c = nn.child[*next];
                *next += 1;
                d.stack.push((c, level - 1, 0));
                continue;
            }
            d.stack.pop();
            s.slab.free(n);
            budget -= 1;
        }
        if d.stack.is_empty() {
            let id = d.id;
            s.doomed.pop();
            s.trees[id as usize] = Tree { root: NodeIdx::NONE, height: 0, count: 0 };
            s.free_ids.push(id);
        }
    }
    !s.doomed.is_empty()
}

/// Des arbres attendent d'être libérés.
pub fn free_pending() -> bool {
    !st().doomed.is_empty()
}

#[inline]
#[allow(clippy::deref_addrof)] // `&raw mut` : pas de référence directe à un `static mut`
fn st() -> &'static mut State {
    unsafe { &mut *&raw mut STATE }
}

/// Un bitset : seul ce numéro vit dans la valeur oxidart. Il ne change jamais (RENAME /
/// MOVE déplacent la valeur, pas l'arbre). Pas de `Drop` : libérer par [`BitsetId::destroy`].
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BitsetId(u32);

/// Occupation du slab des bitsets (tous bitsets confondus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub live_nodes: usize,
    pub live_blocks: usize,
    pub bitsets: usize,
}

impl Stats {
    /// Octets tenus par les blocs (hors tables).
    pub fn block_bytes(&self) -> usize {
        self.live_blocks * size_of::<Block>()
    }
}

pub fn stats() -> Stats {
    let s = st();
    Stats {
        live_nodes: s.slab.live_nodes,
        live_blocks: s.slab.live_blocks(),
        bitsets: s.trees.len() - s.free_ids.len(),
    }
}

impl State {
    #[inline]
    fn tree(&self, id: BitsetId) -> Tree {
        let t = self.trees[id.0 as usize];
        debug_assert!(t.height != 0, "bitset {id:?} libéré");
        t
    }

    /// Coupe l'enfant plein `p.child[i]` en deux et remonte le séparateur dans `p`.
    fn split_child(&mut self, p: NodeIdx, i: usize, leaf: bool) {
        let slab = &mut self.slab;
        let pn = unsafe { &mut *slab.node(p) };
        let c = pn.child[i];
        let r = slab.alloc(pn.tree_id());
        let (cn, rn) = unsafe { (&mut *slab.node(c), &mut *slab.node(r)) };
        debug_assert_eq!(cn.len(), CAP);
        let sep = if leaf {
            // 4 | 3 ; le séparateur est la première clé de droite (recopiée)
            rn.keys[..CAP - 4].copy_from_slice(&cn.keys[4..]);
            rn.set_len(CAP - 4);
            cn.set_len(4);
            rn.keys[0]
        } else {
            // 3 | médiane | 3 ; la médiane monte
            rn.keys[..3].copy_from_slice(&cn.keys[4..]);
            rn.child[..4].copy_from_slice(&cn.child[4..]);
            rn.set_len(3);
            cn.set_len(3);
            cn.keys[3]
        };
        pn.insert_sep(i, sep, r);
    }

    /// Garantit `p.child[i].len > MIN` avant d'y descendre pour supprimer : emprunt à
    /// un voisin, sinon fusion. Rend l'indice où se trouve désormais l'enfant.
    fn fix_child(&mut self, p: NodeIdx, i: usize, leaf: bool) -> usize {
        let slab = &mut self.slab;
        let pn = unsafe { &mut *slab.node(p) };
        let c = pn.child[i];
        let cn = unsafe { &mut *slab.node(c) };
        if i > 0 {
            let ln = unsafe { &mut *slab.node(pn.child[i - 1]) };
            let ll = ln.len();
            if ll > MIN {
                if leaf {
                    cn.leaf_insert(0, ln.keys[ll - 1]);
                    pn.keys[i - 1] = cn.keys[0];
                } else {
                    let cl = cn.len();
                    cn.keys.copy_within(0..cl, 1);
                    cn.child.copy_within(0..cl + 1, 1);
                    cn.keys[0] = pn.keys[i - 1];
                    cn.child[0] = ln.child[ll];
                    cn.set_len(cl + 1);
                    pn.keys[i - 1] = ln.keys[ll - 1];
                }
                ln.set_len(ll - 1);
                return i;
            }
        }
        if i < pn.len() {
            let rn = unsafe { &mut *slab.node(pn.child[i + 1]) };
            if rn.len() > MIN {
                let cl = cn.len();
                if leaf {
                    cn.keys[cl] = rn.keys[0];
                    cn.set_len(cl + 1);
                    rn.leaf_remove(0);
                    pn.keys[i] = rn.keys[0];
                } else {
                    let rl = rn.len();
                    cn.keys[cl] = pn.keys[i];
                    cn.child[cl + 1] = rn.child[0];
                    cn.set_len(cl + 1);
                    pn.keys[i] = rn.keys[0];
                    rn.keys.copy_within(1..rl, 0);
                    rn.child.copy_within(1..rl + 1, 0);
                    rn.set_len(rl - 1);
                }
                return i;
            }
            self.merge(p, i, leaf);
            i
        } else {
            self.merge(p, i - 1, leaf);
            i - 1
        }
    }

    /// Fusionne `p.child[j + 1]` dans `p.child[j]` (deux nœuds à `MIN` clés).
    fn merge(&mut self, p: NodeIdx, j: usize, leaf: bool) {
        let slab = &mut self.slab;
        let pn = unsafe { &mut *slab.node(p) };
        let r = pn.child[j + 1];
        let (ln, rn) = unsafe { (&mut *slab.node(pn.child[j]), &*slab.node(r)) };
        let (ll, rl) = (ln.len(), rn.len());
        if leaf {
            ln.keys[ll..ll + rl].copy_from_slice(&rn.keys[..rl]);
            ln.set_len(ll + rl);
        } else {
            ln.keys[ll] = pn.keys[j];
            ln.keys[ll + 1..ll + 1 + rl].copy_from_slice(&rn.keys[..rl]);
            ln.child[ll + 1..ll + 2 + rl].copy_from_slice(&rn.child[..rl + 1]);
            ln.set_len(ll + 1 + rl);
        }
        pn.remove_sep(j);
        slab.free(r);
    }

    fn free_subtree(&mut self, n: NodeIdx, level: u32) {
        if level > 1 {
            let nn = unsafe { *self.slab.node(n) };
            for &c in &nn.child[..=nn.len()] {
                self.free_subtree(c, level - 1);
            }
        }
        self.slab.free(n);
    }
}

// ─── Construction en bloc et parcours ─────────────────────────────────────────

/// Clés par feuille construite en bloc : une case reste libre, le premier `set` dans une
/// feuille ne la coupe pas.
const BUILD_LEAF: usize = CAP - 1;
/// Enfants par nœud interne construit en bloc (6 clés sur 7, même raison).
const BUILD_INNER: usize = CAP;
/// Profondeur max : nœuds non racine à ≥ 4 enfants, feuilles à ≥ 3 clés, 2³² clés → < 17.
const MAX_HEIGHT: usize = 24;

/// Bitset classique : bit `i` = bit `i % 64` du mot `i / 64`. Au plus 2²⁶ mots (2³² bits).
pub const MAX_WORDS: usize = 1 << 26;

/// Nombre de bits à 1 d'un bitset classique. Boucle simple que LLVM vectorise
/// (`vpopcntq` en AVX-512, `vpshufb` en AVX2).
#[inline]
pub fn popcount_words(words: &[u64]) -> u64 {
    words.iter().map(|w| w.count_ones() as u64).sum()
}

/// Bits à 1 d'un bitset classique, dans l'ordre : saute 8 mots nuls d'un coup (une ligne
/// de cache), puis mot par mot ; dans un mot, `tzcnt` + `blsr`.
pub struct WordBits<'a> {
    words: &'a [u64],
    /// Offset du bit 0 de `cur` (enroulé : part de `-64`).
    base: u32,
    cur: u64,
}

impl<'a> WordBits<'a> {
    #[inline]
    pub fn new(words: &'a [u64]) -> Self {
        assert!(words.len() <= MAX_WORDS, "bitset : plus de 2³² bits");
        Self { words, base: 0u32.wrapping_sub(64), cur: 0 }
    }
}

impl Iterator for WordBits<'_> {
    type Item = u32;
    #[inline]
    fn next(&mut self) -> Option<u32> {
        while self.cur == 0 {
            if let Some((chunk, rest)) = self.words.split_first_chunk::<8>()
                && chunk.iter().fold(0, |a, &w| a | w) == 0
            {
                self.words = rest;
                self.base = self.base.wrapping_add(8 * 64);
                continue;
            }
            let (&w, rest) = self.words.split_first()?;
            self.words = rest;
            self.base = self.base.wrapping_add(64);
            self.cur = w;
        }
        let b = self.cur.trailing_zeros();
        self.cur &= self.cur - 1;
        Some(self.base.wrapping_add(b))
    }
}

impl State {
    fn new_id(&mut self) -> u32 {
        match self.free_ids.pop() {
            Some(id) => id,
            None => {
                let id = self.trees.len();
                assert!(id < MAX_TREES, "bitset : table des arbres pleine");
                self.trees.push(Tree { root: NodeIdx::NONE, height: 0, count: 0 });
                id as u32
            }
        }
    }

    /// Construit de bas en haut l'arbre des `n` clés strictement croissantes de `keys`.
    /// Ni descente ni coupe : les feuilles sont remplies à la suite (réparties à égalité,
    /// ≤ 6 clés), puis chaque niveau prend la première clé de chaque enfant comme
    /// séparateur. La liste `(première clé, nœud)` d'un niveau est réécrite sur place par
    /// le niveau du dessus. Rend `(racine, hauteur)`.
    fn build(&mut self, id: u32, n: u64, keys: &mut impl Iterator<Item = u32>) -> (NodeIdx, u32) {
        let slab = &mut self.slab;
        let mut next = || keys.next().expect("bitset : source plus courte que annoncé");
        if n <= CAP as u64 {
            let r = slab.alloc(id);
            let rn = unsafe { &mut *slab.node(r) };
            for k in &mut rn.keys[..n as usize] {
                *k = next();
            }
            rn.set_len(n as usize);
            return (r, 1);
        }
        // k ≥ 2 feuilles de n / k clés : n / k > 6 (k − 1) / k ≥ 3 = MIN
        let k = n.div_ceil(BUILD_LEAF as u64);
        let (base, extra) = ((n / k) as usize, n % k);
        let mut level: Vec<(u32, NodeIdx)> = Vec::with_capacity(k as usize);
        #[cfg(debug_assertions)]
        let mut prev: Option<u32> = None;
        for i in 0..k {
            let len = base + (i < extra) as usize;
            let l = slab.alloc(id);
            let ln = unsafe { &mut *slab.node(l) };
            for key in &mut ln.keys[..len] {
                *key = next();
                #[cfg(debug_assertions)]
                {
                    assert!(prev.is_none_or(|p| p < *key), "bitset : source non strictement croissante");
                    prev = Some(*key);
                }
            }
            ln.set_len(len);
            level.push((ln.keys[0], l));
        }
        let mut height = 1;
        while level.len() > 1 {
            // m enfants en k nœuds : seul (racine, ≥ 2 enfants) ou m / k > 3,5 → ≥ 4 enfants
            let m = level.len();
            let k = m.div_ceil(BUILD_INNER);
            let (base, extra) = (m / k, m % k);
            let mut from = 0;
            for i in 0..k {
                let c = base + (i < extra) as usize;
                let p = slab.alloc(id);
                let pn = unsafe { &mut *slab.node(p) };
                pn.child[0] = level[from].1;
                for j in 1..c {
                    pn.keys[j - 1] = level[from + j].0;
                    pn.child[j] = level[from + j].1;
                }
                pn.set_len(c - 1);
                // i ≤ from : on n'écrase que des entrées déjà lues
                level[i] = (level[from].0, p);
                from += c;
            }
            level.truncate(k);
            height += 1;
        }
        (level[0].1, height)
    }
}

impl BitsetId {
    /// Nouveau bitset vide (une feuille racine vide).
    pub fn create() -> Self {
        let s = st();
        let id = s.new_id();
        let root = s.slab.alloc(id);
        s.trees[id as usize] = Tree { root, height: 1, count: 0 };
        Self(id)
    }

    fn create_from(n: u64, mut keys: impl Iterator<Item = u32>) -> Self {
        let s = st();
        let id = s.new_id();
        let (root, height) = s.build(id, n, &mut keys);
        s.trees[id as usize] = Tree { root, height, count: n };
        Self(id)
    }

    /// Bitset compact depuis `n` offsets **strictement croissants** (vérifié en debug).
    pub fn from_sorted_iter(n: u64, keys: impl Iterator<Item = u32>) -> Self {
        Self::create_from(n, keys)
    }

    /// Bitset compact depuis des offsets **strictement croissants** (vérifié en debug).
    pub fn from_sorted(keys: &[u32]) -> Self {
        Self::create_from(keys.len() as u64, keys.iter().copied())
    }

    /// Bitset compact depuis un bitset classique (conversion classique → compact) :
    /// un `popcount` pour la taille, puis un passage `tzcnt` sur les mots non nuls.
    pub fn from_words(words: &[u64]) -> Self {
        Self::create_from(popcount_words(words), WordBits::new(words))
    }

    /// Les feuilles dans l'ordre : `f` reçoit les offsets de chaque feuille (triés). Pile
    /// explicite, sans redescente ; précharge la feuille sœur suivante.
    pub fn for_each_leaf(self, mut f: impl FnMut(&[u32])) {
        let s = st();
        let t = s.tree(self);
        let root = s.slab.node(t.root);
        if t.height == 1 {
            let r = unsafe { &*root };
            return f(&r.keys[..r.len()]);
        }
        let leaf_parent = t.height as usize - 1;
        let mut stack = [(root as *const Node, 0usize); MAX_HEIGHT];
        let mut depth = 1;
        while depth > 0 {
            let (n, i) = stack[depth - 1];
            let nn = unsafe { &*n };
            if i > nn.len() {
                depth -= 1;
                continue;
            }
            stack[depth - 1].1 = i + 1;
            let c = s.slab.node(nn.child[i]);
            if depth == leaf_parent {
                if i < nn.len() {
                    crate::prefetch::prefetch_l1(s.slab.node(nn.child[i + 1]) as *const u8);
                }
                let cn = unsafe { &*c };
                f(&cn.keys[..cn.len()]);
            } else {
                stack[depth] = (c, 0);
                depth += 1;
            }
        }
    }

    /// Plus grand offset à 1 (taille du bitset classique équivalent) : descente à droite.
    pub fn last(self) -> Option<u32> {
        let s = st();
        let t = s.tree(self);
        let mut n = unsafe { &*s.slab.node(t.root) };
        for _ in 1..t.height {
            n = unsafe { &*s.slab.node(n.child[n.len()]) };
        }
        n.len().checked_sub(1).map(|i| n.keys[i])
    }

    /// Conversion compact → classique : met à 1 dans `words` chaque bit du bitset (OR,
    /// `words` n'est pas remis à zéro). `words` doit couvrir [`Self::last`].
    pub fn fill_words(self, words: &mut [u64]) {
        self.for_each_leaf(|keys| {
            for &k in keys {
                words[(k >> 6) as usize] |= 1 << (k & 63);
            }
        });
    }

    /// Libère tous les nœuds puis l'`id`. Récursif, d'un bloc : à découper en tranches
    /// pour un gros bitset (écritures bornées).
    pub fn destroy(self) {
        let s = st();
        let t = s.tree(self);
        s.free_subtree(t.root, t.height);
        s.trees[self.0 as usize] = Tree { root: NodeIdx::NONE, height: 0, count: 0 };
        s.free_ids.push(self.0);
    }

    /// Nombre de bits à 1, en O(1).
    #[inline]
    pub fn count(self) -> u64 {
        st().tree(self).count
    }

    pub fn get(self, off: u32) -> bool {
        let s = st();
        let t = s.tree(self);
        let mut n = unsafe { &*s.slab.node(t.root) };
        for _ in 1..t.height {
            n = unsafe { &*s.slab.node(n.child[n.route(off)]) };
        }
        let i = n.lower_bound(off);
        i < n.len() && n.keys[i] == off
    }

    /// Met le bit à 1. Rend l'ancienne valeur.
    pub fn set(self, off: u32) -> bool {
        let s = st();
        let mut t = s.tree(self);
        if unsafe { (*s.slab.node(t.root)).len() } == CAP {
            let nr = s.slab.alloc(self.0);
            unsafe { (*s.slab.node(nr)).child[0] = t.root };
            s.split_child(nr, 0, t.height == 1);
            t.root = nr;
            t.height += 1;
        }
        let mut n = t.root;
        for level in (2..=t.height).rev() {
            let nn = unsafe { &*s.slab.node(n) };
            let mut i = nn.route(off);
            if unsafe { (*s.slab.node(nn.child[i])).len() } == CAP {
                s.split_child(n, i, level == 2);
                let nn = unsafe { &*s.slab.node(n) };
                i += (off >= nn.keys[i]) as usize;
            }
            n = unsafe { (*s.slab.node(n)).child[i] };
        }
        let leaf = unsafe { &mut *s.slab.node(n) };
        let i = leaf.lower_bound(off);
        let old = i < leaf.len() && leaf.keys[i] == off;
        if !old {
            leaf.leaf_insert(i, off);
            t.count += 1;
        }
        s.trees[self.0 as usize] = t;
        old
    }

    /// Met le bit à 0. Rend l'ancienne valeur. Un bitset vide reste en place (une feuille
    /// racine vide) : à l'appelant de supprimer la clé.
    pub fn clear(self, off: u32) -> bool {
        let s = st();
        let mut t = s.tree(self);
        let mut n = t.root;
        let mut level = t.height;
        while level > 1 {
            let mut i = unsafe { (*s.slab.node(n)).route(off) };
            let c = unsafe { (*s.slab.node(n)).child[i] };
            if unsafe { (*s.slab.node(c)).len() } <= MIN {
                i = s.fix_child(n, i, level == 2);
            }
            let c = unsafe { (*s.slab.node(n)).child[i] };
            if n == t.root && unsafe { (*s.slab.node(n)).len() } == 0 {
                // fusion des deux derniers enfants de la racine : la racine disparaît
                s.slab.free(n);
                t.root = c;
                t.height -= 1;
            }
            n = c;
            level -= 1;
        }
        let leaf = unsafe { &mut *s.slab.node(n) };
        let i = leaf.lower_bound(off);
        let old = i < leaf.len() && leaf.keys[i] == off;
        if old {
            leaf.leaf_remove(i);
            t.count -= 1;
        }
        s.trees[self.0 as usize] = t;
        old
    }

    /// Premier bit à 1 d'offset `>= from` (BITPOS, parcours dans l'ordre). Si la feuille
    /// atteinte n'en a pas, on redescend avec le plus petit séparateur `> from` croisé.
    pub fn next_set(self, mut from: u32) -> Option<u32> {
        let s = st();
        let t = s.tree(self);
        loop {
            let mut n = unsafe { &*s.slab.node(t.root) };
            let mut upper = None;
            for _ in 1..t.height {
                let i = n.route(from);
                if i < n.len() {
                    upper = Some(n.keys[i]);
                }
                n = unsafe { &*s.slab.node(n.child[i]) };
            }
            let i = n.lower_bound(from);
            if i < n.len() {
                return Some(n.keys[i]);
            }
            from = upper?;
        }
    }

    /// Offsets `>= from` dans l'ordre, jusqu'à ce que `f` rende `false` (rend alors
    /// `false`). Une descente, puis les feuilles suivantes par pile explicite.
    pub fn for_each_from(self, from: u32, mut f: impl FnMut(u32) -> bool) -> bool {
        let s = st();
        let t = s.tree(self);
        let leaf_level = t.height as usize - 1;
        let mut stack = [(core::ptr::null::<Node>(), 0usize); MAX_HEIGHT];
        let mut n = s.slab.node(t.root) as *const Node;
        for slot in &mut stack[..leaf_level] {
            let nn = unsafe { &*n };
            let i = nn.route(from);
            *slot = (n, i + 1);
            n = s.slab.node(nn.child[i]);
        }
        let mut depth = leaf_level;
        let mut leaf = unsafe { &*n };
        let mut i = leaf.lower_bound(from);
        loop {
            for &k in &leaf.keys[i..leaf.len()] {
                if !f(k) {
                    return false;
                }
            }
            // feuille suivante : remonter au premier ancêtre qui a encore un enfant
            loop {
                if depth == 0 {
                    return true;
                }
                let (p, j) = stack[depth - 1];
                let pn = unsafe { &*p };
                if j > pn.len() {
                    depth -= 1;
                    continue;
                }
                stack[depth - 1].1 = j + 1;
                let mut c = s.slab.node(pn.child[j]) as *const Node;
                while depth < leaf_level {
                    stack[depth] = (c, 1);
                    c = s.slab.node(unsafe { (*c).child[0] });
                    depth += 1;
                }
                leaf = unsafe { &*c };
                i = 0;
                break;
            }
        }
    }

    /// Comme [`Self::destroy`], mais les nœuds sont rendus plus tard par [`free_step`],
    /// par tranches. L'`id` reste réservé jusqu'à la fin (les nœuds le portent encore).
    pub fn destroy_lazy(self) {
        let s = st();
        let t = s.tree(self);
        s.doomed.push(Doomed { id: self.0, stack: alloc::vec![(t.root, t.height, 0)] });
    }

    /// Vérifie tous les invariants de l'arbre (tests).
    #[cfg(test)]
    fn check(self) {
        #[allow(clippy::too_many_arguments)]
        fn walk(s: &State, id: u32, n: NodeIdx, level: u32, lo: Option<u32>, hi: Option<u32>, root: bool, out: &mut Vec<u32>) {
            let nn = unsafe { &*s.slab.node(n) };
            assert_eq!(nn.tree_id(), id);
            let len = nn.len();
            if !root {
                assert!(len >= MIN, "nœud sous le minimum : {len}");
            }
            let keys = &nn.keys[..len];
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "clés non triées {keys:?}");
            for &k in keys {
                assert!(lo.is_none_or(|lo| k >= lo) && hi.is_none_or(|hi| k < hi), "clé {k} hors de [{lo:?}, {hi:?})");
            }
            if level == 1 {
                out.extend_from_slice(keys);
            } else {
                assert!(root && len >= 1 || len >= MIN);
                for i in 0..=len {
                    let l = if i == 0 { lo } else { Some(keys[i - 1]) };
                    let h = if i == len { hi } else { Some(keys[i]) };
                    walk(s, id, nn.child[i], level - 1, l, h, false, out);
                }
            }
        }
        let s = st();
        let t = s.tree(self);
        let mut out = Vec::new();
        walk(s, self.0, t.root, t.height, None, None, true, &mut out);
        assert_eq!(out.len() as u64, t.count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn slab_consistent() {
        let s = st();
        let live: usize = s.slab.masks.iter().map(|m| m.count_ones() as usize).sum();
        assert_eq!(live, s.slab.live_nodes);
        for c in 1..SLOTS {
            let mut b = s.slab.heads[c];
            assert_eq!(b != NONE, s.slab.nonempty & (1 << c) != 0);
            while b != NONE {
                assert_eq!(s.slab.masks[b as usize].count_ones() as usize, c);
                b = s.slab.next[b as usize];
            }
        }
    }

    /// Opérations au hasard contre un `BTreeSet`, sur un domaine de `range` offsets.
    fn differential(seed: u64, range: u64, ops: usize, set_bias: u64) {
        let before = stats();
        let mut rng = Rng(seed);
        let bs = BitsetId::create();
        let mut oracle = BTreeSet::new();
        for step in 0..ops {
            let off = (rng.next() % range) as u32;
            match rng.next() % 100 {
                r if r < set_bias => assert_eq!(bs.set(off), !oracle.insert(off), "set {off}"),
                r if r < 95 => assert_eq!(bs.clear(off), oracle.remove(&off), "clear {off}"),
                _ => {
                    assert_eq!(bs.get(off), oracle.contains(&off));
                    assert_eq!(bs.next_set(off), oracle.range(off..).next().copied());
                }
            }
            if step % 997 == 0 {
                bs.check();
            }
        }
        bs.check();
        assert_eq!(bs.count(), oracle.len() as u64);
        let mut got = Vec::new();
        let mut cur = bs.next_set(0);
        while let Some(k) = cur {
            got.push(k);
            cur = k.checked_add(1).and_then(|n| bs.next_set(n));
        }
        assert!(got.iter().eq(oracle.iter()));
        // tout vider par clear : la hauteur doit retomber à 1
        for k in got {
            assert!(bs.clear(k));
        }
        bs.check();
        assert_eq!(st().tree(bs).height, 1);
        assert_eq!(bs.count(), 0);
        bs.destroy();
        slab_consistent();
        assert_eq!(stats(), before);
    }

    #[test]
    fn for_each_from_and_lazy_destroy() {
        let before = stats();
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for &(n, range) in &[(0usize, 10u64), (5, 100), (300, 2000), (20_000, 1 << 32)] {
            let mut keys: Vec<u32> = (0..n).map(|_| (rng.next() % range) as u32).collect();
            keys.sort_unstable();
            keys.dedup();
            let bs = BitsetId::from_sorted(&keys);
            for _ in 0..200 {
                let from = (rng.next() % (range + 10)).min(u32::MAX as u64) as u32;
                let want: Vec<u32> = keys.iter().copied().filter(|&k| k >= from).take(37).collect();
                let mut got = Vec::new();
                let full = bs.for_each_from(from, |k| {
                    got.push(k);
                    got.len() < 37
                });
                assert_eq!(got, want, "from {from}");
                assert_eq!(full, got.len() < 37);
            }
            bs.destroy_lazy();
            while free_step(7) {}
        }
        slab_consistent();
        assert_eq!(stats(), before);
    }

    #[test]
    fn small_domain_churn() {
        // beaucoup de collisions : emprunts et fusions dans tous les sens
        differential(0x9E37_79B9_7F4A_7C15, 300, 60_000, 55);
    }

    #[test]
    fn wide_domain_growth() {
        differential(0xDEAD_BEEF_1234_5678, u32::MAX as u64 + 1, 40_000, 80);
    }

    #[test]
    fn sequential_and_extremes() {
        let bs = BitsetId::create();
        for k in 0..5000u32 {
            assert!(!bs.set(k));
        }
        assert!(!bs.set(u32::MAX));
        assert!(bs.get(u32::MAX) && !bs.get(u32::MAX - 1));
        assert_eq!(bs.next_set(5000), Some(u32::MAX));
        bs.check();
        for k in (0..5000u32).rev().step_by(2) {
            assert!(bs.clear(k));
        }
        bs.check();
        assert_eq!(bs.count(), 2501);
        bs.destroy();
        slab_consistent();
    }

    #[test]
    fn many_bitsets_share_the_slab() {
        let before = stats();
        let ids: Vec<_> = (0..200).map(|_| BitsetId::create()).collect();
        for (n, &id) in ids.iter().enumerate() {
            for k in 0..(n as u32 % 40) {
                id.set(k * 3 + n as u32);
            }
        }
        for (n, &id) in ids.iter().enumerate() {
            id.check();
            assert_eq!(id.count(), n as u64 % 40);
        }
        // 200 petits bitsets : ~1 nœud chacun, tassés dans des blocs pleins
        let mid = stats();
        let (nodes, blocks) = (mid.live_nodes - before.live_nodes, mid.live_blocks - before.live_blocks);
        assert!(blocks <= nodes.div_ceil(SLOTS) + 1, "{nodes} nœuds dans {blocks} blocs");
        for id in ids {
            id.destroy();
        }
        slab_consistent();
        assert_eq!(stats(), before);
    }

    fn collect(bs: BitsetId) -> Vec<u32> {
        let mut v = Vec::new();
        bs.for_each_leaf(|k| v.extend_from_slice(k));
        v
    }

    #[test]
    fn build_from_sorted_all_sizes() {
        let before = stats();
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for n in (0..200).chain([500, 1000, 4095, 4096, 4097, 50_000]) {
            let mut set = BTreeSet::new();
            if n > 0 {
                set.insert(u32::MAX); // offset extrême toujours présent
            }
            while set.len() < n {
                set.insert(rng.next() as u32);
            }
            let keys: Vec<u32> = set.iter().copied().collect();
            let bs = BitsetId::from_sorted(&keys);
            bs.check();
            assert_eq!(bs.count(), n as u64);
            assert_eq!(collect(bs), keys);
            assert_eq!(bs.last(), keys.last().copied());
            // l'arbre construit en bloc se comporte comme un arbre normal ensuite
            for _ in 0..n.min(2000) {
                let off = match keys.get((rng.next() % (n as u64 + 1)) as usize) {
                    Some(&k) => k,
                    None => rng.next() as u32,
                };
                if rng.next() & 1 == 0 {
                    assert_eq!(bs.set(off), !set.insert(off));
                } else {
                    assert_eq!(bs.clear(off), set.remove(&off));
                }
            }
            bs.check();
            assert!(collect(bs).iter().eq(set.iter()));
            bs.destroy();
        }
        slab_consistent();
        assert_eq!(stats(), before);
    }

    #[test]
    fn words_round_trip() {
        let before = stats();
        let mut rng = Rng(0xC0FF_EE00_1234_5678);
        // (mots, chance sur 64 qu'un mot soit non nul) : longues plages de zéros comprises
        for (len, density) in [(0usize, 0u64), (1, 64), (3, 2), (100, 0), (1000, 1), (1000, 8), (4096, 32), (5000, 64)] {
            let words: Vec<u64> =
                (0..len).map(|_| if rng.next() % 64 < density { rng.next() & rng.next() } else { 0 }).collect();
            let naive: Vec<u32> =
                (0..len * 64).filter(|&i| words[i / 64] >> (i % 64) & 1 == 1).map(|i| i as u32).collect();
            assert!(WordBits::new(&words).eq(naive.iter().copied()));
            assert_eq!(popcount_words(&words), naive.len() as u64);
            let bs = BitsetId::from_words(&words);
            bs.check();
            assert_eq!(collect(bs), naive);
            let mut back = alloc::vec![0u64; len];
            bs.fill_words(&mut back);
            assert_eq!(back, words);
            bs.destroy();
        }
        slab_consistent();
        assert_eq!(stats(), before);
    }

    /// `cargo test --release -p radixox-lib bitset_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "mesure, en release"]
    fn bitset_bench() {
        use std::time::Instant;
        let n = 4_000_000usize;
        // un bit sur 16 en moyenne (écarts 1..=31)
        let mut rng = Rng(42);
        let mut k = 0u32;
        let keys: Vec<u32> = (0..n)
            .map(|_| {
                k += 1 + (rng.next() % 31) as u32;
                k
            })
            .collect();
        let ns = |d: std::time::Duration| d.as_nanos() as f64 / n as f64;

        // à froid : blocs neufs (défauts de page) ; à chaud : mémoire déjà touchée, réutilisée
        let t = Instant::now();
        let bs = BitsetId::from_sorted(&keys);
        let build_cold = t.elapsed();
        bs.destroy();
        let t = Instant::now();
        let bs = BitsetId::from_sorted(&keys);
        let build = t.elapsed();
        let t = Instant::now();
        let inc = BitsetId::create();
        for &x in &keys {
            inc.set(x);
        }
        let incr = t.elapsed();
        let mut shuffled = keys.clone();
        for i in (1..n).rev() {
            shuffled.swap(i, (rng.next() % (i as u64 + 1)) as usize);
        }
        let t = Instant::now();
        let rnd = BitsetId::create();
        for &x in &shuffled {
            rnd.set(x);
        }
        let rand_ins = t.elapsed();
        let mut seen = 0u64;
        let t = Instant::now();
        bs.for_each_leaf(|ks| seen += ks.len() as u64);
        let walk = t.elapsed();
        let mut words = alloc::vec![0u64; (k as usize >> 6) + 1];
        let t = Instant::now();
        bs.fill_words(&mut words);
        let fill = t.elapsed();
        let t = Instant::now();
        let pc = popcount_words(core::hint::black_box(&words));
        let popc = t.elapsed();
        let t = Instant::now();
        let it = WordBits::new(core::hint::black_box(&words)).count();
        let iterw = t.elapsed();
        let t = Instant::now();
        let bw = BitsetId::from_words(&words);
        let from_w = t.elapsed();

        assert_eq!((seen, pc, it as u64, bw.count(), inc.count(), rnd.count()), (n as u64, n as u64, n as u64, n as u64, n as u64, n as u64));
        let nodes = stats().live_nodes;
        std::println!("{n} offsets, {} mots ({} Kio), {nodes} nœuds au total", words.len(), words.len() * 8 / 1024);
        std::println!("from_sorted à froid  {:6.2} ns/offset", ns(build_cold));
        std::println!("from_sorted à chaud  {:6.2} ns/offset", ns(build));
        std::println!("set croissant        {:6.2} ns/offset", ns(incr));
        std::println!("set aléatoire        {:6.2} ns/offset", ns(rand_ins));
        std::println!("for_each_leaf        {:6.2} ns/offset", ns(walk));
        std::println!("fill_words           {:6.2} ns/offset", ns(fill));
        std::println!("popcount_words       {:6.2} ns/offset  ({:.2} ns/mot)", ns(popc), popc.as_nanos() as f64 / words.len() as f64);
        std::println!("WordBits             {:6.2} ns/offset", ns(iterw));
        std::println!("from_words           {:6.2} ns/offset", ns(from_w));
        for b in [bs, inc, rnd, bw] {
            b.destroy();
        }
    }
}
