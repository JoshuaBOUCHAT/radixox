# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> **À chaque session** : lire [`ARCHITECTURE.md`](ARCHITECTURE.md) (vue d'ensemble point
> par point, invariants, carte des documents), puis le document du sujet touché :
> - serveur réseau, connexions, Pub/Sub, commandes bloquantes, boucle io_uring →
>   [`RUNTIME_ARCH.md`](RUNTIME_ARCH.md) ;
> - persistance, mutations d'`oxidart/src/lib.rs`, `valart/`,
>   `radixox-lib/src/save_barrier.rs` → [`SAVE_DESIGN.md`](SAVE_DESIGN.md) ;
> - SCAN → [`SCAN_DESIGN.md`](SCAN_DESIGN.md) ; transactions → [`TX_DESIGN.md`](TX_DESIGN.md) ;
>   ZSet → [`ANALYSE_ART_UNIFIE.md`](ANALYSE_ART_UNIFIE.md) §13 ;
> - AOF, réplication, journal des commandes → [`DURABILITY_DESIGN.md`](DURABILITY_DESIGN.md) ;
> - allocation, GC, compactage des slabs, `maxmemory` → [`MEMORY_ARCH.md`](MEMORY_ARCH.md).

## Projet

RadixOx : base clé-valeur en mémoire qui parle RESP2 (sous-ensemble des commandes Redis),
bâtie sur OxidArt, un radix tree adaptatif. Mono-thread, boucle io_uring maison sans
runtime (noyau ≥ 6.12), Linux uniquement, Rust 2024.

## Workspace

```
oxidart/        arbre des clés : TTL, préfixe / glob / range, descentes entrelacées, save
oxidart-legacy/ ancien arbre (nœuds 64 o), oracle des tests différentiels, non publié
valart/         radix compacts pour les valeurs Hash et Set
radixox-lib/    SharedByte, parseur RESP (cmd.rs) et encodeur, barrière de save
radixox/        lib : `exec` (commandes, encodage, jobs, BGSAVE), `uring` (cœur sans I/O
                de la boucle + faux noyau) ; binaire `radixox` (src/bin/uringloop.rs,
                couche io_uring) ; tests d'intégration
scripts/        bench_vs_valkey.sh (référence du README), bench_zipf.sh (accès Zipf, CPU par cœur), stress_save.sh
```

`HiSlab` (slab mmap + THP) vit dans `radixox-lib/src/hislab/` (ancien crate `hislab`, intégré
le 2026-10-28 ; `~/rust/hislab` n'est plus utilisé par RadixOx).

## Commandes

```bash
cargo build --release -p radixox          # binaire target/release/radixox
RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release -p radixox --target x86_64-unknown-linux-musl --target-dir target/portable   # binaire statique portable (sans RUSTFLAGS : native, AVX-512 du 7600X → SIGILL ailleurs) ; README « Portable and static builds »
cargo run --release --bin radixox         # écoute 127.0.0.1:6379 (RADIXOX_BIND / RADIXOX_PORT)

cargo test -p oxidart --features regex    # arbre (dont tests différentiels contre oxidart-legacy)
cargo test -p radixox-lib                 # parseur, SharedByte, barrière
cargo build -p radixox-lib -p valart -p oxidart --no-default-features --features oxidart/regex   # #![no_std] + alloc : ne doit pas casser
cargo test -p radixox                     # intégration : lance le binaire sur un port de test
cargo test -p radixox --features eviction  # + LRU/LFU (tests sous cfg(feature = "eviction"))
```

- Tests en mono-thread obligatoires (`RUST_TEST_THREADS=1` dans `.cargo/config.toml`) :
  les slabs sont des statics globaux.
- Benchs et tests lourds : courts (ordre de grandeur), sous
  `systemd-run --scope -p MemoryMax=…`, profil CPU `performance`. memtier toujours avec
  `--distinct-client-seed --randomize`. Méthode : RUNTIME_ARCH §12.1.
- Configuration (`RADIXOX_*`) : tableau dans le README, section *Configuration*.

## Profils cargo

`release` : LTO fat, `codegen-units = 1`, **`panic = "abort"`** (un panic arrête le
serveur : aucune donnée client ne doit pouvoir en provoquer un), symboles retirés.
`profiling` : release + symboles (pour `perf`).
