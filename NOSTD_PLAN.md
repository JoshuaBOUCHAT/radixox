# Binaire RadixOx sans `std` ni libc — plan de travail

Document de passation (2026-10-29) : de quoi reprendre le chantier sans autre contexte.
Lire d'abord `CLAUDE.md` et `ARCHITECTURE.md`, puis `RUNTIME_ARCH.md` (boucle io_uring)
avant l'étape 2.

## 0. Objectif et définition de « fini »

Un binaire `radixox` `#![no_std] #![no_main]`, **Rust stable**, lié statiquement **sans
aucune libc** (ni glibc ni musl), qui passe toute la suite de tests d'intégration et tient le
débit du binaire glibc actuel. Le binaire `std` actuel reste construit à côté pendant tout le
chantier (comparaison, repli) ; le retirer est une décision de l'utilisateur, pas du chantier.

Fini quand :
1. `cargo test -p radixox` (et `--features eviction`) passe **contre le binaire no_std**
   (les tests d'intégration lancent `CARGO_BIN_EXE_…` : prévoir le moyen de les pointer
   vers le binaire no_std, voir §5) ;
2. `ldd` dit « statically linked » / « not a dynamic executable », `nm` ne montre aucun
   symbole de la glibc, `env -i ./radixox` démarre ;
3. débit sans pipeline et pipeline 32 dans le bruit du binaire glibc (mesure §6) ;
4. `tests/memory.rs` passe (tout le résident compté) ;
5. README : section *Portable and static builds* mise à jour (commande, coût mesuré).

## 1. Ce qui est déjà fait (ne pas refaire)

Tout est sur `main`, poussé :

| commit | contenu |
|---|---|
| `5115fdb`, `5faeb2e`, `9bb2172` | appels système par `rustix` dans radixox-lib et radixox ; `signalfd4` et `mincore` par la crate `sc` |
| `f64daa3` | radixox-lib `#![no_std]` + `alloc` (feature `std` par défaut) ; module `radixox_lib::sys` |
| `ec76677` | valart `#![no_std]` + `alloc` |
| `7925f49` | oxidart `#![no_std]` + `alloc` ; trait `load::Source` ; `load_file` et `compress` (zstd) sous `std` |
| `61148dc` | crate `radixox-mem` : `memcpy`/`memmove`/`memset`/`memcmp`/`bcmp` en Rust (`#![no_builtins]`, noms C sous la feature `export`) |

Vérification des bibliothèques (doit rester verte) :

```bash
cargo build -p radixox-lib -p valart -p oxidart --no-default-features --features oxidart/regex
cargo tree -p oxidart --no-default-features --features regex -e normal,features | grep -E 'feature "std"|libc|zstd'   # vide
```

`radixox_lib::sys` fournit déjà, sans std ni libc ni allocation : `map_anon`, `unmap`,
`advise`, `remap`, `stderr(&[u8])`, `eprint(fmt::Arguments)` + macro `rx_eprintln!`,
`abort()`, `Instant` (API de `std::time::Instant`), `wall_secs()` (`CLOCK_REALTIME`),
`Once` (`call_once`, `is_completed`), `env(name) -> Option<&'static str>` et
`env_parse(name, default)` (lus une fois dans `/proc/self/environ`).

`radixox-mem`, mesuré contre la glibc (`taskset -c 2 cargo run --release -p radixox-mem
--example vs_libc`, ns/appel, tailles aléatoires 1..=128) : memcpy 2,08 / 2,44, memset
1,85 / 2,07, memcmp 2,78 / 2,69, bcmp 2,87 / 2,69. Au-delà de 1 Kio, memcmp / bcmp sont
1,5 à 1,8× plus lents (la glibc passe en AVX-512) : acceptable pour l'instant, à noter.

Essai déjà fait (`/tmp`, jeté) : un binaire `#![no_std] #![no_main]` avec `_start` en
`global_asm!`, lié avec `-nostartfiles -nostdlib -static`, tourne (2,3 Ko). Sans fonctions
mémoire, l'édition de liens échoue sur `memcpy` / `memset` : c'est ce que `radixox-mem`
règle.

## 2. Décisions prises avec l'utilisateur (ne pas rediscuter)

- **Rust stable**, pas de nightly, pas de `-Zbuild-std`.
- **Pas d'Eyra / origin / c-scape** (« se traîner un vieux re-impl de std ») : vrai `no_std`.
- **Réimplémenter le moins possible de la libc** : appels système par `rustix`, trous par `sc`
  (déjà en dépendance) ; n'écrire soi-même que ce qu'aucune crate stable ne fournit (cas des
  fonctions mémoire). Pas de couche « mini-std » (`Path`, `File`…) : `rustix` directement,
  chemins en `String`.
- **zstd (C) reste** : il ne lui faut que `malloc`/`calloc`/`free` (le binaire les exporte
  déjà, module `c_malloc` de `uringloop.rs`), `memcpy`/`memmove`/`memset` (radixox-mem) et
  `__stack_chk_fail` (compiler zstd avec `-fno-stack-protector`, ou une fonction qui
  appelle `sys::abort`).
- Notre propre anneau io_uring sur `rustix::io_uring` à la place de la crate `io-uring`
  (qui exige `std`).

## 3. Règles du projet (rappel, voir CLAUDE.md et la mémoire)

- Tests en mono-thread (`RUST_TEST_THREADS=1`, statics globaux).
- **Commit + push sur `main` à chaque étape verte** (tests passés), message en français,
  détaillé, chiffres compris ; terminer par la ligne `Co-Authored-By` demandée par le
  harnais. Pas de branche sauf essai jetable.
- Commentaires et docs en français, densité du code alentour ; mettre à jour la doc au fil
  de l'eau (ce fichier, CLAUDE.md, README).
- **Mesurer avant d'affirmer.** Benchs courts (≤ 30 s par passage), sous
  `systemd-run --user --scope -q -p MemoryMax=3G`, gouverneur `performance`, serveur
  épinglé (`RADIXOX_CPU_PIN=0`), memtier sur 3-5,9-11 avec `--distinct-client-seed
  --randomize`, **débit compté côté serveur** (delta de `total_commands_processed` sur
  une fenêtre de 4 s), **ordre ABBA** (une version toujours en second biaise de ~2 %).
  Script de référence : §6.
- Les tests d'intégration laissent leurs serveurs vivants (`std::mem::forget(child)`, tués
  au run suivant par `fuser -k`) : après une suite, tuer ceux des ports 16xxx avant de
  mesurer (`for p in $(pgrep -x radixox); do tr '\0' ' ' </proc/$p/environ | grep -qE
  "RADIXOX_PORT=16[0-9]{3}" && kill $p; done`).
- `release` = `panic = "abort"` : aucune donnée client ne doit pouvoir provoquer un panic.

## 4. Étapes

Chaque étape : tests verts, mesure si le chemin chaud est touché, commit + push.

### Étape 1 — crate `radixox` (lib) en `#![no_std]` + `alloc`

Même recette que les trois autres crates (voir leurs `lib.rs`) :

- `#![no_std]`, `extern crate alloc;`, `#[cfg(any(feature = "std", test))] extern crate std;`,
  module `prelude` (`Box`, `String`, `ToString`, `ToOwned`, `Vec`, `vec!`, `format!`) et
  `use crate::prelude::*;` dans chaque module ;
- modules de test : `use crate::prelude::*;` + `use std::{dbg, eprintln, print, println,
  thread_local};` (**pas** `use std::prelude::rust_2024::*` : ambiguïté de `panic!` avec
  `core`) ; ces imports vont **après** les `//!` d'un module (sinon E0753) ;
- réécriture mécanique `std::{ptr,mem,ops,cell,…}` → `core::`, collections → `alloc::` ;
- feature `std` par défaut dans `radixox/Cargo.toml` ; dépendances de workspace en
  `default-features = false` (déjà le cas pour radixox-lib et valart ; faire de même pour
  oxidart) ;
- `io-uring` reste en dépendance jusqu'à l'étape 2 : la lib n'en dépend pas
  (`uring/` = cœur **sans I/O**, `uring/tests.rs` = faux noyau), seul `bin/uringloop.rs`
  l'utilise.

Inventaire (hors `uring/tests.rs`, qui garde `std`) :

| fichier | fs | env | print | net | Instant/SystemTime | thread | sync | io:: |
|---|---|---|---|---|---|---|---|---|
| `bin/uringloop.rs` | 0 | 38 | 20 | 3 | 9 | 0 | 2 | 29 |
| `exec/aof.rs` | 12 | 0 | 12 | 0 | 10 | 0 | 0 | 28 |
| `exec/persist.rs` | 6 | 2 | 1 | 0 | 4 | 0 | 0 | 11 |
| `exec/bgsave.rs` | 2 | 3 | 3 | 0 | 6 | 0 | 0 | 7 |
| `exec/fs_task.rs` | 7 | 1 | 0 | 0 | 0 | 0 | 0 | 5 |
| `exec/autosave.rs` | 0 | 2 | 0 | 0 | 14 | 0 | 0 | 0 |
| `uring/core.rs` | 0 | 2 | 23 | 1 | 20 | 0 | 1 | 1 |
| `uring/core/repl.rs` | 6 | 0 | 16 | 1 | 5 | 0 | 0 | 4 |
| `uring/core/block.rs`, `uring/conn.rs` | | | | | 9 | | | |
| `tsc.rs` | 1 | 0 | 0 | 0 | 7 | 1 | 1 | 0 |
| `exec/scan.rs`, `auth.rs`, `client.rs`, `jobs.rs`, `mod.rs` | 1 | 2 | 2 | | | | 5 | |

En tout : 32 `Path`/`PathBuf`, 41 `.join(`, 27 `File`, 29 opérations de fichiers
(`read_to_string`, `create_dir_all`, `rename`, `remove_file`, `metadata`…), 70 `io::Result`
/ `io::Error`, 4 `thread_local!` (`exec/mod.rs` ×2, `exec/scan.rs`, `uring/core.rs`),
5 `OnceLock` (`tsc.rs`, `auth.rs`, `uring/core.rs`). Aucune fonction flottante absente de
`core` (`ln`, `powf`, `floor`… : vérifié, zéro).

Remplacements :

- **Horloges** : tout `std::time::Instant` → `radixox_lib::sys::Instant` (le cœur passe
  aujourd'hui `sys::Instant::now()` aux API de `pool` / `large` en plus de son `self.now`
  std : unifier sur `sys::Instant`) ; `SystemTime` → `sys::wall_secs()` (ou une
  `sys::wall_nanos()` à ajouter si la précision sert) ; `tsc.rs` : `thread::sleep` →
  `rustix::thread::nanosleep`, `/proc/cpuinfo` lu par `rustix`.
- **`thread_local!`** → `static mut` (motif prendre-remettre, comme `oxidart/src/krange.rs`
  `SCRATCH`) ou `Cell` dans un `static` avec `unsafe impl Sync` documenté mono-thread.
- **`OnceLock<T>`** → `radixox_lib::sys::Once` + `static mut MaybeUninit<T>` (comme
  `oxidart/src/value.rs` `SLAB_INIT`), ou un petit `Lazy` dans `sys` si plusieurs sites le
  veulent.
- **Environnement** → `radixox_lib::sys::env` / `env_parse`. **Piège** : `/proc/self/environ`
  ne voit pas un `set_var` ultérieur, or `uring/tests.rs` fait `set_var("RADIXOX_DIR", …)`
  avant des appels qui relisent `RADIXOX_DIR` (`exec/persist.rs:19`, à chaque appel). Faire
  lire la configuration **une fois au démarrage** et la passer (champ de `Config`), ou
  garder `std::env` sous `cfg(feature = "std")` pour ces tests. Choisir la première si elle
  reste simple.
- **Fichiers** : `rustix::fs` (`open`, `openat`, `mkdirat`, `renameat`, `unlinkat`, `fstat`,
  `read`, `pwrite`, `fsync`…) sur `OwnedFd` ; chemins en `String` (`format!("{dir}/{name}")`)
  ; `rustix::fs::open` prend tout `impl rustix::path::Arg` (`&str`, `&CStr`). Lire un
  fichier entier : boucle `rustix::io::read` dans un `Vec`.
- **Rechargement du dump** : `oxidart::load::Source` est implémenté pour tout `std::io::Read`
  sous `std` et pour `&[u8]` sans. Pour le binaire no_std : l'exporter
  (`pub use crate::load::Source` dans oxidart, le module `load` est privé) et l'implémenter
  pour un lecteur de fichier `rustix` (dans radixox, sur un type à nous : règle de l'orphelin).
- **Compression zstd** (`oxidart/src/compress.rs`, sous `std` aujourd'hui) : la rendre
  `no_std` — `zstd-safe` fonctionne sans sa feature `std` ; `ZstdReader` doit lire un
  `Source` au lieu d'un `std::io::Read`, `ZstdStream` n'utilise que des tranches. Garder le
  format exact (une frame zstd, `zstd -d dump.rxd` doit marcher) et `tests/bgsave.rs` vert.
- **Erreurs** : `io::Error` → `rustix::io::Errno` là où c'est un appel système ; un enum à
  nous là où le message compte (`Display` par `core::fmt`). `format!` reste dispo (`alloc`).
- **Sorties** : `println!` / `eprintln!` → `rx_eprintln!` et un `rx_println!` à ajouter dans
  `radixox_lib::sys` (stdout, même tampon de 512 o sans allocation ; un message plus long est
  tronqué : vérifier qu'aucune sortie ne dépasse, ou faire un tampon `Vec`).
- **Réseau** : `TcpListener` (écoute dans `uringloop.rs`) → `rustix::net::socket_with`,
  `bind`, `listen`, `sockopt::set_socket_reuseaddr` ; `std::net::SocketAddr` existe dans
  `core::net` (stable) : garder le type, parser `RADIXOX_BIND` avec `core::net`.
- **`HashMap`** → `hashbrown` (déjà en dépendance d'oxidart ; l'ajouter à radixox).

### Étape 2 — notre anneau io_uring sur `rustix::io_uring`

Remplacer la crate `io-uring` dans `bin/uringloop.rs` (seul utilisateur) par un module à
nous, de même forme d'API pour garder le diff de `uringloop.rs` lisible. Surface utilisée
aujourd'hui (relevée dans `uringloop.rs`) :

- mise en place : `setup_single_issuer`, `setup_cqsize`, `setup_sqpoll(idle)` +
  `setup_sqpoll_cpu`, ou `setup_defer_taskrun` + `setup_taskrun_flag` (incompatibles avec
  SQPOLL) ; message d'erreur actuel à garder (noyau ≥ 6.12, seccomp de Docker) ;
- opcodes : `Read`, `Write` (×3), `Send` (`MSG_WAITALL | MSG_NOSIGNAL`), `Recv`,
  `RecvMulti` (sélection de tampon), `AcceptMulti`, `Connect`, `Close`, `AsyncCancel`,
  `OpenAt`, `Fsync` (`DATASYNC`), `Fallocate`, `Ftruncate`, `RenameAt`, `UnlinkAt` ;
  fichiers fixes (`Fixed(slot)`) et `Fd(raw)` ;
- drapeaux de SQE : `ASYNC`, `IO_LINK`, `SKIP_SUCCESS`, `BUFFER_SELECT` ;
- CQE : `more`, `buffer_select`, `buffer_more` ;
- `enter(to_submit, min_complete, flags, Some(&SubmitArgs))` avec `GETEVENTS`, `EXT_ARG`
  (timeout de l'attente groupée, RUNTIME_ARCH §12.5), `SQ_WAKEUP`, `NO_IOWAIT` ;
- `register_files_sparse`, `register_files_update`, `register_buf_ring_with_flags`
  (`IOU_PBUF_RING_INC`, `BufRingEntry`), `register_iowq_max_workers`, `register_iowq_aff`
  (prend aujourd'hui un `libc::cpu_set_t` : passer des octets / un `rustix::thread::CpuSet`).

Points délicats : `mmap` des anneaux (`IORING_FEAT_SINGLE_MMAP`), têtes / queues en atomiques
Acquire / Release, réveil du thread SQPOLL (`IORING_SQ_NEED_WAKEUP` dans les drapeaux de
l'anneau SQ), débordement du CQ (`IORING_SQ_CQ_OVERFLOW`). `rustix::io_uring` fournit
`io_uring_setup` / `enter` / `register` et toutes les structures (`io_uring_sqe`,
`io_uring_cqe`, `io_uring_params`, `io_uring_buf_ring`…). La crate io-uring
(`~/.cargo/registry/src/*/io-uring-0.7.15/src`) sert de référence de comportement.

Validation : toute la suite d'intégration, puis A/B ABBA contre le commit précédent
(sans pipeline et pipeline 32, avec et sans `RADIXOX_SQPOLL`, voir README *Configuration*).

### Étape 3 — le binaire no_std

- Nouvelle cible binaire (ou feature `nostd` du binaire existant) `#![no_std] #![no_main]`.
- `_start` en `global_asm!` (x86-64) : lire `argc` / `argv` / `envp` sur la pile, aligner
  `rsp` sur 16 avant l'appel, appeler une fonction Rust, sortir par `exit_group`.
  `radixox_lib::sys::env` lit déjà `/proc/self/environ` : `envp` n'est pas indispensable.
- `#[panic_handler]` : message (`sys::eprint`) puis `sys::abort()`.
- `#[global_allocator]` = `radixox_lib::raw_alloc::heap::RxAlloc` (déjà le cas) ; exporter
  `malloc` & co. (module `c_malloc`, déjà là) pour zstd ; `radixox-mem` avec `export`.
- Édition de liens par `build.rs` avec `cargo:rustc-link-arg-bins=` (`-nostartfiles`,
  `-nostdlib`, `-static`). **Pas** par `RUSTFLAGS` : ils s'appliqueraient aussi aux scripts
  de build (celui de rustix plante en SIGSEGV). Le binaire obtenu n'est pas PIE (pas d'ASLR) :
  le noter ; un static-pie demanderait de traiter nous-mêmes les relocations.
- zstd : `CFLAGS` sans protection de pile (`-fno-stack-protector`, via `[env]` de
  `.cargo/config.toml` ou le `build.rs` de zstd-sys) ou fournir `__stack_chk_fail`.
- Signaux sans libc : masque par `sc` (`rt_sigprocmask`, masque noyau de 8 octets) ; le
  « second signal = sortie immédiate » (`exit_on_second_signal`) peut se faire **sans
  gestionnaire** : débloquer SIGINT / SIGTERM, l'action par défaut termine le processus
  (code de sortie différent de `_exit(130)` : vérifier `tests/shutdown.rs`). Un vrai
  gestionnaire demanderait `SA_RESTORER` et un trampoline `rt_sigreturn` en assembleur :
  à éviter.
- Pas de TLS dans le binaire no_std (aucun `thread_local!` côté no_std ; le thread
  propriétaire du tas est déjà sous `std` dans radixox-lib).

### Étape 4 — mesures et doc

- A/B ABBA binaire glibc (std) contre binaire no_std : sans pipeline, pipeline 32 ; RSS à
  vide et chargé ; `tests/memory.rs`. Si le pipeline perd (cf. musl −13 %), profiler
  (`perf record` sur le profil `profiling`) avant de conclure : `radixox-mem` est le premier
  suspect (memcmp / bcmp > 1 Kio).
- README (*Portable and static builds*), CLAUDE.md (commande de build), ce fichier
  (état final), mémoire de projet.

## 5. Faire tourner les tests d'intégration contre le binaire no_std

`radixox/tests/common/mod.rs` lance `env!("CARGO_BIN_EXE_radixox")`. Options : une variable
d'environnement (`RADIXOX_TEST_BIN`) qui remplace ce chemin, ou faire du binaire no_std le
binaire `radixox` sous une feature. Ne pas dupliquer les tests.

## 6. Mesure de référence (script)

```bash
S=<dossier temporaire>; A=<binaire avant>; B=<binaire après>
mt() { local p=$1; shift; taskset -c 3-5,9-11 memtier_benchmark -s 127.0.0.1 -p 6610 --protocol=redis \
  --threads 6 --clients 10 --pipeline $p -d 100 --key-prefix key: --distinct-client-seed --randomize \
  --hide-histogram "$@" 2>/dev/null; }
tcp() { redis-cli -p 6610 info | tr -d '\r' | awk -F: '$1=="total_commands_processed"{print $2}'; }
srv() { mt $1 --ratio 1:4 --key-pattern R:R --key-minimum 1 --key-maximum 1000000 --test-time 6 >/dev/null & local m=$!
  sleep 1; local a=$(tcp) t0=$(date +%s%N); sleep 4; local b=$(tcp) t1=$(date +%s%N); wait $m
  echo $(( (b - a) * 1000000000 / (t1 - t0) )); }
for bin in $A $B $B $A; do rm -rf $S/bd && mkdir -p $S/bd
  systemd-run --user --scope -q -p MemoryMax=3G env -i RADIXOX_PORT=6610 RADIXOX_DIR=$S/bd \
    RADIXOX_CPU_PIN=0 RADIXOX_SAVE= $bin >$S/bd.log 2>&1 &
  for _ in $(seq 100); do redis-cli -p 6610 ping >/dev/null 2>&1 && break; sleep 0.05; done
  mt 64 --threads 1 --clients 1 --ratio 1:0 --key-pattern P:P --key-minimum 1 --key-maximum 1000000 --requests allkeys >/dev/null
  echo "$bin p1 $(srv 1) p32 $(srv 32)"; redis-cli -p 6610 shutdown nosave >/dev/null; sleep 0.5; done
```

Repères du 2026-10-29 (binaire glibc, 1 cœur, 7600X) : p1 ≈ 277 k ops/s, p32 ≈ 3,05 M.
Pour un binaire « avant », construire le commit voulu dans un `git worktree` du dossier
temporaire.

## 7. Pièges déjà rencontrés

- `tests/memory.rs` exige que tout le résident soit compté (non compté < 1 Mo) : toute
  nouvelle plage `mmap` écrite doit être comptée (registre des pages, MEMORY_ARCH §10.4) ou
  rester minuscule ; sous THP `always`, la première écriture d'une grande plage prend
  2 Mio d'un coup (`MADV_NOHUGEPAGE` si elle n'en a pas besoin).
- Une feature de dépendance de workspace non coupée (`default-features` absent dans
  `[workspace.dependencies]`) ramène `std` en silence : vérifier par `cargo tree -e features`.
- `foldhash`, `crc32fast` : `std` dans leurs features par défaut (déjà réglé dans valart et
  oxidart, refaire pour toute nouvelle dépendance).
- Le débit pipeliné bouge de ±2 à 3 % au gré de l'inlining de la LTO quand on modifie du code
  loin du chemin chaud (`5faeb2e` : −2,7 % non expliqué, +30 instructions / commande) : ABBA
  et, en cas d'écart, `perf stat` instructions / cycles par commande avant toute conclusion.
- `HiSlab::new` prend `initial_capacity` mais ne précharge rien (l'ancien `madvise`
  combinait deux conseils, invalide) : décision en attente chez l'utilisateur, ne pas
  « corriger » au passage.
- Avec 8 slots annoncés (`ulimit -n 40`), certaines des 8 premières connexions sont déjà
  refusées : antérieur au chantier, à part.
