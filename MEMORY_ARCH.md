# Mémoire v2 — allocateur maison, GC par évacuation, compactage, maxmemory

> **État : conception terminée (2026-10-22) ; v1 faite le 2026-10-23 : étapes 1 à 8 de §16.10, **pool et GC actifs par défaut** (`--cfg radixox_nopool` pour revenir à mimalloc), avec page la plus occupée, marquage par pages au repos et tranches à travail fixe (§16.10, étape 6). La spécification de la v1, à suivre telle quelle, est en §16.** Mesures de départ : §13.1, §13.2. Ce document rassemble les décisions
> prises en discussion le 2026-10-07 (allocation des `SharedByte`, GC) et le 2026-10-21
> (allocateur global d'abord, puis `maxmemory`), complétées le 2026-10-22 (objets moyens §6,
> sélection et réserve du GC §8.2–8.3, noyau §11). §15 liste ce qui reste ouvert, §14 l'ordre
> de travail. Il complète ARCHITECTURE.md §5 (anti-pic de latence) et remplace, pour la
> mémoire, les mentions « compaction des slabs et GC (prévus) ».
>
> Langue des documents et des commentaires du dépôt : **français**. README : anglais.

**Vocabulaire** (fixé le 2026-10-22) :

| mot | taille | ce que c'est |
|---|---|---|
| **bloc** | 64 Kio | unité du bump et du compteur ; unité du GC niveau A |
| **page** | 2 Mio | une huge page : 32 blocs ; unité de préparation, de rendu au système et du GC niveau B |
| granule | 2 o | unité d'alignement et des compteurs |
| pages de 4 Kio / de 1 Gio | — | pages du noyau, toujours avec leur taille |
| pages des nœuds | — | terme d'oxidart (feuilles inline, slotted page), sans rapport |

---

## 0. Décisions, en une page

| # | Décision | Pourquoi |
|---|---|---|
| M1 | **Le chemin chaud n'alloue que par bump.** Le travail lourd (GC, compactage, éviction) se fait dans les creux de la boucle, ou sous pression mémoire | le disque passe par io-wq, la boucle est souvent au repos : c'est notre avantage sur Valkey, dont la défragmentation active vole du temps au thread principal |
| M2 | **Un seul mécanisme d'allocation : bump + compteur d'objets vivants par bloc**, sans en-tête de bloc, sans atomique ni verrou | mono-thread ; libérer = décrémenter ; un bloc à 0 est réutilisé tel quel ; un objet qui survit au tour (envoi en vol) garde simplement son bloc |
| M3 | **Des pools par durée de vie**, tous à nous sauf les slabs ; l'allocateur système ne sert que de repli, compté | chaque pool a sa politique (taille de bloc, GC ou non) et son compte exact |
| M4 | **Plage virtuelle réservée une fois, pages de 2 Mio en huge pages, blocs de 64 Kio** | pression TLB réduite ; aucun `mmap` sur le chemin chaud ; on rend la mémoire par pages entières |
| M5 | **On alloue dans les pages les plus récentes** (file triée par génération), avec un curseur séparé pour les survivants du GC | les vieilles pages se vident d'elles-mêmes ; les allocations neuves tombent sur des lignes chaudes ; les survivants restent denses |
| M6 | **GC par évacuation, parcours linéaire des slabs**, sans pointeur arrière | zéro octet par objet ; séquentiel et préchargeable ; le parcours remplace la réflexivité pointeur → propriétaire |
| M7 | **Compactage HiSlab** : `insert(pop du dernier)` par lots de 8, en pipeline de préchargement | la slab des nœuds se compacte presque gratuitement, sa fin se rend par pages |
| M8 | **`maxmemory` après l'allocateur**, sur ses comptes exacts ; victimes par CLOCK ; GC avant éviction ; la réserve du GC est **dans** le budget | `maxmemory` borne la mémoire réservée par les données, pas seulement les octets demandés |
| M9 | **GC façon G1 / Immix à deux niveaux**, une seule passe : (A) compacter les **blocs** creux pour ne pas grandir, la mémoire est gardée ; (B) vider des **pages** entières, blocs denses compris, pour baisser le RSS quand l'excédent tenu dure. Réserve = `max(2 pages, vivants/16)` | un bloc plein et immortel épingle toute sa page : rendre la mémoire se décide au niveau de la page, pas du bloc ; recopier au bump rend le placement trivial |
| M10 | **THP 2 Mio par défaut, préparées hors de la boucle** ; hugetlbfs (2 Mio puis 1 Gio) en option plus tard | aucune faute de page dans la boucle ; portée TLB ~6 Gio en 2 Mio (Zen 4), au-delà un coût régulier et non des pics |

---

## 1. Point de départ (code au 2026-10-21)

Trois sources de mémoire aujourd'hui :

| source | quoi | où |
|---|---|---|
| mimalloc, appelé directement | `SharedByte`, pages des nœuds, `CompactPath`, objets `HashSmall` / `SetSmall`, débordements valart | `radixox-lib/src/raw_alloc.rs` (`mi_malloc` direct, pas `mi_malloc_aligned`) |
| `HiSlab` (mmap + `MADV_HUGEPAGE`) | nœuds oxidart de 128 o, slab valart | crate `hislab` (`~/rust/hislab`), slots libres suivis par un arbre de bitmaps |
| glibc (pas de `#[global_allocator]` dans le binaire) | tout le reste : `Vec`, buffers des connexions, journal, réplication | — |

Mesures du 2026-10-09 (mimalloc seul, 1 Gio de valeurs) : surcoût de +40 % (8–32 o) à
+10 % (≥ 100 o), plancher dû aux classes de taille espacées de ~12,5 % ; l'en-tête
`SharedByte` de 6 o (`[len u32 | rc u16 | data]`) s'y ajoute. **Après des suppressions
dispersées (50 % ou 90 % au hasard), 0 octet n'est rendu au système**, même après
`mi_collect(true)` : les pages mimalloc de 64 Kio ne sont jamais entièrement vides. Le RSS reste au
pic historique. C'est le problème que ce chantier résout.

---

## 2. Principe : chemin chaud trivial, travail dans les creux

- **Allocation** : avancer un pointeur, incrémenter le compteur du bloc.
- **Libération** : retrouver le bloc par l'adresse (`(addr - base) >> 16`), décrémenter.
- **Tout le reste** (préparer les pages, les rendre, évacuer, compacter, évincer) se fait :
  1. **dans les creux** : la boucle n'a ni CQE ni connexion prête ; budget illimité, mais
     par **tranches courtes** (~50–100 µs) avec, entre deux tranches, une lecture de la
     queue du CQ (mémoire partagée, sans syscall) : dès qu'un CQE attend, on rend la main.
     Le surcoût vu par la première requête après un creux est borné à une tranche ;
  2. **sous pression** : un budget minimal par tour, proportionnel aux octets alloués depuis
     le tour précédent (un *pacer*). Sans lui, un serveur saturé en continu, qui n'a jamais
     de creux, dériverait jusqu'à la limite.
- **Une tâche lourde à la fois.** Priorité : rendre les pages vides (presque gratuit) →
  compacter → évincer (seulement si les octets vivants dépassent le budget). Pendant une
  save, le GC des `SharedByte` continue (§8.5) ; le compactage HiSlab et l'éviction
  attendent (la save tient des idx, ARCHITECTURE §5).

---

## 3. Socle : plage réservée, pages, générations

- **Au démarrage**, chaque pool réserve une grande plage virtuelle contiguë, alignée sur
  2 Mio (réserver 2 Mio de plus et aligner). La réserve ne coûte rien tant qu'on n'y touche
  pas. Le pool d'une adresse se lit par comparaison de plage : `free` n'a pas besoin qu'on
  lui passe le pool.
- **Page** = 2 Mio = 32 blocs de 64 Kio. Les compteurs de blocs vivent dans un `Vec` à
  part (pas d'en-tête dans le bloc) : 32 × `u16` = 64 o, **une ligne de cache par page**.
- **Génération** : un numéro croissant attribué à chaque page quand elle est mise en service.
  Les pages qui ont des blocs libres sont rangées par génération ; on alloue dans la plus
  récente (on prend en tête, pas besoin d'un tas binaire).
- **Préparer la page suivante hors du chemin chaud** : la première écriture dans une page
  neuve coûte une faute de page et la mise à zéro de 2 Mio (de l'ordre de 200 µs à
  ~10 Go/s). On garde toujours au moins une page d'avance, préparée par
  `MADV_POPULATE_WRITE` (à vérifier : `IORING_OP_MADVISE` l'accepte-t-il ? sinon un petit
  thread auxiliaire, qui ne touche à aucune donnée partagée). Si la préparation est en
  retard, on retombe sur la faute synchrone, comptée dans `INFO`.
- **Rendre une page** : seulement quand ses 32 blocs sont vides (rendre 64 Kio casserait la
  huge page), par `MADV_DONTNEED` envoyé par io_uring, donc asynchrone. **Hystérésis** :
  ne rendre que si les pages libres dépassent `max(2, 2 % des pages utilisées)`, sinon un
  serveur qui oscille autour d'une frontière libère et refait des fautes de 2 Mio en boucle.
- **RSS dynamique** (décision du 2026-10-22, remplace la règle ci-dessus) : rendre au
  système n'est pas une décision à part, c'est la sortie du GC. La chaîne est :
  1. **trop de RAM tenue** = excédent soutenu de pages tenues sur blocs vivants + réserve :
     c'est le déclencheur du niveau B du GC (§8.2). Le niveau A (compacter des blocs) ne
     rend rien, il évite seulement de grandir ;
  2. B vide des pages entières, blocs denses compris → liste des pages libres ;
  3. **cible des pages libres = la réserve** (§8.3), calculée sur les **blocs vivants**
     et non réservés (sinon tenir plus de RAM justifierait une réserve plus grosse) ;
  4. au-delà de la cible, une page libre est rendue (`MADV_DONTNEED`) **si elle est restée
     libre plus de D secondes** (défaut ~10 s, `RADIXOX_MEMORY_DECAY`), les plus anciennes
     libres d'abord. C'est le *decay* de jemalloc (`dirty_decay_ms`) : une rafale
     suppression → réinsertion ne rend rien, un tas qui a vraiment fondu est rendu en
     quelques secondes ;
  5. en dessous de la cible, on garde : ce sont les pages d'avance et la destination du
     GC. S'il en manque, le thread auxiliaire en prépare (§3, §11).

  Rendre ne coûte qu'un `MADV_DONTNEED` asynchrone ; la plage virtuelle reste réservée, une
  page rendue redevient « à préparer ». Pas d'éviction de clés là-dedans : évincer des
  données ne relève que de `maxmemory` (§10), quand les octets **vivants** dépassent le
  budget.
- **Réserve permanente** : au moins une page pour l'allocation et une pour la destination du
  GC (§8.3).

---

## 4. Le mécanisme commun : bump + compteur par bloc

- Bloc courant : pointeur `top`, fin `end`. Si l'objet tient : `top += taille`,
  `vivants[bloc] += 1`. Sinon : on jette la fin de bloc (**pas d'objet à cheval sur deux
  blocs**) et on prend le bloc libre suivant.
- `free(p)` : `vivants[bloc(p)] -= 1` ; à 0, le bloc retourne dans la liste libre de sa
  page ; une page dont les 32 blocs sont libres devient candidate au rendu (§3).
- **Agrandir sur place** si l'objet est le dernier alloué du bloc courant (`top`
  recule ou avance) : utile aux buffers qui grossissent.
- **Un seul compteur par bloc : les granules vivants** (2 o ; 32 768 par bloc, tient dans
  un `u16`). `free` connaît la longueur par l'en-tête ; tout objet fait au moins un
  granule, donc bloc vide ⇔ 0. Le même compteur donne la densité au GC (§8.2) et suit les
  écrasements sur place qui rétrécissent un objet (§6). Le nombre d'objets par bloc ne sert
  qu'au vérificateur de debug (§12). (Décision du 2026-10-22, remplace le double compteur.)

---

## 5. Les pools

| durée de vie | pool | taille de bloc | quand le bloc se vide mal | ce qu'il sert |
|---|---|---|---|---|
| un tour ou presque | **court** | 4–16 Kio | rien : tout meurt vite | débordements des `SmallVec` d'arguments (`Cmd`, radixox-lib), messages de réplication (`RxBlock`, `RxLoadData`…), temporaires |
| variable, grossit | **buffers** | moyenne, agrandissement sur place | rendus quand vides (§7) | `out`, `sending`, `rx`, `spans` des connexions |
| longue, données | **`SharedByte`** | 64 Kio | GC (§8) | valeurs, champs et membres |
| longue, données, un seul propriétaire | **objets de structure** | 64 Kio | GC (§8.6) | pages des nœuds, `CompactPath`, objets `HashSmall` / `SetSmall`, débordements valart |
| longue, > 64 Kio | **grands objets** | N blocs contigus, ou N pages au-delà de 2 Mio | jamais évacués | grosses valeurs |
| fixe | **slabs** (hors allocateur) | 128 o | compactage (§9) | nœuds oxidart, nœuds valart |
| bornée par configuration | **système**, non compté dans `maxmemory` | — | — | AOF en attente (jusqu'à 64 Mio en `continuous`), backlog de réplication, buffers de save |
| inconnue | **repli** : mimalloc en `#[global_allocator]`, compté | — | — | std, crates tierces, démarrage |

**Garde-fou du pool court** : un objet qui vit longtemps, aiguillé là par erreur, épingle
tout son bloc. `INFO` affiche les blocs du pool court vivants depuis plus de N tours :
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
- **Écraser sur place** (décision du 2026-10-22) : un `SET` sur une valeur existante de
  rc == 1 dont l'emplacement est **au moins aussi grand** que la nouvelle valeur la
  réécrit dans cet emplacement, sans allouer ni libérer : nouvel en-tête, compteur de la
  bloc décrémenté de `granules(ancienne) − granules(nouvelle)`. La fin de l'emplacement
  devient de l'espace mort que la densité du bloc voit et que le GC récupère (il ne
  copie que la longueur courante). Conditions : rc == 1 (une réponse en vol lit encore les
  anciens octets), et pendant une save le nœud intact est d'abord copié (SAVE_DESIGN.md).
  L'argument déjà alloué au parsing est libéré aussitôt après la copie : c'est le dernier
  objet du bloc courant, `top` recule (§4), rien n'est perdu.
- **Allouer à la bonne place dès le parsing** : un `SET` copie la valeur de l'anneau de
  réception directement dans le pool données (une seule copie). Une valeur qui meurt vite
  (commande refusée, `UNLINK` d'arguments) laisse un trou dans un bloc jeune : c'est le
  cas normal du générationnel, le GC voit ces blocs se vider seuls.
- **Objets moyens** : avec des blocs de 64 Kio et pas d'objet à cheval, un objet de 40 Kio
  peut gaspiller jusqu'à 40 Kio en fin de bloc. Avec G = 16 Kio (grands objets ci-dessous),
  la perte est au plus de 16 Kio par bloc. Les objets dont l'emplacement (en-tête compris)
  dépasse **M = 2 Kio** ont leur propre curseur, qui garde une **fenêtre de 8 blocs ouverts**
  (décision du 2026-10-22) :
  - le reste de chaque bloc est compté en granules de 2 o, dans un `u16` (bloc vide =
    32 768, ça tient) ;
  - **placement Best-Fit** : parmi les blocs où l'objet tient, celui dont le reste est le
    plus serré. Boucle scalaire d'abord ; si le profil la montre, `_mm_minpos_epu16`
    (SSE4.1) donne le minimum et son index en une instruction. La recherche reste
    négligeable devant la copie de plusieurs Kio ;
  - aucun bloc ne tient → bloc neuf ; fenêtre pleine → on ferme le **plus plein**, pas le
    plus vieux ; un bloc dont le reste tombe sous M est fermé tout de suite ;
  - exemple : deux objets de 33 Kio ne tiennent jamais ensemble ; chacun laisse ~31 Kio que
    la fenêtre offre aux objets suivants au lieu de les jeter.

  L'optimum est NP-difficile (bin packing). En ligne avec K blocs ouverts, aucun
  algorithme ne fait mieux que ~1,69 × l'optimum dans le pire cas, et Best-Fit qui ferme le
  plus plein atteint 1,7 dès K = 2 (Csirik et Johnson) ; sur des tailles réalistes la
  perte est de quelques pourcents. Les petits objets ne passent jamais par la fenêtre :
  leur chemin reste un bump. **Aucun objet ne chevauche deux blocs, objets moyens
  compris** (décision ferme du 2026-10-22) : un bloc = un compteur, sans exception.
- **Grands objets (> G = 16 Kio de données)** : **jemalloc** avec son thread de fond
  (décision du 2026-10-22, mesures §13.2), en-tête long (§16.2). Jamais évacués par le GC :
  jemalloc rend leur mémoire seul. Écarté pour la v1 : un pool à nous (N blocs contigus
  d'une page jusqu'à 2 Mio, N pages au-delà), à ne reprendre que si jemalloc déçoit.
- **Reload** : le dump est rechargé en masse ; ses valeurs vont directement dans les pages
  « survivantes » et sortent parfaitement denses. Un redémarrage défragmente.

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

## 8. GC par évacuation (pool `SharedByte`, puis objets de structure)

### 8.1 Parcours

**Linéaire sur les slabs** (décision du 2026-10-21, remplace le curseur lexicographique) :
nœuds HiSlab dans l'ordre des slots, leurs blocs, puis la slab valart. Séquentiel, facile à
précharger (file de nœuds façon AMAC), sans logique métier. Curseur = index de slot.

- Une recompression peut déplacer une valeur d'un nœud situé après le curseur vers un nœud
  situé avant : elle est ratée par cette passe. Son bloc ne tombe pas à 0, la passe
  suivante la rattrape. Accepté.
- **Sous-arbres détachés** (`UNLINK prefix*`, `FLUSHDB`) : le parcours y passe aussi. Sans
  danger : `free_detached` (`oxidart/src/lib.rs`) relit les valeurs dans les nœuds au
  moment de libérer, il libère donc la nouvelle adresse. Au pire on a copié une valeur qui
  allait mourir.
- **Valart, même mécanisme** (2026-10-22) : parcours linéaire des slots occupés de la slab
  valart (128 o), curseur = index de slot, au mieux. Par nœud : les entrées inline (clé et
  valeur `SharedByte`) puis celles de son bloc de débordement. Le parcours ignore à quel
  Hash ou Set appartient le nœud. Une entrée qui remonte d'une feuille vers un ancêtre
  déjà visité (HDEL, SREM) est ratée, et la passe suivante la rattrape, comme une
  recompression côté oxidart. Pendant une save, valart est sauté (§8.5).
- **Piste v2 : la save comme passe de GC pour oxidart.** Le DFS de la save visite déjà
  chaque nœud et lit chaque valeur, donc nœud et octets sont en cache. Une valeur dans un
  bloc marqué pourrait être évacuée par la save au moment où son enregistrement est écrit
  (pas à la visite : la file tient encore une copie sans refcount, SAVE_DESIGN I3).
  Après le `take`, le nœud n'est plus intact, donc pas de capture. On évite ainsi un
  second parcours complet pendant la save. Prix : la save, déjà délicate (I1–I9), porte
  du code de GC, et ses tranches s'allongent de la copie. Valart reste au GC autonome
  après `end_save`. À ne regarder que si deux parcours simultanés (save + GC) se voient
  dans les mesures.

### 8.2 Choisir les blocs

Au début d'une passe, on **marque un lot** de blocs creux (octets vivants sous un seuil),
on n'alloue plus jamais dedans, et chaque pointeur rencontré qui tombe dans un bloc marqué
est recopié vers le curseur des survivants, puis réécrit. Une passe coûte un parcours
complet de l'arbre quel que soit le nombre de blocs : il faut donc marquer **un gros lot**
pour amortir.

Décisions du 2026-10-22 :

**Pas de bin packing ici.** Les survivants sont recopiés au bump dans des blocs vides :
K blocs qui portent L octets vivants en deviennent J ≈ ⌈L / 64 Kio⌉, presque l'optimum. Le
placement ne compte que pour les objets moyens, qui passent par la même fenêtre Best-Fit
qu'à l'allocation (§6). Les trier d'avance (First-Fit Decreasing) demanderait de garder la
passe en tampon, ce qu'interdit §8.5.

**Coût d'un bloc** de densité d : on copie d·64 Kio pour rendre (1−d)·64 Kio, soit
**d / (1−d) octets copiés par octet rendu** : 0,4 à 30 %, 1 à 50 %, 4 à 80 %, 9 à 90 %.

**Deux niveaux, deux métiers** (décision du 2026-10-22). Un bloc de 64 Kio et une page
de 2 Mio ne se compactent pas pour la même raison, ni avec le même critère :

| | **A. compacter des blocs** (64 Kio) | **B. rendre des pages** (2 Mio) |
|---|---|---|
| but | **ne pas grandir** : refaire des blocs vides avec nos trous au lieu de prendre une page neuve | **baisser le RSS** |
| la mémoire | on la garde | on la rend au système |
| déclencheur | stock de blocs vides dans nos pages < réserve, **et** f > 15 % (≥ 8 Mio récupérables) ; au repos f > 5 % | excédent tenu = pages tenues − (blocs vivants + réserve) > `max(réserve, 10 % du tenu)`, **soutenu** pendant D secondes |
| unité choisie | le bloc | la page |
| critère | densité du **bloc** : sous 75 % (charge) ou 90 % (repos) | octets vivants de la **page**, croissants |
| blocs denses | jamais touchés | **évacués aussi** : un bloc plein et immortel épingle toute sa page ; la copier coûte 64 Kio et rend 2 Mio |
| destination | n'importe quel bloc vide | blocs vides des pages **gardées**, jamais d'une page candidate |

f = 1 − octets vivants / blocs réservés du pool.

- **A seul ne fait jamais baisser le RSS** : il remplit les trous, les blocs vides
  qu'il produit restent dans des pages par ailleurs occupées. C'est voulu : tant qu'on ne
  cherche pas à rendre, défragmenter sert seulement à ne pas demander de mémoire neuve.
- **B ignore la densité des blocs** : il regarde la page. Une page à 3 % (un bloc
  plein, 31 vides) est le meilleur candidat qui soit. Les pages marquées ne reçoivent plus
  d'allocation dès le marquage, sinon on les remplit pendant qu'on les vide.
- **B ne fait que choisir, la passe fait le travail.** Quand B décide de lâcher une page :
  1. ses 32 bits dans le **bitmap des blocs marqués** (ci-dessous) : un seul `u32` ;
  2. ses blocs vides sortent de la liste libre ; si le bloc courant du bump, le curseur
     des survivants ou un bloc de la fenêtre des objets moyens (§6) est dans cette page, on
     le ferme ;
  3. un bloc de la page qui tombe à 0 en cours de passe **ne sert pas** de destination
     (exception à §8.3) ;
  4. la passe suivante hérite de la page dans son ensemble marqué ; s'il n'y a pas de passe
     prévue, B en demande une. Un objet épinglé (§8.4) retarde la page à la passe
     d'après ;
  5. quand ses 32 compteurs sont à 0, la page est rendue **tout de suite**, sans attendre
     le *decay* de §3 : B l'a déjà attendu avant de décider.

  Les grands objets (> 64 Kio) vivent dans leur propre pool (§5) et n'épinglent donc
  jamais une page de `SharedByte`.
- **Bitmap des blocs marqués** : 1 bit par bloc de 64 Kio de la plage réservée du pool
  (§3), donc indexé par l'adresse : `off = addr − base ; off < len && bit(off >> 16)`.
  Une soustraction, une comparaison, un décalage, une lecture : pas de table de hachage
  (5–20 ns, et grossit avec le nombre de blocs marqués), pas de scan SIMD (borné à
  quelques dizaines de blocs), pas de deux étages. Taille : 1/524 288 de la plage, soit
  16 Kio pour 8 Gio, 128 Kio pour 64 Gio ; il vit en L1/L2 pendant la passe (~1–4 ns par
  test) quand la lecture du nœud qui porte le pointeur coûte jusqu'à ~80 ns. A pose des
  bits un par un, B un `u32` par page ; la passe ne sait pas qui a marqué. Pas dans le
  tableau des compteurs (2 o par bloc, 16 fois plus gros).
- **Une seule passe sert les deux** : les blocs de A et les pages de B forment l'ensemble
  marqué d'une même passe d'évacuation ; le parcours complet (le coût fixe) est partagé. La
  somme de leurs octets vivants est bornée par la réserve (règle d'Immix : une passe dont
  les vivants ne tiennent pas dans la réserve se bloque faute de destination). Sous
  pression, A passe d'abord.
- **L'allocation prépare B sans rien copier** : quand il faut un bloc vide, le prendre
  dans la page **la plus occupée** qui en a un (au lieu de la plus récente, M5) : les pages
  creuses ne reçoivent plus rien, se vident seules, et B n'a souvent plus qu'à attendre le
  *decay* (§3). Arbitrage avec M5 (pages récentes = lignes chaudes) à mesurer.
- **Arrêt** : A s'arrête quand une passe rend moins d'une page de blocs ; B quand
  l'excédent repasse sous son seuil.

Au repos la copie ne coûte presque rien au métier, mais pour A monter le seuil de densité
ne rapporte pas autant qu'on le croit : avec une réserve R, une passe rend au mieux
**R·(1−d)/d** (4 Mio de réserve : 4 Mio rendus à 50 %, 0,44 Mio à 90 %), et la copie
pollue un peu les caches. Valeurs de départ, à régler sur mesures. Références : G1
(`G1MixedGCLiveThresholdPercent = 85`, `G1HeapWastePercent = 5`), Redis `activedefrag`
(`ignore-bytes 100mb`, `threshold-lower 10`).

### 8.3 Réserve

Sans pointeur arrière, un bloc marqué ne se vide en général qu'à la fin de la passe (son
premier objet peut être au début de la slab, le dernier à la fin). Évacuer B blocs de
densité d demande donc **B·d blocs libres d'avance**. Exemple : 10 Mio de réserve, blocs à
80 % → une passe évacue 12,5 Mio et rend 2,5 Mio. Un bloc dont le compteur tombe à 0 en
cours de passe peut aussitôt servir de destination (le compteur le dit), mais on ne compte
pas dessus.

**Taille (décision du 2026-10-22) : `réserve = max(2 pages, H / 16)`**, H = octets
vivants du pool arrondis en blocs (pas les blocs réservés : sinon plus on tient de RAM,
plus la réserve grossit et moins on en rend, §3), réglable par `RADIXOX_GC_RESERVE`. Le nombre de passes pour absorber
une fragmentation f vaut ≈ f·H·d / (R·(1−d)). Avec f = 20 %, d = 50 % : R fixe de 4 Mio →
3 passes à 64 Mio, 51 à 1 Gio, 512 à 10 Gio (le GC ne rattrape jamais) ; R = H/16 →
~3 passes à toute taille ; H/8 → ~1,6, mais 12,5 % de la mémoire retirée à l'utilisateur.
G1 réserve 10 % (`G1ReservePercent`). Le plancher de 2 pages (64 blocs) ne joue que sous
64 Mio.

Ce qui rend la réserve moins chère :
- elle se **recycle** : les blocs rendus par une passe servent de réserve à la suivante,
  elle borne la vitesse du GC, pas ce qu'il peut rendre au total ;
- les blocs libres des pages partielles comptent, pas seulement les pages vides ;
- ce sont les mêmes blocs que la page d'avance préparée hors du chemin chaud (§3) ;
- elle est dans le budget de `maxmemory` (§10.3).

### 8.4 Objets épinglés (rc > 1)

Un `SharedByte` tenu ailleurs (réponse en vol, Pub/Sub, commande en cours) ne peut pas être
déplacé. Heuristique : on garde l'**index du nœud et de l'entrée** (instable), puis plus
tard on vérifie que le slot est vivant (`is_occupied` de hislab) et que l'entrée contient
**exactement le même pointeur**. C'est sûr parce qu'**on n'alloue jamais dans un bloc
marquée** : une adresse égale désigne forcément le même objet (pas d'ABA). Si la
vérification échoue (recompression, nœud compacté), la passe suivante le rattrape. Coût :
un accès au nœud par objet épinglé, sans copie de clé ni descente.

### 8.5 Interdits

- **GC pendant une save : permis** (décision du 2026-10-22, remplace « pas de GC pendant
  une save »). Déplacer un `SharedByte` ne change pas la vue T0 : mêmes octets, autre
  adresse. Les nœuds ne bougent pas, donc la pile DFS (des idx) et le bitset `intact`
  restent valides. Conditions :
  1. la réécriture du pointeur par le GC **ne passe pas par la barrière** (`before_write`) :
     ce n'est pas une mutation, elle ne doit pas capturer. Un point d'écriture à part,
     réservé au GC, documenté comme tel. Un nœud intact déplacé est lu plus tard par la
     save à sa nouvelle adresse, avec les mêmes octets ;
  2. entre deux tranches, la save ne tient aucun pointeur brut (SAVE_DESIGN I5 :
     `own_pending` clone avant toute pause) et les images oxidart tiennent un clone : tout
     ce que la save garde a rc > 1, donc est épinglé (§8.4) ;
  3. **valart est sauté pendant une save** : ses images sont des copies bit à bit **sans
     refcount** (SAVE_DESIGN §2.A), une valeur à rc == 1 peut donc être lue par une image ;
     la déplacer et libérer l'ancienne adresse serait une lecture de mémoire libérée.
     Plus tard si besoin : déplacer quand même, mais différer la libération de l'ancienne
     copie jusqu'à `end_save` (comme `deferred_bytes`). **Écarté : un bitmap des nœuds
     valart capturés que le GC sauterait.** Il serait faux pour la même raison que le
     bitset `captured` écarté par SAVE_DESIGN §2.B : HDEL / SREM font remonter une entrée
     d'une feuille capturée vers un ancêtre non capturé, et l'image de la feuille la lit
     encore ;
  4. le **compactage HiSlab** (§9) reste interdit pendant une save : lui déplace des nœuds,
     donc des idx.
- Rien de daté ne survit à une tranche (règle d'or, ARCHITECTURE §1.1) : le GC ne garde
  entre deux tranches que son curseur (index de slot, toléré parce que le parcours ne
  promet rien) et ses indices épinglés vérifiés à la relecture.
- Tout porteur d'un pointeur vers les octets d'un `SharedByte` au-delà du tour doit tenir
  un clone (rc). Aucune iovec brute vers une valeur sans rc.

### 8.6 Objets de structure

Pages des nœuds, `CompactPath`, objets `HashSmall` / `SetSmall`, débordements valart : un
seul propriétaire chacun, que le parcours linéaire visite **avant** d'atteindre l'objet
(le nœud qui le pointe). Ils sont plus simples à évacuer que les `SharedByte` (pas de rc).
Les pages de nœuds sont redimensionnées en place : réallocation = nouvelle adresse dans le
pool, comme une évacuation.

### 8.7 Chiffres et bascules (2026-10-22)

Valeurs de départ, réglables, à confirmer sur mesures.

| quoi | valeur |
|---|---|
| bloc / page / granule | 64 Kio / 2 Mio / 2 o |
| objets moyens (§6) | emplacement > M = 2 Kio : fenêtre de 8 blocs, Best-Fit |
| grands objets (§6) | données > G = 16 Kio : jemalloc + `background_thread` |
| réserve (§8.3) | `max(2 pages, vivants / 16)` |
| A sous charge | blocs < 75 %, si f > 15 % et ≥ 8 Mio récupérables |
| A au repos | blocs < 90 %, si f > 5 % |
| B | excédent > `max(réserve, 10 % du tenu)` soutenu D = 10 s |
| rendu d'une page libre au-delà de la cible (§3) | après 10 s libre |
| tranche | 50–100 µs ; 512 slots entre deux lectures du CQ (à recaler sur le coût mesuré par slot) |
| « au repos » | la boucle a dormi au moins une fois dans les 10 dernières ms |

**Pacer sous charge** (§2) : une passe visite N slots et doit finir avant que les
allocations aient consommé la réserve R, d'où

`budget du tour (slots) = max(plancher, octets alloués ce tour × N / R)`.

Au repos, pas de limite : on avance par tranches.

**Cycle de vie d'une passe** :

| cas | règle |
|---|---|
| marquage | figé au début de la passe ; B ne marque qu'entre deux passes |
| une save démarre en pleine passe | la passe **continue** (§8.5) ; les `SharedByte` de valart sont sautés jusqu'à `end_save` et rattrapés par la passe suivante |
| la réserve s'épuise en pleine passe | on prend une page neuve (le RSS monte un moment), comptée dans `INFO` ; la passe ne bloque jamais |
| bloc marqué non vide en fin de passe (objet raté ou épinglé) | reste marqué pour la passe suivante ; démarqué au bout de 2 passes |

**Bascules**, de la mémoire la plus large à la plus serrée : au repos A à 90 % (+ B si
excédent) → sous charge A à 75 % + pacer → peu de blocs libres : A passe avant B →
vivants > budget : `maxmemory` (pas en v1).

**`INFO`** : en plus de §10.1, passes abandonnées, pages neuves prises faute de réserve,
blocs démarqués.

**Critères d'acceptation de la v1** :
- RSS après suppression de 90 % des clés puis GC : ≤ vivants × 1,15 + réserve
  (aujourd'hui 10 × les vivants, §13.1) ;
- écrasements au hasard pendant 30 s : RSS stable à ±15 % ;
- débit et p99.9 identiques à mimalloc sur `scripts/bench_vs_valkey.sh` ;
- première requête après un creux : surcoût ≤ une tranche ;
- débit d'évacuation au repos (Mo/s) : relevé, pas imposé.

**Saves fréquentes : tranché le 2026-10-22.** Le GC tourne pendant les saves (§8.5) : des
saves qui se suivent (toutes les 30 s, ou en continu sous AOF : 118 s vues sous YCSB à
1 cœur) ne l'empêchent plus de finir. Reste : sous saves continues, les `SharedByte` de
valart ne sont jamais évacués ; si une charge Hash/Set le montre, passer à la libération
différée de §8.5 point 3.

---

## 9. Compactage HiSlab

Une tâche de fond fait `insert(pop du dernier nœud)` : le dernier slot occupé part dans le
premier slot libre, la fin de la slab se rend par pages de 2 Mio (THP).

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

- `used_data` : **blocs réservés** des pools données (`SharedByte`, objets de structure,
  grands objets) + slots vivants des slabs × 128 o. C'est ce que borne `maxmemory`.
- `used_service` : pools court et buffers, repli. Bornés par leurs propres limites
  (buffers de sortie par client, plafond de l'AOF), **pas** par l'éviction : évincer des
  clés pour un client lent n'a pas de sens.
- Système (AOF, backlog, save) : non compté, borné par configuration, affiché.
- `INFO` : par pool, octets vivants et blocs réservés, fragmentation exacte
  (réservé / vivant), RSS, pages prêtes et rendues, passes de GC, octets évacués, objets
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

### 10.2 bis Compteur dans la valeur (décisions de l'utilisateur, 2026-10-23)

Remplace le tableau à part de §10.2 pour les chaînes et les conteneurs (le tableau reste
l'option des entiers, qui n'ont pas d'en-tête).

- **Où** : dans l'objet, pas dans le nœud — le nœud n'a aucun octet libre (128 o, ligne 1
  pleine, ligne 2 = cellules ; vérifié par `offset_of!`) et une feuille inline paierait une
  cellule de 8 o. Chaînes : l'en-tête du `SharedByte` (la ligne de la valeur, lue de toute
  façon pour répondre). Conteneurs : l'en-tête du petit bloc Hash / Set (6 octets libres)
  ou la racine valart ; les champs et membres ne paient rien. Budget accepté : de l'ordre
  de 2 o par clé (« 10M clés = 20 Mio, largement respectable »).
- **Niche de `rc`** : `rc` passe de 16 à 8 bits, les 8 bits libérés portent le compteur
  (0 o de plus). Prérequis : **les lectures ne clonent plus** les `SharedByte`. `GET`,
  `HGET`, `MGET`, `HGETALL`… encodent depuis `&SharedByte` / `&[u8]` (on lit pour recopier
  dans la réponse puis on relâche : le clone n'apporte rien et coûte deux écritures dans
  l'en-tête par lecture — un gain en soi, éviction ou pas). Détenteurs légitimes restants :
  l'arbre, l'image de save, les rares commandes qui gardent une valeur (jobs, Pub/Sub) →
  `rc` reste à quelques unités ; `debug_assert` dans `clone` sous le seuil, saturation
  conservée en release.
- **Politique** : LFU par compteur de Morris sur 8 bits (comme Redis), **décroissance par
  l'aiguille de balayage** (pas d'horodatage) ; CLOCK = le même compteur réduit à « touché
  depuis le passage de l'aiguille ».
- **Feature cargo `eviction`** (`radixox-lib`, relayée par `oxidart` et `radixox` :
  `cargo install radixox --features eviction`) : désactivée, le chemin de lecture n'écrit
  rien de plus ; les deux configurations compilent et passent les tests.
- **Balayage** : réutilise les énumérateurs du GC (`GcVisitor`) — un visiteur de plus qui
  lit et fait décroître les compteurs et note les candidats.
- **`volatile-*` très rapides** : les nœuds qui portent une clé avec TTL sont déjà tagués
  dans la `TaggedHiSlab` ; l'aiguille de `volatile-lfu` / `volatile-lru` / `volatile-ttl`
  ne parcourt **que les nœuds tagués**, jamais les clés sans TTL ; `volatile-ttl` lit
  l'échéance déjà dans la feuille.

### 10.3 Déclenchement

- **Deux niveaux** : l'éviction de fond démarre à `maxmemory − réserve − marge` ; la
  commande ne fait que tester `used_data > maxmemory`, n'évince elle-même (petit budget) que
  si le fond est dépassé, et ne répond `-OOM` que si rien n'est évinçable ou en
  `noeviction`.
- **La réserve du GC (§8.3) est dans le budget** : à la limite sans réserve, on ne peut plus
  compacter et on évince des données qu'un GC aurait gardées.
- **GC avant éviction** ; n'évincer que si les octets vivants dépassent le budget.
- **Entre deux victimes froides, prendre celle de la page la plus vide** : l'éviction aide le
  GC au lieu de trouer la mémoire au hasard.
- Grosses valeurs évincées : détachées puis libérées par tranches (mécanisme d'`UNLINK`).

---

## 11. Huge pages et noyau

- THP doit être en mode `madvise` ou `always` (`/sys/kernel/mm/transparent_hugepage/enabled`).
  Sinon tout marche, en pages de 4 Kio : à afficher dans `INFO` et au démarrage.
- Réservation : `MAP_NORESERVE`. Sous `vm.overcommit_memory=2`, la réservation compte dans
  la limite de commit : alternative `PROT_NONE` puis `mprotect` par page, au moment de la
  préparation (hors chemin chaud).
- `MADV_DONTNEED` d'une page entière alignée rend la huge page ; un `MADV_DONTNEED` partiel la
  casserait.

Ajouts du 2026-10-22 :

- **Préparer une page** (thread auxiliaire ou io-wq, jamais la boucle) : `MADV_POPULATE_WRITE`
  (5.14+), puis `MADV_COLLAPSE` (6.1+) si le noyau a dû donner des pages de 4 Kio faute de
  zone libre de 2 Mio ; vérifier par `AnonHugePages` (`/proc/self/smaps_rollup`), compté dans `INFO`.
  Avec `defrag=defer+madvise`, une faute sur une région `MADV_HUGEPAGE` peut compacter de
  façon synchrone : sans danger si **toutes** les fautes ont lieu hors de la boucle.
- **Pas de réservation THP par programme** : Linux n'a pas d'appel « garde-moi une huge
  page ». Notre réserve préparée en tient lieu. Seul hugetlbfs réserve vraiment.
- **Découpage des THP sous-utilisées** (6.12+, `shrink_underused`, compteur
  `thp_underused_split_page` dans `/proc/vmstat`) : une page de réserve préparée est toute à
  zéro, la cible idéale. De mémoire du code (`thp_underused()`), le shrinker ne fait rien
  quand `khugepaged/max_ptes_none` vaut 511 (le défaut) ; à vérifier dans les sources, et
  la doc de déploiement dira de ne pas le baisser.
- **Portée du TLB** : un raté de TLB n'est pas une faute, le walker matériel relit la table
  en RAM (3 niveaux en 2 Mio, coût fixe ~20–40 cycles). Zen 4 : L1 DTLB 72 entrées
  (144 Mio en 2 Mio), L2 DTLB 3 072 (**6 Gio** en 2 Mio, 12 Mio en 4 Kio) ; Intel récents
  3–4 Gio. Au-delà, un coût régulier de quelques pourcents (à mesurer avec
  `ls_l1_d_tlb_miss.all_l2_miss`), pas des pics ; les préchargements des descentes
  entrelacées le cachent en partie. Redis désactive les THP (`disable-thp`) à cause du CoW
  de son `fork` et vit avec des ratés dès quelques dizaines de Mio ; RadixOx ne forke pas.
- **Le jitter vient de ce que le noyau fait à nos pages**, pas du TLB. Une fois touchée,
  une page ne refait plus de faute, quelle que soit la taille du tas, sauf si le noyau la
  retire ou la modifie :

  | source | parade (déploiement) |
  |---|---|
  | compaction (kcompactd) qui migre nos pages | `mlock` + `vm.compact_unevictable_allowed=0` |
  | NUMA balancing (fautes provoquées pour migrer) | `numactl --cpunodebind --membind`, ou `kernel.numa_balancing=0` |
  | swap | `mlock`, `MemorySwapMax=0`, `MemoryMin=` |
  | khugepaged | inutile si nos pages naissent en huge pages ; ne touche que le repli |

  Option envisagée : `RADIXOX_MLOCK=1` (`mlock2(MLOCK_ONFAULT)`, demande
  `LimitMEMLOCK=infinity`).
- **Plus tard, hugetlbfs** (`MAP_HUGETLB`), la seule vraie réservation : pool fixé par
  l'admin, réservé au `mmap`, jamais migré ni swappé. D'abord en 2 Mio (rien ne change au
  design). Puis **1 Gio** pour les gros serveurs dédiés (tas de plusieurs dizaines de Gio) :
  le L1 DTLB de Zen 4 garde les pages de 1 Gio, soit 72 Gio couverts ; réservation au boot
  (`hugepagesz=1G hugepages=N`), tout préparé au démarrage (`MAP_POPULATE`, mise à zéro
  ~100 ms par page), plus de rendu à l'OS en dessous du Gio. Les pages de 2 Mio deviennent
  des subdivisions logiques. À n'activer que si `perf stat` montre des parcours de table
  qui comptent.

---

## 12. Tests

Le GC est la pierre angulaire du projet : il lui faut au moins le filet de la save sous
mutation (SAVE_DESIGN.md).
- **Mode stress** : GC et compactage à **chaque tranche**, branchés sous les tests
  différentiels contre l'oracle `oxidart-legacy` et les tests de modèle (`BTreeMap`).
- **Vérificateur debug** après chaque tranche : recompte des octets et objets vivants de
  chaque bloc ; chaque pointeur de l'arbre tombe sur un objet vivant d'un bloc non rendu ;
  chaque `parent_idx` et index d'enfant cohérents ; tags TTL et CLOCK suivent leurs nœuds.
- **Cas dédiés** : chaîne parent → enfant → petit-enfant dans un même lot de compactage ;
  objet épinglé pendant toute une passe puis relâché ; recompression qui fait passer une
  valeur derrière le curseur ; `UNLINK prefix*` en cours pendant le GC.
- **AddressSanitizer**, procédure de SAVE_DESIGN.md.
- Faux noyau (`radixox/src/uring/tests.rs`) : préparation et rendu de pages asynchrones,
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

### 13.2 Grosses valeurs : mimalloc ou jemalloc (2026-10-22)

Programme C (dans le scratchpad de la session, à refaire au besoin) : 512 Mo de valeurs,
fixes de 100 Kio ou log-uniformes de 4 Kio à 2 Mio, écrites ; `free` de 50 % au hasard ;
RSS à t+0, t+1 s, t+12 s ; `free` de 90 % du reste ; mêmes relevés ; réallocation de tout.
Chaque cas sous `systemd-run --scope -p MemoryMax=1G`. mimalloc compilé depuis les sources
de `libmimalloc-sys` 0.1.47 (v2 = celle du dépôt, et v3), jemalloc 5.3 du système.

RSS en Mo (100 Kio fixes / 4 Kio–2 Mio) :

| allocateur | plein | −50 %, t+12 s (vivants 256) | −90 % du reste, t+12 s (vivants ~25) | rechargé |
|---|---|---|---|---|
| mimalloc v2 (dépôt) | 559 / 561 | 559 / 561 | 280 / 290 | 539 / 549 |
| mimalloc v3 | 581 / 553 | 582 / 553 | 582 / 373 | 584 / 589 |
| jemalloc, réglages par défaut | 555 / 535 | 555 / 535 | 524 / 530 | 607 / 537 |
| **jemalloc, `background_thread:true`** | 585 / 544 | **288 / 276** | **44 / 47** | 549 / 537 |
| glibc | 516 / 516 | 516 / 285 | 516 / 88 | 516 / 514 |

- mimalloc ne rend rien après −50 % et garde 11 × les vivants après −90 %.
- jemalloc sans thread de fond ne rend rien non plus : son *decay* n'avance que sur des
  appels d'allocation. Avec `background_thread`, il revient à vivants + ~20 Mo en ~10 s
  (`dirty_decay_ms` par défaut 10 000). C'est le réglage de Redis (`jemalloc-bg-thread yes`).
- glibc rend bien les objets servis par `mmap` (au-delà de son seuil dynamique) et rien
  des autres : écarté.

**Décision** : objets de plus de G = 16 Kio de données → jemalloc avec `background_thread`.

---

## 14. Ordre de travail

0. Mesures de départ (§13).
1. **Socle** : plage réservée, pages de 2 Mio avec génération, préparation et rendu
   asynchrones, réserve, comptes par pool ; repli mimalloc compté en `#[global_allocator]`.
2. **Pool `SharedByte`** : bump + compteurs, sans GC ; un bloc n'est réutilisé que vide.
3. **Grands objets** (> 64 Kio).
4. **Pool court**, puis **buffers de connexion** rendus quand vides (§7 v1).
5. **GC par évacuation** (§8), curseur des survivants, réserve, épinglés ; puis objets de
   structure (§8.6).
6. **Compactage HiSlab** (§9).
7. **`maxmemory`** (§10).
8. Envoi par morceaux partagés (§7 v2).

### 14.1 Périmètre de la v1 (2026-10-22)

**Le pool `SharedByte` seul, avec le GC niveau A.** Un pool sans GC (étape 2) sert à
mesurer le chemin chaud, jamais à passer par défaut : le bump ne réutilise un bloc que
vide, et sous des écrasements au hasard un bloc ne se vide qu'une fois **toutes** ses
valeurs écrasées (collectionneur de coupons) ; le RSS monterait à plusieurs fois les
octets vivants, pire que mimalloc (+10 % au rechargement, §13.1).

Dans la v1 :
- **en-tête compact dès la v1** : pointeur sur les données, en-tête lu à reculons, forme
  courte (bit 15 de `len_lo` à 0) sous 32 Kio, la quasi-totalité des valeurs : une branche
  bien prédite et une lecture de `u16` alignée. Granule et alignement de 2 o ; plus aucune
  écriture de `u32` aligné (le code actuel en fait une) ;
- **écrasement sur place** (§6) et **un seul compteur** en granules (§4) ;
- **objets > G = 16 Kio** : jemalloc, compté ; `free` aiguille par le bit « externe » de l'en-tête (§16.2) ;
- **GC niveau A** sur le parcours linéaire des slots occupés de la HiSlab (§8.1), puis la
  slab valart. Le parcours ignore la logique métier mais connaît le **layout** : par slot,
  le tag dit quels mots sont des `SharedByte` (valeur du nœud, feuilles inline, champs et
  membres valart). C'est l'API à poser en premier : un énumérateur d'emplacements
  `&mut SharedByte` par plage de slots, dans oxidart et valart ;
- **épinglés** : la règle d'unicité (§6) dit qu'un `SharedByte` de l'arbre a rc == 1 ;
  rc > 1 → épinglé (§8.4). Pas de comptage d'occurrences dans une même structure : si un
  cas de double emplacement apparaît, on le corrige par une copie profonde ;
- **interrupteur `--cfg`** sur le modèle de `radixox_sysalloc` pour revenir à mimalloc
  (comparaisons, ASan) ;
- **mode débogage** : blocs libérés remplis d'un motif ou en `PROT_NONE`, vérificateur
  qui recompte granules et objets par bloc, une passe de GC entre chaque commande dans les
  tests différentiels, et un **journal activable** (`RADIXOX_GC_LOG`) : objets évacués,
  objets épinglés, blocs rendus à la liste libre, pages libérées ;
- **simple** : faute de page synchrone dans la boucle acceptée mais comptée dans `INFO`,
  liste des blocs libres LIFO, pas de niveau B, pas de `maxmemory`.

Grosses valeurs : **jemalloc + `background_thread`**, tranché par la mesure de §13.2.
**La spécification complète de la v1, sans décision restante, est en §16.**

---

## 15. Questions ouvertes

- ~~Contenu des gros conteneurs valart entre deux tranches de GC~~ : réglé le 2026-10-22,
  le parcours est linéaire sur la slab valart et ignore les conteneurs (§8.1).
- **Prérequis du GC** : partage légitime d'un `SharedByte` à deux endroits de l'arbre, à
  éliminer partout (copie profonde), à auditer commande par commande **avant** d'activer
  le GC. Une valeur à deux emplacements aurait rc == 2 et serait épinglée pour toujours (au
  mieux), ou déplacée par un emplacement en laissant l'autre pointer sur un bloc libéré si
  les deux tombent à rc == 1 par un autre chemin. Un `debug_assert!(rc == 1)` dans le
  parcours du GC sur les tests différentiels le détecte.
- Allocation dans la page la plus occupée (aide le niveau B, §8.2) contre la plus récente
  (lignes chaudes, M5) : à mesurer.
- ~~Grosses valeurs : mimalloc ou jemalloc~~ : jemalloc, mesuré (§13.2).
- Seuils M = 2 Kio et G = 16 Kio (§6) : décidés, à recaler si la répartition des tailles
  mesurée le demande. Taille de bloc du pool court (§5) : ouvert (hors v1).
- `IORING_OP_MADVISE` avec `MADV_POPULATE_WRITE` (§3).
- Coût du bit CLOCK sur un bench de GET (§10.2), avec et sans préchargement.
- Taille des tranches ; valeurs de départ du déclenchement et des seuils de densité (§8.2)
  et facteur de la réserve (§8.3), à régler sur mesures.
- Découpage des THP sous-utilisées (§11) : confirmer dans les sources que
  `max_ptes_none = 511` le désactive, et son interaction avec `mlock`.

Ajouts du 2026-10-22 (pas encore tranchés) :

- **La save bloque compactage et éviction** (le GC des `SharedByte`, lui, continue
  depuis le 2026-10-22, §8.5) alors que sous AOF les saves sont
  longues (118 s vues sous YCSB à 1 cœur) et se suivent, et que c'est pendant la save que la
  mémoire monte (copies de la barrière). Pistes : **GC derrière le curseur de la save** (un
  nœud déjà écrit par le DFS n'est plus intact : y déplacer ses `SharedByte` ne déclenche
  aucune capture ; seuls les nœuds intacts sont interdits) ; **maxmemory pendant une save** :
  évincer une clé intacte déclenche une copie, donc consomme au lieu de rendre → au plafond,
  refuser les écritures, abandonner la save, ou compter les copies dans une réserve à part
  (Valkey ne compte pas non plus son CoW dans `maxmemory`).
- **Pool par save pour les copies de la barrière** : elles vivent exactement une save ; toutes
  leurs blocs meurent à `end_save` → rendre des pages entières au lieu de `drain_leftover`
  par tranches, et le compteur du pool donne la « mémoire des copies de save » à mettre
  dans `INFO` (DURABILITY_DESIGN.md §16).
- **Worker de persistance optionnel** (DURABILITY_DESIGN.md §16) : invariant à écrire —
  l'allocateur est sans verrou ni atomique et le rc de `SharedByte` est un `u16` non
  atomique ; un worker ne tient jamais un `SharedByte` et n'alloue jamais dans nos pools ;
  le thread métier ne lui passe que des octets encodés (pool système).
- **Fragmentation des grands objets** (§6, jamais évacués) : des suites de N blocs libérés au
  hasard trouent les pages, une page ne se rend qu'entièrement vide → liste des suites
  libres triée par longueur, placement qui regroupe les grands objets.
- **Mode débogage du GC** (repris dans le périmètre v1, §14.1) : un pointeur périmé après évacuation est une lecture de mémoire
  libérée silencieuse → en test, remplir les blocs évacués d'un motif ou les passer
  `PROT_NONE` jusqu'à réutilisation ; une passe de GC entre chaque commande dans les tests
  différentiels.
- **Préparer les pages hors boucle est obligatoire sous pression mémoire** (§3) : avec
  `defrag=defer+madvise`, une faute sur une région `MADV_HUGEPAGE` peut lancer une
  compaction directe du noyau sur le thread fautif (vu : machine à ~500 Mo libres pendant un
  jeu).
Ajout du 2026-10-23 (après l'étape 1, à reprendre plus tard) :

- **Objets hors de l'arbre : externes en v1, provisoire.** Tout l'état est pseudo-statique
  (slabs en `static mut`, cœur unique) : le GC peut visiter n'importe quelle structure, il
  suffit d'un énumérateur, pas seulement l'arbre. Deux pistes pour sortir les membres de
  ZSet, canaux Pub/Sub et noms de client de jemalloc :
  - **les visiter** : énumérateurs pour `ZSET_SLAB`, `subs` / `channels`, `Client::name`.
    Pour le ZSet, un membre est à deux endroits (`sorted` et `scores`, `rc == 2`) : un
    visiteur propre au ZSet déplace les deux ensemble (mêmes octets, donc ni l'ordre ni le
    hachage ne changent ; écrire une clé de `BTreeSet` / `HashMap` en place demande de
    l'`unsafe`) ; ou attendre le ZSet sur HiSlab (ANALYSE_ART_UNIFIE.md §13), où un membre
    n'aurait qu'un emplacement propriétaire ;
  - **des blocs réservés aux objets longs** (pool par durée de vie, M3) : bien pour Pub/Sub
    et les noms (peu de mouvement, volume négligeable) ; mauvais pour un ZSet qui bouge
    sans être visité (un bloc bump ne se réutilise que vide, collectionneur de coupons
    comme en §14.1, là où jemalloc réutilise les trous un par un).

- **Le GC a besoin de repos sur la boucle** : il tourne par le même chemin que la save
  (tranches au repos + plancher). Les gains de RUNTIME_ARCH.md §17.6 (envois et recv
  délégués, attente groupée) sont ce qui lui donne ce repos sous charge.

---

## 16. Spécification d'implémentation de la v1 (2026-10-22)

Cette section suffit pour implémenter la v1 sans décision restante. Elle reprend les choix
des sections précédentes et fixe chaque valeur, nom et emplacement. En cas de conflit avec
une section plus haute, **§16 fait foi pour la v1**. Vocabulaire : en tête du document.

### 16.1 Périmètre

Dans la v1 : en-tête compact, pool `SharedByte` (bump, compteurs, pages, rendu au système),
grands objets dans jemalloc, écrasement sur place, GC niveau A (y compris pendant les
saves), énumérateurs oxidart / valart / List, mode débogage, `INFO`, réglages.

Hors v1 : GC niveau B (on rend seulement les pages devenues vides d'elles-mêmes), objets
de structure (`CompactPath`, pages des nœuds, blocs `HashSmall` / `SetSmall`, débordements
valart : ils restent dans mimalloc), pool court, buffers de connexion, compactage HiSlab,
`maxmemory`, générations (M5 : la v1 prend les blocs libres en LIFO), thread auxiliaire
de préparation des pages, hugetlbfs.

### 16.2 En-tête de `SharedByte`

`SharedByte` garde un seul champ `ptr: NonNull<u8>`, qui pointe désormais **sur les
données**. L'en-tête est lu à reculons, en `u16` alignés (l'adresse des données est
toujours paire) :

```text
forme courte (pool)        : [len_lo u16][rc u16][données…]          en-tête 4 o
forme longue (externe)     : [len_hi u16][len_lo u16][rc u16][données…]   en-tête 6 o
rc     = *(ptr − 2)
len_lo = *(ptr − 4) ; bit 15 = EXTERNE
len    = EXTERNE ? (len_hi << 15) | (len_lo & 0x7FFF) : len_lo     (len_hi = *(ptr − 6))
```

- **EXTERNE = 0** : objet du pool, `len ≤ G = 16 384`. Emplacement = 4 + len arrondi au
  granule supérieur (2 o) ; ses granules = `(4 + len + 1) / 2`.
- **EXTERNE = 1** : objet alloué par jemalloc, base = `ptr − 6`. Longueur max
  2³¹ − 1 (`assert!`). Toute valeur de plus de G octets est externe.
- **Externe forcé** : `SharedByte::from_slice_external(data)` donne toujours la forme
  longue, quelle que soit la taille. Sert aux `SharedByte` qui vivent longtemps hors de
  l'arbre (§16.9).
- `rc` : `u16` saturant, inchangé (à `u16::MAX`, immortel).
- `free` (dernier `drop`) lit EXTERNE : 1 → `jemalloc::free(ptr − 6)` ; 0 → pool (§16.4).
  Pas de test de plage d'adresse sur ce chemin.
- `to_uppercase` (seul autre constructeur interne) passe par les mêmes fonctions.
- Repli de la plage épuisée (§16.3) : l'objet est alloué externe, même s'il est petit.

Tests unitaires (`radixox-lib`) : longueurs 0, 1, 2, 16 383, 16 384, 16 385, 1 Mio ;
forme forcée externe ; `rc` saturé ; `to_uppercase` partagé et non partagé.

### 16.3 Plage, blocs, pages

Nouveau module `radixox-lib/src/pool.rs`, état dans un `static mut` (le serveur est
mono-thread ; les tests le sont déjà, `RUST_TEST_THREADS=1`), initialisé à la première
allocation.

- **Plage** : `mmap(PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE)` de
  `RADIXOX_POOL_VIRT_GB` Gio (défaut **256**) + 2 Mio, base alignée sur 2 Mio,
  `madvise(MADV_HUGEPAGE)` sur toute la plage. Si `mmap` échoue : pool désactivé, tout
  externe, message au démarrage.
- **Compteurs** : `live: *mut u16`, un par bloc (granules vivants), dans un second `mmap`
  `MAP_NORESERVE` de `nb_blocs × 2` o (8 Mio de virtuel pour 256 Gio).
- **Bitmaps** (1 bit par bloc, même technique) : `marked` (GC), `open` (bloc courant d'un
  curseur : le compteur à 0 ne le libère pas).
- **Page** (2 Mio, 32 blocs) : `struct PageState { free_mask: u32, state: u8 }`,
  `state ∈ {Unused, Active, Empty, Released}`, dans un `Vec` indexé par numéro de page,
  agrandi au fil des pages mises en service.
  - `Unused` : jamais touchée (au-delà de `next_page`).
  - `Active` : au moins un bloc occupé ou ouvert.
  - `Empty` : 32 blocs libres, mémoire encore tenue ; entrée dans `empty_pages`.
  - `Released` : rendue par `MADV_DONTNEED` ; réutilisable comme une page neuve.
- **Listes** :
  - `pages_with_free: Vec<u32>` (pile LIFO) : pages `Active` dont `free_mask != 0`.
    Suppression paresseuse : au dépilement, ignorer une page dont le masque est vide ou
    l'état n'est plus `Active`.
  - `empty_pages: VecDeque<(u32, Instant)>` : pages `Empty` et l'instant où elles le sont
    devenues (les plus anciennes devant).
  - `released_pages: Vec<u32>` : pages `Released`.
- **Prendre un bloc** (`take_block`) : dans l'ordre, (1) dépiler `pages_with_free`, bit le
  plus bas du masque ; (2) une page de `empty_pages` (la plus récente, en queue) ; (3) une
  page de `released_pages` ; (4) `next_page += 1` (si la plage est pleine : repli externe,
  compteur `mem_pool_range_full`). Les cas 3 et 4 font une faute de page synchrone dans
  la boucle (v1), comptée dans `mem_pool_sync_faults`. Le bloc pris est marqué `open`.
- **Libérer un bloc** (compteur tombé à 0 et bloc non `open`, ou bloc fermé avec un
  compteur à 0) : bit mis dans `free_mask` ; si le masque était vide, la page est empilée
  dans `pages_with_free` ; si le masque devient plein (`u32::MAX`), la page passe `Empty`
  et entre dans `empty_pages`. Démarquer le bloc s'il était marqué.
- **Rendre des pages** (`release_step`, appelé seulement depuis `idle_step`, jamais sous
  charge) : tant que `empty_pages.len() × 2 Mio > réserve` (§16.6) et que la plus ancienne
  est vide depuis plus de `RADIXOX_MEMORY_DECAY` secondes (défaut **10**) :
  `madvise(base_page, 2 Mio, MADV_DONTNEED)` (appel direct en v1), état `Released`.

### 16.4 Allocation et libération dans le pool

Trois curseurs bump : `main` (emplacements ≤ M = 2 Kio), `medium` (fenêtre, > M), `survivor`
(destination du GC, toutes tailles ≤ G). Un curseur = bloc courant + `top` + `end`.

- **`alloc_main(size)`** : si `top + size ≤ end` : `top += size`, `live[bloc] += size / 2`.
  Sinon fermer le bloc (bit `open` à 0 ; s'il a `live == 0`, le libérer), `take_block`,
  recommencer. Pas d'objet à cheval sur deux blocs, jamais.
- **`alloc_medium(size)`** : fenêtre de 8 blocs ouverts, reste de chacun en granules
  (`[u16; 8]`, 0 = case vide). Best-Fit : parmi les cases dont le reste ≥ taille, celle
  au plus petit reste (boucle scalaire). Aucune : prendre un bloc ; si la fenêtre est
  pleine, fermer la case au plus petit reste. Une case dont le reste tombe sous M est
  fermée tout de suite. Chaque case a son propre `top` (bump dans son bloc).
- **`alloc_survivor(size)`** : comme `main`, sur son propre bloc courant (≤ M) ; au-delà de
  M, une seconde fenêtre de 8 blocs réservée aux survivants (même code que `medium`).
- **`free_pool(ptr)`** : `bloc = (ptr − 4 − base) >> 16`, `g = granules(len)`. Si l'objet
  est le dernier du bloc courant de son curseur (`ptr − 4 + 2g == top`), `top -= 2g`.
  Puis `live[bloc] -= g` ; à 0 et bloc non `open` → libérer le bloc (§16.3).
- **Comptes globaux** : `live_bytes` (somme des emplacements vivants), `held_blocks`
  (blocs de pages `Active` non libres). Servent au déclenchement (§16.6) et à `INFO`.

### 16.5 Écrasement sur place

`SharedByte::overwrite_from(&mut self, src: &[u8]) -> bool` : vrai et fait si `rc == 1`,
EXTERNE == 0, `src.len() ≤ G` et `granules(src.len()) ≤ granules(self.len())` ; alors copie
des données, nouveau `len_lo`, `live[bloc] -= granules(ancien) − granules(nouveau)`,
`live_bytes` ajusté. Sinon faux et rien ne change.

Branché dans `OxidArt::set_raw` (`oxidart/src/lib.rs`), aux deux endroits qui remplacent
une valeur existante : la branche *self-val* (`take_self` + `put_self`) et la branche
`Step::SameLeaf` (`update_leaf`). **Toujours après `node_mut(idx)`** : la barrière de save
a alors déjà capturé le nœud s'il était intact, et sa capture tient un clone, donc
`rc == 2` et l'écrasement est refusé tout seul. Si la nouvelle valeur est `Tag::Bytes` et
l'ancienne aussi, tenter `overwrite_from` ; en cas de succès, mettre à jour le TTL comme
aujourd'hui et laisser tomber la `SharedByte` de l'argument (dernier objet du bloc
courant : `top` recule, §16.4).

### 16.6 GC niveau A

**Énumérateurs** (le parcours ne connaît que le layout, §8.1) :

- `hislab` : ajouter `slot_span(&self) -> u32` (= `lvl1.len() << 9`) et
  `next_occupied(&self, from: u32) -> Option<u32>` sur `HiSlab` et `TaggedHiSlab`
  (nouvelle version du crate `hislab`, à publier avant radixox).
- `OxidArt::gc_visit(&mut self, from: u32, budget: u32, f: &mut impl FnMut(&mut SharedByte)) -> Option<u32>`
  : visite les slots occupés de `map` à partir de `from`, au plus `budget` slots, rend le
  slot suivant (ou `None` à la fin). Par nœud : la valeur propre si `tag == Bytes` ; chaque
  feuille `Bytes` (`node.iter()` puis `leaf_val_mut(radix)`) ; les entrées des blocs
  `HashSmall` / `SetSmall` (`small.rs`, `n` entrées de `W` `SharedByte`). Accès au nœud par
  `map.get_unchecked_mut`, **pas par `node_mut`** : réécrire un pointeur vers les mêmes
  octets n'est pas une mutation et ne doit pas passer par la barrière (§8.5). C'est la
  seule exception au point de passage ; un commentaire le dit à cet endroit.
- `ValTrees::gc_visit(...)` (même signature) : slots occupés de `nodes` ; par nœud
  (`HashNode` / `SetNode`) les clés et valeurs inline puis celles du bloc de débordement ;
  puis, une fois les slots finis, les `CollisionBuckets` (Hash puis Set).
- Listes : `gc_visit_lists(...)` sur `LIST_SLAB` (`VecDeque::iter_mut`).
- ZSet : **non visité** (chaque membre est dans `sorted` et dans `scores`, `rc == 2`). Ses
  membres sont alloués externes (§16.9) et n'épinglent donc aucun bloc.

**La fonction appliquée à chaque `SharedByte`** (`pool::gc_relocate(&mut SharedByte)`) :

1. EXTERNE → rien. Adresse hors plage → rien.
2. `marked` faux pour son bloc → rien.
3. `rc != 1` → épinglé : `gc_pinned += 1`, rien.
4. Sinon : `new = alloc_survivor(emplacement)`, copie de l'en-tête et des données,
   `let old = mem::replace(sb, new); drop(old);` (le `drop` fait tomber rc à 0 et libère
   l'ancien emplacement par le chemin normal). `gc_evacuated_objects/bytes += …`.

**Réserve** : `R = max(2 pages, live_bytes / RADIXOX_GC_RESERVE_DIV)`, diviseur par défaut
**16**.

**Déclenchement** (évalué au plus toutes les 100 ms, depuis `run_jobs` et `idle_step`,
seulement s'il n'y a pas de passe en cours) :

- `held = held_blocks × 64 Kio` ; `f = 1 − live_bytes / held` ; `récupérable = held − live_bytes`.
- `au_repos` = `idle_step` a été appelé dans les 10 dernières ms.
- **au repos** : passe si `f > 5 %` ; seuil de densité **90 %**.
- **sous charge** : passe si `f > 15 %` **et** `récupérable ≥ 8 Mio` **et** blocs libres
  disponibles (blocs libres des pages `Active` + pages `Empty`) < R ; seuil **75 %**.
- Une passe qui a rendu moins de 2 Mio de blocs suspend le déclenchement pendant 10 s.

**Marquage** (début de passe, figé ensuite) : blocs non `open`, `live > 0`, densité
`live × 2 / 65 536` sous le seuil ; tri par densité croissante ; on marque tant que la
somme de leurs octets vivants ≤ R. Liste `marked_list: Vec<(u32 /*bloc*/, u8 /*passes*/)>`.

**Passe** : curseur `(phase, slot)` avec `phase ∈ {Oxidart, Valart, Lists}`. Valart est
**sauté si une save est en cours** au moment d'y entrer (`OxidArt::is_saving()`), et la
passe se termine alors après les listes. Fin de passe : pour chaque bloc encore marqué
(`live > 0`), `passes += 1` ; à 2, démarqué (`gc_unmarked_blocks += 1`) ; les autres restent
marqués pour la passe suivante, qui démarre aussitôt si le déclenchement le permet.

**Cadence** :

- `idle_step` : une tranche de **512 slots** par appel (la boucle `idle_work` enchaîne les
  appels tant que personne n'attend), ordre : jobs de connexion, libérations, save, **GC**,
  puis `release_step`.
- `run_jobs` (sous charge, une fois par tour) : `budget = max(64, alloc_tour × N / R)`
  slots, avec `alloc_tour` = octets alloués dans le pool depuis le tour précédent et `N` =
  somme des `slot_span` d'oxidart, valart et des listes.
- Une page neuve prise faute de bloc libre pendant une passe : permis, compté dans
  `gc_fresh_pages`.

**Pendant une save** (§8.5) : la passe continue sur oxidart et les listes ; aucune
capture (accès sans barrière) ; valart sauté ; compactage HiSlab inexistant en v1.

### 16.7 jemalloc

- Dépendance `tikv-jemalloc-sys` dans `radixox-lib`, **sans** la fonctionnalité
  `unprefixed_malloc_on_supported_platforms` : les symboles restent préfixés, jemalloc ne
  remplace pas le `malloc` du processus et ne sert qu'aux objets externes.
- Configuration par le symbole `malloc_conf` préfixé (vérifier le nom exact,
  `_rjem_malloc_conf`, dans la documentation de la version retenue) :
  `"background_thread:true,dirty_decay_ms:10000,muzzy_decay_ms:0"`. Au démarrage, relire
  `opt.background_thread` par `mallctl` et le signaler dans `INFO` (`mem_external_bg_thread`).
  Le thread de fond de jemalloc ne touche à aucune donnée de RadixOx.
- Comptes tenus par RadixOx : `mem_external_bytes` (somme des `6 + len` vivants),
  `mem_external_objects`.

### 16.8 Interrupteurs, réglages, `INFO`, journal

- `--cfg radixox_sysalloc` (existant, ASan) et nouveau `--cfg radixox_nopool` : pas de pool
  ni de jemalloc, tous les `SharedByte` en forme longue alloués par `raw_alloc` (mimalloc,
  ou malloc sous `radixox_sysalloc`). Le GC devient un no-op (aucune adresse dans la plage).
- `--cfg radixox_gc_debug` : blocs libérés remplis de `0xDB` ; vérificateur appelable
  (`pool::verify(&OxidArt)`) qui recompte les granules de chaque bloc à partir des
  énumérateurs et les compare à `live` ; une passe de GC complète forcée après chaque
  commande dans les tests différentiels.

| variable | défaut | effet |
|---|---|---|
| `RADIXOX_GC` | `1` | `0` : pas de GC (pool et rendu des pages actifs) |
| `RADIXOX_GC_RESERVE_DIV` | `16` | réserve = `max(2 pages, vivants / div)` |
| `RADIXOX_MEMORY_DECAY` | `10` | secondes avant de rendre une page vide au-delà de la réserve |
| `RADIXOX_POOL_VIRT_GB` | `256` | plage virtuelle du pool |
| `RADIXOX_GC_LOG` | `0` | `1` : une ligne sur stderr par passe (début, blocs marqués, fin : évacués, épinglés, démarqués, durée) et par page rendue |

À ajouter au tableau *Configuration* du README.

`INFO`, nouvelle section `# Memory` (ou ajout à la section existante) :
`mem_pool_live_bytes`, `mem_pool_held_bytes`, `mem_pool_fragmentation` (f, 2 décimales),
`mem_pool_pages_active`, `mem_pool_pages_empty`, `mem_pool_pages_released`,
`mem_pool_sync_faults`, `mem_pool_range_full`, `mem_external_bytes`,
`mem_external_objects`, `mem_external_bg_thread`, `gc_enabled`, `gc_running`,
`gc_passes`, `gc_evacuated_objects`, `gc_evacuated_bytes`, `gc_pinned`,
`gc_unmarked_blocks`, `gc_fresh_pages`, `gc_last_pass_ms`, `gc_reserve_bytes`.

### 16.9 `SharedByte` vivant hors de l'arbre

Ils épingleraient leur bloc pour toujours (le GC ne les voit pas) : ils sont **convertis en
externe** à l'endroit où ils sont gardés (`SharedByte::into_external(self) -> SharedByte`,
copie si l'objet est du pool, sinon rien) :

- membres de ZSet : à l'insertion dans `ZSetInner` (`oxidart/src/zset_inner.rs`, `insert`) ;
- Pub/Sub : clés de `subs` (`radixox/src/uring/core.rs`) et `channels` des connexions
  (`radixox/src/uring/conn.rs`) ;
- nom de client (`radixox/src/exec/client.rs`, `name`).

Les temporaires (arguments de commande, trames de réponse, journal, réplication, motif
d'un job KEYS) restent dans le pool : ils meurent vite, leur bloc se vide seul.

**Audit d'unicité (prérequis, §15)** : avant d'activer le GC par défaut, le test
différentiel en mode `radixox_gc_debug` vérifie après chaque commande que chaque
`SharedByte` visité par les énumérateurs a `rc == 1`. Tout cas `rc > 1` hors ZSet est un
partage à remplacer par une copie profonde.

### 16.10 Ordre d'implémentation

Chaque étape : tests verts, commit, push (règle du dépôt) ; mesure courte quand indiquée.

1. **En-tête** (§16.2) + jemalloc pour > G + `from_slice_external` / `into_external` et
   leurs branchements (§16.9) ; le reste encore dans mimalloc. Mesure : débit et RSS sur
   `scripts/bench_vs_valkey.sh` (pas de régression).
   **Fait le 2026-10-23.** tikv-jemalloc-sys 0.6.1 (jemalloc 5.3.0), `_rjem_malloc_conf`
   exporté depuis `raw_alloc.rs` ; `INFO` relit `background_thread` (l'état effectif,
   plutôt que `opt.background_thread`) et le thread `jemalloc_bg_thd` est bien là dans le
   binaire release. `free` des externes par `sdallocx` (taille connue). `OwnedByte` n'est
   plus `Send`. ZSet : un membre existant garde l'objet stocké (aucune copie au changement
   de score), seuls les nouveaux sont convertis.
2. **Pool** (§16.3, §16.4) sans GC, derrière `--cfg radixox_pool` (désactivé par défaut à
   cette étape). Tests unitaires du pool : rembobinage du `top`, réutilisation d'un bloc à
   0, page vide puis rendue, plage pleine → externe, fenêtre Best-Fit.
   **Fait le 2026-10-23** (`radixox-lib/src/pool.rs`). Écarts à §16.3–16.4 :
   - `release_step` tourne sur un **timer** du cœur (`Timer::MemRelease`, 1 s ; 10 ms
     tant qu'il en reste), au plus 4 pages par appel : `idle_step` ne tourne que s'il y a
     du travail de fond, une boucle au repos complet n'aurait jamais rendu ses pages ;
   - `PageState` porte un drapeau `in_stack` : une page n'est jamais deux fois dans
     `pages_with_free` (la suppression paresseuse seule laissait des doublons s'accumuler) ;
   - un bloc **ouvert** dont le compteur tombe à 0 repart du début (le `top` revient à la
     base du bloc), pas seulement quand l'objet libéré est le dernier ;
   - le curseur `survivor` et le bitmap `marked` attendent l'étape 6 (GC) ;
   - `INFO` : les `mem_pool_*` de §16.8 (+ `mem_pool_reserve_bytes`), absents tant que le
     pool n'a rien alloué.
   Essai : 1M × 100 o → 104 o par valeur dans le pool, fragmentation 0,00, RSS 136 Mo ;
   FLUSHDB puis 12 s → 47 pages rendues (réserve 2), RSS 42 Mo.
   **Sans GC, le pool ne tient pas les écrasements** (mesuré le 2026-10-23, comme prévu en
   §14.1) : 1M × 100 o puis 5 s de SET au hasard → RSS 250 Mo contre 142 Mo pour mimalloc,
   fragmentation 0,53 (presque aucun bloc ne se vide) ; YCSB A (`bench_compare.sh`) : pic
   de RSS 2 119 Mo contre 1 569 Mo.
3. **Écrasement sur place** (§16.5). Test : SET répétés de même taille → `live_bytes`
   stable, aucune allocation ; SET pendant une save → valeur sauvegardée = ancienne.
   **Fait le 2026-10-23.** Au-delà de §16.5, branché aussi dans les **champs de Hash** (le
   cas de YCSB A) : petit bloc (`HashMut::set`) et valart (`hash_set`, collisions
   comprises). **Correction de §16.5 :** `rc == 1` ne protège que des captures qui clonent
   (nœud oxidart, petit bloc, vue T0 des collisions). L'image d'un nœud valart
   (`HashNode::image_copy`) copie ses `SharedByte` bit à bit (rc reste à 1, les remplacées
   vivent par `deferred_bytes`) : valart refuse l'écrasement tant qu'une save est active.
   Un test retire cette garde et voit le dump faux (`overwrite_during_save_keeps_t0`).
   Pas d'écrasement si la présence du TTL d'une feuille change (elle changerait de
   taille) : chemin normal. La capacité est celle de la valeur courante (un écrasement
   plus court perd la fin). Mesure, 1M × 100 o puis 5 s de SET au hasard : même taille →
   RSS 136 Mo stable, fragmentation 0,00 ; tailles de 50 à 150 o → 246 Mo, fragmentation
   0,52 (le GC, étape 6).
4. **`hislab`** : `slot_span`, `next_occupied` ; publication.
   **Fait le 2026-10-23** (hislab af853d3, 0.2.4), **publié sur crates.io** le même jour
   (tag `v0.2.4`) ; le workspace en dépend depuis crates.io.
5. **Énumérateurs** (§16.6) et `pool::verify` ; test différentiel en `radixox_gc_debug`
   avec l'audit d'unicité.
   **Fait le 2026-10-23** (`oxidart/src/gc.rs`). `gc_visit` / `gc_visit_valart` /
   `gc_visit_lists` (contrat de §16.6 ; les collisions de valart sont visitées d'un coup en
   fin de slab), `gc_visit_all`, `gc_census` + `pool::verify_census` : par bloc, recensé ≤
   `live` (l'égalité est impossible quand d'autres objets vivent hors de l'arbre, l'oracle
   des tests différentiels alloue dans le même pool) ; égalité exacte du total dans un test
   dédié (`gc_census_matches_tree_bytes`). Blocs libérés remplis de `0xDB` sous
   `radixox_gc_debug`.
   **Audit d'unicité** (`gc_audit_unique`) après chaque opération des tests différentiels
   et, sous `radixox_gc_debug`, après chaque commande du serveur (`run_cmd`, sauf job
   parqué ; `RADIXOX_GC_AUDIT=0` le coupe). **Aucun partage dans l'arbre trouvé.** Les cas
   vus étaient légitimes et sont exclus : save en cours, **et feuilles d'images après
   `end_save` tant que `drain_deferred` ne les a pas vidées** (`save.leftover`, clones) ;
   valart et les listes sont des slabs globaux, audités seulement quand un seul `OxidArt`
   vit dans le processus (compteur `LiveTree` ; tests à plusieurs nœuds) et valart hors de
   **sa** save (`ValTrees::is_saving`, pas celle de l'arbre : c'est aussi la condition du
   GC). Le harnais différentiel donne désormais des copies indépendantes à l'arbre testé et
   à l'oracle, comme le serveur. Deux tests d'intégration à garantie temporelle
   (`kill9_continuous_keeps_an_exact_prefix`, marge de 200 ms ; `restart_reloads_the_dump`,
   TTL) ne tiennent pas l'audit O(arbre) par commande : verts avec `RADIXOX_GC_AUDIT=0`
   (remplissage `0xDB` toujours actif).
6. **GC niveau A** : marquage, passe, cadence, save, journal, `INFO`. Tests : passe forcée
   entre chaque commande (différentiel), passe pendant une save (dump identique à celui
   d'une save sans GC), objet épinglé tout le long d'une passe puis relâché, valeur ratée
   (recompression derrière le curseur) rattrapée à la passe suivante.
   **Fait le 2026-10-23.** Pool : marquage (`gc_begin`), curseur et fenêtre des survivants,
   fin de passe (`gc_end_pass`), démarquage à la libération d'un bloc ; relocalisation par
   `SharedByte::gc_relocate` ; pilote `oxidart::gc::GcPass` (oxidart → valart → listes) ;
   cœur : `Timer::Gc`, tranches dans `run_jobs` et `idle_step`, `INFO` (`gc_*`),
   `RADIXOX_GC`, `RADIXOX_GC_LOG`. Écarts à §16.6 :
   - **déclenchement par un timer de 100 ms** (`run_jobs` et `idle_step` ne tournent que
     s'il y a déjà du travail : sans timer, aucune passe ne démarrerait) ;
   - **au repos = rien reçu depuis 10 ms** (`last_rx`), pas « `idle_step` appelé » ;
   - **la save de valart est testée à chaque tranche** de la phase valart, pas seulement à
     l'entrée : une save peut démarrer entre deux tranches, et l'image d'un nœud valart
     copie ses `SharedByte` bit à bit. C'est `ValTrees::is_saving` (valart est global) ;
   - **aucun bloc marquable = passe faible** : déclenchement suspendu 10 s (sinon un
     marquage à vide toutes les 100 ms) ;
   - passe forcée des tests (`gc_full_pass`) : ferme d'abord tous les curseurs, marque tous
     les blocs non vides sans limite de réserve, démarque à la fin ; lancée après **chaque
     opération** des tests différentiels dès que le pool est compilé (pas seulement en
     `radixox_gc_debug`), saves en cours comprises : le dump reste exact.
   Mesures (1M × 100 o, `taskset` serveur sur le CPU 0, profil `powersave`) : 5 s de SET
   de 50 à 150 o au hasard → fragmentation **0,05** (0,52 sans GC), tenu 109 Mo pour
   104 Mo vivants, 20 passes de ~280 ms sous charge, 228 K ops/s p99 0,35 ms (pas d'A/B
   encore) ; DEL de 90 % puis 3 s de repos → tenu **10,5 Mo** pour 10,4 Mo vivants ; après
   le délai, 59 pages rendues, RSS 80 Mo. **Limite** : ces 161 blocs restent épars sur 20
   pages (une page ne se rend qu'entière) ; le critère de l'étape 7 (« RSS après −90 % puis
   GC ≤ vivants × 1,15 + R ») demande de regrouper les survivants (niveau B, ou le choix de
   page de §15 : allouer dans la page la plus occupée).
   **Branche `gc-page-occupancy` (2026-10-23, décision à valider par l'utilisateur)** :
   - `take_block` prend dans la **page la plus occupée** : pages actives trouées en listes
     doublement chaînées par nombre de blocs libres (1 à 31) + masque des listes non vides,
     O(1) exact (remplace la pile LIFO paresseuse et `in_stack`). Seul, sans effet mesuré
     sur le scénario « DEL 90 % » (20 et 23 pages actives) : après une suppression
     uniforme, toutes les pages sont à ~10 %, marquer bloc par bloc ne vide aucune page ;
   - **niveau B réduit au marquage** (`Mark::Pages`) : au repos, si les pages actives
     dépassent `vivants × 1,15 + R` (+ 1 page de marge) alors que les blocs ne sont plus
     fragmentés, une passe marque **tous** les blocs des pages les plus creuses (denses
     compris, pages sans bloc ouvert, dans la réserve) ; ces pages sortent des listes
     (`evacuating`) pour que les survivants n'y retournent pas, et y reviennent en fin de
     passe si elles ne se sont pas vidées ;
   - mesure, même scénario : **6 pages actives au lieu de 20**, RSS 52 Mo au lieu de 80 ;
     pool = 6 pages + 2 de réserve = 16 Mio pour 10,4 Mo vivants, soit le critère de l'étape
     7 (12 + 4 Mio). Deux passes `Pages` (102 puis 52 blocs, 8 ms et 1 ms) après les passes
     de niveau A au repos. Débit non comparé (profil `powersave`, mesures bruitées : 168 K à
     228 K ops/s selon le passage, sans changement de code).
   - **Tranches à travail fixe** (choix de l'utilisateur, 2026-10-23) : une tranche
     s'arrête au premier plafond atteint, **10 000 slots visités ou 2 blocs (128 Kio)
     recopiés**, la même sous charge (une par tour, dans `run_jobs`) et au repos
     (enchaînées dans `idle_step`). Remplace le budget `max(64, alloués × N / R)` de §16.6 :
     les temporaires (arguments, rendus aussitôt) gonflaient « alloués » sans consommer de
     bloc, et le GC travaillait trop par tour sous charge. `INFO` : `gc_max_slice_us`, pour
     régler les plafonds sur mesure (ordre de grandeur attendu : 100–200 µs pour 10 000
     slots). Le compteur `allocated` du chemin chaud est retiré.
   - **bench_compare.sh (YCSB A, 500 K enregistrements, 2026-10-23, avant les tranches
     fixes)** : 1 cœur, pool = mimalloc (231 K contre 227 K ops/s, p99 838 contre 863 µs,
     RSS 191 contre 183 Mo) ; 3 cœurs, pool −8 % une fois ramené au Valkey de chaque run
     (ratio 1,17 contre 1,26), GC coupé compris (1,15). `perf` (qualitatif, machine
     chargée) : allocation + libération du pool ≈ mimalloc sur la boucle (~1,7 % contre
     ~2,2 %) ; le surplus = GC sous charge (`gc_relocate` + `gc_slice` ≈ 3,5 %), passes
     déclenchées au repos (creux de 10 ms du client) pour reboucher les trous laissés par
     les temporaires du chargement. À remesurer avec les tranches fixes ; la vraie racine
     est le pool court des temporaires (§5).
   - **A/B après les tranches fixes** (2026-10-23, machine au repos, `SERVER_CORES=3`,
     500 K / 4 M, paires alternées B A B A) : RadixOx 431 732 / 429 139 / 438 452 /
     434 169 ops/s (mimalloc / pool / mimalloc / pool), soit −0,6 % et −1 % bruts, −2 %
     ramené au Valkey de chaque run (ratios 1,210 / 1,210 / 1,234 / 1,186) : le −8 % a
     disparu. READ p99 +3 à 4 %, p99.9 +6 à 20 % (786→831, 617→747 µs), p99.99 égal ; RSS
     +8 à 10 Mo (YCSB A ne supprime rien). Le critère de l'étape 7 (« p99.9 identique »)
     n'est pas strictement tenu : à trancher (activer avec ce surcoût, ou d'abord le pool
     court des temporaires, §5).
7. **Pool + GC activés par défaut** (`radixox_pool` retiré, `radixox_nopool` devient
   l'interrupteur) une fois les critères de §8.7 tenus : RSS après −90 % puis GC
   ≤ vivants × 1,15 + R ; écrasements au hasard 30 s : RSS stable à ±15 % ; débit et
   p99.9 identiques à mimalloc ; première requête après un creux ≤ une tranche.
   **Fait le 2026-10-23, sur décision de l'utilisateur**, sans attendre le pool court : le
   critère « p99.9 identique » n'est pas strictement tenu (+6 à 20 % à 3 cœurs, débit −1 à
   −2 %, étape 6). `radixox_pool` retiré ; `radixox_nopool` (ou `radixox_sysalloc`, ASan ne
   voyant pas les objets du pool) ramène la forme courte dans mimalloc et rend le GC inerte.
8. Mettre à jour `ARCHITECTURE.md`, le README (configuration, `INFO`) et l'état en tête de
   ce document. **Fait le 2026-10-23.**

---

## 17. Objets de structure dans le pool (après la v1, 2026-10-23)

Suite de §8.6 : les allocations de l'arbre quittent mimalloc pour devenir déplaçables par le
GC. Mesure de départ (1M clés uuid4 base64 → 1M valeurs, + 100K `probe`, 1,1M clés en
tout) : arène mimalloc 42 Mo, slab des nœuds 35 Mo, pool (valeurs) 28 Mo, RSS 122 Mo ;
après suppression de la moitié par `UNLINK c*`, **rien de l'arbre n'est rendu** (RSS 114 Mo
au bout de 16 s, seul le pool baisse). Recensement des tailles (test temporaire) :

| objet | uuid b64 | YCSB `user<chiffres>` |
|---|---|---|
| suffixes `CompactPath` sur le tas | 1M (un par clé), tous ≤ 24 o, 24 Mo | 1M, 73 % ≤ 16 o, reste ≤ 24 o, 18 Mo |
| pages des nœuds | 96K, 6,5 Mo, 87 % ≤ 64 o, max 768 o | 61K, 2,6 Mo, max 112 o |
| préfixes `CompactPath` sur le tas | 0 | 0 |
| nœuds (slab de 128 o) | 276K, 35 Mo | 411K, 53 Mo |

### 17.1 `CompactPath` (fait, branche `pool-structures`)

- Forme tas = un objet du pool des `SharedByte`, **sans compteur de références** (choix de
  l'utilisateur : propriétaire unique, le `rc` serait 2 o par clé et un test pour rien).
  Pointeur sur les données, en-tête lu à reculons en `u16` : forme courte `[len u16]`
  (≤ 16 Kio, pool ou mimalloc sans pool), forme longue `[len_hi][len_lo | 0x8000]` dans
  jemalloc. Données à adresse paire : le bit 0 distingue toujours le tas de l'inline.
- `pool::short_alloc` / `short_free` / `short_shrink` (publics, choix pool ou mimalloc au
  même endroit pour `SharedByte` et `CompactPath`), `pool::ENABLED`.
- GC : trait `oxidart::gc::GcVisitor` (`bytes` + `path`, une closure sur `&mut SharedByte`
  en est un) ; `Node::for_each_path_mut` (préfixe, suffixes des feuilles, `Loc.sfx`) ;
  `CompactPath::gc_relocate` (jamais épinglé) ; `pool_slot` pour `verify_census`. Copies
  bit à bit vérifiées : les images de save recopient le suffixe (`from_slice`), le transit
  `Raw` de `node.rs` ne traverse pas de GC.
- Mesure (même scénario) : arène mimalloc **12 Mo** (pages des nœuds seules), pool 56 Mo
  (48,6 Mo vivants : valeurs + chemins), RSS chargé 120 Mo ; après suppression de la
  moitié, RSS **96 Mo** au bout de 16 s (114 Mo avant), pool 32 Mo.

### 17.2 Pages des nœuds (fait, branche `pool-structures`)

- Deux curseurs de plus dans le **même** `Pool` (§17.3) : `STRUCT` et `STRUCT_SURVIVOR`,
  qui n'allouent que des multiples de 8 et restent donc alignés sur 8 (les pages
  contiennent des `u32` / `u64` lus alignés) ; leurs blocs ne mélangent pas les deux
  alignements. Taille des pages inchangée (`alloc_class`, la marge de croissance).
- `pool::struct_alloc` / `struct_free` : pool jusqu'à 16 Kio, jemalloc au-delà ou plage
  pleine, mimalloc sans pool ; le `free` est aiguillé par l'appartenance à la plage (une
  page n'a pas d'en-tête). `relocate8` et `struct_slot` pour le GC et le recensement ;
  `GcVisitor::page`, `Node::page_mut` ; la page est visitée avant les valeurs et les
  suffixes, lus ensuite à sa nouvelle place.
- Mesure (scénario uuid de §17) : arène mimalloc **< 1 Mo**, RSS chargé **118 Mo**, après
  suppression de la moitié **90 Mo** au bout de 16 s (pool 66 → 38 Mo).

### 17.4 Petits blocs Hash/Set et débordements valart (fait, branche `pool-structures`)

- `small.rs` : `struct_alloc` / `struct_free` (taille `8 + cap × W × 8`, déjà multiple de
  8), `block_bytes` ; débordements valart (`overflow.rs`) : `struct_alloc` + remise à zéro
  (le pool ne rend pas de mémoire à zéro), taille arrondie à 8 à l'allocation comme à la
  libération (`child_cap × 4` peut ne pas l'être) ; en test de valart, toujours
  l'allocateur système (ASan).
- **Une seule passe de GC** (choix de l'utilisateur) : elle parcourt les nœuds, là où sont
  les pointeurs. `visit_val` déplace le petit bloc (`GcVisitor::page`) puis visite ses
  entrées à sa nouvelle place ; valart a son trait `ValVisitor` (`bytes`, `block`),
  `gc_block` donne le bloc sans tag au visiteur et remet le tag ; oxidart fait la
  passerelle (`Bridge`). Le bloc d'un nœud valart est visité avant ses entrées.
- Plus aucune arène mimalloc dans le serveur. Mesure, 1M de Hash à un champ façon YCSB
  (`user<chiffres>`, `field0`, 100 o) puis `UNLINK user1*` … `user5*` (56 % des clés) :
  pool 178 → 74 Mo, slab des nœuds 53 → 29 Mo (compactage), **RSS 246 → 118 Mo** au bout
  de 16 s.

### 17.5 Mémoire contre Valkey (2026-10-23, `main` 875fb11)

Méthode (scripts du scratchpad, non versionnés) : 1M clés par scénario, mêmes données et
mêmes clés supprimées pour tous (RadixOx `UNLINK prefix*`, Valkey `SCAN MATCH` + `UNLINK`),
un cœur chacun, sans persistance, RSS stabilisé (15 s ; 30 s pour Valkey avec
`activedefrag`, qui travaille progressivement). Tailles de valeurs en cloche (normale bornée)
à la demande de l'utilisateur : des valeurs toutes à 100 o tombent dans une seule classe de
jemalloc et avantagent Valkey sur les suppressions groupées (avec 100 o fixes : Valkey
164 → 94 Mo, RadixOx 140 → 86 Mo). Valkey 9.1.2 compilé depuis les sources (jemalloc
modifié ; le paquet d'Arch, compilé avec le jemalloc du système, refuse `activedefrag`),
`active-defrag-ignore-bytes 1mb`.

| scénario | RadixOx | Valkey | Valkey `activedefrag` |
|---|---|---|---|
| chaînes `key:N` 0–200 o, suppression 56 % | **142 → 86** | 170 → 138 | 168 → 102 |
| UUID → UUID 22 o, suppression 50 % | 108 → 70 | 93 → 93 | **93 → 66** |
| Hash à un champ 0–200 o, suppression 56 % | **246 → 118** | 330 → 330 | 329 → 168 |
| chaînes 0–200 o puis réécriture 100–300 o | **142 → 258** | 168 → 342 | 168 → 287 |

Pics (`VmHWM`) : RadixOx 142 / 114 / 246 / 258, Valkey `activedefrag` 168 / 96 / 329 /
341. Cas faible de RadixOx : clés aléatoires courtes (peu de préfixes communs). Non mesuré :
le CPU de chaque mécanisme ; un seul passage par case.

### 17.6 Suite

1. **Compactage de la slab valart** (même mécanisme que §17.2 bis, idx d'enfants propres).
2. ~~Publier `hislab` 0.2.5, fusionner `pool-structures` dans `main`~~ : fait le 2026-10-23.
2. **Compactage HiSlab** (§9) : le plus gros poste restant (35 Mo uuid, 53 Mo YCSB) ; la
   slab ne rétrécit jamais. **En arrière-plan** (tranches au repos, comme le GC), avec
   **libération sur critères** (excédent tenu qui dure, comme le niveau B) — idée de
   l'utilisateur, 2026-10-23, à reprendre plus tard. Spec (discussion du 2026-10-23) :
   - **machine à états** : déclenchée sur critère de défragmentation, suit `occupés` et
     `span` ; ne rend que la **fin** de la slab (adresse = `idx × 128`) : on déplace les
     slots occupés les plus hauts vers les trous les plus bas, puis on rend les pages de
     2 Mio (16 384 nœuds) devenues libres en fin. Une page de fin déjà vide se rend sans
     rien déplacer ; vérifier que `insert` comble les trous par le bas (alors les
     insertions aident) ;
   - **critères (utilisateur), avec hystérésis** : déclenchement si le libre sous la fin
     dépasse **3 Mio** (**4 Mio** au-delà de ~200–500 Mio de slab) ; une page n'est rendue
     que s'il reste **≥ 2 Mio** libres ensuite ; si les insertions pendant le compactage
     ont ramené le libre sous le seuil, on ne rend rien. Variante à mesurer : seuil
     proportionnel `max(2 Mio, slab / 128)` ;
   - **coût d'un déplacement** = lignes de cache touchées au hasard : copie (2 lignes
     source + 2 destination) **4**, parent (`parent_idx` + radix → `set_child_idx`) **2**,
     **+1 par enfant** (son `parent_idx`, jusqu'à 256) ; tranche ≤ **~4 096 unités**
     (~200–400 µs si chaque défaut est payé à son tour). **Pipeline de 8 nœuds à déplacer**
     (§9.2, correction de l'utilisateur) : pendant le lot courant, préchargement des
     sources, des trous de destination, des parents et des enfants des 8 nœuds suivants,
     dont les défauts se recouvrent ; coût par unité ≈ latence d'un défaut / profondeur du
     pipeline. Un nœud n'est jamais coupé entre deux tranches (un 256 enfants = 262
     unités). Le bit de TTL de la `TaggedHiSlab` suit le nœud. **À mesurer** : coût réel
     par unité, avec et sans pipeline ;
   - **interdits** : pendant une save (DFS et occupation T0 indexés par `idx`, §8.5) ; tant
     qu'un job long (KEYS, `UNLINK prefix*`, libération de FLUSHDB) ou un curseur SCAN tient
     des `idx` (v1 : attendre ; plus tard, les corriger) ; `root_idx` suit. La slab valart
     (idx d'enfants à elle) se compacte à part, même mécanisme.

### 17.2 bis Compactage de la slab des nœuds (fait, branche `pool-structures`)

- `hislab` 0.2.5 (**publié** sur crates.io le 2026-10-23, tag `v0.2.5`) : `len`,
  `last_occupied`, `move_down` (vers le plus petit trou, le tag TTL suit), `release_above`
  (`MADV_DONTNEED` des pages de 2 Mio libres en fin, borne du plus haut slot touché).
- `oxidart::compact` : `compact_want` (trous sous la fin > 3 Mio, 4 Mio au-delà de 256 Mio
  de slab ; cible = `arrondi(occupés + 2 Mio, page)`, aucune passe si la cible n'est pas
  sous la fin), `compact_step` (tranche bornée en unités), `relink` (parent ou `root_idx`,
  `parent_idx` des enfants, tampon réutilisé), fin : rendu au-dessus de `max(dernier
  occupé, occupés + libre gardé)` (les insertions pendant la passe sont prises en compte).
- Serveur : déclenché par `Timer::Gc` quand aucune passe de GC ne tourne, ni save, ni job ;
  tranches dans `run_jobs` et `idle_step` ; arrêtée sans rendu si une save ou un job
  apparaît ; `INFO` `compact_*` ; journal sous `RADIXOX_GC_LOG`.
- Tests : 200K clés dont TTL, 3/4 supprimées, compactage par tranches de 512 unités →
  `check` (parents, radix, tags), contenu, TTL ; fuzz différentiels : compactage complet
  forcé tous les 64 pas hors save (slab dense vérifiée, oracle d'accord).
- Mesure (scénario uuid) : 51 536 nœuds déplacés, 12 Mio rendus ; slab 35 → 21 Mo ; RSS
  après suppression de la moitié **74 Mo** au bout de 16 s (90 avant, 114 au départ).
  **~120 ns par unité** sans pipeline (tranche de 4 096 unités = 489 µs) → tranche
  ramenée à **1 024 unités**.
- **Pipeline de machines à états** (conception de l'utilisateur, 2026-10-23) :
  `oxidart::compact::PIPELINE` = 8 déplacements en vol, étapes `claim` (source la plus
  haute non prise, trou le plus bas réservé par `reserve_lowest`, préchargement des deux)
  → `Header` (préchargement du parent et de la page de la source) → `Links` (préchargement
  de la ligne `parent_idx` de chaque enfant et de la page du parent, où l'entrée de
  l'enfant peut se trouver ; sautée s'il n'y a rien à attendre) → `Commit` (copie
  `move_to`, parent, enfants, d'un coup, `idx` relus à ce moment-là). Une machine finie
  est remplacée au tour suivant ; le pipeline est vidé en fin de tranche. `hislab` :
  `prev_occupied`, `reserve_lowest` / `unreserve`, `move_to`, `slot_ptr`. Test dédié :
  chaînes profondes en fin de slab (nœud et parent en vol ensemble), tranches de 7 unités.
- Mesure (scénario uuid, plus longue tranche de 1 024 unités) : **41 µs** (~40 ns par
  unité) avec K = 8, 48 µs avec K = 1 (étapes préchargées, une seule machine) : ×3 par
  rapport à la version sans pipeline, surtout grâce au préchargement par étapes ; K = 8 ne
  gagne ici que 15 % (petite slab, haut de slab encore en cache) — à remesurer sur une
  grosse slab froide.
- **Remesure par le temps total** (`INFO compact_total_us`, 2026-10-23, L3 de 32 Mio,
  profil `performance`) — ns par nœud déplacé :

  | K | 1,1M clés (51 536 nœuds) | 5M clés (211 186 nœuds) |
  |---|---|---|
  | 1 | 127 | 135 |
  | 2 | 92 | 116 |
  | 4 | 88 | 109 |
  | **8** | 116 (bruit, passe de 5 ms) | **83 – 87** (deux runs) |
  | 16 | — | 127 – 135 (deux runs) |

  K = 8 retenu (choix de l'utilisateur confirmé). Lecture (hypothèse, à confirmer par
  `perf stat` : défauts L1, préchargements abandonnés) : un déplacement touche ~8 lignes,
  K = 8 en demande déjà ~64 pour une vingtaine de tampons de défauts du L1 ; ~87 ns pour ~8
  lignes ≈ 11 ns par ligne, la limite de parallélisme mémoire du cœur ; à K = 16 les
  préchargements débordent et se gênent. Plus longue tranche de 1 024 unités sur la grosse
  slab froide : 120–150 µs → tranche ramenée à **512 unités**.
- **`perf stat` pendant le compactage seul** (banc temporaire, 5M clés, compteurs activés
  par `--control` autour de la passe, 210 657 nœuds) — **l'hypothèse ci-dessus est
  fausse** :

  | K | ns/nœud | cycles | IPC | défauts L1 en vol (moy.) | remplissages DRAM |
  |---|---|---|---|---|---|
  | 1 | 157 | 179M | 1,44 | 2,4 | 0,6M |
  | 8 | 91 | 104M | 2,43 | 6,1 | 1,0M |
  | 16 | 126 | 143M | 1,75 | 5,5 | 1,4M |

  Les tampons de défauts ne sont pas saturés (6 en vol sur ~24). À K = 8 la passe est
  **limitée par les instructions** : ~1 200 instructions par déplacement (IPC 2,4 ≈ 500
  cycles ≈ 91 ns), la mémoire est presque recouverte. K = 16 : distance de préchargement
  trop longue (un tour ≈ 2 400 cycles), les lignes sont évincées du L1 avant usage
  (remplissages DRAM +40 %, IPC 1,75). **Suite** : réduire les instructions par
  déplacement — suspect principal `node.iter()`, qui décode toutes les entrées (feuilles
  comprises) pour trouver les enfants, deux fois (étape `Links` et validation) ; parcours
  par le bitmap des enfants seul ; à confirmer par `perf record`.

### 17.3 Principe : des pages communes à tous les pools (utilisateur, 2026-10-23)

Les blocs et les pages de 2 Mio sont un **système de pagination commun** : rendre 2 Mio au
système pour un pool puis redemander 2 Mio juste après pour un autre est le contresens à
éviter. Un « pool » est un curseur (ou une fenêtre) qui prend ses blocs dans le stock
commun d'un seul `Pool` ; seul ce stock rend de la mémoire, sur une réserve **globale**.
Déjà vrai pour `SharedByte` et `CompactPath` (même `Pool`) ; les pages des nœuds, les petits
blocs Hash/Set et les débordements valart seront des curseurs de plus (alignement 8), pas
un second allocateur. Deux nuances :

- **affinité des pages par mobilité** : un bloc non déplaçable (pool court des temporaires,
  buffers de connexion) épingle sa page pour le niveau B ; `take_block` doit regrouper les
  blocs non déplaçables dans leurs propres pages, tout en puisant dans le même stock et la
  même réserve ;
- **HiSlab** : plage `mmap` à elle (adresse = `idx × 128`), donc pas de pages communes. La
  partager demande une HiSlab par chunks de 2 Mio pris dans le `Pool` (table
  `idx → chunk`) ; à trancher avec le compactage. D'ici là, une réserve globale (pool +
  slab) éviterait au moins de rendre d'un côté ce que l'autre reprend.

