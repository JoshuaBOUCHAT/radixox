//! `#![no_std]` + `alloc` : tout ce que la crate demande au système passe par [`sys`]
//! (rustix et `sc`, sans libc). La feature `std` (par défaut) n'ajoute que ce qui sert à un
//! processus `std` : thread propriétaire du tas, trace des allocations, `errno` du `malloc`
//! exporté, `std::io::Error` des constructeurs.
#![no_std]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

/// Ce que le prélude de `std` donnait, pour un module `no_std`.
#[allow(unused_imports)]
pub(crate) mod prelude {
    pub use alloc::borrow::ToOwned;
    pub use alloc::boxed::Box;
    pub use alloc::string::{String, ToString};
    pub use alloc::vec::Vec;
    pub use alloc::{format, vec};
}

pub mod array_vec;
pub mod big_buf;
pub mod cmd;
pub mod gen_arena;
pub mod handle;
pub mod hislab;
pub mod inline_childs;
pub mod large;
pub mod pages;
pub mod pool;
pub mod prefetch;
pub mod raw_alloc;
pub mod resident;
pub mod rng;
pub mod save_barrier;
pub mod shared_byte;
pub mod shared_frame;
pub mod sys;
pub mod small_vec;
#[cfg(test)]
mod test;
pub mod thin_vec;
pub mod varint;
