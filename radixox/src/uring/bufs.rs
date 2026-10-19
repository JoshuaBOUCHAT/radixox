//! Tampons de réception partagés par toutes les connexions (anneau de tampons
//! fournis, mode incrémental : RUNTIME_ARCH.md §5.1, §5.4).
//!
//! Le noyau écrit chaque morceau reçu à la suite du précédent dans le tampon
//! en tête de l'anneau, quel que soit le socket. Un tampon revient à l'anneau
//! quand le noyau a fini d'y écrire (dernier morceau sans `F_BUF_MORE`) **et**
//! qu'aucune frame en attente ne le vise plus (`refs == 0`).

/// Identifiant d'un tampon dans l'anneau.
pub type Bid = u16;

pub struct BufPool {
    data: Vec<Box<[u8]>>,
    /// Prochain octet que le noyau écrira (mode incrémental).
    head: Vec<u32>,
    /// Frames en attente (`Item::Ring`) qui visent ce tampon.
    refs: Vec<u32>,
    /// Le noyau a rendu le tampon (dernier morceau sans `F_BUF_MORE`).
    kernel_done: Vec<bool>,
    /// Tampons actuellement dans l'anneau (utilisables par le noyau).
    available: u32,
    size: u32,
}

impl BufPool {
    pub fn new(count: Bid, size: u32) -> Self {
        let n = count as usize;
        Self {
            data: (0..n).map(|_| vec![0u8; size as usize].into_boxed_slice()).collect(),
            head: vec![0; n],
            refs: vec![0; n],
            kernel_done: vec![false; n],
            available: count as u32,
            size,
        }
    }

    pub fn count(&self) -> Bid {
        self.data.len() as Bid
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// Adresse du tampon, pour l'enregistrer dans l'anneau du noyau. Le noyau
    /// n'y écrit qu'entre deux restitutions : jamais pendant qu'une `&[u8]`
    /// obtenue par [`slice`](Self::slice) vit (tout passe par `enter`, hors des
    /// emprunts du cœur).
    pub fn ptr(&mut self, bid: Bid) -> *mut u8 {
        self.data[bid as usize].as_mut_ptr()
    }

    /// Un morceau de `len` octets vient d'arriver dans `bid` ; rend son offset.
    pub fn chunk(&mut self, bid: Bid, len: u32, buf_more: bool) -> u32 {
        let b = bid as usize;
        let off = self.head[b];
        debug_assert!(off + len <= self.size, "morceau hors du tampon");
        self.head[b] = off + len;
        if !buf_more {
            self.kernel_done[b] = true;
            self.available -= 1;
        }
        off
    }

    pub fn slice(&self, bid: Bid, off: u32, len: u32) -> &[u8] {
        &self.data[bid as usize][off as usize..(off + len) as usize]
    }

    /// `n` frames de plus visent `bid`.
    pub fn retain(&mut self, bid: Bid, n: u32) {
        self.refs[bid as usize] += n;
    }

    pub fn release(&mut self, bid: Bid) {
        let r = &mut self.refs[bid as usize];
        debug_assert!(*r > 0, "release sans retain");
        *r -= 1;
    }

    /// Plus rien ne le vise et le noyau l'a rendu : peut retourner à l'anneau.
    pub fn recyclable(&self, bid: Bid) -> bool {
        self.refs[bid as usize] == 0 && self.kernel_done[bid as usize]
    }

    /// Remis à zéro avant d'être rendu à l'anneau.
    pub fn recycle(&mut self, bid: Bid) {
        let b = bid as usize;
        debug_assert!(self.recyclable(bid));
        self.head[b] = 0;
        self.kernel_done[b] = false;
        self.available += 1;
    }

    /// Au moins un tampon est dans l'anneau : un recv peut être réarmé.
    pub fn any_available(&self) -> bool {
        self.available > 0
    }

    pub fn refs(&self, bid: Bid) -> u32 {
        self.refs[bid as usize]
    }

    /// Accès d'écriture simulant le noyau (tests du cœur).
    #[cfg(test)]
    pub(crate) fn kernel_write(&mut self, bid: Bid, off: u32, bytes: &[u8]) {
        let b = &mut self.data[bid as usize];
        b[off as usize..off as usize + bytes.len()].copy_from_slice(bytes);
    }
}
