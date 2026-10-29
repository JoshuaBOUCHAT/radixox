//! `radixox-nostd` : le même serveur ([`radixox::server`]) sans `std` ni libc
//! (NOSTD_PLAN.md). Point d'entrée `_start` à nous, `panic` = message puis abort, tas de
//! RadixOx pour Rust comme pour le C lié (zstd : `malloc` exporté), fonctions mémoire de
//! `radixox-mem`. Lié statiquement, non PIE (build.rs) : pas de relocations à traiter au
//! démarrage, pas d'ASLR du binaire. Pas de TLS : rien ici ne lit `%fs`.

#![no_std]
#![no_main]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("radixox-nostd : Linux x86_64 seulement (`_start` en assembleur).");

// memcpy, memmove, memset, memcmp, bcmp sous leurs noms C.
extern crate radixox_mem;

#[global_allocator]
static HEAP: radixox_lib::raw_alloc::heap::RxAlloc = radixox_lib::raw_alloc::heap::RxAlloc;

#[path = "shared/c_malloc.rs"]
mod c_malloc;

// Le noyau saute ici avec `rsp` sur `argc` (puis `argv`, `envp`, auxv), aligné sur 16.
// Rien à lire : l'environnement vient de `/proc/self/environ` (`radixox_lib::sys::env`).
// `rbp` à zéro (fin de la chaîne des cadres), pile réalignée, appel de `start`, qui ne
// rend pas la main.
core::arch::global_asm!(
    ".globl _start",
    ".type _start, @function",
    "_start:",
    "xor ebp, ebp",
    "and rsp, -16",
    "call {start}",
    "ud2",
    start = sym start,
);

extern "C" fn start() -> ! {
    match radixox::server::main() {
        Ok(()) => radixox_lib::sys::exit(0),
        Err(e) => {
            radixox_lib::rx_eprintln!("Error: {e}");
            radixox_lib::sys::exit(1)
        }
    }
}

// `alloc` et `core` sont précompilés avec unwind : leurs pads de nettoyage citent ces deux
// symboles. Avec `panic = "abort"`, aucun unwind n'a lieu, ils ne sont jamais appelés.
#[unsafe(no_mangle)]
extern "C" fn _Unwind_Resume() -> ! {
    radixox_lib::sys::abort()
}

#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    radixox_lib::rx_eprintln!("{info}");
    radixox_lib::sys::abort()
}
