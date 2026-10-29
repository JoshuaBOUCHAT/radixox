//! `radixox` : serveur RESP sur la boucle io_uring maison, processus `std` (glibc).
//! Le serveur lui-même est [`radixox::server`], partagé avec `radixox-nostd`.

#[cfg(not(target_os = "linux"))]
compile_error!("RadixOx requires Linux to run (io_uring).");

/// Allocateur de RadixOx pour tout le processus (MEMORY_ARCH.md §18.12) ; la libc sous
/// `--cfg radixox_sysalloc` (AddressSanitizer).
#[cfg(not(radixox_sysalloc))]
#[global_allocator]
static HEAP: radixox_lib::raw_alloc::heap::RxAlloc = radixox_lib::raw_alloc::heap::RxAlloc;

#[cfg(not(radixox_sysalloc))]
#[path = "shared/c_malloc.rs"]
mod c_malloc;

fn main() {
    if let Err(e) = radixox::server::main() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
