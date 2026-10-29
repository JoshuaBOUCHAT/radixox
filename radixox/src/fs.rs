//! Fichiers par `rustix`, sans `std` : chemins en `&str`, descripteurs `OwnedFd`.
//! Seulement les quelques suites d'appels que la crate répète ; pas de couche
//! `File` / `Path`.

use rustix::fd::AsFd;
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// Contenu entier de `path`.
pub fn read(path: &str) -> Result<Vec<u8>, Errno> {
    let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())?;
    read_fd(fd)
}

/// Lit `fd` jusqu'à la fin.
pub fn read_fd(fd: impl AsFd) -> Result<Vec<u8>, Errno> {
    let mut out = Vec::new();
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(4096));
        }
        match rustix::io::read(&fd, rustix::buffer::spare_capacity(&mut out)) {
            Ok(0) => return Ok(out),
            Ok(_) => {}
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
}
