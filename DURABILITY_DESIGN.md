# Durabilité — AOF et réplication par un seul flux

> Conception du 2026-10-20, **rien d'implémenté**. Remplace le « collecteur d'effets » prévu
> dans RUNTIME_ARCH.md §18 et les principes de l'AOF d'ARCHITECTURE.md §3. Les saves
> automatiques (faites) sont dans SAVE_DESIGN.md, « Saves automatiques ».

## 0. Décisions

1. **Un seul flux** : le même journal sert l'AOF (sur disque) et la réplication (sur le
   réseau). Mêmes blocs, mêmes octets : un réplica s'alimente directement depuis le tampon
   de l'AOF, sans transformation.
2. **On journalise les commandes, pas leurs effets**, telles que reçues (octets RESP de la
   frame). Les fonctions de commande (`exec/*.rs`) ne changent pas.
3. **Un bloc par tour**, qui porte le `now` d'oxidart de ce tour : le rejeu remet la même
   horloge, donc les commandes dépendantes du temps (TTL relatifs, expiration paresseuse)
   rejouent à l'identique. **Les expirations ne sont pas journalisées.**
4. **Exceptions** : une courte liste de commandes non déterministes, réécrites en effets
   (§2). Toute nouvelle commande est déterministe par défaut ou y est ajoutée.
5. **Commandes étalées sur plusieurs tours** : journalisées une fois, à leur point de
   linéarisation (§3).
6. **Le snapshot doit être exact** (§4) : rejouer des commandes suppose un état de départ
   identique au bit près sur tout ce qui compte. Prouvé par les tests, vérifié en
   production par un condensé.

Le prix assumé : rejouer des commandes, c'est rejouer **le code de la version qui relit**.
Le flux porte sa version (§5) ; une montée de version qui change la sémantique d'une
commande impose une réécriture (`BGREWRITEAOF`) avec l'ancienne version avant la montée.

---

## 1. Pourquoi des commandes et pas des effets

Journaliser des effets (`INCR` → `SET k <résultat>`) obligerait chaque commande à dire ce
qu'elle a changé : un `&mut Effects` dans toutes les signatures, du journal mélangé au
métier. Or, à état et horloge identiques, presque toutes nos commandes sont déterministes :

- toutes les commandes d'un tour voient le même `now` (`art.tick()` une fois par tour) ;
- l'arbre est déterministe (pas de graine, pas d'ordre de hachage dans l'état) ;
- `SETNX`, `SET NX/XX`, `INCR`, `HSET` sur un conteneur expiré : même état + même `now` =
  même résultat, l'expiration paresseuse comprise.

Donc **rejouer la frame reçue suffit**, et coûte un memcpy depuis le tampon de réception.

## 2. Ce que contient le flux

| Commande | Dans le flux |
|---|---|
| écritures déterministes (`SET` et options, `SETNX`, `MSET`, `DEL`, `UNLINK`, `EXPIRE`, `PERSIST`, `INCR*`/`DECR*`, `HSET`/`HMSET`/`HDEL`/`HINCRBY`, `SADD`/`SREM`, `ZADD`/`ZREM`/`ZINCRBY`, `FLUSHDB`) | la frame telle quelle |
| écriture dont la réponse montre qu'elle n'a rien fait (`SETNX` → 0, `SET NX` → nul, `DEL`/`SREM`/`HDEL`/`ZREM` → 0, erreur) | rien (gain de place ; la rejouer resterait correct) |
| `SPOP` (tirage aléatoire) | `SREM k <membres tirés>` (lus dans la réponse) |
| lectures, `AUTH`, `CLIENT`, `HELLO`, Pub/Sub, `SAVE`, `BGSAVE`, `INFO` | rien |
| expirations (évicteur, lecture d'une clé expirée) | rien : le rejeu les reproduit avec le `now` des blocs |

**Plus tard**, à réécrire en effets au moment où elles arrivent :

- identifiants générés par le serveur (`XADD *`) → l'identifiant obtenu ;
- commandes bloquantes (`BLPOP`) → l'effet quand elles sont servies (`LPOP k`), pas leur
  arrivée : leur instant de service n'est pas dans le flux ;
- **éviction LRU/LFU** : dépend des accès, pas de l'état → chaque clé évincée → `DEL k` ;
- scripts : seulement s'ils sont déterministes, sinon effets (le choix de Redis 7).

**Règle d'audit** : rien de ce qui modifie l'état ne dépend d'une graine, d'un ordre de
`HashMap`, de l'horloge hors `art.now`, ni du découpage en tranches (§3).

Code : un seul module, `exec/journal.rs`, appelé à un seul endroit (la boucle d'exécution
du cœur, là où le `dirty` des saves automatiques est compté) :

```rust
pub enum Journal<'a> {
    Skip,                   // lecture
    Verbatim,               // la frame reçue
    SRem { key: &'a [u8] }, // SPOP : les membres viennent de la réponse
}

/// Avant exécution : des emprunts sur le tampon reçu, pas sur la commande.
pub fn plan<'a>(cmd: &Cmd<'a>) -> Journal<'a>;
/// Après exécution : la réponse dit si l'écriture a changé quelque chose.
pub fn finish(j: Journal<'_>, reply: &Frame, raw: &[u8], out: &mut Stream);
```

Plomberie : faire suivre les octets bruts de chaque frame jusqu'à l'exécution (le parseur
connaît sa longueur, `Parse::Done(cmd, n)`) ; les écritures passent toutes par
`execute_sync`, qui rend une `Frame` avant l'encodage.

## 3. Commandes étalées dans le temps : le point de linéarisation

Toute écriture qui dure plusieurs tours déclare l'instant où elle prend effet pour les
autres, et n'est journalisée qu'une fois, à cet instant :

| Classe | Exemples | Pourquoi c'est sûr | Journalisée |
|---|---|---|---|
| **effet décidé au départ** | `UNLINK prefix*`, `FLUSHDB` | sous-arbre détaché d'un coup ; la libération ensuite est physique, invisible | au début |
| **effet à la fin, clés verrouillées** | gros `HDEL`, copie de sous-arbre, transaction (TX_DESIGN.md) | le verrou de lecture par clé (RUNTIME_ARCH §8.1) parque les écrivains : rien ne s'intercale | à la fin |
| **ni l'un ni l'autre** (appliqué par tranches, visible entre-temps, sans verrou) | un hypothétique `EXPIRE prefix* 10` progressif | le rejeu doit reproduire l'entrelacement | `BEGIN id cmd`, puis `STEP id n` à chaque tranche |

`STEP` porte la **quantité** traitée (`n` éléments), pas la durée : nos tranches sont
bornées en temps (TSC, passe au repos), leur taille ne se rejoue pas. Le rejeu exécute la
même tranche de `n` éléments au même endroit du flux.

Règle de conception : **éviter la troisième classe** ; toute commande longue se ramène à
détacher ou à verrouiller. La revue d'une commande longue commence par « où est son point
de linéarisation ? ».

## 4. Exactitude du snapshot

Le rejeu part du dump de base : il doit être exactement l'état au T0, sur tout ce qui
change le résultat d'une commande.

- **Contenu** : valeurs, types **et encodage** (un `Int` revient `Int`, sinon `INCR`
  diverge), TTL à la précision de la mémoire (la seconde aujourd'hui), clés expirées pas
  encore supprimées avec leur échéance.
- **Coupure** : le T0 du BGSAVE tombe entre deux tours ; la barrière copy-on-write fige
  l'état à cet instant. Le dump porte le numéro de séquence du dernier bloc qu'il contient.
- **Preuve** : `reload_roundtrip_random` (SAVE_DESIGN.md) applique déjà 300 mutations
  identiques à l'arbre et à son rechargement. À étendre : tous les types, TTL, et des flux
  aléatoires passés par le vrai exécuteur (le générateur de `uring/tests.rs`) ;
  **oracle** : `état(serveur) == rejeu(dump au T0, flux après T0)`.
- **Filet en production** : le DFS de la save calcule un condensé de l'état métier (clé,
  type, valeur, échéance) écrit dans le pied du dump ; le rechargement le recalcule et
  **refuse de rejouer** si l'empreinte diffère. Une save imparfaite est arrêtée avant d'être
  propagée.

## 5. Format

Un **bloc par tour** qui a journalisé quelque chose ; mêmes octets sur disque et sur le
réseau :

```
magic      u32   "RXJ1"
version    u16   du format et de la sémantique des commandes
flags      u16   (BEGIN/STEP présents, …)
len        u32   octets de la charge
crc32c     u32   de l'en-tête après lui et de la charge
seq        u64   numéro du bloc, croissant, jamais réutilisé
offset     u64   octets du flux avant ce bloc (WAIT, reprise d'un réplica)
now        u64   art.now du tour (secondes), monotone
charge           commandes RESP du tour, à la suite
```

- **`now` monotone** : `max(now précédent, horloge)`. Si l'horloge murale du primaire
  recule, une clé déjà expirée (et peut-être supprimée sur un réplica ou au rejeu) ne doit
  pas « revivre ».
- Charge en RESP : lisible, rejouable à la main (`redis-cli --pipe` après extraction).
- **Segments préalloués** (`fallocate`) : la fin logique est le premier en-tête invalide ;
  le fichier ne grandit pas, `fdatasync` n'a pas de métadonnées ext4 à commiter.

## 6. Écriture

- Pendant le tour, `journal::finish` ajoute au tampon du flux. Fin de tour : un bloc.
- Double tampon (`fill` / `writing`, comme BGSAVE), **une écriture en vol**, sur le
  worker fichiers (`RADIXOX_FILE_CPUS`) : la boucle ne copie ni ne synchronise rien.

| `appendfsync` | Écriture | fsync | Réponses |
|---|---|---|---|
| `no` | fin de tour, ou tampon ≥ ~1 Mo | le noyau | immédiates |
| `everysec` (défaut) | idem | timer de 1 s, io-wq | immédiates |
| `always` | write + fsync liés (`IOSQE_IO_LINK`) en fin de tour | **un seul en vol**, groupé | retenues jusqu'au fsync qui les couvre |

**`always` sans bloquer la boucle** (commit groupé) : chaque réponse est marquée de
l'offset du flux au moment de son exécution ; elle ne part que quand l'offset synchronisé
l'a dépassé. Les lectures qui ont vu une écriture pas encore synchronisée attendent aussi
(sinon un client lit une donnée qu'un crash ferait disparaître) ; une lecture qui n'en a
vu aucune part tout de suite. Ordre par connexion conservé. Tout ce qui arrive pendant un
fsync part dans le suivant : le regroupement se fait entre connexions, pipeline ou non.
Coût réel fixé par le disque : ~20–100 µs sur NVMe avec PLP, ~1–5 ms sur un SSD grand
public (vidage du cache du disque).

**Disque trop lent** : au-delà d'un plafond de tampon (~64 Mo), les **écrivains sont
parqués** jusqu'à ce que le disque rattrape ; les lectures continuent (Valkey bloque la
boucle entière).

## 7. Réécriture (compaction)

Multi-part comme Redis 7 (base + incréments + manifeste) :

1. Au T0 d'un BGSAVE (entre deux tours), ouvrir le segment `incr N+1` : le dump contient
   exactement les blocs jusqu'à la fin de `incr N` (seq dans son en-tête).
2. Dump écrit, fsync, `rename`, fsync du dossier.
3. Manifeste réécrit (fichier temporaire, `rename`, fsync du dossier) : base = ce dump,
   incréments = `N+1…`.
4. **Seulement ensuite**, supprimer les segments ≤ N.

Un crash entre 2 et 3 laisse l'ancien manifeste, valide (ancienne base + tous les
segments) ; entre 3 et 4, des segments en trop, ignorés. Déclenchement automatique :
`auto-aof-rewrite-percentage 100`, `auto-aof-rewrite-min-size 64mb` (Valkey), et
`BGREWRITEAOF`.

## 8. Reprise au démarrage

1. Lire le manifeste ; charger le dump de base ; **vérifier son condensé** (§4).
2. Rejouer les segments dans l'ordre, bloc par bloc : `art.set_now(bloc.now)`, puis les
   commandes par l'exécuteur normal.
3. Bloc invalide **en fin** du dernier segment (écriture coupée) : tronquer et continuer
   (`aof-load-truncated yes`) ; **au milieu** : refuser de démarrer.
4. Reprendre l'horloge réelle (toujours ≥ le dernier `now`).

## 9. Réplication par le même flux

- **Synchronisation complète** : le réplica reçoit un dump (BGSAVE, envoyé fichier ou en
  flux) avec son `seq`, puis les blocs suivants.
- **Reprise partielle** : un tampon circulaire en mémoire garde les derniers blocs
  (`repl-backlog-size`) ; un réplica qui revient avec son dernier `seq` repart de là s'il est
  encore dans le tampon, sinon synchronisation complète.
- **Application** : exactement le rejeu du §8 (`art.set_now(bloc.now)` puis les
  commandes). Le réplica **n'expire rien selon sa propre horloge** : son évicteur ne
  supprime que ce qui est expiré au dernier `now` appliqué ; ses lectures masquent une clé
  expirée sans la supprimer (le choix de Redis depuis 3.2).
- **`WAIT` et réplication synchrone** : un réplica acquitte des offsets ; le marquage des
  réponses du §6 sert tel quel — une réponse part quand `min(offset synchronisé, offset
  acquitté par N réplicas)` l'a dépassée (`min-replicas-to-write`). Le fsync groupé et la
  réplication synchrone sont le même mécanisme.

## 10. Paramètres (noms de Valkey)

`RADIXOX_APPENDONLY` (non par défaut), `RADIXOX_APPENDFSYNC` (`everysec`),
`RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE` / `_MIN_SIZE`, `RADIXOX_AOF_LOAD_TRUNCATED` (oui) ;
commande `BGREWRITEAOF` ; `INFO` : `aof_enabled`, `aof_rewrite_in_progress`,
`aof_last_write_status`, `aof_current_size`, `aof_base_size`, `aof_delayed_fsync`.
Plus tard : `REPLICAOF`, `repl-backlog-size`, `min-replicas-to-write`, `WAIT`.

## 11. Ordre d'implémentation

1. `exec/journal.rs` (`plan` / `finish`) et le flux en mémoire ; **oracle** : flux
   aléatoires, `état(serveur) == rejeu(flux)` sur un arbre vide, puis depuis un dump au T0.
2. Condensé du contenu dans le dump, vérifié au rechargement.
3. Format de bloc, segments préalloués, rejeu au démarrage, troncature de fin.
4. `appendfsync no` et `everysec`, worker fichiers, plafond de tampon.
5. Rotation au T0, manifeste, `BGREWRITEAOF`, réécriture automatique.
6. `always` : marquage des réponses par offset, un fsync en vol.
7. Réplication : synchronisation complète, tampon de reprise, `WAIT`.

## 12. Cas limites

- **`UNLINK prefix*` pendant une save** : journalisé au début ; si le T0 tombe pendant la
  libération, le dump voit déjà le sous-arbre détaché (invisible), cohérent avec le flux
  (l'`UNLINK` précède la coupure). Idem `FLUSHDB`.
- **Écriture refusée** (`MISCONF`, `NOAUTH`, `WRONGTYPE`) : réponse d'erreur → rien.
- **Connexion parquée** (KEYS) : lecture, rien.
- **Écrivains parqués par le plafond du tampon** : leurs commandes sont journalisées quand
  elles s'exécutent, pas quand elles arrivent : l'ordre du flux est l'ordre d'exécution.
- **Montée de version** : un flux d'une `version` différente n'est rejoué que si la
  nouvelle version le déclare compatible ; sinon **refus de démarrer**, avec la consigne de
  faire `BGREWRITEAOF` avec l'ancienne version avant la montée (jamais de perte silencieuse
  du journal).
- **Horloge qui recule** : `now` monotone (§5).
