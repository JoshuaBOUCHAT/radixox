//! Tampon d'octets sur un mapping à lui, pour les grosses réponses construites
//! d'un seul jet (KEYS ; MEMORY_ARCH.md §18.11).
//!
//! Réservé en `MAP_NORESERVE` (seules les pages écrites comptent) avec
//! `MADV_HUGEPAGE` (écriture séquentielle : un défaut par 2 Mio). Il grandit par
//! `mremap` : le noyau déplace les entrées de table de pages, sans copier les
//! données ; pas d'effet « doublement + copie » d'un `Vec`. Rendu au système
//! d'un coup par `munmap` à la destruction.
//!
//! L'adresse peut changer en grandissant : ne pas garder de pointeur dedans
//! pendant qu'on écrit. Une fois rempli, il ne bouge plus (envoi direct par le
//! noyau, `pending_send`).

use std::ptr::null_mut;

/// Première réservation, puis doublement (virtuel seulement).
const FIRST: usize = 2 << 20;

// Mono-thread (ARCHITECTURE.md §1) : compte sans atomique.
static mut BYTES: u64 = 0;

/// Octets écrits dans les `BigBuf` vivants (`INFO` : `mem_bigbuf_bytes`).
pub fn bytes() -> u64 {
    unsafe { BYTES }
}

pub struct BigBuf {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl Default for BigBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl BigBuf {
    /// Vide, sans mapping (rien n'est réservé avant la première écriture).
    pub const fn new() -> Self {
        Self { ptr: null_mut(), len: 0, cap: 0 }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        if self.cap == 0 { &[] } else { unsafe { std::slice::from_raw_parts(self.ptr, self.len) } }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        if self.cap == 0 { &mut [] } else { unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) } }
    }

    #[cold]
    fn grow(&mut self, need: usize) {
        let cap = need.max(self.cap * 2).max(FIRST).next_multiple_of(FIRST);
        let p = unsafe {
            if self.cap == 0 {
                libc::mmap(
                    null_mut(),
                    cap,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                    -1,
                    0,
                )
            } else {
                libc::mremap(self.ptr as *mut _, self.cap, cap, libc::MREMAP_MAYMOVE)
            }
        };
        // Plage virtuelle épuisée : comme un échec d'allocation.
        if p == libc::MAP_FAILED {
            std::alloc::handle_alloc_error(std::alloc::Layout::from_size_align(cap, 1).unwrap());
        }
        if self.cap == 0 {
            unsafe { libc::madvise(p, cap, libc::MADV_HUGEPAGE) };
        }
        // Registre des pages : la capacité entière (au plus 2 × ce qui est écrit), comptée
        // au `mremap` plutôt qu'à chaque écriture.
        crate::pages::charge(cap - self.cap);
        self.ptr = p as *mut u8;
        self.cap = cap;
    }

    #[inline]
    pub fn extend_from_slice(&mut self, b: &[u8]) {
        let end = self.len + b.len();
        if end > self.cap {
            self.grow(end);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(b.as_ptr(), self.ptr.add(self.len), b.len());
            BYTES += b.len() as u64;
        }
        self.len = end;
    }

    /// `$len\r\n<bytes>\r\n` (bulk string RESP).
    pub fn push_bulk(&mut self, bytes: &[u8]) {
        let mut head = [0u8; 24];
        head[0] = b'$';
        let mut digits = [0u8; 20];
        let (mut n, mut i) = (bytes.len(), 20);
        loop {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let d = 20 - i;
        head[1..1 + d].copy_from_slice(&digits[i..]);
        head[1 + d..3 + d].copy_from_slice(b"\r\n");
        self.extend_from_slice(&head[..3 + d]);
        self.extend_from_slice(bytes);
        self.extend_from_slice(b"\r\n");
    }
}

impl Drop for BigBuf {
    fn drop(&mut self) {
        if self.cap != 0 {
            unsafe {
                BYTES -= self.len as u64;
                libc::munmap(self.ptr as *mut _, self.cap);
                crate::pages::credit(self.cap);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_without_losing_bytes() {
        let mut b = BigBuf::new();
        assert!(b.as_slice().is_empty());
        let mut model = Vec::new();
        for i in 0..400_000u32 {
            let k = format!("key:{i}");
            b.push_bulk(k.as_bytes());
            model.extend_from_slice(format!("${}\r\n{k}\r\n", k.len()).as_bytes());
        }
        assert!(b.len() > 2 * FIRST, "au moins deux mremap");
        assert_eq!(b.as_slice(), &model[..]);
        let before = bytes();
        drop(b);
        assert_eq!(bytes(), before - model.len() as u64);
    }

    #[test]
    fn empty_bulk_and_header_room() {
        let mut b = BigBuf::new();
        b.extend_from_slice(&[0; 4]);
        b.push_bulk(b"");
        b.as_mut_slice()[..2].copy_from_slice(b"*1");
        assert_eq!(b.as_slice(), b"*1\0\0$0\r\n\r\n");
    }
}
