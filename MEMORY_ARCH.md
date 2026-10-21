# Mémoire v2 — allocateur maison, GC par évacuation, compactage, maxmemory

> **État : conception (2026-10-21), rien d'implémenté ; mesures de départ faites (§13.1).** Ce document rassemble les décisions
> prises en discussion le 2026-10-07 (allocation des `SharedByte`, GC) et le 2026-10-21
> (allocateur global d'abord, puis `maxmemory`). §15 liste ce qui reste ouvert, §14 l'ordre
> de travail. Il complète ARCHITECTURE.md §5 (anti-pic de latence) et remplace, pour la
> mémoire, les mentions « compaction des slabs et GC (prévus) ».
>
> Langue des documents et des commentaires du dépôt : **français**. README : anglais.

---

## 0. Décisions, en une page

| # | Décision | Pourquoi |
|---|---|---|
| M1 | **Le chemin chaud n'alloue que par bump.** Le travail lourd (GC, compactage, éviction) se fait dans les creux de la boucle, ou sous pression mémoire | le disque passe par io-wq, la boucle est souvent au repos : c'est notre avantage sur Valkey, dont la défragmentation active vole du temps au thread principal |
| M2 | **Un seul mécanisme d'allocation : bump + compteur d'objets vivants par page**, sans en-tête de page, sans atomique ni verrou | mono-thread ; libérer = décrémenter ; une page à 0 est réutilisée telle quelle ; un objet qui survit au tour (envoi en vol) garde simplement sa page |
| M3 | **Des pools par durée de vie**, tous à nous sauf les slabs ; l'allocateur système ne sert que de repli, compté | chaque pool a sa politique (taille de page, GC ou non) et son compte exact |
| M4 | **Plage virtuelle réservée une fois, chunks de 2 Mio en huge pages, pages de 64 Kio** | pression TLB réduite ; aucun `mmap` sur le chemin chaud ; on rend la mémoire par chunks entiers |
| M5 | **On alloue dans les chunks les plus récents** (file triée par génération), avec un curseur séparé pour les survivants du GC | les vieux chunks se vident d'eux-mêmes ; les allocations neuves tombent sur des lignes chaudes ; les survivants restent denses |
| M6 | **GC par évacuation, parcours linéaire des slabs**, sans pointeur arrière | zéro octet par objet ; séquentiel et préchargeable ; le parcours remplace la réflexivité pointeur → propriétaire |
| M7 | **Compactage HiSlab** : `insert(pop du dernier)` par lots de 8, en pipeline de préchargement | la slab des nœuds se compacte presque gratuitement, sa fin se rend par chunks |
| M8 | **`maxmemory` après l'allocateur**, sur ses comptes exacts ; victimes par CLOCK ; GC avant éviction ; la réserve du GC est **dans** le budget | `maxmemory` borne la mémoire réservée par les données, pas seulement les octets demandés |

---

## 1. Point de départ (code au 2026-10-21)

Trois sources de mémoire aujourd'hui :

| source | quoi | où |
|---|---|---|
| mimalloc, appelé directement | `SharedByte`, pages des nœuds, `CompactPath`, blocs `HashSmall` / `SetSmall`, débordements valart | `radixox-lib/src/raw_alloc.rs` (`mi_malloc` direct, pas `mi_malloc_aligned`) |
| `HiSlab` (mmap + `MADV_HUGEPAGE`) | nœuds oxidart de 128 o, slab valart | crate `hislab` (`~/rust/hislab`), slots libres suivis par un arbre de bitmaps |
| glibc (pas de `#[global_allocator]` dans le binaire) | tout le reste : `Vec`, buffers des connexions, journal, réplication | — |

Mesures du 2026-10-09 (mimalloc seul, 1 Gio de valeurs) : surcoût de +40 % (8–32 o) à
+10 % (≥ 100 o), plancher dû aux classes de taille espacées de ~12,5 % ; l'en-tête
`SharedByte` de 6 o (`[len u32 | rc u16 | data]`) s'y ajoute. **Après des suppressions
dispersées (50 % ou 90 % au hasard), 0 octet n'est rendu au système**, même après
`mi_collect(true)` : les pages de 64 Kio ne sont jamais entièrement vides. Le RSS reste au
pic historique. C'est le problème que ce chantier résout.

---

## 2. Principe : chemin chaud trivial, travail dans les creux

- **Allocation** : avancer un pointeur, incrémenter le compteur de la page.
- **Libération** : retrouver la page par l'adresse (`(addr - base) >> 16`), décrémenter.
- **Tout le reste** (préparer les chunks, les rendre, évacuer, compacter, évincer) se fait :
  1. **dans les creux** : la boucle n'a ni CQE ni connexion prête ; budget illimité, mais
     par **tranches courtes** (~50–100 µs) avec, entre deux tranches, une lecture de la
     queue du CQ (mémoire partagée, sans syscall) : dès qu'un CQE attend, on rend la main.
     Le surcoût vu par la première requête après un creux est borné à une tranche ;
  2. **sous pression** : un budget minimal par tour, proportionnel aux octets alloués depuis
     le tour précédent (un *pacer*). Sans lui, un serveur saturé en continu, qui n'a jamais
     de creux, dériverait jusqu'à la limite.
- **Une tâche lourde à la fois.** Priorité : rendre les chunks vides (presque gratuit) →
  compacter → évincer (seulement si les octets vivants dépassent le budget). Rien de tout
  ça pendant une save (elle tient des idx et des pointeurs, ARCHITECTURE §5).

---

## 3. Socle : plage réservée, chunks, générations

- **Au démarrage**, chaque pool réserve une grande plage virtuelle contiguë, alignée sur
  2 Mio (réserver 2 Mio de plus et aligner). La réserve ne coûte rien tant qu'on n'y touche
  pas. Le pool d'une adresse se lit par comparaison de plage : `free` n'a pas besoin qu'on
  lui passe le pool.
- **Chunk** = 2 Mio = 32 pages de 64 Kio. Les compteurs de pages vivent dans un `Vec` à
  part (pas d'en-tête dans la page) : 32 × `u16` = 64 o, **une ligne de cache par chunk**.
- **Génération** : un numéro croissant attribué à chaque chunk quand il est mis en service.
  Les chunks qui ont des pages libres sont rangés par génération ; on alloue dans le plus
  récent (on prend en tête, pas besoin d'un tas binaire).
- **Préparer le chunk suivant hors du chemin chaud** : la première écriture dans un chunk
  neuf coûte une faute de page et la mise à zéro de 2 Mio (de l'ordre de 200 µs à
  ~10 Go/s). On garde toujours au moins un chunk d'avance, préparé par
  `MADV_POPULATE_WRITE` (à vérifier : `IORING_OP_MADVISE` l'accepte-t-il ? sinon un petit
  thread auxiliaire, qui ne touche à aucune donnée partagée). Si la préparation est en
  retard, on retombe sur la faute synchrone, comptée dans `INFO`.
- **Rendre un chunk** : seulement quand ses 32 pages sont vides (rendre 64 Kio casserait la
  huge page), par `MADV_DONTNEED` envoyé par io_uring, donc asynchrone. **Hystérésis** :
  ne rendre que si les chunks libres dépassent `max(2, 2 % des chunks utilisés)`, sinon un
  serveur qui oscille autour d'une frontière libère et refait des fautes de 2 Mio en boucle.
- **Réserve permanente** : au moins un chunk pour l'allocation et un pour la destination du
  GC (§8.3).

---

## 4. Le mécanisme commun : bump + compteur par page

- Page courante : pointeur `top`, fin `end`. Si l'objet tient : `top += taille`,
  `vivants[page] += 1`. Sinon : on jette la fin de page (**pas d'objet à cheval sur deux
  pages**) et on prend la page libre suivante.
- `free(p)` : `vivants[page(p)] -= 1` ; à 0, la page retourne dans la liste libre de son
  chunk ; un chunk dont les 32 pages sont libres devient candidat au rendu (§3).
- **Agrandir sur place** si l'objet est le dernier alloué de la page courante (`top`
  recule ou avance) : utile aux buffers qui grossissent.
- Compter des **objets** (pas des octets) suffit pour savoir quand une page est vide ; le
  GC a besoin des **octets vivants** pour classer les pages (§8.2) : compteur en granules
  de 2 o (32 768 granules par page, tient dans un `u16`, pas de piège off-by-one).

---

## 5. Les pools

| durée de vie | pool | taille de page | quand la page se vide mal | ce qu'il sert |
|---|---|---|---|---|
| un tour ou presque | **court** | 4–16 Kio | rien : tout meurt vite | débordements des `SmallVec` d'arguments (`Cmd`, radixox-lib), messages de réplication (`RxBlock`, `RxLoadData`…), temporaires |
| variable, grossit | **buffers** | moyenne, agrandissement sur place | rendus quand vides (§7) | `out`, `sending`, `rx`, `spans` des connexions |
| longue, données | **`SharedByte`** | 64 Kio | GC (§8) | valeurs, champs et membres |
| longue, données, un seul propriétaire | **blocs de structure** | 64 Kio | GC (§8.6) | pages des nœuds, `CompactPath`, blocs `HashSmall` / `SetSmall`, débordements valart |
| longue, > 64 Kio | **grands objets** | N pages contiguës, ou N chunks au-delà de 2 Mio | jamais évacués | grosses valeurs |
| fixe | **slabs** (hors allocateur) | 128 o | compactage (§9) | nœuds oxidart, nœuds valart |
| bornée par configuration | **système**, non compté dans `maxmemory` | — | — | AOF en attente (jusqu'à 64 Mio en `continuous`), backlog de réplication, buffers de save |
| inconnue | **repli** : mimalloc en `#[global_allocator]`, compté | — | — | std, crates tierces, démarrage |

**Garde-fou du pool court** : un objet qui vit longtemps, aiguillé là par erreur, épingle
toute sa page. `INFO` affiche les pages du pool court vivantes depuis plus de N tours :
une erreur d'aiguillage se voit tout de suite.

**Router par durée de vie** : `GlobalAlloc::alloc(layout)` ne reçoit que taille et
alignement, il ne sait pas qui appelle. Les pools s'appellent donc explicitement : nos
types (`SharedByte`, `Node`, `CompactPath`, buffers) appellent leur pool comme `raw_alloc`
le fait déjà ; pour les collections, `allocator-api2` (`Vec<T, A>`, `hashbrown`) sur
stable. On écarte un « pool courant » posé dans une variable de thread : une allocation qui
s'en échappe serait une lecture de mémoire libérée, sans aide du compilateur.

**Migration par étapes** : pendant la transition, `free` aiguille par plage d'adresse ; un
pointeur hors de nos plages part chez mimalloc. On peut donc migrer un type à la fois.

---

## 6. Pool `SharedByte`

- **Règle d'unicité** (2026-10-07) : un `SharedByte` n'a qu'**un seul emplacement dans
  l'arbre** ; les seules autres copies sont temporaires (I/O, réponses en vol, Pub/Sub).
  `COPY`, `SUNIONSTORE`, etc. copient en profondeur. C'est ce qui permet au GC de réécrire
  l'unique pointeur.
- **En-tête compact** (proposé le 2026-10-07, à mesurer) : pointeur sur les données, en-tête
  lu à reculons, `[len_hi u16 si long][len_lo u16, bit 15 = forme longue][rc u16][data…]`.
  rc à `ptr − 2` (offset fixe, pas de branche au clone / drop), longueur à `ptr − 4`.
  Alignement et granule de 2 o.
- **Allouer à la bonne place dès le parsing** : un `SET` copie la valeur de l'anneau de
  réception directement dans le pool données (une seule copie). Une valeur qui meurt vite
  (commande refusée, `UNLINK` d'arguments) laisse un trou dans une page jeune : c'est le
  cas normal du générationnel, le GC voit ces pages se vider seules.
- **Objets moyens** : avec des pages de 64 Kio et pas d'objet à cheval, un objet de 40 Kio
  peut gaspiller jusqu'à 40 Kio en fin de page. Un curseur bump séparé pour les objets au
  delà de quelques Kio (seuil à mesurer) évite de sacrifier les fins de page des petits.
- **Grands objets (> 64 Kio)** : jamais un `mmap` par objet (syscall sur le chemin chaud).
  N pages contiguës d'un chunk jusqu'à 2 Mio, N chunks contigus de la plage au-delà. Jamais
  évacués : copier 1 Mio pour compacter ne rapporte rien.
- **Reload** : le dump est rechargé en masse ; ses valeurs vont directement dans les chunks
  « survivants » et sortent parfaitement denses. Un redémarrage défragmente.

---

## 7. Buffers de connexion

- **Réception** : déjà fait. `BufPool` (`radixox/src/uring/bufs.rs`) est un anneau partagé
  par toutes les connexions, avec un compteur par tampon : c'est déjà le schéma bump +
  compteur.
- **v1, règle simple** : après le CQE d'un envoi, si `sending` est vide et que sa capacité
  dépasse un seuil (~16 Kio), on le rend ; idem pour `out`, `rx` et `spans`. Le cas d'un
  buffer sur cinq qui vit longtemps et en épingle cinq disparaît.
- **v2, envoi symétrique de la réception** : les réponses s'écrivent dans des morceaux pris
  dans un pool partagé, comptés ; la connexion garde la liste de ses morceaux, les envoie
  par `sendmsg` sur plusieurs iovecs, chaque morceau est rendu à son CQE. Une connexion
  inactive ne coûte plus rien. Prépare `SEND_ZC` (qui devra tenir un clone, donc un rc, de
  tout `SharedByte` envoyé sans copie : il est épinglé jusqu'au CQE).

---

## 8. GC par évacuation (pool `SharedByte`, puis blocs de structure)

### 8.1 Parcours

**Linéaire sur les slabs** (décision du 2026-10-21, remplace le curseur lexicographique) :
nœuds HiSlab dans l'ordre des slots, leurs pages, puis la slab valart. Séquentiel, facile à
précharger (file de nœuds façon AMAC), sans logique métier. Curseur = index de slot.

- Une recompression peut déplacer une valeur d'un nœud situé après le curseur vers un nœud
  situé avant : elle est ratée par cette passe. Sa page ne tombe pas à 0, la passe
  suivante la rattrape. Accepté.
- **Sous-arbres détachés** (`UNLINK prefix*`, `FLUSHDB`) : le parcours y passe aussi. Sans
  danger : `free_detached` (`oxidart/src/lib.rs`) relit les valeurs dans les nœuds au
  moment de libérer, il libère donc la nouvelle adresse. Au pire on a copié une valeur qui
  allait mourir.

### 8.2 Choisir les pages

Au début d'une passe, on **marque un lot** de pages creuses (octets vivants sous un seuil),
on n'alloue plus jamais dedans, et chaque pointeur rencontré qui tombe dans une page marquée
est recopié vers le curseur des survivants, puis réécrit. Une passe coûte un parcours
complet de l'arbre quel que soit le nombre de pages : il faut donc marquer **un gros lot**
pour amortir.

### 8.3 Réserve

Sans pointeur arrière, une page marquée ne se vide en général qu'à la fin de la passe (son
premier objet peut être au début de la slab, le dernier à la fin). Évacuer B pages de
densité d demande donc **B·d pages libres d'avance**. Exemple : 10 Mio de réserve, pages à
80 % → une passe évacue 12,5 Mio et rend 2,5 Mio. Une page dont le compteur tombe à 0 en
cours de passe peut aussitôt servir de destination (le compteur le dit), mais on ne compte
pas dessus. Viser ~10 Mio de réserve sous forte charge.

### 8.4 Objets épinglés (rc > 1)

Un `SharedByte` tenu ailleurs (réponse en vol, Pub/Sub, commande en cours) ne peut pas être
déplacé. Heuristique : on garde l'**index du nœud et de l'entrée** (instable), puis plus
tard on vérifie que le slot est vivant (`is_occupied` de hislab) et que l'entrée contient
**exactement le même pointeur**. C'est sûr parce qu'**on n'alloue jamais dans une page
marquée** : une adresse égale désigne forcément le même objet (pas d'ABA). Si la
vérification échoue (recompression, nœud compacté), la passe suivante le rattrape. Coût :
un accès au nœud par objet épinglé, sans copie de clé ni descente.

### 8.5 Interdits

- Pas de GC pendant une save.
- Rien de daté ne survit à une tranche (règle d'or, ARCHITECTURE §1.1) : le GC ne garde
  entre deux tranches que son curseur (index de slot, toléré parce que le parcours ne
  promet rien) et ses indices épinglés vérifiés à la relecture.
- Tout porteur d'un pointeur vers les octets d'un `SharedByte` au-delà du tour doit tenir
  un clone (rc). Aucune iovec brute vers une valeur sans rc.

### 8.6 Blocs de structure

Pages des nœuds, `CompactPath`, blocs `HashSmall` / `SetSmall`, débordements valart : un
seul propriétaire chacun, que le parcours linéaire visite **avant** d'atteindre le bloc
(le nœud qui le pointe). Ils sont plus simples à évacuer que les `SharedByte` (pas de rc).
Les pages de nœuds sont redimensionnées en place : réallocation = nouvelle adresse dans le
pool, comme une évacuation.

---

## 9. Compactage HiSlab

Une tâche de fond fait `insert(pop du dernier nœud)` : le dernier slot occupé part dans le
premier slot libre, la fin de la slab se rend par chunks de 2 Mio (THP).

### 9.1 Références à mettre à jour

- le **parent** : O(1) grâce à `parent_idx` ;
- le `parent_idx` de **chaque enfant** (jusqu'à 256 en `Wide256`) ;
- `root_idx`, les **tags TTL** (bitmap tagué), le futur **tableau CLOCK** (§10), les indices
  épinglés du GC (tolérés, §8.4).

### 9.2 Pipeline (lots de 8)

| tour | précharge | travail fait pendant l'attente | dépend de |
|---|---|---|---|
| 1 | 8 derniers nœuds (2 lignes chacun) + 8 slots libres (en écriture) | — | rien |
| 2 | 8 parents (2 lignes) + enfants des 8 nœuds | **copie des 8 nœuds** (source et destination déjà en cache) | tour 1 |
| 3 | emplacement exact de l'index de l'enfant chez chaque parent (cellule inline ou page) | réécriture des `parent_idx` des enfants | tour 2 |
| 4 | — | réécriture des index chez les parents, tags TTL, CLOCK | tour 3 |

En régime établi, on recouvre aussi d'un lot à l'autre : préchargements du lot k+1 pendant
la copie du lot k et les réécritures du lot k−1. Les slots libres se prennent dans l'arbre
de bitmaps de hislab (pas de liste chaînée dans les slots, donc pas de sauts dépendants).

**Limite matérielle** : un cœur ne garde qu'un nombre limité de lignes en cours de
chargement (de l'ordre de 10 à 20 selon le CPU). 8 nœuds × 2 lignes l'atteignent déjà :
les enfants d'un `Wide256` passent par lots séparés dans le même pipeline.

### 9.3 Pièges

- **Parent ou enfant dans le même lot** : les derniers slots sont souvent des nœuds créés
  ensemble. Avant toute réécriture, traduire chaque index (parent, enfant) par la table
  ancien → nouveau du lot (8 entrées, recherche linéaire), sinon on écrit dans un slot
  qui vient d'être libéré. Test dédié : une chaîne parent → enfant → petit-enfant dans les
  8 derniers slots.
- **Jobs `Detached`** (`UNLINK prefix*`, `FLUSHDB`) : leur pile garde des index de nœuds,
  et un nœud empilé peut avoir un parent déjà libéré (`parent_idx` vers un slot mort, peut
  être déjà réattribué). Déplacer ce nœud écrirait dans ce slot : corruption silencieuse.
  **v1 : pas de compactage tant qu'un job `Detached` est en cours**, comme pendant une
  save.

---

## 10. maxmemory

### 10.1 Ce qu'on compte

- `used_data` : **pages réservées** des pools données (`SharedByte`, blocs de structure,
  grands objets) + slots vivants des slabs × 128 o. C'est ce que borne `maxmemory`.
- `used_service` : pools court et buffers, repli. Bornés par leurs propres limites
  (buffers de sortie par client, plafond de l'AOF), **pas** par l'éviction : évincer des
  clés pour un client lent n'a pas de sens.
- Système (AOF, backlog, save) : non compté, borné par configuration, affiché.
- `INFO` : par pool, octets vivants et pages réservées, fragmentation exacte
  (réservé / vivant), RSS, chunks prêts et rendus, passes de GC, octets évacués, objets
  épinglés, fautes de page synchrones.

### 10.2 Victimes : CLOCK

Les feuilles inline n'ont pas d'en-tête, et 4 o par clé coûteraient 5 à 10 % sur des
petites clés. CLOCK (seconde chance) : un bit de référence par clé, mis à 1 à l'accès ; un
curseur balaie les nœuds (même mécanique que le futur évicteur TTL à curseur) : bit à 1 →
remis à 0 ; bit à 0 → évincée. Pas de tirage, pas de pool de candidats, pas de tri.

Le bit vit **hors du nœud** : un `u64` par nœud dans un tableau à part, bit `radix % 64`
(le radix d'une entrée est stable, son rang ne l'est pas). 8 o par nœud de 128 o ; en
`Wide256`, 4 clés partagent un bit. Hors du nœud, poser un bit pendant une save ne copie
pas le nœud. Coût : une ligne de cache de plus par lecture, à précharger avec le nœud dans
les descentes entrelacées (à mesurer).

Politiques v1 envisagées : `noeviction`, `allkeys-lru`, `volatile-lru` (CLOCK),
`volatile-ttl` (bitmap TTL existant). LFU plus tard (compteur à 2 bits, ou CLOCK-Pro).

### 10.3 Déclenchement

- **Deux niveaux** : l'éviction de fond démarre à `maxmemory − réserve − marge` ; la
  commande ne fait que tester `used_data > maxmemory`, n'évince elle-même (petit budget) que
  si le fond est dépassé, et ne répond `-OOM` que si rien n'est évinçable ou en
  `noeviction`.
- **La réserve du GC (§8.3) est dans le budget** : à la limite sans réserve, on ne peut plus
  compacter et on évince des données qu'un GC aurait gardées.
- **GC avant éviction** ; n'évincer que si les octets vivants dépassent le budget.
- **Entre deux victimes froides, prendre celle du chunk le plus vide** : l'éviction aide le
  GC au lieu de trouer la mémoire au hasard.
- Grosses valeurs évincées : détachées puis libérées par tranches (mécanisme d'`UNLINK`).

---

## 11. Huge pages et noyau

- THP doit être en mode `madvise` ou `always` (`/sys/kernel/mm/transparent_hugepage/enabled`).
  Sinon tout marche, en pages de 4 Kio : à afficher dans `INFO` et au démarrage.
- Réservation : `MAP_NORESERVE`. Sous `vm.overcommit_memory=2`, la réservation compte dans
  la limite de commit : alternative `PROT_NONE` puis `mprotect` par chunk, au moment de la
  préparation (hors chemin chaud).
- `MADV_DONTNEED` d'un chunk entier aligné rend la huge page ; un `MADV_DONTNEED` partiel la
  casserait.

---

## 12. Tests

Le GC est la pierre angulaire du projet : il lui faut au moins le filet de la save sous
mutation (SAVE_DESIGN.md).
- **Mode stress** : GC et compactage à **chaque tranche**, branchés sous les tests
  différentiels contre l'oracle `oxidart-legacy` et les tests de modèle (`BTreeMap`).
- **Vérificateur debug** après chaque tranche : recompte des octets et objets vivants de
  chaque page ; chaque pointeur de l'arbre tombe sur un objet vivant d'une page non rendue ;
  chaque `parent_idx` et index d'enfant cohérents ; tags TTL et CLOCK suivent leurs nœuds.
- **Cas dédiés** : chaîne parent → enfant → petit-enfant dans un même lot de compactage ;
  objet épinglé pendant toute une passe puis relâché ; recompression qui fait passer une
  valeur derrière le curseur ; `UNLINK prefix*` en cours pendant le GC.
- **AddressSanitizer**, procédure de SAVE_DESIGN.md.
- Faux noyau (`radixox/src/uring/tests.rs`) : préparation et rendu de chunks asynchrones,
  tranches interrompues par un CQE.

---

## 13. Mesures

**Avant de commencer** (machine au repos, profil `performance`) :
1. allocations par commande (allocateur global qui compte, profil `profiling`, memtier
   GET/SET pipeline 1 et 16, quelques secondes) : nombre et tailles ;
2. part de `malloc` / `free` / `mi_*` dans le profil CPU (`perf record`) ;
3. octets par clé et RSS après suppressions aléatoires (50 %, 90 %), à comparer aux
   chiffres mimalloc de §1.

Le résultat fixe l'ordre : si les temporaires pèsent, le pool court passe avant ; sinon le
pool `SharedByte` d'abord (le gros de la RAM).

### 13.1 Mesures de départ (2026-10-21)

Machine au repos, profil `performance`, build `profiling`, serveur sous `systemd-run`
(`MemoryMax=6G`), `RADIXOX_SAVE=""`, AOF coupé. memtier : `-t 2 -c 25 -d 100`,
`--distinct-client-seed --randomize`, 1 M clés.

**Allocations par commande.** Build `--cfg radixox_sysalloc` (toutes les allocations, y
compris celles de `raw_alloc`, passent par la libc) et une bibliothèque `LD_PRELOAD` qui
compte `malloc` / `calloc` / `realloc` / `free` par classe de taille, remise à zéro et
relevée par signal autour d'un nombre fixe de requêtes (1 M, 200 k pour GET pipeline 1).

| charge | allocations / commande | dont | `realloc` / commande |
|---|---|---|---|
| SET, clés neuves, pipeline 16 | 2,31 | 1 × ≤ 128 o (la valeur), **1 × ≤ 8 o** (la réponse), 0,3 × ≤ 32 o (structure) | 0,007 |
| SET, écrasement, pipeline 16 | 2,01 | 1 × ≤ 128 o, 1 × ≤ 8 o | 0,007 |
| GET, pipeline 16 | 0,006 | buffers | 0,007 |
| GET, pipeline 1 | 0,11 | buffers (16 o – 2 Kio) | 0,10 |
| 1 SET : 10 GET, pipeline 16 | 0,19 | ≈ les SET | 0,007 |

Le GET n'alloue presque rien. Le SET alloue sa valeur, qui vit, et **un temporaire de 8 o :
la réponse `+OK`**, construite par `Frame::SimpleString(SharedByte::from_slice(b"OK"))`
(`exec/string.rs`, 9 endroits de ce genre dans `exec/`). **Corrigé le 2026-10-21** :
`SharedFrame::SimpleString` porte un `&'static str` ; recompté sur le SET par écrasement,
2,01 → 1,006 allocation par commande (ne reste que la valeur). En dehors de lui, les temporaires du chemin
chaud sont négligeables : **le pool court n'est pas prioritaire**.

**Part de l'allocateur dans le CPU** (`perf record -F 1999` sur le processus, 4 s, build
normal donc mimalloc) : fonctions `mi_*`, `malloc`, `free`, `realloc`, noyau compris dans le
total.

| charge | débit | allocateur | noyau |
|---|---|---|---|
| SET, écrasement, pipeline 16 | 1,42 M ops/s | **8,3 %** | 35,9 % |
| 1 SET : 10 GET, pipeline 16 | 1,70 M ops/s | 1,4 % | 41,9 % |

Sur les écritures, un bump (avancer un pointeur, décrémenter un compteur) peut reprendre
l'essentiel de ces 8 % ; sur un mélange courant, le gain CPU est faible.

**Octets par clé et RSS après suppressions** (clés `key:0000000` à `key:0999999` chargées
par `redis-cli --pipe`, puis `DEL` d'une moitié tirée au hasard, puis de 80 % du reste,
puis rechargement complet ; RSS lu dans `/proc/<pid>/status`) :

| valeur | RSS à vide | 1 M clés | o / clé | 500 k clés | 100 k clés | o / clé vivante à 100 k | rechargé à 1 M |
|---|---|---|---|---|---|---|---|
| 100 o | 8,4 Mo | 141,1 Mo | 135 | 141,1 Mo | 141,1 Mo | **1 358** | 155,4 Mo |
| 16 o | 8,3 Mo | 63,2 Mo | 56 | 63,3 Mo | 63,3 Mo | **562** | 63,3 Mo |

**Aucun octet n'est rendu** : après la suppression de 90 % des clés, le RSS est celui du
plein, soit 10 fois la mémoire vivante. Le rechargement réutilise les trous, mais pas tous
(+10 % à 100 o). C'est le chiffre que ce chantier doit faire tomber : RSS à 100 k clés
proche de 100 k × 135 o + la réserve.

**Conséquence sur l'ordre** : on garde §14. Pool `SharedByte` d'abord (la mémoire), puis
le GC ; le pool court plus tard. La réponse `+OK` est corrigée à part (ci-dessus).

**Critères à chaque étape** : octets par clé à nombre de clés fixe ; RSS après
suppressions aléatoires puis une passe de GC ; débit et p99.9 identiques à mimalloc sur
`scripts/bench_vs_valkey.sh` ; latence de la première requête après un creux (borne =
une tranche).

---

## 14. Ordre de travail

0. Mesures de départ (§13).
1. **Socle** : plage réservée, chunks de 2 Mio avec génération, préparation et rendu
   asynchrones, réserve, comptes par pool ; repli mimalloc compté en `#[global_allocator]`.
2. **Pool `SharedByte`** : bump + compteurs, sans GC ; une page n'est réutilisée que vide.
3. **Grands objets** (> 64 Kio).
4. **Pool court**, puis **buffers de connexion** rendus quand vides (§7 v1).
5. **GC par évacuation** (§8), curseur des survivants, réserve, épinglés ; puis blocs de
   structure (§8.6).
6. **Compactage HiSlab** (§9).
7. **`maxmemory`** (§10).
8. Envoi par morceaux partagés (§7 v2).

---

## 15. Questions ouvertes

- Contenu des gros conteneurs valart entre deux tranches de GC (non ordonné, peut être
  restructuré entre deux tranches) ; on accepte un peu plus de fragmentation pour eux.
- Partage légitime d'un `SharedByte` à deux endroits : à éliminer partout (copie profonde),
  à auditer commande par commande.
- Seuil des objets moyens (§6) et taille de page du pool court (§5).
- `IORING_OP_MADVISE` avec `MADV_POPULATE_WRITE` (§3).
- Coût du bit CLOCK sur un bench de GET (§10.2), avec et sans préchargement.
- Taille des tranches et seuil de densité des pages marquées (§8.2), à régler sur mesures.
