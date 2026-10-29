//! Édition de liens du binaire `radixox-nostd` (NOSTD_PLAN.md, étape 3) : sans les
//! fichiers de démarrage ni les bibliothèques du C (`_start` est le nôtre, aucune libc),
//! statique, non PIE (aucune relocation à appliquer au démarrage). Par
//! `rustc-link-arg-bins` et non par `RUSTFLAGS`, qui s'appliqueraient aussi aux scripts
//! de build.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_NOSTD_BIN").is_some() {
        for arg in ["-nostartfiles", "-nostdlib", "-static", "-no-pie"] {
            println!("cargo:rustc-link-arg-bin=radixox-nostd={arg}");
        }
    }
}
