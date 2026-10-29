//! Suites d'opérations de fichier hors de la boucle (DURABILITY_DESIGN.md §8.6).
//!
//! `open` avec création, `fsync`, `fallocate`, `rename`, `unlink` n'ont pas de
//! forme non bloquante : sous ext4 chargé, chacun peut attendre le commit du
//! journal (10 à 25 ms mesurés, plus pendant la suppression d'un gros fichier).
//! Appelés par la boucle, ils gèlent tous les clients. Un [`FsTask`] décrit la
//! suite à faire ; le cœur en soumet une étape à la fois (`Op::Fs`), io_uring
//! l'exécute sur un worker io-wq, le CQE fait passer à la suivante. La suite
//! s'arrête à la première erreur. Le faux noyau des tests et l'arrêt du serveur
//! l'exécutent d'une traite ([`FsTask::run_blocking`]).

use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{FallocateFlags, Mode, OFlags};
use rustix::io::Errno;

/// Descripteur visé par une étape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdRef {
    /// Fichier tenu par le demandeur, ouvert pendant toute la suite.
    Raw(RawFd),
    /// `n`-ième fichier ouvert par la suite elle-même.
    Opened(usize),
}

#[derive(Debug)]
enum Step {
    Open { path: CString, flags: OFlags, mode: Mode },
    Fallocate { fd: FdRef, off: u64, len: u64 },
    /// `data` : `fdatasync`, sinon `fsync`.
    Fsync { fd: FdRef, data: bool },
    Write { fd: FdRef, buf: Vec<u8>, off: u64 },
    Ftruncate { fd: FdRef, len: u64 },
    Rename { from: CString, to: CString },
    /// `missing_ok` : un fichier déjà absent n'est pas une erreur.
    Unlink { path: CString, missing_ok: bool },
    Close { fd: FdRef },
}

/// L'étape à soumettre, descripteurs résolus. Les pointeurs restent valides
/// jusqu'au CQE : les chaînes et tampons vivent dans la suite, qui ne les
/// déplace pas.
#[derive(Debug, Clone, Copy)]
pub enum StepRef<'a> {
    Open { path: &'a CStr, flags: OFlags, mode: Mode },
    Fallocate { fd: RawFd, off: u64, len: u64 },
    Fsync { fd: RawFd, data: bool },
    Write { fd: RawFd, buf: &'a [u8], off: u64 },
    Ftruncate { fd: RawFd, len: u64 },
    Rename { from: &'a CStr, to: &'a CStr },
    Unlink { path: &'a CStr },
    Close { fd: RawFd },
}

/// Une suite d'opérations de fichier, exécutée dans l'ordre.
#[derive(Debug, Default)]
pub struct FsTask {
    steps: Vec<Step>,
    pc: usize,
    /// Fichiers ouverts par la suite (`-1` : fermé ou repris par le demandeur).
    opened: Vec<RawFd>,
    /// Octets déjà écrits par l'étape `Write` en cours (écriture courte).
    wrote: usize,
    err: Option<(usize, i32)>,
}

fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("chemin sans octet nul")
}

impl FsTask {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ouvre `path` ; le fichier devient `FdRef::Opened(n)`, `n` rendu.
    pub fn open(&mut self, path: &Path, flags: OFlags, mode: Mode) -> FdRef {
        let n = self.steps.iter().filter(|s| matches!(s, Step::Open { .. })).count();
        self.steps.push(Step::Open { path: cpath(path), flags: flags | OFlags::CLOEXEC, mode });
        FdRef::Opened(n)
    }

    pub fn fallocate(&mut self, fd: FdRef, off: u64, len: u64) {
        self.steps.push(Step::Fallocate { fd, off, len });
    }

    pub fn fsync(&mut self, fd: FdRef, data: bool) {
        self.steps.push(Step::Fsync { fd, data });
    }

    pub fn write(&mut self, fd: FdRef, buf: Vec<u8>, off: u64) {
        self.steps.push(Step::Write { fd, buf, off });
    }

    pub fn ftruncate(&mut self, fd: FdRef, len: u64) {
        self.steps.push(Step::Ftruncate { fd, len });
    }

    pub fn rename(&mut self, from: &Path, to: &Path) {
        self.steps.push(Step::Rename { from: cpath(from), to: cpath(to) });
    }

    pub fn unlink(&mut self, path: &Path, missing_ok: bool) {
        self.steps.push(Step::Unlink { path: cpath(path), missing_ok });
    }

    pub fn close(&mut self, fd: FdRef) {
        self.steps.push(Step::Close { fd });
    }

    /// fsync du dossier (ouvert, synchronisé, refermé) : rend durables les
    /// créations, renommages et suppressions qui y ont été faits.
    pub fn sync_dir(&mut self, dir: &Path) {
        let d = self.open(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty());
        self.fsync(d, false);
        self.close(d);
    }

    /// Écriture atomique d'un petit fichier : `tmp` écrit et synchronisé, puis
    /// renommé en `name`, puis fsync du dossier.
    pub fn replace_file(&mut self, dir: &Path, tmp: &str, name: &str, bytes: Vec<u8>) {
        let f = self.open(&dir.join(tmp), OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC, Mode::from_raw_mode(0o644));
        self.write(f, bytes, 0);
        self.fsync(f, true);
        self.close(f);
        self.rename(&dir.join(tmp), &dir.join(name));
        self.sync_dir(dir);
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Plus rien à soumettre : suite finie, ou arrêtée par une erreur.
    pub fn finished(&self) -> bool {
        self.err.is_some() || self.pc == self.steps.len()
    }

    /// Première erreur : numéro d'étape et `errno`.
    pub fn error(&self) -> Option<std::io::Error> {
        self.err.map(|(_, e)| std::io::Error::from_raw_os_error(e))
    }

    fn fd(&self, r: FdRef) -> RawFd {
        match r {
            FdRef::Raw(fd) => fd,
            FdRef::Opened(n) => self.opened[n],
        }
    }

    /// L'étape à soumettre, s'il en reste une.
    pub fn step(&self) -> Option<StepRef<'_>> {
        if self.finished() {
            return None;
        }
        Some(match &self.steps[self.pc] {
            Step::Open { path, flags, mode } => StepRef::Open { path, flags: *flags, mode: *mode },
            Step::Fallocate { fd, off, len } => StepRef::Fallocate { fd: self.fd(*fd), off: *off, len: *len },
            Step::Fsync { fd, data } => StepRef::Fsync { fd: self.fd(*fd), data: *data },
            Step::Write { fd, buf, off } => {
                StepRef::Write { fd: self.fd(*fd), buf: &buf[self.wrote..], off: off + self.wrote as u64 }
            }
            Step::Ftruncate { fd, len } => StepRef::Ftruncate { fd: self.fd(*fd), len: *len },
            Step::Rename { from, to } => StepRef::Rename { from, to },
            Step::Unlink { path, .. } => StepRef::Unlink { path },
            Step::Close { fd } => StepRef::Close { fd: self.fd(*fd) },
        })
    }

    /// Résultat de l'étape soumise (valeur du CQE : `>= 0` ou `-errno`).
    pub fn done(&mut self, res: i32) {
        debug_assert!(!self.finished());
        let step = &self.steps[self.pc];
        if res < 0 {
            if !matches!(step, Step::Unlink { missing_ok: true, .. } if res == -Errno::NOENT.raw_os_error()) {
                self.err = Some((self.pc, -res));
                return;
            }
        } else {
            match step {
                Step::Open { .. } => self.opened.push(res),
                Step::Write { buf, .. } => {
                    if res == 0 {
                        self.err = Some((self.pc, Errno::IO.raw_os_error()));
                        return;
                    }
                    self.wrote += res as usize;
                    if self.wrote < buf.len() {
                        // Écriture courte : la même étape repart avec le reste.
                        return;
                    }
                    self.wrote = 0;
                }
                Step::Close { fd: FdRef::Opened(n) } => self.opened[*n] = -1,
                _ => {}
            }
        }
        self.pc += 1;
    }

    /// Reprend le `n`-ième fichier ouvert (la suite ne le fermera plus).
    pub fn take_file(&mut self, n: usize) -> Option<File> {
        let fd = std::mem::replace(self.opened.get_mut(n)?, -1);
        // SAFETY: fd ouvert par la suite, possédé par elle seule jusqu'ici.
        (fd >= 0).then(|| unsafe { File::from_raw_fd(fd) })
    }

    /// Exécute l'étape courante par un appel bloquant : même résultat qu'un CQE.
    pub fn exec_blocking(step: StepRef<'_>) -> i32 {
        use rustix::{fs, io};
        use std::os::fd::{BorrowedFd, IntoRawFd, OwnedFd};
        // SAFETY: les descripteurs d'une étape restent ouverts jusqu'à son résultat.
        let b = |fd: RawFd| unsafe { BorrowedFd::borrow_raw(fd) };
        let r = match step {
            StepRef::Open { path, flags, mode } => fs::open(path, flags, mode).map(|f| f.into_raw_fd()),
            StepRef::Fallocate { fd, off, len } => fs::fallocate(b(fd), FallocateFlags::empty(), off, len).map(|()| 0),
            StepRef::Fsync { fd, data: true } => fs::fdatasync(b(fd)).map(|()| 0),
            StepRef::Fsync { fd, data: false } => fs::fsync(b(fd)).map(|()| 0),
            StepRef::Write { fd, buf, off } => io::pwrite(b(fd), buf, off).map(|n| n as i32),
            StepRef::Ftruncate { fd, len } => fs::ftruncate(b(fd), len).map(|()| 0),
            StepRef::Rename { from, to } => fs::rename(from, to).map(|()| 0),
            StepRef::Unlink { path } => fs::unlink(path).map(|()| 0),
            // SAFETY: l'étape possède `fd` et ne le reprend pas.
            StepRef::Close { fd } => {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
                Ok(0)
            }
        };
        r.unwrap_or_else(|e| -e.raw_os_error())
    }

    /// Toute la suite, d'une traite (faux noyau, arrêt du serveur).
    pub fn run_blocking(&mut self) {
        while let Some(step) = self.step() {
            let res = Self::exec_blocking(step);
            self.done(res);
        }
    }
}

impl Drop for FsTask {
    fn drop(&mut self) {
        for &fd in &self.opened {
            if fd >= 0 {
                // SAFETY: fd ouvert par la suite et jamais repris.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("radixox-fs-task-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn replace_file_then_take_and_unlink() {
        let d = tmpdir("replace");
        let mut t = FsTask::new();
        t.replace_file(&d, "m.tmp", "m", b"hello".to_vec());
        let seg = t.open(&d.join("seg"), OFlags::RDWR | OFlags::CREATE | OFlags::EXCL, Mode::from_raw_mode(0o644));
        t.fallocate(seg, 0, 4096);
        t.fsync(seg, false);
        t.run_blocking();
        assert!(t.error().is_none(), "{:?}", t.error());
        assert_eq!(std::fs::read(d.join("m")).unwrap(), b"hello");
        assert!(!d.join("m.tmp").exists());
        let FdRef::Opened(n) = seg else { unreachable!() };
        let f = t.take_file(n).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 4096);

        let mut t = FsTask::new();
        t.unlink(&d.join("absent"), true);
        t.unlink(&d.join("m"), false);
        t.run_blocking();
        assert!(t.error().is_none());
        assert!(!d.join("m").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stops_at_the_first_error_and_short_writes_resume() {
        let d = tmpdir("err");
        let mut t = FsTask::new();
        t.unlink(&d.join("absent"), false);
        t.replace_file(&d, "x.tmp", "x", b"never".to_vec());
        t.run_blocking();
        assert_eq!(t.error().unwrap().raw_os_error(), Some(Errno::NOENT.raw_os_error()));
        assert!(!d.join("x.tmp").exists(), "rien après l'étape en échec");

        // Écriture courte rendue par le noyau : l'étape repart avec le reste.
        let mut t = FsTask::new();
        let f = t.open(&d.join("w"), OFlags::WRONLY | OFlags::CREATE, Mode::from_raw_mode(0o644));
        t.write(f, b"abcdef".to_vec(), 0);
        let res = FsTask::exec_blocking(t.step().unwrap());
        t.done(res);
        let StepRef::Write { fd, .. } = t.step().unwrap() else { panic!() };
        // SAFETY: fd ouvert par la suite.
        assert_eq!(rustix::io::pwrite(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, b"abc", 0), Ok(3));
        t.done(3);
        let StepRef::Write { buf, off, .. } = t.step().unwrap() else { panic!("écriture à reprendre") };
        assert_eq!((buf, off), (&b"def"[..], 3));
        t.run_blocking();
        assert!(t.finished() && t.error().is_none());
        assert_eq!(std::fs::read(d.join("w")).unwrap(), b"abcdef");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
