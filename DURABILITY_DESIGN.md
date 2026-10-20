# Durabilité — AOF et réplication par un seul flux de commandes

> **État : étapes 1–3 faites (2026-10-20)** : `exec/journal.rs` (plan / finish, format de
> bloc, `Stream` en mémoire, rejeu `apply`), plomberie dans `Core::exec_window`, tests 1 et 2 ;
> dump v2 (position du journal, condensé `oxidart/src/digest.rs` vérifié au reload), test 3 ;
> `exec/aof.rs` (manifeste, segments préalloués, reprise `recover`, création `create`,
> nettoyage), test 4. Le flux n'est pas encore écrit sur disque pendant le service
> (`Config::journal`, désactivé dans le binaire, pas de `RADIXOX_APPENDONLY`). Suite : étape 4. Ce document doit suffire à
> un agent qui ne connaît pas le projet : §1 donne le contexte du code, §2–§11 la conception,
> §12 les tests, §13 l'ordre de travail avec ses critères d'acceptation. Il remplace le
> « collecteur d'effets » de RUNTIME_ARCH.md §18 et complète ARCHITECTURE.md §3.
> Les saves automatiques (faites) sont décrites dans SAVE_DESIGN.md, « Saves automatiques ».
>
> Langue des documents et des commentaires du dépôt : **français**. README : anglais.

---

## 0. Décisions, en une page

| # | Décision | Pourquoi |
|---|---|---|
| D1 | **Un seul flux** sert l'AOF (disque) et la réplication (réseau) : mêmes blocs, mêmes octets | un seul mécanisme à écrire, tester et prouver ; un réplica s'alimente depuis le tampon de l'AOF sans transformation |
| D2 | On journalise **les commandes reçues** (octets RESP de la frame), pas leurs effets | les fonctions de commande (`exec/*.rs`) ne changent pas ; à état et horloge égaux, nos commandes sont déterministes |
| D3 | **Un bloc par tour** de boucle (découpé si trop gros), qui porte le `now` d'oxidart du tour | le rejeu remet la même horloge : TTL relatifs et expirations paresseuses rejouent à l'identique ; **les expirations ne sont pas journalisées** |
| D4 | Une **courte liste de commandes non déterministes** est réécrite en effets (`SPOP` → `SREM`) | le hasard ne se rejoue pas |
| D5 | Toute commande qui dure plusieurs tours a un **point de linéarisation** et n'est journalisée qu'une fois, à ce point | pas besoin de journaliser ses tranches |
| D6 | Le snapshot est **exact** : prouvé par un oracle, vérifié en production par un **condensé du contenu** | rejouer des commandes suppose un état de départ identique sur tout ce qui compte |
| D7 | **Réplica « suivi »** : un serveur normal, poussé par le primaire (qui s'y connecte comme un client), avec un seul mode qui change trois choses (horloge pilotée par le flux, écritures des autres clients refusées, un acquittement par bloc) | presque aucun code propre au réplica ; promotion = sortir du mode |
| D8 | **Un tampon, N+1 opérations** : le bloc est écrit une fois en mémoire puis soumis au disque et à chaque réplica (io_uring) | coût par réplica = des SQE, pas de copie ni d'encodage sur le thread principal |
| D9 | `appendfsync always` en **commit groupé** : réponses retenues jusqu'au fsync qui les couvre, un seul fsync en vol, la boucle ne bloque jamais | durabilité stricte au débit d'`everysec` sur un disque à fsync rapide ; le même marquage sert à `WAIT` |
| D10 | **Un seul CRC partout** : CRC32 IEEE (`crc32fast`, déjà utilisé par le dump) | pas de girouette |
| D11 | Avec l'AOF actif, **tout BGSAVE est une réécriture** (nouvelle base) | une seule notion de snapshot |

Le prix assumé de D2 : rejouer des commandes, c'est rejouer **le code de la version qui
relit**. Le flux porte une version de sémantique (§6.1) ; une version incompatible refuse de
démarrer (§9).

---

## 1. Contexte du code (à lire avant tout)

### 1.1 Modèle d'exécution

- **Un processus = un thread = une base.** Pas de verrou, pas d'atomique. Linux ≥ 6.12,
  io_uring sans runtime async. Voir ARCHITECTURE.md et RUNTIME_ARCH.md.
- **Cœur sans I/O** : `radixox/src/uring/core.rs` (`Core`). Il reçoit des `Completion` et émet
  des `Op` ; le binaire `radixox/src/bin/uringloop.rs` (`Server`) les traduit en CQE / SQE. Les
  tests pilotent le cœur avec un faux noyau : `radixox/src/uring/tests.rs` (`Fake`).
- **Un tour de boucle** (`Server::run`, `uringloop.rs`) :
  1. `core.art.tick()` : **horloge murale d'oxidart, `art.now`, en secondes (`u64`)**, lue
     une seule fois par tour ; `core.set_now(Instant::now())` : horloge monotone des timers ;
  2. `harvest()` : CQE → `core.complete(..)` ;
  3. `core.fire_timers()` : évicteur TTL (`Timer::Evict`, 1 s), balayage d'inactivité,
     saves automatiques (`Timer::AutoSave`, 100 ms) ;
  4. `core.run_batch()` par tranches : exécution des commandes (`Core::exec_window`) ;
  5. `core.run_jobs()` si du travail de fond est dû (jobs, tranches de BGSAVE sous plancher) ;
  6. `core.flush()`, `submit_ops()` ;
  7. `idle_work()` : travail de fond tant que `IORING_SQ_TASKRUN` est à 0 (RUNTIME_ARCH §8.2) ;
  8. `wait()` (au repos) ou `submit()`.
- **Exécution d'une commande** : le parseur (`radixox-lib/src/cmd.rs`, `Cmd::parse` →
  `Parse::Done(cmd, n)`, `n` = longueur de la frame) emprunte les octets reçus ;
  `Core::exec_window` → `run_cmd` → `exec::execute_into` (`radixox/src/exec/mod.rs`). Les
  écritures passent par `execute_sync`, qui **rend une `Frame` avant son encodage**. Les
  commandes qui dépendent du serveur (`BGSAVE`, `SAVE`, `KEYS`, `UNLINK`, `FLUSHDB`, `INFO`,
  `LASTSAVE`, Pub/Sub) rendent `Executed::Runtime` → `After::*`, traité dans
  `Core::finish_frame` **après** l'exécution de toute la fenêtre.
- **Fenêtres** : `stops_window` arrête une fenêtre après une barrière, `KEYS`, `UNLINK`,
  `BGSAVE`, `SAVE`, `PUBLISH`. Une commande de ce type est donc **la dernière exécutée de sa
  fenêtre**, et son `finish_frame` passe avant la fenêtre suivante.
- **Point d'accroche des écritures** : dans `exec_window`, juste avant `run_cmd`, le cœur
  compte déjà les écritures pour les saves automatiques (`Cmd::write_weight`,
  `persist.note_writes`) et refuse les écritures après un BGSAVE en échec (`MISCONF`). Le
  journal s'accroche **au même endroit**.

### 1.2 Temps et TTL

- oxidart stocke des **échéances absolues en secondes** (`exp = now + ttl.as_secs()`,
  `oxidart/src/lib.rs`) ; `expired(exp, now)`. `PX`/`PEXPIRE` sont arrondis à la seconde.
- `art.set_now(n)` existe (utilisé par les tests) : c'est ce que le rejeu utilise.
- Expiration **paresseuse** (une lecture ou une écriture voit une clé expirée comme absente et
  peut la supprimer) et **active** (`evict_expired`, échantillonnage, en pause pendant une save).

### 1.3 Saves

- `BGSAVE` : `exec/bgsave.rs` (`BgSave`), tranches dans la boucle, **pas de `fork`** : barrière
  copy-on-write applicative (`radixox-lib/src/save_barrier.rs`, `OxidArt::begin_save` fige
  `save.t0 = art.now`). Le T0 est posé **dans `finish_frame` du `BGSAVE`**, c'est-à-dire juste
  après la commande `BGSAVE` dans l'ordre d'exécution (§1.1, fenêtres). Une save automatique
  démarre dans `fire_timers`, au début d'un tour.
- Fichiers : `RADIXOX_DIR/dump.rxd.tmp` → écritures (une en vol, `Op::FileWrite`, CQE
  `Completion::File`, `Core::on_file`) → `fdatasync` (`Op::FileSync`) → `rename` → fsync du
  dossier. `RADIXOX_FILE_CPUS` met ces écritures sur un worker io-wq épinglé hors du cœur.
- Format du dump (`oxidart/src/save.rs`, `mod format`, version 1) :
  `"RXDB" | u8 version | u8 unité TTL | u64 LE t0 | u64 LE graine valart`, section ZSet/List,
  enregistrements triés (DFS), `T_END`, pied `u32 LE crc32 IEEE | "RXDE"`. **Les clés expirées
  au T0 ne sont pas écrites** ; les TTL sont des écarts au T0.
- Rechargement : `exec/persist.rs::load_at_startup` ; dump illisible = refus de démarrer.
- Saves automatiques, `LASTSAVE`, `INFO persistence` : `exec/autosave.rs` (`Persistence`).

### 1.4 Jobs (travail étalé)

- `conn_jobs` : `KEYS` et `UNLINK` longs (la connexion est parquée, le job répond) ;
  `jobs` : libération de `FLUSHDB` ; `bgsave`. Étapes : `run_jobs` (pendant le tour) et
  `idle_step` (au repos). `UNLINK prefix*` et `FLUSHDB` **détachent** leur sous-arbre d'un coup :
  les clés disparaissent immédiatement, la libération qui suit est physique.

### 1.5 Tests et conventions

- `RUST_TEST_THREADS=1` (`.cargo/config.toml`) : les slabs sont des statics globaux.
- `panic = "abort"` en release : **aucune donnée client ne doit pouvoir provoquer un panic**
  (un flux de réplication ou un AOF corrompu non plus : erreurs, jamais `unwrap`).
- Oracles existants : `oxidart/src/test_save.rs::reload_roundtrip_random` (dump → rechargement
  → 300 mutations identiques sur les deux arbres → comparaison), `uring/tests.rs`
  (`random_cmd`, `oracle`, `random_streams_match_oracle`), tests différentiels oxidart contre
  `oxidart-legacy`. Intégration : `radixox/tests/` (`common::start_server_env`, qui lance le
  binaire avec `RADIXOX_SAVE=""`), `tests/bgsave.rs`.
- Configuration par variables `RADIXOX_*` (tableau du README, section *Configuration*) ; noms
  calqués sur Valkey.
- Benchs : courts, sous `systemd-run --scope -p MemoryMax=…`, profil CPU `performance`
  (CLAUDE.md, RUNTIME_ARCH §12.1). **Ne jamais écrire de dump ou d'AOF de bench sur `/tmp`**
  (tmpfs : fsync gratuit, mesures fausses).

---

## 2. Vocabulaire

| Terme | Sens |
|---|---|
| **flux** | suite ordonnée de blocs, identique sur disque et sur le réseau |
| **bloc** | commandes d'un tour (ou d'une partie de tour), avec en-tête : `seq`, `offset`, `now`, CRC |
| `seq` | numéro de bloc, `u64`, croissant de 1 en 1, jamais réutilisé dans une histoire |
| `offset` | octets du flux avant ce bloc (somme des tailles de blocs) ; sert à `WAIT` et au commit groupé |
| `replid` | identifiant d'**histoire** (20 octets aléatoires, affiché en hexadécimal) ; change à chaque promotion |
| `now` | `art.now` (secondes) du tour du bloc, monotone dans une histoire |
| **base** | dump qui contient exactement les blocs `≤ base_seq` |
| **segment** | fichier de blocs consécutifs (`incr`) |
| **manifeste** | fichier texte qui dit quelle base et quels segments forment l'état |
| **point de coupure** | position exacte dans l'ordre d'exécution où un BGSAVE fige son T0 |

---

## 3. Ce que contient le flux

### 3.1 Table

| Commande | Dans le flux |
|---|---|
| écritures déterministes : `SET` (toutes options), `SETNX`, `MSET`, `DEL`, `UNLINK`, `EXPIRE`, `PERSIST`, `INCR`, `DECR`, `INCRBY`, `DECRBY`, `HSET`, `HMSET`, `HDEL`, `HINCRBY`, `SADD`, `SREM`, `ZADD`, `ZREM`, `ZINCRBY`, `FLUSHDB` | **la frame reçue, octet pour octet** |
| écriture sans effet, vue dans sa réponse : `SETNX` → `:0`, `SET NX/XX` → nul, `DEL`/`UNLINK`/`EXPIRE`/`PERSIST`/`HDEL`/`SADD`/`SREM`/`ZREM` → `:0` ; toute réponse d'erreur (`WRONGTYPE`, `MISCONF`, `NOAUTH`, `READONLY`…) | **rien** (optimisation : la rejouer resterait correct, sauf l'erreur qui n'a rien changé non plus) |
| `SPOP` | `SREM <clé> <membres tirés>` (lus dans la réponse) ; réponse vide → rien |
| lectures, `PING`, `AUTH`, `HELLO`, `CLIENT`, `COMMAND`, `SELECT`, `INFO`, `LASTSAVE`, `DBSIZE`, `KEYS`, `SCAN`, `KRANGE`, Pub/Sub, `SAVE`, `BGSAVE` | rien |
| expirations (évicteur, expiration paresseuse) | **rien** (§4) |

Attention aux réponses qui ressemblent à « sans effet » mais n'en sont pas : un `INCR` qui
répond `:0` a écrit. La table d'exceptions se décide **commande par commande**, jamais par
la seule forme de la réponse.

### 3.2 À réécrire en effets quand elles arriveront

- identifiants générés par le serveur (`XADD *`) → l'identifiant obtenu ;
- commandes bloquantes (`BLPOP`, …) → leur effet **au moment où elles sont servies**
  (`LPOP k`) : l'instant de service ne se déduit pas du flux ;
- **éviction `maxmemory`** (LRU/LFU) : dépend des accès → chaque clé évincée → `DEL k`, dans le
  bloc du tour où elle est évincée ;
- scripts : seulement déterministes ; sinon effets (choix de Redis 7).

### 3.3 Invariants de déterminisme (à respecter par toute commande)

- **DET1** — À état métier et `art.now` égaux, une écriture produit le même état et la même
  réponse.
- **DET2 — clé expirée = clé absente**, qu'elle soit encore physiquement dans l'arbre ou non.
  Sur le primaire l'évicteur l'a peut-être supprimée, au rejeu peut-être pas encore : si une
  écriture « voyait » la clé expirée, le rejeu divergerait. C'est ce qui autorise à ne pas
  journaliser les expirations.
- **DET3** — Rien de ce qui modifie l'état ne dépend d'une graine aléatoire, d'un ordre de
  `HashMap`, d'une horloge autre que `art.now`, ni de la taille d'une tranche bornée en temps.
  Exception connue : la graine valart, qui **voyage dans le dump** (en-tête) et rend l'ordre
  interne identique après rechargement ; elle ne doit influencer que l'ordre des réponses, et
  `SPOP` (seule commande qui en dépend pour l'état) est réécrite.
- **DET4** — `now` monotone dans une histoire (§4).

### 3.4 Code

Un seul module, `radixox/src/exec/journal.rs`, appelé à un seul endroit (`exec_window`, à côté
de `persist.note_writes`) :

```rust
/// Ce qu'une commande laissera dans le flux, décidé avant son exécution (des
/// emprunts sur les octets reçus, pas sur la commande, qui est consommée).
pub enum Journal<'a> {
    Skip,                   // lecture
    Verbatim,               // la frame reçue
    SRem { key: &'a [u8] }, // SPOP : les membres viennent de la réponse
}

pub fn plan<'a>(cmd: &Cmd<'a>) -> Journal<'a>;

/// Après exécution : la réponse dit si l'écriture a changé quelque chose.
pub fn finish(j: Journal<'_>, reply: &[u8], raw: &[u8], now: u64, out: &mut Stream);
```

Fait ainsi (étape 1) : `raw` (les `n` octets de la frame, `Parse::Done(cmd, n)`) suit chaque
`(slot, cmd, raw)` de la fenêtre ; `reply` = **les octets que la commande a ajoutés à `out`**
(`out[start..]` autour de `run_cmd`), pas la `Frame` : `execute_into` ne change pas, et SPOP
relit ses membres dans ces octets (que nous avons encodés nous-mêmes). `UNLINK` et `FLUSHDB`
font leur effet dans `runtime()`, à l'intérieur de `run_cmd` : même point d'accroche ; un
`UNLINK` parqué n'a pas encore de réponse (`reply` vide) et est journalisé (effet décidé au
départ, §5). Les fonctions `cmd_*` ne changent pas. Une commande inline est journalisée telle
quelle (le parseur du rejeu l'accepte).

---

## 4. Le `now` : pourquoi, quand

**Pourquoi.** Des écritures dépendent de l'heure :

```
t     : SET k v EX 10    → k expire à t+10
t+5   : SETNX k w        → échoue (k vivant)
```

Rejoué une heure plus tard avec l'horloge réelle, `k` est expiré : `SETNX` réussit, l'état
diverge. Concernés : TTL relatifs (`SET … EX`, `EXPIRE`), et toute écriture qui tombe sur une
clé expirée (`SETNX`, `SET NX/XX`, `INCR`, `HSET`/`SADD`/`ZADD` qui recréent un conteneur au lieu
de fusionner).

**Quand.** Dans **l'en-tête de chaque bloc**. Toutes les commandes d'un tour voient le même
`art.now` (un `tick` par tour). Écrire le `now` seulement quand il change serait plus compact,
mais un bloc ne serait plus autonome (un réplica qui reprend au milieu, un rejeu qui démarre
à un segment devraient chercher le dernier changement) ; 8 octets par tour qui écrit, c'est
~8 Ko/s à 1 000 tours/s.

**Règles.**

- `now` du bloc = **la valeur exacte utilisée par l'arbre** (`art.now`, secondes). Passer à la
  milliseconde = nouvelle `version` du flux.
- **Monotone** : le primaire tient `last_now` = **le plus grand `now` posé sur l'arbre**, écrit
  dans un bloc ou non (`Stream::observe_now`, appelé par `Core::tick` / `set_wall`) ; si
  l'horloge murale recule, `art.now` est ramené à `last_now` pour le tour. Pas seulement le
  dernier `now` écrit : une lecture à `t`, dans un tour sans écriture, peut avoir supprimé
  paresseusement une clé expirée à `t` ; si l'horloge revenait à `t-1`, cette clé serait
  vivante au rejeu et absente ici (divergence trouvée par le test 1, graine 412).
- Un bloc se ferme **à la fin du tour**, après `idle_work` et avant le `tick` suivant : une étape
  de job faite au repos (un futur `STEP`, §5) tombe dans le bloc de son tour.
- Le rejeu (§9) et un réplica (§10) font `art.set_now(bloc.now)` avant d'appliquer un bloc, et
  ne laissent **jamais** leur horloge locale décider d'une expiration qui change l'état.

---

## 5. Commandes étalées dans le temps : point de linéarisation

| Classe | Exemples | Pourquoi c'est sûr | Journalisée |
|---|---|---|---|
| **effet décidé au départ** | `UNLINK prefix*`, `UNLINK` long, `FLUSHDB` | sous-arbre détaché d'un coup ; la libération est physique, invisible | la frame, **au départ** (quand la commande s'exécute, avant le parquage) |
| **effet à la fin, clés verrouillées** | futur gros `HDEL`, copie de sous-arbre, transactions (TX_DESIGN.md) | le verrou de lecture par clé (RUNTIME_ARCH §8.1) parque les écrivains sur ces clés : rien ne s'intercale | la frame, **à la fin** (dans le bloc du tour où le job répond) |
| **ni l'un ni l'autre** (appliqué par tranches, visible entre-temps, sans verrou) | un hypothétique `EXPIRE prefix* 10` progressif | le rejeu doit reproduire l'entrelacement | `RXJOB BEGIN <id> <frame>` puis `RXJOB STEP <id> <n>` à chaque tranche |

- `STEP` porte la **quantité** traitée (`n` éléments), jamais une durée : nos tranches sont
  bornées en temps (TSC, passe au repos) et leur taille ne se rejoue pas. Le rejeu exécute la
  même tranche de `n` éléments au même endroit du flux.
- **Règle de conception : éviter la troisième classe.** Toute commande longue se ramène à
  détacher ou à verrouiller. La revue d'une commande longue commence par « où est son point de
  linéarisation ? ». `RXJOB` n'est pas implémenté tant qu'aucune commande n'en a besoin.

---

## 6. Format

Tous les entiers en **little-endian**. CRC = CRC32 IEEE (`crc32fast`).

### 6.1 Bloc

```
off  taille  champ
 0     4     magic      "RXJ1"
 4     2     version    sémantique des commandes + format (1)
 6     2     flags      bit 0 CONT : suite du bloc précédent (même tour, même now)
                        bit 1 CUT  : un BGSAVE a figé son T0 juste après ce bloc (§8)
                        autres bits : 0 (un bit inconnu = bloc refusé)
 8     4     len        octets de la charge
12     4     crc        CRC32 des octets 0..12, puis 16..40, puis de la charge
                        (tout le bloc sauf ce champ)
16     8     seq
24     8     offset     octets du flux avant ce bloc
32     8     now        secondes
40   len     charge     commandes RESP (tableaux de bulk strings), à la suite
```

- Taille d'en-tête : 40 octets. **Remplissage : 1 MiB** (taille d'un tampon du pool, §7.2) :
  le bloc est fermé dès qu'il l'atteint ; un tour plus gros produit plusieurs blocs, les
  suivants avec `CONT`, même `now`. Une commande seule plus grosse (une frame va jusqu'à
  `query_limit`, 1 Gio) fait un bloc à elle seule : le pool devra servir ce cas hors tampons
  de 1 MiB (étape 8).
- Un bloc vide n'est pas écrit (tour sans écriture), sauf s'il porte `CUT`.
- Validité d'un bloc lu : magic, `len ≤ 1 Gio`, CRC exact, **puis** `version` acceptée, flags
  connus, `seq = seq précédent + 1`, `offset = offset précédent + 40 + len précédent`,
  `now ≥ now précédent`. Le CRC passe avant la version : un en-tête coupé suivi des zéros de la
  préallocation se lit « version 0 » et n'est pas une autre version (trouvé par le test 4).
- **Coupure ou incohérence** (`BadBlock::is_torn`) : seuls un bloc incomplet, un CRC faux ou des
  octets quelconques peuvent venir d'une écriture coupée. Un bloc **intact** dont la séquence,
  la version ou les drapeaux ne vont pas (segment manquant, autre histoire) n'est jamais
  tronqué, même avec `RADIXOX_AOF_LOAD_TRUNCATED=yes` : refus de démarrer.

### 6.2 Segments

- Dossier : `RADIXOX_DIR/appendonlydir/` (nom de Valkey).
- Segment : `journal.<n>.rxj`, `n` croissant. Le fichier commence directement par des blocs.
- **Préallocation** par paliers de **64 MiB** : `fallocate(fd, 0, debut, 64 MiB)` (mode 0 :
  la taille du fichier augmente tout de suite, l'espace est réservé et se lit comme des zéros).
  La **fin logique** est le premier bloc invalide (§6.1) ; un en-tête tout à zéro est la fin
  normale. La taille ne change qu'au palier suivant : entre deux paliers, `fdatasync` n'a pas
  de métadonnées ext4 (taille) à commiter.
- Un segment n'est jamais réécrit au milieu : on ajoute, on préalloue, on tronque à la fin
  logique quand il est fermé (rotation).

### 6.3 Base (dump format version 2)

La base est un dump `RXDB` dont l'en-tête gagne trois champs et le pied un condensé :

```
en-tête v2 : "RXDB" | u8 version=2 | u8 unité TTL | u64 t0 | u64 graine valart
             | u64 base_seq | [u8; 20] replid | u64 base_offset
pied v2    : u32 crc32 (tout ce qui précède) | u32 condensé | "RXDE"
```

- `base_seq` / `base_offset` : la base contient exactement les blocs `≤ base_seq`.
- **Fait** : tout dump est écrit en v2 (`dump.rxd` sans AOF : `base_seq = 0`, `replid` nul) ;
  le lecteur accepte v1 et v2 (pied de 8 octets en v1, 12 en v2). Côté oxidart :
  `JournalPos` (opaque), `save_start_at(out, &pos)`, `LoadStats { t0, pos, digest_checked }`.
  L'oracle `oxidart-legacy` lit l'en-tête v2 (il ignore la position).
- **Condensé** (§6.4) : vérifie que l'état rechargé est l'état sauvé.
- **Rejeu** : la base doit être chargée **sans sauter les clés expirées depuis la save**
  (`art.set_now(0)` avant `load_file`, puis l'horloge des blocs) : une clé vivante au T0 et
  expirée au moment du redémarrage l'était peut-être encore quand une commande du journal
  l'a vue (`SETNX` rejoué à `t0+2` réussirait sinon). Étape 3.

### 6.4 Condensé du contenu

Indépendant de l'ordre et de l'encodage du fichier, calculé deux fois par des codes
**différents** (le DFS de save, puis un parcours de l'arbre rechargé) :

```
préfixe(clé, type, échéance) = part(clé) ‖ u8 type ‖ u64 échéance absolue (0 = sans)
part(x) = u32 len ‖ x       (longueurs : « ab‖c » ≠ « a‖bc »)
chaîne / entier : crc32(préfixe ‖ part(valeur canonique))
hash            : un terme par champ : crc32(préfixe ‖ part(champ) ‖ part(valeur))
                  + un terme de tête : crc32(préfixe(type | 0x40) ‖ part(cardinal u64))
set             : idem par membre, + terme de tête
zset / list     : un terme par clé : crc32(préfixe ‖ part(cardinal) ‖ part(Σ éléments)),
                  élément zset = crc32(part(membre) ‖ part(score f64)),
                  élément list = crc32(part(rang u64) ‖ part(élément))
condensé = somme (wrapping, u64) des termes, tronquée à u32 dans le pied
```

Fait (`oxidart/src/digest.rs`) : côté save, le DFS calcule les termes à la visite de la clé
(`SaveCursor::record` : chemin, type, échéance ; valeurs inline et petits conteneurs) et, pour
un conteneur valart, relit les entrées que chaque étape du sérialiseur vient d'écrire (des
entrées entières) ; ZSet / List : sommes calculées dans la section du T0. Côté reload :
`OxidArt::content_digest`, parcours de l'arbre construit, valeurs lues en place ; les clés
sautées (expirées depuis la save) ajoutent leurs termes depuis le décodage.

**Coût mesuré** (`examples/save_bench`, 2026-10-20) : reload de 885 k chaînes 111 ms dont
51 ms de condensé (57 ns/clé) ; 442 k hash d'un champ 91 ms dont 52 ms. Le parcours après
coup coûte des défauts de cache : ≈ +80 % sur le temps de reload. Arbitrage possible si c'est
trop : vérification optionnelle, ou condensé calculé pendant la construction (moins
indépendant du décodage).

- Valeur canonique : octets pour une chaîne, `i64 LE` pour un entier (**le type compte** :
  `Int` et `String "10"` ne donnent pas le même terme, DET1 en dépend pour `INCR`).
- Une somme est indépendante de l'ordre (les deux parcours n'ont pas à visiter les éléments
  dans le même ordre). Les clés expirées au T0 ne sont pas comptées (elles ne sont pas écrites).
- Au rechargement : calcul après le chargement, sur l'arbre ; **écart = refus de rejouer le
  journal et refus de démarrer**, avec un message qui dit quel fichier est en cause.

### 6.5 Manifeste

`appendonlydir/manifest`, texte, une directive par ligne, `#` commente :

```
# radixox aof manifest
version 1
replid 3f9a1c0e5b7d2f4a6c8e0b1d3f5a7c9e1b3d5f7a
base base.17.rxdb seq 120345 offset 9876543
incr journal.18.rxj
incr journal.19.rxj
```

- Écrit en entier dans `manifest.tmp`, `fdatasync`, `rename`, fsync du dossier.
- Les fichiers non cités sont des restes (crash pendant une rotation) : ignorés, supprimés au
  démarrage après un chargement réussi.

---

## 7. Écriture du flux

### 7.1 Construction des blocs

- `Stream` (dans le cœur) : bloc en cours (tampon du pool), `seq`, `offset`, `last_now`,
  `replid`.
- `journal::finish` ajoute des commandes au bloc en cours. Bloc plein (1 MiB) → fermé (`CONT`
  sur le suivant). Fin de tour → fermé s'il n'est pas vide.
- Fermer un bloc : écrire l'en-tête (CRC calculé une fois), le mettre dans la **file des blocs
  fermés**, avancer `seq`/`offset`.

### 7.2 Un tampon, N+1 opérations (disque + réplicas)

- **Pool de tampons de 1 MiB**, alloués et **enregistrés** une fois (`register_buffers`) : un
  bloc fermé est soumis par `WRITE_FIXED` au segment et par `SEND_ZC` (ou `SENDMSG_ZC`, §10.4)
  à chaque réplica, **sans copie ni réencodage**.
- **Compteur de références** par bloc : `1` (disque, si l'AOF est actif) `+ nb réplicas qui ne
  l'ont pas encore entièrement envoyé` `+ 1` tant qu'il est dans le backlog de réplication
  (§10.5). Rendu au pool à 0. Un bloc n'est **jamais modifié** tant que son compteur n'est pas
  nul (même invariant que `sending` et le tampon d'écriture du BGSAVE).
- `SEND_ZC` libère le tampon à la **notification** (seconde CQE), pas à la première.
- Pas d'ordre imposé entre disque et réseau.

### 7.3 Écriture sur disque

- **Une écriture en vol** au plus (comme le BGSAVE) ; les blocs fermés s'enchaînent : une
  écriture peut couvrir plusieurs blocs consécutifs (plusieurs SQE liées, ou un `WRITEV`).
- Sur le worker fichiers si `RADIXOX_FILE_CPUS` est défini (`IOSQE_ASYNC`, RUNTIME_ARCH §8.2) :
  la copie dans le cache de pages et le fsync ne prennent pas le cœur.
- Partage avec le BGSAVE : un seul canal de fichiers en vol aujourd'hui (`K_FILE`,
  `Core::on_file`). Il faut **deux canaux** (dump, journal) avec chacun son `user_data`.

### 7.4 Modes de fsync (`RADIXOX_APPENDFSYNC`)

| Mode | Écriture | fsync | Réponses |
|---|---|---|---|
| `no` | dès qu'un bloc est fermé | jamais (le noyau) | immédiates |
| `everysec` (défaut) | idem | `fdatasync` toutes les secondes (timer), sur io-wq ; jamais deux en vol | immédiates |
| `always` | idem, **write + fsync liés** (`IOSQE_IO_LINK`) | un seul en vol ; les blocs fermés pendant ce temps partent dans le suivant | retenues jusqu'au fsync qui les couvre (§7.5) |

`everysec` : si le fsync précédent dure plus de 2 s, Valkey retarde les écritures ; ici le
plafond du §7.6 joue ce rôle sans bloquer la boucle.

### 7.5 Commit groupé (`always`) et marquage des réponses

- Deux compteurs : `written` (offset de fin du dernier bloc fermé, côté mémoire) et `durable`
  (offset couvert par le dernier fsync terminé).
- Pendant l'exécution, chaque réponse reçoit la marque `m = offset de fin du flux après sa
  commande` **si une écriture non durable existe** (`written > durable` ou bloc en cours non
  vide) ; sinon pas de marque.
  - une écriture est marquée par son propre bloc ;
  - **une lecture est marquée aussi** : elle a pu voir une valeur pas encore durable ; si on
    répondait et que le serveur plantait, le client aurait lu une donnée perdue.
- Structure par connexion (`ConnOut`) : `holds: VecDeque<(usize /*fin de la réponse dans out*/,
  u64 /*marque*/)>`. `flush()` n'envoie que jusqu'au premier `hold` dont la marque est
  `> durable`. Ordre des réponses d'une connexion conservé.
- CQE du fsync → `durable` avance → les connexions concernées redeviennent `dirty`.
- Le même mécanisme sert à `WAIT` et à la réplication synchrone (§10.6) : la condition de sortie
  devient `marque ≤ min(durable, offset acquitté par N réplicas)`.
- Coût fixé par le disque : ~20–100 µs par fsync sur NVMe avec protection contre les coupures
  (PLP), ~1–5 ms sur un SSD grand public (vidage de son cache). Le regroupement se fait entre
  connexions : pipeline ou non, tout ce qui arrive pendant un fsync part dans le suivant.

### 7.6 Disque trop lent : plafond

- Au-delà de `RADIXOX_AOF_MAX_PENDING_MB` (défaut 64) de blocs fermés non écrits, **les
  écritures des clients sont parquées** (la connexion attend, comme un job) ; les lectures
  continuent ; reprise quand le retard repasse sous la moitié.

### 7.7 Erreurs d'écriture

- `everysec` / `no` : `aof_last_write_status:err`, écritures refusées
  (`-MISCONF Errors writing to the AOF file: <erreur>`), nouvel essai chaque seconde depuis le
  dernier octet écrit avec succès ; retour à `ok` au premier succès.
- `always` : write ou fsync en échec → **arrêt du processus** (log puis `exit(1)`), comme
  Valkey : des réponses retenues ne peuvent plus être garanties, et un fsync raté laisse le
  cache de pages dans un état inconnu (on ne réessaie jamais un fsync en échec).
- `ENOSPC` à la préallocation d'un palier : même traitement qu'une erreur d'écriture.

---

## 8. Coupure, réécriture, rotation

Avec l'AOF actif, **tout BGSAVE est une réécriture** : il produit une nouvelle base et coupe le
journal (D11). Les règles de save automatique (`RADIXOX_SAVE`) et
`auto-aof-rewrite-percentage` déclenchent la même chose ; `BGREWRITEAOF` est un alias de
`BGSAVE` dans ce mode. `SAVE` (bloquante) fait une réécriture synchrone.

### 8.1 Coupure exacte

1. Au T0 (dans `finish_frame` du `BGSAVE`, ou au début du tour pour une save automatique) :
   **fermer le bloc en cours avec le drapeau `CUT`** (même vide). Toutes les commandes
   exécutées avant le T0 sont dans les blocs `≤ base_seq = seq de ce bloc` ; celles d'après,
   même dans le même tour, dans les suivants (même `now`, `CONT` si c'est le même tour).
2. Fermer le segment courant `journal.N.rxj` à ce bloc et ouvrir `journal.N+1.rxj`.
3. Le dump est écrit avec `base_seq`, `base_offset`, `replid` (§6.3), sous le nom
   `base.<N+1>.rxdb.tmp`.

### 8.2 Ordre sacré de publication

1. Base : écritures, `fdatasync`, `rename` en `base.<N+1>.rxdb`, fsync du dossier.
2. Manifeste : `base` = la nouvelle, `incr` = `journal.N+1…` ; `manifest.tmp`, `fdatasync`,
   `rename`, fsync du dossier.
3. **Seulement ensuite** : supprimer l'ancienne base et les segments `≤ N`.

Crash entre 1 et 2 : l'ancien manifeste reste valide. Il cite l'ancienne base et tous les
segments, `journal.N+1` compris, puisque §8.3 l'y a ajouté dès son ouverture : le rejeu passe
par `journal.N` puis `journal.N+1`, rien n'est perdu, et la nouvelle base est un reste ignoré.
Crash entre 2 et 3 : ancienne base et anciens segments en trop, non cités, supprimés au
démarrage.

### 8.3 Ouverture d'un segment

Ouvrir `journal.N+1.rxj` = le créer, le préallouer, l'ajouter au manifeste (réécriture complète,
`rename`, fsync du dossier) **avant** d'y écrire le premier bloc. Ainsi le manifeste cite
toujours tous les segments qui contiennent des blocs, quelle que soit l'étape du crash.

### 8.4 Déclenchement automatique

`RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE` (100) et `RADIXOX_AUTO_AOF_REWRITE_MIN_SIZE` (64 MiB) :
réécriture quand la taille des segments depuis la dernière base dépasse `min_size` et
`pourcentage × taille de la base`. Vérifié avec le timer des saves automatiques.

### 8.5 Interaction avec les jobs

- `UNLINK prefix*` ou `FLUSHDB` journalisés au départ, T0 posé pendant leur libération : le
  dump voit le sous-arbre déjà détaché (invisible), cohérent avec le flux (la commande
  précède la coupure).
- Écrivains parqués (plafond §7.6, futurs verrous) : journalisés quand ils **s'exécutent**.
  L'ordre du flux est l'ordre d'exécution.

---

## 9. Reprise au démarrage

1. `appendonlydir/manifest` absent et AOF actif : démarrer depuis `dump.rxd` s'il existe (ou
   vide), puis **faire immédiatement une réécriture** (création de la base et du premier
   segment), comme Valkey qui crée son AOF au premier démarrage.
2. Lire le manifeste ; `version` connue, sinon refus.
3. Charger la base (chemin `load_at_startup`) ; vérifier le CRC puis le **condensé** (§6.4).
4. Rejouer chaque segment cité, dans l'ordre, bloc par bloc :
   - valider le bloc (§6.1) ; `seq` du premier bloc après la base = `base_seq + 1` ;
   - `version` du bloc différente de celle du binaire et non déclarée compatible : **refus de
     démarrer**, message : faire `BGREWRITEAOF` avec l'ancienne version avant la montée ;
   - `art.set_now(bloc.now)`, puis chaque commande de la charge par l'exécuteur normal, avec un
     client interne authentifié, sans réponse, sans journalisation (on ne réécrit pas ce qu'on
     relit) ;
5. Bloc invalide **à la fin du dernier segment** (écriture coupée par le crash, `is_torn`) :
   tronquer le segment à la fin du dernier bloc valide, continuer, journaliser un avertissement
   (`RADIXOX_AOF_LOAD_TRUNCATED=yes`, défaut). Bloc invalide **ailleurs**, ou bloc intact
   incohérent : refus de démarrer. Fait : le dernier segment est toujours ramené à sa fin
   logique (`set_len`) ; le prochain palier est préalloué à l'écriture.
6. Reprendre `replid`, `seq`, `offset`, `last_now` ; l'horloge réelle reprend avec
   `max(last_now, horloge)`.
7. Supprimer les fichiers non cités.

---

## 10. Réplication

### 10.1 Modèle : le primaire pousse, le réplica « suit »

- Le **primaire** se connecte au réplica **comme un client normal** (TCP, `AUTH` si le réplica a
  un mot de passe) et lui envoie des commandes RESP. Pas de protocole à part.
- Le **réplica** est un serveur RadixOx ordinaire. Il ne sait qu'il est réplica que par un
  **mode « suivi »**, dans lequel il entre à la première commande de réplication reçue d'une
  connexion authentifiée. Ce mode change trois choses, et seulement trois :
  1. **horloge pilotée** : `art.now` = `now` du dernier bloc appliqué ; le `tick` local est
     ignoré ; l'évicteur ne supprime que ce qui est expiré à ce `now` (DET2, §4) ; les lectures
     d'autres clients **masquent** une clé expirée selon l'horloge locale sans la supprimer ;
  2. **écritures des autres clients refusées** : `-READONLY You can't write against a read only
     replica.` (texte de Valkey) ; lectures servies ;
  3. **une réponse par bloc**, qui sert d'acquittement (pas de réponse par commande).
- **Promotion** : `REPLICAOF NO ONE` sort du mode : horloge locale (monotone depuis le dernier
  `now`), écritures acceptées, **nouveau `replid`**, `seq` continue.
- Configuration (règles de save, fichiers, mots de passe…) : **propre à chaque nœud**, jamais
  répliquée.

### 10.2 Commandes

Côté **source** (le primaire, ou un réplica qui chaîne) :

| Commande | Effet |
|---|---|
| `REPLICATE <host> <port> [AUTH <pass>]` | ajoute une cible : connexion, synchronisation (§10.3), puis flux ; reconnexion automatique toutes les secondes en cas de coupure |
| `REPLICATE STOP <host> <port>` | retire la cible |
| `RADIXOX_REPLICATE="h1:p1,h2:p2"` | cibles au démarrage |

Côté **cible** (réservées à la réplication ; refusées hors connexion authentifiée) :

| Commande | Réponse | Effet |
|---|---|---|
| `RXHELLO <replid>` | `:<seq>` si la cible suit cette histoire (dernier `seq` appliqué), sinon `:-1` | rien |
| `RXLOAD <dump>` | `+OK` ou erreur | vide la base, charge le dump (bulk string, flux du dump v2), vérifie CRC et condensé, adopte `replid`/`seq`/`offset`, entre en mode suivi |
| `RXBLOCK <bloc>` | `:<seq>` (acquittement, voir 10.6) | valide le bloc (§6.1, continuité `seq`), `art.set_now(now)`, applique la charge, entre en mode suivi si besoin |
| `REPLICAOF NO ONE` | `+OK` | promotion |

`<bloc>` est un **bulk string dont le contenu est exactement le bloc du disque**. Envoi sans
copie : `SENDMSG(_ZC)` avec deux iovecs, le préfixe RESP (`*2\r\n$7\r\nRXBLOCK\r\n$<n>\r\n`, petit
tampon propre à l'envoi) puis le tampon partagé du bloc, puis `\r\n`.

### 10.3 Synchronisation

1. La source envoie `RXHELLO <son replid>`.
2. Réponse `:<s>` avec `s` dans le backlog (§10.5) : **reprise partielle**, envoi des blocs
   `s+1…`.
3. Sinon **synchronisation complète** : BGSAVE (une réécriture si l'AOF est actif, sinon un dump
   en mémoire ou dans un fichier temporaire), `RXLOAD` du dump (envoyé par `splice` depuis le
   fichier : fichier → pipe → socket, sans passer par l'espace utilisateur), puis les blocs
   `base_seq+1…` (gardés dans le backlog pendant la save).

### 10.4 Envoi : zéro copie, chiffrement

- `SEND_ZC` / `SENDMSG_ZC` sur le tampon enregistré du bloc : intéressant ici (gros blocs, vraie
  carte réseau ; en loopback le noyau recopie de toute façon).
- **kTLS** (cible de production) : le noyau chiffre pendant l'envoi ; on soumet toujours le
  même tampon en clair, le modèle tient (une copie de chiffrement par réplica, faite par le
  noyau, sauf déchargement matériel).
- **TLS en espace utilisateur** (rustls) : un chiffré par réplica ; le disque garde le tampon
  partagé. À éviter pour la réplication.
- Rattrapage d'un réplica dont les blocs ne sont plus en mémoire mais encore dans les segments :
  `splice` depuis le segment.

### 10.5 Backlog et réplica lent

- Backlog = les derniers blocs fermés gardés en mémoire (compteur de références, §7.2), jusqu'à
  `RADIXOX_REPL_BACKLOG_MB` (défaut 64).
- Un réplica qui retient des blocs au-delà de `RADIXOX_REPL_MAX_LAG_MB` (défaut 256, le
  `client-output-buffer-limit replica` de Valkey) est déconnecté ; il repassera par §10.3.

### 10.6 Acquittements, `WAIT`, réplication synchrone

- La réponse `:<seq>` à `RXBLOCK` est l'acquittement : la source tient pour chaque réplica
  l'offset acquitté (`offset` de fin du bloc `seq`).
- `WAIT <n> <timeout>` : la connexion est parquée jusqu'à ce que `n` réplicas aient acquitté
  l'offset de sa dernière écriture (ou le délai).
- `RADIXOX_MIN_REPLICAS_TO_WRITE=n` : marquage du §7.5 avec la condition
  `marque ≤ min(durable, n-ième plus grand offset acquitté)`. Le fsync groupé et la réplication
  synchrone sont **le même mécanisme**.

### 10.7 Chaînage et AOF du réplica

- Un réplica en mode suivi peut lui-même `REPLICATE` : il retransmet les blocs **tels que
  reçus** (mêmes octets, même `replid`).
- Un réplica avec l'AOF actif écrit les blocs reçus **tels quels** dans son journal : journaux
  identiques octet pour octet.

### 10.8 Basculement

Opérateur ou orchestrateur, sur le réplica choisi : `REPLICAOF NO ONE`, puis `REPLICATE` vers
les autres. Les autres réplicas répondent `:-1` à `RXHELLO` (nouveau `replid`) : synchronisation
complète. (Reprise partielle après basculement = « second identifiant » du PSYNC2 de Redis : le
nouveau primaire accepterait l'ancien `replid` jusqu'au `seq` de sa promotion. Plus tard.)

---

## 11. Configuration, commandes, INFO

| Variable | Défaut | Valkey |
|---|---|---|
| `RADIXOX_APPENDONLY` | `no` | `appendonly` |
| `RADIXOX_APPENDFSYNC` | `everysec` | `appendfsync` |
| `RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE` | `100` | `auto-aof-rewrite-percentage` |
| `RADIXOX_AUTO_AOF_REWRITE_MIN_SIZE` | `64mb` | `auto-aof-rewrite-min-size` |
| `RADIXOX_AOF_LOAD_TRUNCATED` | `yes` | `aof-load-truncated` |
| `RADIXOX_AOF_MAX_PENDING_MB` | `64` | (pas d'équivalent ; retard d'`everysec`) |
| `RADIXOX_REPLICATE` | vide | (`replicaof`, mais en poussée) |
| `RADIXOX_REPL_BACKLOG_MB` | `64` | `repl-backlog-size` |
| `RADIXOX_REPL_MAX_LAG_MB` | `256` | `client-output-buffer-limit replica` |
| `RADIXOX_MIN_REPLICAS_TO_WRITE` | `0` | `min-replicas-to-write` |

Commandes : `BGREWRITEAOF`, `WAIT`, `REPLICATE`, `REPLICAOF NO ONE`, et les commandes internes
`RXHELLO`, `RXLOAD`, `RXBLOCK` (§10.2).

`INFO` :

- `# Persistence` : `aof_enabled`, `aof_rewrite_in_progress`, `aof_last_write_status`,
  `aof_last_fsync_status`, `aof_current_size`, `aof_base_size`, `aof_pending_bytes`,
  `aof_delayed_fsync` ;
- `# Replication` : `role:master|replica`, `replid`, `master_repl_offset` (`offset` courant),
  `repl_seq`, `connected_replicas`, puis par réplica `replicaN:ip=…,port=…,state=sync|online,offset=…,lag=…`.

---

## 12. Tests (à écrire avant ou avec le code, jamais après)

1. **Oracle du flux** (unitaire, faux noyau) : flux de commandes aléatoires (`random_cmd`
   étendu à tous les types, TTL compris, horloge déplacée entre les tours) ; journaliser ;
   rejouer sur un arbre vide → **état identique** (comparaison métier : clés, types, valeurs,
   échéances). Puis depuis un dump pris au milieu : `état == rejeu(dump au T0, blocs > base_seq)`.
2. **DET2** : pour chaque écriture, l'exécuter avec et sans `evict_expired()` juste avant →
   même état et même réponse.
3. **Condensé** : save → reload → condensés égaux ; altérer un octet de valeur dans un dump au
   CRC recalculé → condensé différent → refus.
4. **Format** : blocs tronqués à chaque offset, octet corrompu, `seq` sauté, `now` qui recule,
   flag inconnu → bloc refusé ; troncature acceptée seulement en fin de dernier segment.
5. **Crash (`kill -9`)** (intégration, sous `systemd-run`) : un client écrit des clés numérotées
   et note chaque acquittement ; `kill -9` à un instant aléatoire (pendant écritures, fsync,
   rotation, écriture du manifeste) ; redémarrage ; vérifier :
   - l'état est un **préfixe exact** de l'histoire (jamais une écriture inventée, jamais un trou) ;
   - `always` : **toutes** les écritures acquittées sont présentes ;
   - `everysec` : perte bornée (~1–2 s) ; `no` : préfixe exact, perte quelconque.
   Répéter (centaines d'itérations courtes) ; chaque itération ≤ 30 s, ~1 M clés max
   (CLAUDE.md).
6. **Coupure** : `BGSAVE` au milieu d'un pipeline (`SET a 1`, `BGSAVE`, `SET a 2`) : base = `a=1`,
   segment suivant = `SET a 2` ; reprise = `a=2`.
7. **Réplication** : réplica ≡ primaire (comparaison métier) après flux aléatoire ; coupure
   réseau puis reprise partielle ; backlog dépassé → synchronisation complète ; promotion →
   nouveau `replid`, `RXHELLO` de l'ancien → `:-1` ; réplica écrit par un autre client →
   `READONLY` ; horloge du réplica décalée de ±1 h → toujours identique.
8. **Commit groupé** : en `always`, une réponse (écriture ou lecture qui a vu une écriture) ne
   part pas avant le fsync qui la couvre (faux noyau : retenir le CQE du fsync).

---

## 13. Ordre d'implémentation (une étape = un commit vert, poussé)

| # | Étape | Fini quand |
|---|---|---|
| 1 ✅ | `exec/journal.rs` (`plan`, `finish`, `Stream` en mémoire), plomberie `raw` + réponse | test 1 (oracle depuis vide) et test 2 verts — `uring/tests.rs` : `journal_replay_matches_state`, `expired_keys_are_absent_for_writes` (1000 graines vertes) |
| 2 ✅ | Dump v2 : `base_seq`, `replid`, `base_offset`, condensé ; vérification au rechargement | test 3 ; tous les tests de save existants verts ; v1 toujours lisible — `oxidart/src/test_save.rs` : `save_digest_is_the_t0_state_under_mutations`, `reload_checks_digest_even_with_expired_keys`, `reload_refuses_altered_content_with_valid_crc`, `reload_reads_version_1`, `journal_position_roundtrip` |
| 3 ✅ | Format de bloc, segments préalloués, manifeste, rejeu au démarrage, troncature | test 4 — `uring/tests.rs` : `aof_recovery_matches_state_across_segments`, `aof_truncated_tail_is_cut_only_at_the_end`, `aof_unlisted_files_are_removed` ; le test 6 (coupure) passe à l'étape 5, qui introduit `CUT` |
| 4 | `appendfsync no` / `everysec`, deux canaux de fichiers, plafond §7.6, erreurs §7.7, `INFO aof_*` | test 5 en `everysec` et `no` |
| 5 | Réécriture : coupure `CUT`, rotation, ordre §8.2–8.3, automatique, `BGREWRITEAOF` | tests 5 (avec rotations) et 6 ; test 1 depuis un dump |
| 6 | `always` : marquage §7.5, un fsync en vol | tests 5 (`always`) et 8 ; bench `always` contre Valkey `appendfsync always` (même disque, pas tmpfs) |
| 7 | Réplication : mode suivi, `RXHELLO`/`RXLOAD`/`RXBLOCK`, `REPLICATE`, backlog, `REPLICAOF NO ONE` | test 7 |
| 8 | Pool de tampons enregistrés, `SEND(MSG)_ZC`, `splice`, `WAIT`, `min-replicas-to-write` | test 7 + mesure deux machines |

Chaque étape met à jour ce document (section « État ») et le README (Status, Persistence,
Configuration, commandes supportées).

---

## 14. Cas limites

- **Écriture refusée** (`MISCONF`, `NOAUTH`, `WRONGTYPE`, `READONLY`) : réponse d'erreur → rien.
- **Connexion parquée** (`KEYS`) : lecture, rien.
- **`QUIT` au milieu d'un pipeline** : les commandes exécutées avant sont journalisées, pas les
  suivantes (elles ne s'exécutent pas).
- **`FLUSHDB` puis BGSAVE dans le même tour** : `FLUSHDB` journalisé avant la coupure ; la base
  est vide.
- **Horloge qui recule** sur le primaire : `now` monotone (§4).
- **Réplica qui reçoit un `seq` non contigu** : refuse le bloc, ferme la connexion ; la source
  repasse par `RXHELLO`.
- **Deux sources poussent vers le même réplica** : la seconde reçoit une erreur
  (`-ERR already following <replid>`) tant que la première est connectée.
- **Promotion pendant l'application d'un bloc** : impossible (un thread ; `REPLICAOF NO ONE`
  passe entre deux commandes).
- **Montée de version** : bloc d'une `version` inconnue → refus de démarrer (§9), jamais de
  perte silencieuse.
- **Réplica en mode suivi qui reçoit `BGSAVE` d'un client** : autorisé (dump local) ; avec son
  propre AOF actif, c'est une réécriture locale à `base_seq` = son dernier bloc appliqué.

---

## 15. Écarté (ne pas reproposer sans fait nouveau)

| Option | Raison |
|---|---|
| Collecteur d'effets passé à chaque commande (`&mut Effects`) | alourdit toutes les signatures, mélange journal et métier |
| Journaliser des effets calculés (`INCR` → `SET k <résultat>`) partout | même alourdissement ; gardé seulement pour les commandes non déterministes |
| Crochets dans oxidart (l'arbre signale ses mutations) | opérations physiques au lieu de métier ; l'AOF entrerait dans la bibliothèque embarquée |
| `fork` pour la réécriture | voir ARCHITECTURE.md §3 (copie de pages, bibliothèque embarquée) |
| Synchronisation complète en régénérant des commandes (`SET`/`HSET`…) depuis le snapshot | plus lent que le dump binaire et risque d'imprécision (encodage, TTL) ; le dump a déjà son chemin prouvé et son condensé |
| Protocole de réplication à part (PSYNC de Redis, réplica qui tire) | le modèle « le primaire pousse des commandes » réutilise l'exécuteur et le format du disque |
| Répliquer la configuration | propre à chaque nœud, comme Redis |
| `now` seulement quand il change (`TICK`) | blocs non autonomes pour la reprise ; 8 octets par tour ne valent pas la complication |
| Un second algorithme de somme de contrôle (CRC32C, xxh3) | un seul CRC partout (D10) |

---

## 16. Questions ouvertes

- `PX` / `PEXPIRE` arrondis à la seconde aujourd'hui (§1.2) : incompatibilité Redis à corriger
  un jour ; le `now` des blocs passera alors en millisecondes (nouvelle `version`).
- Reprise partielle après basculement (second identifiant, §10.8).
- Comptage mémoire du pool, du backlog et des tampons d'envoi dans le futur `maxmemory`.
