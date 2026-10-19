//! Boucle io_uring maison, sans runtime (RUNTIME_ARCH.md). Ce module contient
//! le cœur **sans I/O** : machines à états des connexions, files de frames,
//! tirage par tours, envoi et fermeture. Il reçoit des [`Completion`] et émet
//! des [`Op`] ; la couche io_uring du binaire `radixox-uring` les traduit en
//! CQE / SQE, et les tests les fabriquent (faux noyau, `tests.rs`).

pub mod bufs;
pub mod conn;
mod core;

#[cfg(test)]
mod tests;

pub use bufs::Bid;
pub use conn::Mode;
pub use core::{Completion, Config, Core, ECANCELED, ENOBUFS, Op, Slot};
