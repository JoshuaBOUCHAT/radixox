//! Erreur de la crate, sans `std` : un code du noyau (`errno`), un message, ou les
//! deux (le message cite alors le chemin ou l'opération).

#[allow(unused_imports)]
use crate::prelude::*;

use core::fmt;

use radixox_lib::sys::OsError;
use rustix::io::Errno;

pub struct Error {
    os: Option<OsError>,
    /// Vide : seul le code compte.
    msg: String,
}

pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    /// Message seul (donnée invalide, état refusé…).
    pub fn msg(msg: impl Into<String>) -> Self {
        Self { os: None, msg: msg.into() }
    }

    /// Code (s'il y en a un) et message complet.
    pub fn new(os: Option<OsError>, msg: String) -> Self {
        Self { os, msg }
    }

    /// Code du noyau, s'il y en a un.
    pub fn os(&self) -> Option<OsError> {
        self.os
    }

    pub fn is_not_found(&self) -> bool {
        self.os == Some(OsError(Errno::NOENT.raw_os_error()))
    }

    /// Préfixe `what: ` au message (chemin, opération) ; le code reste.
    pub fn ctx(self, what: &str) -> Self {
        Self { os: self.os, msg: format!("{what}: {self}") }
    }
}

impl From<Errno> for Error {
    fn from(e: Errno) -> Self {
        Self { os: Some(e.into()), msg: String::new() }
    }
}

impl From<OsError> for Error {
    fn from(e: OsError) -> Self {
        Self { os: Some(e), msg: String::new() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.os, self.msg.is_empty()) {
            (Some(os), true) => write!(f, "{os}"),
            _ => f.write_str(&self.msg),
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl core::error::Error for Error {}

#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        match e.os {
            Some(os) if e.msg.is_empty() => std::io::Error::from_raw_os_error(os.0),
            Some(os) => std::io::Error::new(std::io::Error::from_raw_os_error(os.0).kind(), e.msg),
            None => std::io::Error::other(e.msg),
        }
    }
}
