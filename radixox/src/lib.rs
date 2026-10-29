//! RadixOx : commandes, encodage, jobs, persistance (`exec`) et cœur sans I/O de la
//! boucle io_uring (`uring`). `no_std` + `alloc` sans la feature `std` (défaut) : tout
//! ce que la lib demande au système passe par `rustix` et `radixox_lib::sys`.

#![no_std]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

/// Ce que le prélude de `std` donnait, pour un module `no_std`.
pub(crate) mod prelude {
    #[allow(unused_imports)]
    pub use alloc::borrow::ToOwned;
    pub use alloc::boxed::Box;
    pub use alloc::string::{String, ToString};
    pub use alloc::vec::Vec;
    pub use alloc::{format, vec};
}
#[allow(unused_imports)]
use prelude::*;

pub mod error;
pub mod exec;
pub mod fs;
mod mono;
pub mod ring;
pub mod server;
pub mod tsc;
pub mod uring;

pub use error::{Error, Result};
