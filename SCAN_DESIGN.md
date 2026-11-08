# SCAN — curseur côté serveur

Implémenté le 2026-10-18 : `oxidart/src/krange.rs` (`scan_keys`),
`oxidart/src/regex.rs` (`glob_match`, `glob_literal_prefix`),
`radixox/src/bin/resp_cmd/scan.rs` (table de curseurs).

## Pourquoi pas le modèle Valkey

Valkey ne garde aucun état : le curseur est une position dans la table de
hachage, avancée en bits inversés pour survivre aux redimensionnements (et,
depuis le `kvstore` de 7.2, le numéro de slot dans les bits faibles). Rien à
expirer, mais doublons possibles et COUNT seulement indicatif.

Chez nous un indice de nœud n'est pas stable (compactage HiSlab,
recompression). La position est donc **la dernière clé examinée**, gardée côté
serveur ; la reprise se fait strictement après, dans l'ordre de l'arbre :

- aucune clé rendue deux fois, toute clé présente du début à la fin du scan
  rendue une fois ;
- sans `COUNT`, pages de 1000 (Redis : 10, une simple indication chez lui).
  Chaque page redescend depuis la racine : à 10, un scan complet coûtait
  ~15× plus de CPU serveur (redis-py `scan_iter` n'envoie pas COUNT) ;
- `COUNT` exact : `COUNT` clés retenues par appel, sauf si le budget
  d'examen (`max(COUNT, 1000)` clés, expirées et rejetées comprises) est
  épuisé avant ;
- `MATCH prefix*` ne parcourt que le sous-arbre du préfixe ; un motif
  quelconque parcourt le sous-arbre de son préfixe littéral et teste chaque
  clé avec `glob_match` (pas de DFA à compiler à chaque appel).

Le client reçoit un entier, pas la clé : redis-py (`int(cursor)`) et go-redis
(`ParseUint`) refusent un curseur non numérique.

## Table de curseurs

Une seule liste doublement chaînée (indices u32) sert de free list et de LRU :

```
tête → [libres…] [actifs, du moins au plus récemment utilisé] ← queue
```

- appel sur un curseur actif : le slot va en queue ;
- scan terminé (réponse 0) : clé libérée, slot en tête ;
- nouveau scan qui ne finit pas en une page : la tête si elle est libre ou
  inactive depuis `RADIXOX_SCAN_IDLE_SECS` (génération +1), sinon la table
  grandit jusqu'à `RADIXOX_SCAN_MAX_CURSORS` ; au-delà, **le nouveau scan est
  refusé** (`ERR max number of SCAN cursors reached`), ceux en cours continuent.
  Ni arrêt du serveur ni purge de tous les curseurs : un client fautif ne doit
  pas pénaliser les autres.
- expiration paresseuse : un scan inactif n'est évincé que si son slot est
  demandé ; son client peut reprendre d'ici là.
- un scan fini en une page n'occupe aucun slot.
- plafond par connexion (`RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT`, 128) : un
  curseur appartient à la connexion qui l'a ouvert, même repris depuis une
  autre (pool, reconnexion). Une connexion à son plafond libère d'abord ses
  propres curseurs inactifs : balayage depuis la tête (les inactifs y sont)
  jusqu'au premier des siens, en libérant au passage les inactifs des autres
  croisés en route (évinçables de toute façon, chacun n'est parcouru qu'une
  fois). Sinon : `ERR max number of SCAN cursors for this connection
  reached`. Protège contre un client bogué qui abandonne ses scans ; pas
  contre un attaquant authentifié qui ouvre beaucoup de connexions.

Curseur envoyé : `gen × max + idx + 1` (`max` fixé au démarrage). Il reste
petit (peu de chiffres décimaux sur le réseau) tant que la table et les
générations le sont, ne vaut jamais 0 et reste ≤ `i64::MAX` (la génération
reboucle avant) pour les clients qui le lisent en signé. Un entrelacement PDEP
donnerait des tailles comparables sans dépendre de BMI2.

Curseur inconnu, terminé ou repris (génération différente) :
`ERR invalid or expired cursor`. Seul écart avec Redis, qui accepte tout
curseur ; repartir de 0 ferait boucler un client lent indéfiniment.

## HSCAN / SSCAN (2026-11-08)

`valart/src/scan.rs` (parcours), `oxidart` `sscan_page` / `hscan_page`,
`radixox/src/exec/scan.rs` (`cmd_sscan`, `cmd_hscan`). Même table de curseurs, mêmes
pages que SCAN (1000 sans COUNT, COUNT exact, budget d'examen `max(COUNT, 1000)`, MATCH
testé sur le membre / champ). Petit Set / Hash (bloc inline) : tout en une page, curseur
0, comme Redis avec ses listpacks.

Un Set ou un Hash est un trie valart sur le **hash** du membre, pas sur ses octets :
pas d'ordre lexicographique à suivre. Et une entrée vit au premier slot libre du chemin
de son hash, donc sa place dépend de l'histoire (une suppression remonte une feuille).
La position ne peut donc pas être une place dans le trie. Elle est **la dernière entrée
examinée**, comme pour SCAN, dans un ordre total qui ne dépend que du membre :
**clé d'ordre** = le hash lu dans l'ordre de la descente (`h0` en poids fort,
`h.swap_bytes()`), puis les octets du membre. Position envoyée au serveur de curseurs :
8 octets de clé d'ordre + le membre. Mêmes garanties que SCAN : aucune entrée rendue
deux fois, toute entrée présente du début à la fin rendue une fois, quelles que soient
les écritures entre deux pages.

Parcours : dans un nœud, slots pris par radix croissant (≤ 16 : tri sur place ; au-delà,
table radix → rang, O(256 + n) sans comparaison), en partant du radix de la position ;
les groupes avant sont sautés sans y descendre. L'entrée d'un slot à enfant partage
`h0..hd` avec son sous-arbre mais pas la suite : elle **flotte** dans le sous-arbre
jusqu'à sa place (au plus 7, une par niveau). Le hash de chaque entrée vue est recalculé
(le nœud n'en garde que le radix et l'octet de contrôle) ; les membres sont donc lus de
toute façon, et préchargés 8 slots à l'avance ; les nœuds enfants deux slots à l'avance,
puis leurs premiers membres un slot à l'avance (les deux étages du parcours de SCAN).

Mesure (2026-11-08, Set et Hash de 1M membres, scan complet, CPU serveur relevé dans
`/proc`, min de 3, contre Valkey 9.1) :

| | RadixOx | Valkey |
|---|---|---|
| SSCAN COUNT 1000 | 80 ms | 90 ms |
| HSCAN COUNT 1000 | 100 ms | 140 ms |
| SSCAN MATCH `*:7*` COUNT 1000 | 80 ms | 140 ms |
| SSCAN MATCH `*:7*` COUNT 10 | 170 ms | 610 ms |
| SSCAN COUNT 10 | 890 ms (100k appels) | 550 ms (72k appels) |

COUNT 10 : ~9 µs par appel contre 7,6 µs ; l'écart total vient surtout du COUNT exact
(Valkey rend ~14 éléments par appel). Le tri des slots de la racine à chaque page coûtait
1200 ms ; la table radix → rang l'a ramené à 890.

### Performances : chantier ouvert (prochaine session)

Jugement de l'utilisateur (2026-11-08) : **performances discutables**, à reprendre. Rien
n'a été profilé : les pistes ci-dessous sont des hypothèses, à vérifier par `perf`
avant de coder (mesurer avant d'affirmer).

Ce que coûte une page aujourd'hui :
- **redescente depuis la racine à chaque page**, avec construction de la table radix →
  rang (256 cases + n) à chaque nœud du chemin de la position ;
- **un hash recalculé (foldhash) pour chaque entrée vue**, alors que la clé d'ordre
  n'est utile que pour : les entrées flottantes (slot à enfant), le tri d'un groupe de
  plus d'une entrée, la comparaison à `from` sur le chemin de la position, et la
  position de la dernière entrée examinée. Ailleurs (groupes d'une seule entrée hors du
  chemin de `from`), le chemin donne déjà l'ordre : piste la plus prometteuse ;
- **copie de la page** : `sscan_page` / `hscan_page` clonent chaque membre (et valeur)
  dans un `Vec<SharedByte>` avant l'encodage ; SCAN encode depuis une `ScanPage` à plat.
  Encoder directement dans le tampon de sortie pendant le parcours ;
- MATCH testé après lecture du membre : normal (il faut le lire), mais le clone ne doit
  avoir lieu que pour les retenus (déjà le cas) ;
- coût fixe par appel (table de curseurs, encodage, réseau) : le comparer à celui de
  SCAN sur le même nombre d'appels pour savoir ce qui revient au parcours.

Pistes plus lourdes :
- **garder le chemin entre deux pages** (pile des nœuds) plutôt que redescendre : interdit
  par la règle d'or (ARCHITECTURE.md §1.1 : aucun idx de slab ne survit à un tour) sauf à
  le valider par un compteur de modifications par conteneur (ou global à valart), à
  ajouter à chaque mutation ; ne vaut que si la redescente pèse vraiment ;
- éviter le recalcul du hash en le stockant : 8 o par entrée, contraire aux choix mémoire
  de valart (le nœud ne garde que radix + contrôle) ; probablement non.

Méthode pour la reprise : Set et Hash de 1M membres (`member:{i}`, `field:{i}`), scan
complet COUNT 10 / 1000 / MATCH `*:7*`, CPU du serveur par `/proc/<pid>/stat` (ticks de
10 ms : préférer `perf stat -p` pour la précision), min de 3, Valkey 9.1 sur la même
machine ; puis `perf record` sur RadixOx pour répartir le coût par page (redescente,
hash, clone, encodage, curseur). Cible : COUNT 10 au moins au coût par appel de Valkey
(7,6 µs), COUNT 1000 nettement sous Valkey.

## Configuration

| Variable | Défaut | Effet |
|---|---|---|
| `RADIXOX_SCAN_IDLE_SECS` | `60` | inactivité après laquelle un curseur peut être repris |
| `RADIXOX_SCAN_MAX_CURSORS` | `65536` | curseurs actifs au plus (1 ≤ n < 2³²) |
| `RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT` | `128` | curseurs actifs au plus par connexion |

`INFO` expose `scan_cursors` (curseurs actifs).

## Écarté / reporté

- **BSCAN** (attendre un slot libre) : l'expiration ne produit aucun
  événement, il faudrait file d'attente + minuteur, et l'infrastructure des
  commandes bloquantes n'existe pas encore. Côté client natif : retry avec
  backoff sur l'erreur de plafond. À reconsidérer avec BLPOP.
- Curseur = clé brute envoyée au client : incompatible avec les clients qui
  parsent un entier.
