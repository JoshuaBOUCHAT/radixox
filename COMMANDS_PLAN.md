# Couverture des commandes Redis — plan de travail

Document de passation (2026-10-30) : de quoi implémenter les commandes Redis manquantes
sans autre contexte. Lire d'abord `CLAUDE.md` et `ARCHITECTURE.md`. Rien n'est commencé.

## 0. Objectif

Passer de **114 commandes Redis** (2026-10-30, comptées dans `dispatch_inner` de
`radixox-lib/src/cmd.rs`, hors `KRANGE` `KREVRANGE` `REPLICATE` et les `RX*` internes)
à **~180**, en trois lots : ~38 petites commandes sur l'existant, ~22 plus lourdes,
puis les bitmaps (7). Pouvoir écrire dans le README « 180 commandes Redis » et que ce soit
vrai à la lettre : mêmes réponses, mêmes erreurs, mêmes cas limites que Redis 7.

Avant de commencer : recouper la liste ci-dessous avec la référence (`COMMAND DOCS` d'un
`valkey-server` local, ou redis.io/commands). La liste a été faite de mémoire ; une
commande déjà présente ou un oubli ne doit pas surprendre.

## 1. Chemin d'une commande dans le code (vérifié sur `PERSIST`, 2026-10-30)

| Étape | Fichier | Quoi |
|---|---|---|
| 1 | `radixox-lib/src/cmd.rs`, `enum Cmd` | une variante (arguments empruntés `&'a [u8]`, valeurs à garder en `SharedByte`) |
| 2 | idem, `dispatch_inner` | le parse : `exact!(n)` / `at_least!(n)`, `int()` / `uint()`, options insensibles à la casse, `CmdError::Syntax` sinon |
| 3 | idem, `is_write`, `write_weight`, `main_key` | écriture ? (AOF, réplication, mode suivi, `maxmemory`, saves auto), poids pour l'admission `maxmemory`, clé unique pour le préchargement |
| 4 | `radixox/src/exec/journal.rs`, `plan` | journalisation : `Verbatim` (rejouable telle quelle), `IfChanged` (seulement si elle a changé quelque chose), ou réécrite (non déterministe : voir `SPop` → `SRem`). **Une commande dont le résultat dépend de l'aléa ou d'un calcul flottant doit être journalisée par son effet** |
| 5 | `radixox/src/exec/mod.rs` (`execute_into`) et le module de la famille (`string.rs`, `hash.rs`, `sset.rs`, `zset.rs`, `list.rs`) | l'exécution ; chemin AMAC (`hint_keys` / `execute_hinted`) seulement pour les commandes chaudes à une clé |
| 6 | `oxidart/src/…`, `valart/` | la primitive sur l'arbre si elle manque ; toute mutation passe par la barrière de save (SAVE_DESIGN.md) |
| 7 | `radixox/tests/<famille>.rs` | tests d'intégration contre le serveur, avec les cas limites et les erreurs exactes de Redis |
| 8 | `README.md`, section *Commands* | la commande dans le tableau, le compte mis à jour |

## 2. Règles (CLAUDE.md, mémoire du projet)

- **Aucun panic provoqué par un client** (`panic = "abort"` en release) : indices, offsets,
  tailles et flottants vérifiés.
- **Jamais bloquant** : une commande sur une valeur géante (opérations d'ensembles,
  `COPY`, `SORT`, `HRANDFIELD` avec un grand `count`…) ne fige pas la boucle. Écritures :
  détacher puis libérer par tranches ; lectures : par tranches, non atomiques entre
  tranches (mémoire `project_bounded_commands`, jobs de `exec/jobs.rs`).
- **`no_std`** : la lib `radixox` est `#![no_std]` + `alloc` (`crate::prelude`, `rustix`,
  `radixox_lib::sys`). Vérifier `cargo build -p radixox --lib --no-default-features`.
- **Deux binaires** : la suite d'intégration doit passer contre `radixox` et contre
  `radixox-nostd` (`cargo nostd` puis `RADIXOX_TEST_BIN=$PWD/target/release/radixox-nostd
  cargo test -p radixox --test '*'`), et avec `--features eviction`.
- Tests en mono-thread ; après une suite, tuer les serveurs restés sur les ports 16xxx.
- **Commit + push sur `main` par groupe vert**, message en français avec ce qui a été
  vérifié. Mesure ABBA (NOSTD_PLAN.md §6) seulement si le chemin chaud est touché (parse
  de `GET` / `SET`, `exec_window`).
- `COMMAND COUNT` répond 0 aujourd'hui (`exec/client.rs`, `cmd_command`) : le faire
  répondre le vrai compte fait partie du lot 1.

## 3. Lot 1 — petites commandes sur l'existant (~38)

### Strings
| Commande | Points à respecter |
|---|---|
| `APPEND key value` | crée la clé si absente ; rend la nouvelle longueur ; garde le TTL |
| `STRLEN key` | 0 si absente ; `WRONGTYPE` sur un autre type |
| `GETRANGE key start end` (+ `SUBSTR`, alias) | bornes incluses, indices négatifs depuis la fin, bornes ramenées dans la valeur, `""` si vide ou inversé |
| `SETRANGE key offset value` | complète avec des octets nuls ; `ERR offset is out of range` au-delà de 512 Mo ; valeur vide sur clé absente : 0 sans créer la clé ; rend la longueur |
| `GETSET key value` | ancienne valeur ; **retire le TTL** |
| `GETDEL key` | |
| `GETEX key [EX s \| PX ms \| EXAT t \| PXAT t \| PERSIST]` | journaliser l'effet sur le TTL en absolu (`PEXPIREAT`) ou `PERSIST` |
| `INCRBYFLOAT key incr` | garde le TTL ; erreurs `ERR value is not a valid float`, `ERR increment would produce NaN or Infinity` ; format de sortie de Redis (pas d'exposant, zéros de fin retirés, 17 chiffres significatifs) ; **journalisé comme `SET key <résultat> KEEPTTL`** (comme Redis) |
| `MSETNX k v [k v …]` | 1 si toutes posées, 0 si une seule existait (aucune posée) ; atomique |
| `PSETEX key ms value` | |

### Clés et TTL
| Commande | Points à respecter |
|---|---|
| `EXPIRE` / `PEXPIRE` : options `NX` `XX` `GT` `LT` | absentes aujourd'hui (`exact!(2)`) ; `GT` / `LT` : une clé sans TTL vaut l'infini |
| `EXPIREAT` / `PEXPIREAT key t [NX\|XX\|GT\|LT]` | date passée : la clé est supprimée (réponse 1) |
| `EXPIRETIME` / `PEXPIRETIME key` | -1 sans TTL, -2 absente |
| `TOUCH key [key …]` | nombre de clés existantes ; met à jour l'accès (LRU / LFU sous `eviction`) |
| `RANDOMKEY` | nil si vide ; descente aléatoire dans l'arbre, sauter les expirées ; journal : lecture |
| `RENAME src dst` | `ERR no such key` ; garde le TTL de `src` ; écrase `dst` quel que soit son type ; `src == dst` : OK ; gros types (sous-arbres valart, ZSet, List) : déplacer la poignée, ne pas copier |
| `RENAMENX src dst` | 0 si `dst` existe ; `src == dst` : 0 |

### Hash
| Commande | Points à respecter |
|---|---|
| `HSETNX key field value` | |
| `HSTRLEN key field` | |
| `HINCRBYFLOAT key field incr` | mêmes règles que `INCRBYFLOAT` ; journalisé comme `HSET key field <résultat>` |
| `HRANDFIELD key [count [WITHVALUES]]` | `count` négatif : avec répétitions ; positif : distincts, au plus la taille ; borné (voir §2) |
| `HSCAN key cursor [MATCH p] [COUNT n] [NOVALUES]` | reprendre l'infrastructure de curseurs de `ZSCAN` (`exec/scan.rs`, SCAN_DESIGN.md) |

### Set
| Commande | Points à respecter |
|---|---|
| `SMISMEMBER key m [m …]` | tableau de 0 / 1 |
| `SRANDMEMBER key [count]` | comme `HRANDFIELD` pour le signe de `count` |
| `SMOVE src dst member` | `WRONGTYPE` si `src` ou `dst` n'est pas un Set ; 0 si absent de `src` |
| `SSCAN key cursor [MATCH p] [COUNT n]` | comme `HSCAN` |

### Serveur et Pub/Sub
| Commande | Points à respecter |
|---|---|
| `TIME` | `[secondes, microsecondes]` en bulk strings |
| `FLUSHALL [ASYNC\|SYNC]` | = `FLUSHDB` (une seule base) |
| `ROLE` | maître : `["master", offset, [[ip, port, offset], …]]` ; réplica : `["slave", host, port, état, offset]` (état de `uring/core/repl.rs`) |
| `PSUBSCRIBE` / `PUNSUBSCRIBE` | motifs glob de Redis (`stringmatchlen` : `*` `?` `[…]` `\`) ; message `["pmessage", motif, canal, msg]` ; mêmes compteurs d'abonnements que `SUBSCRIBE` |
| `PUBSUB CHANNELS [motif]` / `NUMSUB [canal …]` / `NUMPAT` | depuis le registre Pub/Sub du cœur |
| `COMMAND COUNT` / `COMMAND LIST` | le vrai compte et la vraie liste (une table unique à côté de `dispatch_inner`, pour qu'elle ne dérive pas) |

## 4. Lot 2 — plus lourdes (~22)

| Commande | Points à respecter |
|---|---|
| `SINTER` `SUNION` `SDIFF` + `SINTERSTORE` `SUNIONSTORE` `SDIFFSTORE` + `SINTERCARD numkeys k … [LIMIT n]` | la logique existe côté ZSet (`ZUNION` `ZINTER` `ZDIFF`) ; bornées (§2) ; `…STORE` sur une destination d'un autre type l'écrase |
| `ZRANGESTORE dst src min max [BYSCORE\|BYLEX] [REV] [LIMIT o c]` | réutiliser le parse de `ZRANGE` |
| `ZMPOP numkeys k … MIN\|MAX [COUNT n]`, `LMPOP numkeys k … LEFT\|RIGHT [COUNT n]` | première clé non vide ; réponse `[clé, [éléments]]` |
| `BZMPOP` / `BLMPOP timeout numkeys …` | sur l'infrastructure des commandes bloquantes (connexions parquées, RUNTIME_ARCH.md §7, `uring/core/block.rs`) ; journal `Blocking` comme `BLMOVE` |
| `COPY src dst [DB n] [REPLACE]` | copie profonde pour Hash / Set / ZSet / List (pas de partage de poignée) ; `DB` ≠ 0 : erreur ; bornée (§2) |
| `OBJECT ENCODING\|FREQ\|IDLETIME\|REFCOUNT\|HELP` | encodages : nommer nos formes (inline, valart, colonnes, B+tree…) ; `FREQ` / `IDLETIME` selon la politique (`eviction`) |
| `MEMORY USAGE key [SAMPLES n]` | depuis la comptabilité de l'allocateur (MEMORY_ARCH.md) |
| `SORT` / `SORT_RO key [BY p] [LIMIT o c] [GET p …] [ASC\|DESC] [ALPHA] [STORE dst]` | `BY` / `GET` vont lire d'autres clés : borné, et `SORT` avec `STORE` est une écriture |
| `MULTI` `EXEC` `DISCARD` `WATCH` `UNWATCH` | voir §6 : sémantique Redis exacte |

## 5. Lot 3 — bitmaps (7)

Des strings, pas un nouveau type (comme Redis) : bits numérotés depuis le bit de poids fort
du premier octet.

| Commande | Points à respecter |
|---|---|
| `SETBIT key offset 0\|1` | agrandit avec des zéros ; offset < 2³² (`ERR bit offset is not an integer or out of range`) ; rend l'ancien bit |
| `GETBIT key offset` | 0 hors de la valeur |
| `BITCOUNT key [start end [BYTE\|BIT]]` | popcount (SIMD : voir `radixox-mem` et le SWAR d'oxidart) |
| `BITPOS key bit [start [end [BYTE\|BIT]]]` | cas limites de Redis : chercher un 0 sans `end` dans une valeur pleine rend la longueur en bits |
| `BITOP AND\|OR\|XOR\|NOT dst k …` | valeurs de longueurs différentes complétées par des zéros ; borné (§2) |
| `BITFIELD key [GET t o] [SET t o v] [INCRBY t o i] [OVERFLOW WRAP\|SAT\|FAIL] …` / `BITFIELD_RO` | types `i1`…`i64`, `u1`…`u63`, offsets `#n` ; le plus long des sept |

## 6. Décisions déjà prises pour la suite (session du 2026-10-30)

- **`MULTI` / `EXEC` / `DISCARD` / `WATCH` / `UNWATCH`** : la sémantique de Redis à la
  lettre (`+QUEUED`, `-EXECABORT` sur erreur de syntaxe à la mise en file, **pas
  d'annulation** sur erreur à l'exécution, `WATCH` = clés marquées modifiées à chaque
  écriture, test d'un drapeau « un WATCH actif » d'abord pour ne rien coûter au chemin
  chaud). Un `EXEC` part dans le flux AOF / réplication en un seul bloc.
- **La syntaxe `MULTI ATOMIC` de TX_DESIGN.md est abandonnée** au profit de **commandes
  custom en Rust, liées statiquement** (le serveur est une lib : `radixox::server`), en
  trois niveaux : une fonction (tout-ou-rien : écritures mises de côté par `Ctx`,
  appliquées si `Ok`), un parcours filtré fourni (l'utilisateur ne donne que le filtre),
  une machine à états (`trait Job`, une tranche par tour, connexion parquée, reprise sur
  une clé, atomique par tranche seulement). Journaliser les **effets**, pas la commande
  (un réplica n'a pas besoin du même binaire). TX_DESIGN.md est à réorienter dans ce sens
  avant d'implémenter ; ce sera après les lots 1 à 3.
