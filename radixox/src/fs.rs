//! Fichiers par `rustix`, sans `std` : chemins en `&str`, descripteurs `OwnedFd`.
//! Seulement les quelques suites d'appels que la crate répète ; pas de couche
//! `File` / `Path`.

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result};

/// `dir/name`.
pub fn join(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

/// Ouvre `path` (`O_CLOEXEC` ajouté) ; l'erreur cite le chemin.
pub fn open(path: &str, flags: OFlags, mode: Mode) -> Result<OwnedFd> {
    rustix::fs::open(path, flags | OFlags::CLOEXEC, mode).map_err(|e| Error::from(e).ctx(path))
}

/// Lecture seule.
pub fn open_read(path: &str) -> Result<OwnedFd> {
    open(path, OFlags::RDONLY, Mode::empty())
}

/// Création ou remise à zéro, écriture seule (comme `File::create`).
pub fn create(path: &str) -> Result<OwnedFd> {
    open(path, OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC, Mode::from_raw_mode(0o644))
}

/// Contenu entier de `path`.
pub fn read(path: &str) -> Result<Vec<u8>> {
    let fd = open_read(path)?;
    read_fd(fd).map_err(|e| Error::from(e).ctx(path))
}

/// Lit `fd` jusqu'à la fin.
pub fn read_fd(fd: impl AsFd) -> core::result::Result<Vec<u8>, Errno> {
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

/// Lit autant d'octets que possible à partir de `off` (moins seulement à la fin du
/// fichier).
pub fn pread_full(fd: impl AsFd, buf: &mut [u8], off: u64) -> core::result::Result<usize, Errno> {
    let mut got = 0;
    while got < buf.len() {
        match rustix::io::pread(&fd, &mut buf[got..], off + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Écrit tout `buf` à la position courante.
pub fn write_all(fd: impl AsFd, mut buf: &[u8]) -> core::result::Result<(), Errno> {
    while !buf.is_empty() {
        match rustix::io::write(&fd, buf) {
            Ok(0) => return Err(Errno::IO),
            Ok(n) => buf = &buf[n..],
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Écrit tout `buf` à partir de `off`.
pub fn pwrite_all(fd: impl AsFd, mut buf: &[u8], mut off: u64) -> core::result::Result<(), Errno> {
    while !buf.is_empty() {
        match rustix::io::pwrite(&fd, buf, off) {
            Ok(0) => return Err(Errno::IO),
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Taille du fichier ouvert.
pub fn len(fd: impl AsFd) -> core::result::Result<u64, Errno> {
    Ok(rustix::fs::fstat(fd)?.st_size as u64)
}

/// Taille du fichier `path`.
pub fn path_len(path: &str) -> core::result::Result<u64, Errno> {
    Ok(rustix::fs::stat(path)?.st_size as u64)
}

/// fsync du dossier : rend durables les créations, renommages et suppressions qui y
/// ont été faits.
pub fn sync_dir(dir: &str) -> Result<()> {
    let d = open(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
    rustix::fs::fsync(d).map_err(|e| Error::from(e).ctx(dir))
}

/// `dir` et ses parents, comme `mkdir -p`.
pub fn create_dir_all(dir: &str) -> Result<()> {
    let mut end = 0;
    while end < dir.len() {
        end = dir[end + 1..].find('/').map_or(dir.len(), |i| end + 1 + i);
        match rustix::fs::mkdir(&dir[..end], Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(Error::from(e).ctx(&dir[..end])),
        }
    }
    Ok(())
}

/// Noms des entrées de `dir` (sans `.` ni `..`, noms non UTF-8 sautés).
pub fn read_dir(dir: &str) -> Result<Vec<String>> {
    let fd = open(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
    let mut d = rustix::fs::Dir::read_from(fd).map_err(|e| Error::from(e).ctx(dir))?;
    let mut out = Vec::new();
    while let Some(entry) = d.next() {
        let entry = entry.map_err(|e| Error::from(e).ctx(dir))?;
        let Ok(name) = entry.file_name().to_str() else { continue };
        if name != "." && name != ".." {
            out.push(name.into());
        }
    }
    Ok(out)
}

/// Source du rechargement d'un dump (`OxidArt::load_file`) : lectures directes, la
/// fenêtre de lecture est celle d'oxidart.
pub struct FdSource(pub OwnedFd);

impl oxidart::Source for FdSource {
    fn read_some(&mut self, buf: &mut [u8]) -> core::result::Result<usize, oxidart::LoadError> {
        loop {
            match rustix::io::read(&self.0, &mut *buf) {
                Err(Errno::INTR) => {}
                r => return r.map_err(|e| oxidart::LoadError::Os(e.into())),
            }
        }
    }
}

/// Lecture séquentielle tamponnée (petits enregistrements, comme les blocs d'un
/// segment de l'AOF).
pub struct BufReader {
    fd: OwnedFd,
    buf: Vec<u8>,
    pos: usize,
    end: usize,
}

impl BufReader {
    pub fn new(fd: OwnedFd, cap: usize) -> Self {
        Self { fd, buf: vec![0; cap], pos: 0, end: 0 }
    }

    /// Remplit `out` autant que possible (moins seulement à la fin du fichier).
    pub fn read_full(&mut self, out: &mut [u8]) -> core::result::Result<usize, Errno> {
        let mut got = 0;
        while got < out.len() {
            if self.pos == self.end {
                // Grande lecture : directement dans `out`.
                if out.len() - got >= self.buf.len() {
                    match rustix::io::read(&self.fd, &mut out[got..]) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(Errno::INTR) => {}
                        Err(e) => return Err(e),
                    }
                    continue;
                }
                match rustix::io::read(&self.fd, &mut self.buf[..]) {
                    Ok(0) => break,
                    Ok(n) => (self.pos, self.end) = (0, n),
                    Err(Errno::INTR) => continue,
                    Err(e) => return Err(e),
                }
            }
            let n = (self.end - self.pos).min(out.len() - got);
            out[got..got + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            got += n;
        }
        Ok(got)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_dir_all_read_dir_and_buffered_reads() {
        let base = format!("{}/radixox-fs-{}", std::env::temp_dir().display(), std::process::id());
        let _ = std::fs::remove_dir_all(&base);
        let deep = format!("{base}/a/b");
        create_dir_all(&deep).unwrap();
        create_dir_all(&deep).unwrap();
        let f = create(&join(&deep, "x")).unwrap();
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        write_all(&f, &data).unwrap();
        drop(f);
        assert_eq!(read(&join(&deep, "x")).unwrap(), data);
        assert_eq!(path_len(&join(&deep, "x")), Ok(10_000));
        assert_eq!(read_dir(&deep).unwrap(), ["x"]);
        let mut r = BufReader::new(open_read(&join(&deep, "x")).unwrap(), 64);
        let (mut back, mut piece) = (Vec::new(), [0u8; 37]);
        loop {
            let n = r.read_full(&mut piece).unwrap();
            back.extend_from_slice(&piece[..n]);
            if n < piece.len() {
                break;
            }
        }
        assert_eq!(back, data);
        let e = open_read(&join(&deep, "absent")).unwrap_err();
        assert!(e.is_not_found());
        assert!(e.to_string().ends_with("/a/b/absent: No such file or directory (os error 2)"), "{e}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
