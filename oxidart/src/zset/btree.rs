//! B+tree compté du ZSet (ANALYSE_ART_UNIFIE.md §13.4) : l'ordre `(score, membre)`, les
//! rangs et les plages.
//!
//! Entrée = `(score u64 ordonné, poignée u32)`. L'arbre ne connaît pas les membres : à score
//! égal, un [`Tie`] compare deux poignées (octets des membres via la table, ou la poignée
//! elle-même dans les tests). Nœuds de 512 o dans une `HiSlab` à eux (idx `u32`) :
//! - feuille : `scores [u64; L]`, `poignées [u32; L]` en colonnes, chaînage `prev` / `next` ;
//! - interne : séparateurs `(score, poignée)` (clé complète), `enfants [u32]`, `comptes
//!   [u32]` (taille de chaque sous-arbre).
//!
//! Convention : le séparateur `i` est **la plus petite clé de l'enfant `i + 1`** (donc une
//! clé vivante, et sa poignée aussi : un séparateur périmé garderait une poignée libérée,
//! puis réutilisée, et fausserait l'ordre des ex-aequo). Supprimer cette clé remplace le
//! séparateur par la suivante ; la mise à jour sur place ne touche jamais la première
//! entrée d'une feuille.
//!
//! Comptes mis à jour en descendant, pas de pointeur parent : un split remonte par la pile
//! de la descente. Split 90/10 sur le bord droit (insertions croissantes), 50/50 sinon. À
//! la suppression, pas d'emprunt : une feuille vide est rendue, deux feuilles voisines sous
//! ¼ fusionnent, une racine à un seul enfant disparaît.

use std::cmp::Ordering;

use hislab::HiSlab;

/// Entrées par feuille.
pub const L: usize = 41;
/// Enfants par nœud interne.
pub const I: usize = 25;
const NONE: u32 = u32::MAX;
/// Hauteur maximale : 25^12 dépasse largement 2^32 entrées.
const MAX_H: usize = 12;
/// Sous ce remplissage, deux feuilles voisines fusionnent.
const MERGE_BELOW: usize = L / 4;

#[derive(Clone)]
pub struct Leaf {
    n: u16,
    prev: u32,
    next: u32,
    scores: [u64; L],
    handles: [u32; L],
}

#[derive(Clone)]
pub struct Internal {
    /// Nombre d'enfants (≥ 1).
    n: u16,
    sep_s: [u64; I - 1],
    sep_h: [u32; I - 1],
    child: [u32; I],
    count: [u32; I],
}

#[derive(Clone)]
#[repr(align(64))]
pub enum BNode {
    Leaf(Leaf),
    Internal(Internal),
}

const _: () = assert!(size_of::<BNode>() == 512);

static mut SLAB: Option<HiSlab<BNode>> = None;

#[inline]
fn slab() -> &'static mut HiSlab<BNode> {
    #[allow(static_mut_refs)]
    unsafe {
        match SLAB.as_mut() {
            Some(s) => s,
            None => SLAB.insert(HiSlab::new(0, 1 << 24).expect("slab du B+tree des ZSet")),
        }
    }
}

/// Mono-thread ; l'appelant ne garde jamais deux références au même nœud.
#[inline]
fn leaf(i: u32) -> &'static mut Leaf {
    match unsafe { slab().get_unchecked_mut(i) } {
        BNode::Leaf(l) => l,
        BNode::Internal(_) => unreachable!("feuille attendue"),
    }
}

#[inline]
fn internal(i: u32) -> &'static mut Internal {
    match unsafe { slab().get_unchecked_mut(i) } {
        BNode::Internal(n) => n,
        BNode::Leaf(_) => unreachable!("nœud interne attendu"),
    }
}

fn new_leaf() -> u32 {
    slab().insert(BNode::Leaf(Leaf { n: 0, prev: NONE, next: NONE, scores: [0; L], handles: [0; L] }))
}

fn new_internal() -> u32 {
    slab().insert(BNode::Internal(Internal { n: 0, sep_s: [0; I - 1], sep_h: [0; I - 1], child: [NONE; I], count: [0; I] }))
}

fn free_node(i: u32) {
    slab().remove(i);
}

/// Préchargement d'un nœud (chaînes AMAC, plages).
#[inline]
pub fn prefetch(i: u32) {
    let p = slab().slot_ptr(i) as *const u8;
    for off in [0, 64, 128, 192, 256, 320, 384, 448] {
        radixox_lib::prefetch::prefetch_l1(unsafe { p.add(off) });
    }
}

/// Octets tenus par la slab des nœuds.
pub fn slab_nodes() -> u32 {
    slab().len()
}

/// Ordre des membres à score égal : `a` comparé à `b` (jamais appelé avec `a == b`).
pub trait Tie {
    fn tie(&self, a: u32, b: u32) -> Ordering;
}

impl<F: Fn(u32, u32) -> Ordering> Tie for F {
    fn tie(&self, a: u32, b: u32) -> Ordering {
        self(a, b)
    }
}

#[inline]
fn kcmp(t: &impl Tie, a: (u64, u32), b: (u64, u32)) -> Ordering {
    a.0.cmp(&b.0).then_with(|| if a.1 == b.1 { Ordering::Equal } else { t.tie(a.1, b.1) })
}

/// Recherche dichotomique : premier `i` de `0..n` où `pred(i)` est faux (`pred` vrai puis
/// faux).
#[inline]
fn partition(n: usize, mut pred: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pred(mid) { lo = mid + 1 } else { hi = mid }
    }
    lo
}

/// Position dans une feuille : `leaf`, rang `pos` dans la feuille.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub leaf: u32,
    pub pos: u16,
}

/// Parcours des entrées à partir d'un curseur, dans un sens.
pub type Entries = std::iter::Map<std::iter::Successors<Cursor, fn(&Cursor) -> Option<Cursor>>, fn(Cursor) -> (u64, u32)>;

/// En-tête de l'arbre (dans l'en-tête du ZSet). `root == NONE` : vide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BTree {
    root: u32,
    first: u32,
    last: u32,
    /// 0 : la racine est une feuille.
    height: u8,
    len: u32,
}

impl Default for BTree {
    fn default() -> Self {
        Self::new()
    }
}

impl BTree {
    pub const fn new() -> Self {
        Self { root: NONE, first: NONE, last: NONE, height: 0, len: 0 }
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn root(&self) -> u32 {
        self.root
    }

    pub fn height(&self) -> u8 {
        self.height
    }

    /// Ajoute `(s, h)`, absente (le ZSet le sait par son index).
    pub fn insert(&mut self, s: u64, h: u32, tie: &impl Tie) {
        let key = (s, h);
        self.len += 1;
        if self.root == NONE {
            let l = new_leaf();
            let lf = leaf(l);
            lf.n = 1;
            lf.scores[0] = s;
            lf.handles[0] = h;
            (self.root, self.first, self.last, self.height) = (l, l, l, 0);
            return;
        }
        let mut path = [(NONE, 0usize); MAX_H];
        let mut idx = self.root;
        let mut right_edge = true;
        for d in 0..self.height as usize {
            let node = internal(idx);
            let n = node.n as usize;
            let ci = partition(n - 1, |i| kcmp(tie, (node.sep_s[i], node.sep_h[i]), key) != Ordering::Greater);
            node.count[ci] += 1;
            right_edge &= ci == n - 1;
            path[d] = (idx, ci);
            idx = node.child[ci];
        }
        let lf = leaf(idx);
        let n = lf.n as usize;
        let pos = partition(n, |i| kcmp(tie, (lf.scores[i], lf.handles[i]), key) == Ordering::Less);
        debug_assert!(pos == n || kcmp(tie, (lf.scores[pos], lf.handles[pos]), key) != Ordering::Equal, "déjà présente");
        if n < L {
            lf.scores.copy_within(pos..n, pos + 1);
            lf.handles.copy_within(pos..n, pos + 1);
            lf.scores[pos] = s;
            lf.handles[pos] = h;
            lf.n += 1;
            return;
        }
        // Feuille pleine : split.
        right_edge &= pos == n;
        let mut ts = [0u64; L + 1];
        let mut th = [0u32; L + 1];
        ts[..pos].copy_from_slice(&lf.scores[..pos]);
        th[..pos].copy_from_slice(&lf.handles[..pos]);
        ts[pos] = s;
        th[pos] = h;
        ts[pos + 1..].copy_from_slice(&lf.scores[pos..]);
        th[pos + 1..].copy_from_slice(&lf.handles[pos..]);
        let k = if right_edge { L * 9 / 10 } else { L.div_ceil(2) };
        let old_next = lf.next;
        lf.n = k as u16;
        lf.scores[..k].copy_from_slice(&ts[..k]);
        lf.handles[..k].copy_from_slice(&th[..k]);
        let r = new_leaf();
        let lr = leaf(r);
        let rn = L + 1 - k;
        lr.n = rn as u16;
        lr.scores[..rn].copy_from_slice(&ts[k..]);
        lr.handles[..rn].copy_from_slice(&th[k..]);
        lr.prev = idx;
        lr.next = old_next;
        leaf(idx).next = r;
        if old_next == NONE { self.last = r } else { leaf(old_next).prev = r }
        let mut carry = ((ts[k], th[k]), r, rn as u32);
        // Remontée par la pile.
        for d in (0..self.height as usize).rev() {
            let (p, ci) = path[d];
            let ((ss, sh), r, cnt) = carry;
            let node = internal(p);
            node.count[ci] -= cnt;
            let n = node.n as usize;
            if n < I {
                node.child.copy_within(ci + 1..n, ci + 2);
                node.count.copy_within(ci + 1..n, ci + 2);
                node.sep_s.copy_within(ci..n - 1, ci + 1);
                node.sep_h.copy_within(ci..n - 1, ci + 1);
                node.child[ci + 1] = r;
                node.count[ci + 1] = cnt;
                node.sep_s[ci] = ss;
                node.sep_h[ci] = sh;
                node.n += 1;
                return;
            }
            // Interne plein : I + 1 enfants à répartir.
            let mut tc = [0u32; I + 1];
            let mut tn = [0u32; I + 1];
            let mut tss = [0u64; I];
            let mut tsh = [0u32; I];
            tc[..=ci].copy_from_slice(&node.child[..=ci]);
            tn[..=ci].copy_from_slice(&node.count[..=ci]);
            tc[ci + 1] = r;
            tn[ci + 1] = cnt;
            tc[ci + 2..].copy_from_slice(&node.child[ci + 1..]);
            tn[ci + 2..].copy_from_slice(&node.count[ci + 1..]);
            tss[..ci].copy_from_slice(&node.sep_s[..ci]);
            tsh[..ci].copy_from_slice(&node.sep_h[..ci]);
            tss[ci] = ss;
            tsh[ci] = sh;
            tss[ci + 1..].copy_from_slice(&node.sep_s[ci..]);
            tsh[ci + 1..].copy_from_slice(&node.sep_h[ci..]);
            let k = if right_edge { I * 9 / 10 } else { (I + 1).div_ceil(2) };
            node.n = k as u16;
            node.child[..k].copy_from_slice(&tc[..k]);
            node.count[..k].copy_from_slice(&tn[..k]);
            node.sep_s[..k - 1].copy_from_slice(&tss[..k - 1]);
            node.sep_h[..k - 1].copy_from_slice(&tsh[..k - 1]);
            let q = new_internal();
            let nq = internal(q);
            let rn = I + 1 - k;
            nq.n = rn as u16;
            nq.child[..rn].copy_from_slice(&tc[k..]);
            nq.count[..rn].copy_from_slice(&tn[k..]);
            nq.sep_s[..rn - 1].copy_from_slice(&tss[k..]);
            nq.sep_h[..rn - 1].copy_from_slice(&tsh[k..]);
            let qcnt = tn[k..].iter().sum();
            carry = ((tss[k - 1], tsh[k - 1]), q, qcnt);
        }
        // La racine s'est coupée.
        let ((ss, sh), r, cnt) = carry;
        let root = new_internal();
        let nr = internal(root);
        nr.n = 2;
        nr.child[0] = self.root;
        nr.child[1] = r;
        nr.count[0] = self.len - cnt;
        nr.count[1] = cnt;
        nr.sep_s[0] = ss;
        nr.sep_h[0] = sh;
        self.root = root;
        self.height += 1;
    }

    /// Descente exacte vers `(s, h)` sans rien modifier : la pile, la position, et le
    /// séparateur égal à la clé s'il y en a un (nœud, rang).
    fn locate(&self, s: u64, h: u32, tie: &impl Tie, path: &mut [(u32, usize); MAX_H], sep: &mut Option<(u32, usize)>) -> Option<Cursor> {
        if self.root == NONE {
            return None;
        }
        let key = (s, h);
        let mut idx = self.root;
        for slot in path.iter_mut().take(self.height as usize) {
            let node = internal(idx);
            let ci = partition(node.n as usize - 1, |i| kcmp(tie, (node.sep_s[i], node.sep_h[i]), key) != Ordering::Greater);
            if ci > 0 && node.sep_s[ci - 1] == s && node.sep_h[ci - 1] == h {
                *sep = Some((idx, ci - 1));
            }
            *slot = (idx, ci);
            idx = node.child[ci];
        }
        let lf = leaf(idx);
        let pos = partition(lf.n as usize, |i| kcmp(tie, (lf.scores[i], lf.handles[i]), key) == Ordering::Less);
        (pos < lf.n as usize && lf.scores[pos] == s && lf.handles[pos] == h).then_some(Cursor { leaf: idx, pos: pos as u16 })
    }

    /// Position de `(s, h)`.
    pub fn find(&self, s: u64, h: u32, tie: &impl Tie) -> Option<Cursor> {
        self.locate(s, h, tie, &mut [(NONE, 0); MAX_H], &mut None)
    }

    /// Retire `(s, h)` ; faux si absente.
    pub fn remove(&mut self, s: u64, h: u32, tie: &impl Tie) -> bool {
        let mut path = [(NONE, 0usize); MAX_H];
        let mut sep = None;
        let Some(c) = self.locate(s, h, tie, &mut path, &mut sep) else { return false };
        let height = self.height as usize;
        for &(p, ci) in &path[..height] {
            internal(p).count[ci] -= 1;
        }
        self.len -= 1;
        let lf = leaf(c.leaf);
        let (pos, n) = (c.pos as usize, lf.n as usize);
        lf.scores.copy_within(pos + 1..n, pos);
        lf.handles.copy_within(pos + 1..n, pos);
        lf.n -= 1;
        let n = n - 1;
        // La clé était un séparateur : la suivante prend sa place (si le sous-arbre se vide,
        // le séparateur part avec lui plus bas).
        if let Some((p, si)) = sep {
            let succ = if pos < n {
                Some((lf.scores[pos], lf.handles[pos]))
            } else if lf.next != NONE {
                let nl = leaf(lf.next);
                Some((nl.scores[0], nl.handles[0]))
            } else {
                None
            };
            if let Some((ss, sh)) = succ {
                let node = internal(p);
                node.sep_s[si] = ss;
                node.sep_h[si] = sh;
            }
        }
        if n == 0 {
            self.unlink_leaf(c.leaf);
            free_node(c.leaf);
            self.remove_child(&path, height);
        } else if n < MERGE_BELOW && height > 0 {
            self.try_merge(path[height - 1]);
        }
        self.collapse_root();
        true
    }

    /// Feuille sous ¼ : fusion avec une voisine du même parent, sous ¼ aussi.
    fn try_merge(&mut self, (p, ci): (u32, usize)) {
        let node = internal(p);
        let n = node.n as usize;
        let (left_ci, right_ci) = if ci + 1 < n && (leaf(node.child[ci + 1]).n as usize) < MERGE_BELOW {
            (ci, ci + 1)
        } else if ci > 0 && (leaf(node.child[ci - 1]).n as usize) < MERGE_BELOW {
            (ci - 1, ci)
        } else {
            return;
        };
        let (a, b) = (node.child[left_ci], node.child[right_ci]);
        let lb = leaf(b).clone();
        let la = leaf(a);
        let (na, nb) = (la.n as usize, lb.n as usize);
        la.scores[na..na + nb].copy_from_slice(&lb.scores[..nb]);
        la.handles[na..na + nb].copy_from_slice(&lb.handles[..nb]);
        la.n += nb as u16;
        self.unlink_leaf(b);
        free_node(b);
        let node = internal(p);
        node.count[left_ci] += node.count[right_ci];
        Self::drop_child(node, right_ci);
    }

    /// Retire l'enfant `ci` (et le séparateur qui le borde) d'un nœud interne.
    fn drop_child(node: &mut Internal, ci: usize) {
        let n = node.n as usize;
        node.child.copy_within(ci + 1..n, ci);
        node.count.copy_within(ci + 1..n, ci);
        if n > 1 {
            let si = ci.saturating_sub(1);
            node.sep_s.copy_within(si + 1..n - 1, si);
            node.sep_h.copy_within(si + 1..n - 1, si);
        }
        node.n -= 1;
    }

    /// L'enfant au niveau `depth` du chemin a disparu : retiré de son parent, et ainsi de
    /// suite tant qu'un parent se vide.
    fn remove_child(&mut self, path: &[(u32, usize); MAX_H], mut depth: usize) {
        while depth > 0 {
            let (p, ci) = path[depth - 1];
            let node = internal(p);
            Self::drop_child(node, ci);
            if node.n > 0 {
                return;
            }
            free_node(p);
            depth -= 1;
        }
        debug_assert_eq!(self.len, 0);
        *self = Self::new();
    }

    /// Racine interne à un seul enfant : hauteur − 1.
    fn collapse_root(&mut self) {
        while self.height > 0 && internal(self.root).n == 1 {
            let old = self.root;
            self.root = internal(old).child[0];
            free_node(old);
            self.height -= 1;
        }
    }

    fn unlink_leaf(&mut self, l: u32) {
        let (prev, next) = {
            let lf = leaf(l);
            (lf.prev, lf.next)
        };
        if prev == NONE { self.first = next } else { leaf(prev).next = next }
        if next == NONE { self.last = prev } else { leaf(next).prev = prev }
    }

    /// Change le score de `(old, h)` en `new` sur place si l'ordre de la feuille le
    /// permet (aucun compte ne bouge), sinon retire puis insère. Faux si absente.
    pub fn update(&mut self, old: u64, h: u32, new: u64, tie: &impl Tie) -> bool {
        let Some(c) = self.find(old, h, tie) else { return false };
        let lf = leaf(c.leaf);
        let (pos, n) = (c.pos as usize, lf.n as usize);
        let key = (new, h);
        // jamais la première entrée d'une feuille : elle peut être un séparateur
        let fits = match new.cmp(&old) {
            Ordering::Equal => true,
            Ordering::Greater => pos > 0 && pos + 1 < n && kcmp(tie, key, (lf.scores[pos + 1], lf.handles[pos + 1])) == Ordering::Less,
            Ordering::Less => pos > 0 && kcmp(tie, key, (lf.scores[pos - 1], lf.handles[pos - 1])) == Ordering::Greater,
        };
        if fits {
            lf.scores[pos] = new;
        } else {
            self.remove(old, h, tie);
            self.insert(new, h, tie);
        }
        true
    }

    /// Nombre d'entrées strictement avant la sonde, et la position de la première entrée
    /// qui ne l'est pas (`None` : au-delà de la fin). `probe(s, h)` = la sonde comparée à
    /// l'entrée `(s, h)`.
    pub fn lower_bound(&self, mut probe: impl FnMut(u64, u32) -> Ordering) -> (u32, Option<Cursor>) {
        if self.root == NONE {
            return (0, None);
        }
        let mut rank = 0;
        let mut idx = self.root;
        for _ in 0..self.height {
            let node = internal(idx);
            let ci = partition(node.n as usize - 1, |i| probe(node.sep_s[i], node.sep_h[i]) != Ordering::Less);
            rank += node.count[..ci].iter().sum::<u32>();
            idx = node.child[ci];
        }
        let lf = leaf(idx);
        let pos = partition(lf.n as usize, |i| probe(lf.scores[i], lf.handles[i]) == Ordering::Greater);
        rank += pos as u32;
        let c = if pos < lf.n as usize {
            Some(Cursor { leaf: idx, pos: pos as u16 })
        } else {
            (lf.next != NONE).then_some(Cursor { leaf: lf.next, pos: 0 })
        };
        (rank, c)
    }

    /// Chaînes AMAC : enfant du nœud interne `i` vers lequel descend la sonde
    /// (`probe(s, h)` = sonde comparée à l'entrée), préchargé.
    #[inline]
    pub fn child_toward(i: u32, mut probe: impl FnMut(u64, u32) -> Ordering) -> u32 {
        let node = internal(i);
        let ci = partition(node.n as usize - 1, |k| probe(node.sep_s[k], node.sep_h[k]) != Ordering::Less);
        let c = node.child[ci];
        prefetch(c);
        c
    }

    /// Feuilles voisines de `l` : `(prev, next)`, `u32::MAX` aux bouts.
    #[inline]
    pub fn leaf_links(l: u32) -> (u32, u32) {
        let lf = leaf(l);
        (lf.prev, lf.next)
    }

    /// Poignées de la feuille `l` (préchargement des cases d'une plage).
    #[inline]
    pub fn leaf_handles(l: u32) -> &'static [u32] {
        let lf = leaf(l);
        &lf.handles[..lf.n as usize]
    }

    /// Rang de `(s, h)` (présente).
    pub fn rank(&self, s: u64, h: u32, tie: &impl Tie) -> u32 {
        self.lower_bound(|es, eh| kcmp(tie, (s, h), (es, eh))).0
    }

    /// Position de l'entrée de rang `r` (`None` : `r ≥ len`).
    pub fn select(&self, mut r: u32) -> Option<Cursor> {
        if r >= self.len {
            return None;
        }
        let mut idx = self.root;
        for _ in 0..self.height {
            let node = internal(idx);
            let mut ci = 0;
            while r >= node.count[ci] {
                r -= node.count[ci];
                ci += 1;
            }
            idx = node.child[ci];
        }
        Some(Cursor { leaf: idx, pos: r as u16 })
    }

    pub fn first(&self) -> Option<Cursor> {
        (self.first != NONE).then_some(Cursor { leaf: self.first, pos: 0 })
    }

    pub fn last(&self) -> Option<Cursor> {
        (self.last != NONE).then(|| Cursor { leaf: self.last, pos: leaf(self.last).n - 1 })
    }

    /// Entrée sous le curseur.
    #[inline]
    pub fn entry(c: Cursor) -> (u64, u32) {
        let lf = leaf(c.leaf);
        (lf.scores[c.pos as usize], lf.handles[c.pos as usize])
    }

    /// Entrée suivante dans l'ordre.
    #[inline]
    pub fn next(c: Cursor) -> Option<Cursor> {
        let lf = leaf(c.leaf);
        if (c.pos + 1) < lf.n {
            Some(Cursor { leaf: c.leaf, pos: c.pos + 1 })
        } else {
            (lf.next != NONE).then_some(Cursor { leaf: lf.next, pos: 0 })
        }
    }

    /// Entrée précédente.
    #[inline]
    pub fn prev(c: Cursor) -> Option<Cursor> {
        if c.pos > 0 {
            return Some(Cursor { leaf: c.leaf, pos: c.pos - 1 });
        }
        let p = leaf(c.leaf).prev;
        (p != NONE).then(|| Cursor { leaf: p, pos: leaf(p).n - 1 })
    }

    /// Feuille suivante de celle du curseur (préchargement des plages).
    pub fn next_leaf(c: Cursor) -> u32 {
        leaf(c.leaf).next
    }

    /// Entrées dans l'ordre, à partir de `c`.
    pub fn iter_from(c: Option<Cursor>) -> Entries {
        let next: fn(&Cursor) -> Option<Cursor> = |&c| Self::next(c);
        std::iter::successors(c, next).map(Self::entry as fn(Cursor) -> (u64, u32))
    }

    /// Entrées dans l'ordre inverse, à partir de `c`.
    pub fn iter_back_from(c: Option<Cursor>) -> Entries {
        let prev: fn(&Cursor) -> Option<Cursor> = |&c| Self::prev(c);
        std::iter::successors(c, prev).map(Self::entry as fn(Cursor) -> (u64, u32))
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, u32)> {
        Self::iter_from(self.first())
    }

    /// Construit l'arbre depuis des entrées déjà triées (rechargement, passage petite →
    /// grande forme) : feuilles pleines de gauche à droite, puis les niveaux internes.
    pub fn from_sorted(entries: impl IntoIterator<Item = (u64, u32)>) -> Self {
        let mut t = Self::new();
        // (idx, compte, première clé) du niveau en cours
        let mut level: Vec<(u32, u32, (u64, u32))> = Vec::new();
        let mut cur = NONE;
        for (s, h) in entries {
            if cur == NONE || leaf(cur).n as usize == L {
                let l = new_leaf();
                if cur != NONE {
                    leaf(cur).next = l;
                    leaf(l).prev = cur;
                } else {
                    t.first = l;
                }
                level.push((l, 0, (s, h)));
                cur = l;
            }
            let lf = leaf(cur);
            lf.scores[lf.n as usize] = s;
            lf.handles[lf.n as usize] = h;
            lf.n += 1;
            level.last_mut().unwrap().1 += 1;
            t.len += 1;
        }
        if cur == NONE {
            return t;
        }
        t.last = cur;
        while level.len() > 1 {
            let mut up = Vec::with_capacity(level.len().div_ceil(I));
            for group in level.chunks(I) {
                let q = new_internal();
                let node = internal(q);
                node.n = group.len() as u16;
                for (i, &(c, cnt, k)) in group.iter().enumerate() {
                    node.child[i] = c;
                    node.count[i] = cnt;
                    if i > 0 {
                        node.sep_s[i - 1] = k.0;
                        node.sep_h[i - 1] = k.1;
                    }
                }
                up.push((q, group.iter().map(|g| g.1).sum(), group[0].2));
            }
            level = up;
            t.height += 1;
        }
        t.root = level[0].0;
        t
    }

    /// Rend tous les nœuds (DEL d'un ZSet ; O(nœuds)).
    pub fn free_all(&mut self) {
        fn rec(i: u32, h: u8) {
            if h > 0 {
                let node = internal(i);
                let (n, child) = (node.n as usize, node.child);
                for &c in &child[..n] {
                    rec(c, h - 1);
                }
            }
            free_node(i);
        }
        if self.root != NONE {
            rec(self.root, self.height);
        }
        *self = Self::new();
    }

    /// Feuilles et nœuds internes (mesures).
    pub fn node_counts(&self) -> (u32, u32) {
        fn rec(i: u32, h: u8, acc: &mut (u32, u32)) {
            if h == 0 {
                acc.0 += 1;
                return;
            }
            acc.1 += 1;
            let node = internal(i);
            for &c in &node.child[..node.n as usize] {
                rec(c, h - 1, acc);
            }
        }
        let mut acc = (0, 0);
        if self.root != NONE {
            rec(self.root, self.height, &mut acc);
        }
        acc
    }

    /// Vérificateur d'invariants : comptes, ordre, bornes des séparateurs, aucun nœud vide,
    /// hauteur uniforme, chaînage des feuilles conforme à l'ordre, `len`.
    pub fn verify(&self, tie: &impl Tie) -> Result<(), String> {
        if self.root == NONE {
            return if self.len == 0 && self.first == NONE && self.last == NONE && self.height == 0 {
                Ok(())
            } else {
                Err(format!("arbre vide incohérent : {self:?}"))
            };
        }
        let mut leaves = Vec::new();
        let (n, _) = Self::verify_rec(self.root, self.height, None, None, tie, &mut leaves)?;
        if n != self.len {
            return Err(format!("len {} mais {n} entrées", self.len));
        }
        if leaves.first() != Some(&self.first) || leaves.last() != Some(&self.last) {
            return Err("first / last ne sont pas les feuilles extrêmes".into());
        }
        for (i, &l) in leaves.iter().enumerate() {
            let lf = leaf(l);
            let prev = if i == 0 { NONE } else { leaves[i - 1] };
            let next = leaves.get(i + 1).copied().unwrap_or(NONE);
            if lf.prev != prev || lf.next != next {
                return Err(format!("chaînage de la feuille {l} : ({}, {}) au lieu de ({prev}, {next})", lf.prev, lf.next));
            }
        }
        let mut last: Option<(u64, u32)> = None;
        for e in self.iter() {
            if let Some(p) = last
                && kcmp(tie, p, e) != Ordering::Less
            {
                return Err(format!("ordre : {p:?} puis {e:?}"));
            }
            last = Some(e);
        }
        Ok(())
    }

    /// Rend le nombre d'entrées et la plus petite clé du sous-arbre.
    fn verify_rec(i: u32, h: u8, lo: Option<(u64, u32)>, hi: Option<(u64, u32)>, tie: &impl Tie, leaves: &mut Vec<u32>) -> Result<(u32, (u64, u32)), String> {
        let in_bounds = |k: (u64, u32)| {
            lo.is_none_or(|lo| kcmp(tie, k, lo) != Ordering::Less) && hi.is_none_or(|hi| kcmp(tie, k, hi) == Ordering::Less)
        };
        match (h, unsafe { slab().get_unchecked(i) }) {
            (0, BNode::Leaf(lf)) => {
                if lf.n == 0 {
                    return Err(format!("feuille {i} vide"));
                }
                for j in 0..lf.n as usize {
                    if !in_bounds((lf.scores[j], lf.handles[j])) {
                        return Err(format!("feuille {i} : entrée {j} hors des bornes des séparateurs"));
                    }
                }
                leaves.push(i);
                Ok((lf.n as u32, (lf.scores[0], lf.handles[0])))
            }
            (h, BNode::Internal(node)) if h > 0 => {
                let n = node.n as usize;
                if n == 0 {
                    return Err(format!("interne {i} sans enfant"));
                }
                let mut total = 0;
                let mut min = (0, 0);
                for c in 0..n {
                    let clo = if c == 0 { lo } else { Some((node.sep_s[c - 1], node.sep_h[c - 1])) };
                    let chi = if c + 1 == n { hi } else { Some((node.sep_s[c], node.sep_h[c])) };
                    let (got, cmin) = Self::verify_rec(node.child[c], h - 1, clo, chi, tie, leaves)?;
                    if got != node.count[c] {
                        return Err(format!("interne {i} : compte de l'enfant {c} = {} au lieu de {got}", node.count[c]));
                    }
                    if c == 0 {
                        min = cmin;
                    } else if clo != Some(cmin) {
                        return Err(format!("interne {i} : séparateur {} = {clo:?}, plus petite clé de l'enfant {cmin:?}", c - 1));
                    }
                    total += got;
                }
                Ok((total, min))
            }
            _ => Err(format!("nœud {i} : mauvaise sorte à la hauteur {h}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    thread_local! {
        /// Poignées retirées : le départage ne doit jamais en voir une (séparateur périmé).
        static DEAD: std::cell::RefCell<std::collections::HashSet<u32>> = Default::default();
    }

    /// Ex-aequo départagés par la poignée elle-même, à l'envers (pour ne pas confondre
    /// avec l'ordre de comparaison par défaut).
    fn tie() -> impl Tie {
        |a: u32, b: u32| {
            DEAD.with(|d| assert!(!d.borrow().contains(&a) && !d.borrow().contains(&b), "poignée retirée comparée"));
            b.cmp(&a)
        }
    }

    fn model_key((s, h): (u64, u32)) -> (u64, std::cmp::Reverse<u32>) {
        (s, std::cmp::Reverse(h))
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn check(t: &BTree, model: &BTreeSet<(u64, std::cmp::Reverse<u32>)>) {
        let tie = tie();
        t.verify(&tie).unwrap();
        let got: Vec<_> = t.iter().map(model_key).collect();
        let want: Vec<_> = model.iter().copied().collect();
        assert_eq!(got, want);
    }

    /// Contre un `BTreeSet` + rangs calculés : insertions, suppressions, mises à jour,
    /// rangs, sélections, bornes ; peu de scores distincts (ex-aequo nombreux).
    #[test]
    fn random_against_btreeset() {
        let tie = tie();
        for (seed, scores) in [(1u64, 8u64), (2, 1 << 40), (3, 1)] {
            let mut rng = Rng(0x2545_F491_4F6C_DD1D ^ seed);
            let mut t = BTree::new();
            let mut model = BTreeSet::new();
            // poignée → score courant
            let mut live: Vec<(u32, u64)> = Vec::new();
            let mut next_h = 0u32;
            DEAD.with(|d| d.borrow_mut().clear());
            for step in 0..30_000 {
                let op = rng.below(10);
                if live.is_empty() || op < 5 {
                    let s = rng.below(scores);
                    let h = next_h;
                    next_h += 1;
                    t.insert(s, h, &tie);
                    model.insert(model_key((s, h)));
                    live.push((h, s));
                } else if op < 8 {
                    let i = rng.below(live.len() as u64) as usize;
                    let (h, s) = live.swap_remove(i);
                    assert!(t.remove(s, h, &tie));
                    assert!(model.remove(&model_key((s, h))));
                    DEAD.with(|d| d.borrow_mut().insert(h));
                } else {
                    let i = rng.below(live.len() as u64) as usize;
                    let (h, s) = live[i];
                    let ns = if rng.below(2) == 0 { s.wrapping_add(1) % scores.max(2) } else { rng.below(scores) };
                    assert!(t.update(s, h, ns, &tie));
                    model.remove(&model_key((s, h)));
                    model.insert(model_key((ns, h)));
                    live[i].1 = ns;
                }
                if step % 997 == 0 {
                    check(&t, &model);
                    // rangs et sélections
                    let sorted: Vec<_> = model.iter().copied().collect();
                    for _ in 0..50 {
                        if sorted.is_empty() {
                            break;
                        }
                        let r = rng.below(sorted.len() as u64) as usize;
                        let (s, std::cmp::Reverse(h)) = sorted[r];
                        assert_eq!(t.rank(s, h, &tie), r as u32);
                        let c = t.select(r as u32).unwrap();
                        assert_eq!(BTree::entry(c), (s, h));
                        assert_eq!(t.find(s, h, &tie), Some(c));
                        // borne par score seul : premier score ≥ x
                        let x = rng.below(scores);
                        let (rk, c) = t.lower_bound(|es, _| if x <= es { Ordering::Less } else { Ordering::Greater });
                        let want = sorted.partition_point(|k| k.0 < x);
                        assert_eq!(rk as usize, want);
                        assert_eq!(c.map(|c| model_key(BTree::entry(c))), sorted.get(want).copied());
                    }
                    assert_eq!(t.select(t.len()), None);
                }
            }
            check(&t, &model);
            // retour arrière depuis la fin
            let back: Vec<_> = BTree::iter_back_from(t.last()).map(model_key).collect();
            let mut want: Vec<_> = model.iter().copied().collect();
            want.reverse();
            assert_eq!(back, want);
            // tout vider
            for (h, s) in live.drain(..) {
                assert!(t.remove(s, h, &tie));
            }
            assert!(t.is_empty());
            t.verify(&tie).unwrap();
            t.free_all();
        }
    }

    /// Insertions croissantes : split 90/10, feuilles remplies à ~88 %.
    #[test]
    fn sequential_inserts_fill_leaves() {
        let tie = tie();
        let mut t = BTree::new();
        let n = 100_000u32;
        for i in 0..n {
            t.insert(i as u64, i, &tie);
        }
        t.verify(&tie).unwrap();
        let (leaves, internals) = t.node_counts();
        let fill = n as f64 / (leaves as f64 * L as f64);
        assert!(fill > 0.85, "remplissage {fill}");
        assert!(internals < leaves / 15);
        assert!(t.height() <= 4);
        // suppression de tout sauf un sur 50 : fusions, racine qui baisse
        for i in 0..n {
            if i % 50 != 0 {
                assert!(t.remove(i as u64, i, &tie));
            }
        }
        t.verify(&tie).unwrap();
        assert_eq!(t.len(), n / 50);
        let (leaves, _) = t.node_counts();
        assert!(leaves < (n / 50) / 2, "fusions : {leaves} feuilles pour {} entrées", n / 50);
        t.free_all();
    }

    #[test]
    fn from_sorted_matches_inserts() {
        let tie = tie();
        for n in [0u32, 1, 40, 41, 42, 1000, 41 * 25 + 1, 30_000] {
            let mut t = BTree::from_sorted((0..n).map(|i| ((i / 3) as u64, u32::MAX - i)));
            t.verify(&tie).unwrap();
            assert_eq!(t.len(), n);
            for r in (0..n).step_by(97) {
                assert_eq!(BTree::entry(t.select(r).unwrap()), ((r / 3) as u64, u32::MAX - r));
            }
            // l'arbre reconstruit accepte les mutations
            for i in 0..n.min(500) {
                t.insert(1 << 50, i, &tie);
            }
            t.verify(&tie).unwrap();
            t.free_all();
        }
    }
}
