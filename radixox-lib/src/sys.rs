//! Appels système de la mémoire et sortie d'erreur, par `rustix` : appels directs au noyau,
//! sans passer par la libc (et sans allouer : utilisables depuis l'allocateur lui-même).
//! Enveloppes minces, les sites d'appel gardent leur forme ; les retours ignorés le sont
//! comme avant (`munmap` / `madvise` d'une plage à nous ne peut échouer qu'à tort).

use std::ptr::null_mut;

use rustix::mm::{self, MapFlags, MremapFlags, ProtFlags};

pub use rustix::mm::Advice;

/// `mmap` anonyme, privé, lecture-écriture ; `noreserve` : rien n'est engagé avant d'être
/// touché (`MAP_NORESERVE`).
#[inline]
pub fn map_anon(len: usize, noreserve: bool) -> rustix::io::Result<*mut u8> {
    let flags = if noreserve { MapFlags::PRIVATE | MapFlags::NORESERVE } else { MapFlags::PRIVATE };
    // SAFETY: nouvelle plage, aucune adresse imposée.
    unsafe { mm::mmap_anonymous(null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, flags) }.map(|p| p.cast())
}

/// # Safety
/// `[p, p + len)` est une plage mappée par nous, plus utilisée.
#[inline]
pub unsafe fn unmap(p: *mut u8, len: usize) {
    let _ = unsafe { mm::munmap(p.cast(), len) };
}

/// # Safety
/// `[p, p + len)` est dans une plage mappée par nous ; pour `LinuxDontNeed`, son contenu
/// n'est plus utilisé (relu comme des zéros).
#[inline]
pub unsafe fn advise(p: *mut u8, len: usize, advice: Advice) {
    let _ = unsafe { mm::madvise(p.cast(), len, advice) };
}

/// `mremap` avec `MREMAP_MAYMOVE`.
///
/// # Safety
/// `[p, p + old)` est une plage mappée par nous ; après succès, seule la nouvelle adresse vaut.
#[inline]
pub unsafe fn remap(p: *mut u8, old: usize, new: usize) -> rustix::io::Result<*mut u8> {
    unsafe { mm::mremap(p.cast(), old, new, MremapFlags::MAYMOVE) }.map(|p| p.cast())
}

/// Message sur stderr sans allouer (écriture partielle ignorée).
pub fn stderr(msg: &[u8]) {
    let _ = rustix::io::write(rustix::stdio::stderr(), msg);
}
