# Analyse — "Tout dans l'ART" et TTL par valeur (Hash/Set/ZSet fields)

> ⚠️ Journal : les renvois à `ARCHITECTURE.md` visent sa version d'avant le 2026-10-19
> (`git log -- ARCHITECTURE.md`), réécrite depuis. Ce qui reste actif ici : §11–12 (valart),
> §13 (design ZSet).
>
> Contre-analyse de `ARCHITECTURE.md` (produit par un IA sans accès au code) confronté
> à l'état réel du code sur la branche `custom_parser`, + étude de faisabilité du
> corollaire discuté : éliminer HashMap/BTreeMap/BTreeSet/VecDeque des conteneurs et
> tout stocker comme sous-arbres ART natifs, pour obtenir **TTL par champ de hash /
> par membre de set** avec la même mécanique que le TTL par clé top-level.
>
> Date : 2026-07-29. Basé sur lecture directe de `oxidart/src/{lib,value,hcommand,
> scommand,zset_inner,node_childs,compact_path}.rs` et `radixox-lib/src/shared_byte.rs`.

---

## 1. Verdict express

| Volet | Verdict |
|---|---|
| `ARCHITECTURE.md` §0-2 (modèle d'exécution, invariants async) | **Cohérent avec le code**, mais c'est une politique à faire respecter, pas quelque chose déjà vérifié par le compilateur partout (voir §2). |
| `ARCHITECTURE.md` §3 (persistance RDB/AOF, undo-log) | **Zéro implémentation existante.** Tout le chapitre est prospectif — aucun `bitset_dirty`, aucun AOF, aucun RDB dans le repo. C'est un plan, pas un état des lieux. À ne pas citer comme acquis. |
| `ARCHITECTURE.md` §6 (data-as-ART-node pour Hash/Set/ZSet/List) | **Diagnostic correct** (le `HashMap::Large` est bien un risque de resize O(n) synchrone), mais la solution proposée est **incomplète pour ZSet et inadaptée telle quelle pour List** (voir §4). |
| Corollaire "TTL par valeur via ART unifié" | **L'idée est bonne et le mécanisme existe déjà** (`exp_and_radix` est un champ par-nœud, pas par-arbre) — mais le **coût mémoire par champ** est le point qui décide si c'est rentable, et il est significatif (voir §3). Redis lui-même a évité cette voie pour cette raison exacte (voir §5). |

---

## 2. Ce que le document externe ne pouvait pas savoir (delta avec le code réel)

Le document a été écrit sans le code sous les yeux ; quelques inexactitudes factuelles à corriger avant de s'en servir comme référence :

- **Structure du workspace** : 3 membres réels — `oxidart`, `radixox`, `radixox-lib` (pas de `radixox-server` séparé, pas de `radixox-common` protobuf sur cette branche — ce découpage vient du `CLAUDE.md` racine, qui documente une version antérieure/différente du projet).
- **Taille de nœud** : le document dit "slot = 64 octets, PLEINS, zéro bit libre" comme contrainte dure. C'est vrai *aujourd'hui* :
  ```rust
  #[repr(C, align(64))]
  struct Node {
      compression: CompactPath,   // 8 (union ptr/inline)
      childs: Childs,            // 31 (packed: 6×u32 + 6×u8 + u8 len)
      tag: Tag,                  // 1
      val: ValUnion,             // 8
      exp_and_radix: ExpAndRadix,// 8
      overflow_idx: u32,         // 4
      parent_idx: u32,           // 4
  }
  ```
  Total logique 64B, aligné/paddé à 64 par `align(64)`. Donc oui, la contrainte "slot plein" est réelle et **tout champ ajouté à `Node` coûte du padding perdu ou une refonte de layout** — le document a raison d'insister là-dessus.
- **TTL est déjà par-nœud, pas par-arbre.** C'est le fait le plus important pour la suite de cette analyse : `exp_and_radix: ExpAndRadix` vit **dans chaque `Node`**, avec `parent_idx` pour permettre l'éviction active à rebours. Ce n'est **pas** une propriété du tree entier ni réservée aux clés top-level — **n'importe quel nœud du slab peut expirer indépendamment**, dès aujourd'hui. C'est la base factuelle qui rend le corollaire de l'utilisateur *techniquement* gratuit si un champ de hash devient un vrai `Node`.
- **`SharedByte` est déjà refcounté et immuable par construction** (`radixox-lib/src/shared_byte.rs`) : bloc heap `[len:u32 | rc:u16 | pad:2 | data]`, alloué via mimalloc, `clone()` = incrément de rc. Donc le point ouvert §6.9 de `ARCHITECTURE.md` ("les valeurs sont-elles mutées en place ou réallouées ?") est **déjà tranché en pratique** pour les valeurs bytes : `SharedByte` ne s'édite jamais en place (toute écriture recrée un `SharedByte`), donc le futur undo-log de persistance peut se contenter de pointeur + incrément de rc, pas de copie lourde. Ce point du doc externe est donc résolu par le code existant, pas une question ouverte.
- **`InnerHCommand`** correspond exactement à ce que décrit §6.1 : `Small(SmallVec<2,…>)` / `Large(HashMap<SharedByte,SharedByte>)`, seuil `THRESHOLD = 16`. Le diagnostic du doc (resize O(n) sur `Large`) est vérifié dans le code : `HashMap::insert` classique, pas de rehash incrémental.
- **`ZSetInner`** (`zset_inner.rs`) est un **double index** `BTreeSet<(OrderedFloat<f64>, SharedByte)>` (ordre par score) + `HashMap<SharedByte, OrderedFloat<f64>>` (lookup O(1) par membre). Le document ne mentionne jamais ce cas précis — c'est le conteneur le plus délicat à migrer (voir §4.3).
- **`ScommandBTreeSet`** (`scommand.rs`) : Set est actuellement un simple `BTreeSet<SharedByte>` (pas de `Small`/`Large` split du tout !) — donc contrairement à Hash, **Set n'a même pas encore l'optimisation "petit → inline"** que `ARCHITECTURE.md` prête à "tous les conteneurs à variante Large". C'est un écart direct entre le document et le code : Set n'a pas de variante `Small`, il utilise systématiquement un `BTreeSet` alloué même pour 1 membre.

---

## 3. Le corollaire de l'utilisateur : chiffrer le coût réel

L'idée : au lieu de `Tag::Hash → idx dans HASH_SLAB<InnerHCommand>`, faire pointer la valeur `Hash`/`Set`/`ZSet` vers un **sous-arbre du même ART** (racine = un `Node` du slab principal, enfants = un par champ/membre, adressés par les mêmes `Childs`/`HugeOverflow` déjà utilisés au niveau racine). Chaque champ devient alors un `Node` complet et hérite **gratuitement** de `exp_and_radix` → `HEXPIRE`-like sur un champ individuel, sans code neuf pour l'expiration elle-même (juste la commande RESP qui appelle `set_exp` sur le nœud-champ au lieu du nœud-clé).

### 3.1 Coût mémoire par entrée — comparaison chiffrée

| Représentation | Coût fixe / entrée | Détail |
|---|---|---|
| `InnerHCommand::Small` (actuel, petit hash) | ~16 B (2×`SharedByte` = 2×8B pointeur) + alloc heap champ (6B header + N) + alloc heap valeur (6B header + M) | Pas d'indirection de structure — juste 2 pointeurs dans un `SmallVec` inline |
| `InnerHCommand::Large` (actuel, gros hash) | ~16 B pointeurs + overhead `HashMap` (facteur de charge std ~1.1×, pas de métadonnée SwissTable ici car `std::collections::HashMap`) + mêmes allocs heap champ/valeur | Resize O(n) au grandissement — le vrai problème identifié |
| **Champ = `Node` ART complet (proposition)** | **64 B fixes** (le `Node` entier) + alloc heap champ si >7B (sinon inline dans `CompactPath`) + alloc heap valeur si Bytes non-inline | Le nœud lui-même remplace l'entrée `HashMap`, mais son overhead fixe (`Childs` 31B, `overflow_idx`, `parent_idx`, `exp_and_radix`) est payé **même pour un champ qui n'a jamais besoin de TTL ni d'enfants** |

**Verdict chiffré** : pour un hash à 1M de champs courts (~10-20B chacun, cas fréquent — compteurs, flags, petits attributs), passer de `HashMap` à "champ = Node" multiplie le coût de structure par **~3-4×** (64B fixes vs ~16-24B). Sur 1M champs, ça représente ~48 MB de plus *par hash*, juste en overhead de structure, avant même de compter les données. Ce n'est pas rédhibitoire (RadixOx tourne déjà avec des RSS de plusieurs GB sur 5M clés dans les benchmarks archivés), mais ce n'est **pas neutre** — à mettre en face du bénéfice avant de trancher.

### 3.2 Où le bénéfice devient net (au lieu de juste "cool")

Le TTL par champ n'est intéressant à ce coût **que** si le champ est individuellement volatil — sessions avec attributs à durée de vie différente, caches à granularité fine, rate-limiting par sous-clé. Pour un hash "objet métier" classique (champs stables, jamais de TTL individuel), le surcoût mémoire est payé pour un bénéfice jamais utilisé. → **Ne pas migrer tous les Hash par défaut** ; réserver le sous-arbre ART aux conteneurs qui déclarent explicitement vouloir du TTL de champ (cf. §6, migration progressive), garder `Small`/`Large` classique en fallback pour le cas générique.

---

## 4. Faisabilité par type de conteneur

### 4.1 Set — meilleur candidat

Un membre de Set n'a pas de "valeur" séparée de sa clé : le membre **est** la clé du champ. Migrer `BTreeSet<SharedByte>` vers un sous-arbre ART où chaque membre est un `Node` (souvent sans `val`, juste `tag=Tag::None` + présence = appartenance) est direct, et **Set n'a même pas de variante `Small` aujourd'hui** (voir §2) — donc c'est aussi l'occasion de corriger ça au passage, avec un seul mécanisme (`Small` inline + sous-arbre ART) au lieu de créer un troisième pattern.

`SISMEMBER` / `SADD` / `SREM` deviennent des traversées `traverse_to_key` sur le sous-arbre — code déjà écrit et testé pour l'arbre principal, réutilisé tel quel (c'est exactement l'argument §6.4 du document externe, et il tient pour ce cas précis).

### 4.2 Hash — candidat correct, avec la réserve mémoire du §3

Fonctionne bien tant que la clé de traversée du sous-arbre est le nom du champ (`field`) et la valeur du `Node` est soit inline (`Int`/petit `Bytes` dans `ValUnion`), soit un `SharedByte` pointé. `HGETALL` sur un gros hash devient une itération ordonnée du sous-arbre — **mais l'ordre devient lexicographique sur le nom du champ, pas l'ordre d'insertion**. Redis garantit l'ordre d'insertion pour les hash encodés en `listpack` (petits hash) ; RadixOx le fait déjà pour `Small` (SmallVec = ordre d'insertion). Si `Large`/sous-arbre change cet ordre pour les gros hash, c'est un **changement de sémantique observable côté client** (des tests qui itèrent `HGETALL` et comparent l'ordre casseraient). À documenter explicitement comme divergence assumée, pas à laisser comme effet de bord silencieux.

### 4.3 ZSet — la proposition telle quelle ne suffit pas

Le double index (`sorted` par score, `scores` par membre) existe *parce que* deux ordres incompatibles sont nécessaires : ordre par score (`ZRANGE`) et lookup O(1) par membre (`ZSCORE`, `ZINCRBY`, `ZREM`). Un seul sous-arbre ART clé-par-membre donne l'équivalent de `scores` (bon), mais **pas** l'équivalent de `sorted` : un ART est ordonné lexicographiquement sur les octets de la clé, donc pour obtenir l'ordre par score il faut une **deuxième** structure clé sur `(score encodé big-endian, membre)` — ARCHITECTURE.md §2.1 le dit très bien en théorie ("encoder les clés numériques en big-endian pour aligner ordre binaire et ordre sémantique") mais **ne l'applique pas** à ZSet dans son plan §6.3. Conséquence concrète : migrer ZSet vers "tout ART" ne supprime pas le double index, ça le **déplace** — deux sous-arbres au lieu d'un `BTreeSet`+`HashMap`, avec le même total de 2 entrées par membre, mais chaque entrée coûte maintenant 64B (Node) au lieu d'une entrée `BTreeSet`/`HashMap` classique. Le gain ici n'est **pas** la suppression du double index (impossible, c'est la nature du problème), c'est uniquement "plus de resize O(n) global" (le vrai risque du document) — le TTL par membre de ZSet reste possible mais coûte 2× l'overhead décrit en §3.1 (un nœud dans chaque sous-arbre, TTL à synchroniser entre les deux).

### 4.4 List — mauvais candidat pour "tout ART", contrairement à ce que suggère §6.3

Un ART est un **trie sur le contenu de la clé** ; une liste Redis est indexée par **position** (LPUSH/RPUSH/LINDEX/LRANGE), qui change à chaque insertion aux extrémités si on utilise des indices entiers naïfs. Pour que "position" devienne une clé ART stable (pas besoin de renuméroter tout le monde à chaque push), il faut un schéma d'**indexation fractionnaire** (clés du type "milieu de l'intervalle entre voisins", en big-endian, avec ré-équilibrage périodique quand l'espace entre deux clés s'épuise) — c'est le même problème que les CRDT de séquence (Fugue, RGA) ou les listes ordonnées de Figma/Notion. Ce n'est **pas gratuit** : ça demande un algorithme dédié, pas juste "réutiliser le mécanisme d'ART déjà là". `ARCHITECTURE.md` §6.3 traite List comme un cas symétrique de Set/Hash/ZSet ("idem pour Set/ZSet/List : Small | Node(SlabIndex)") — c'est la partie **la moins solide** du plan externe. Recommandation : **exclure List de cette migration** pour l'instant, garder `VecDeque` (le vrai risque de resize O(n) sur `VecDeque` existe aussi mais se traite indépendamment, par ex. `VecDeque` a déjà une croissance amortie en doublement, moins pathologique qu'un `HashMap` rehash sous charge de type "toutes les clés d'un coup").

---

## 5. Validation externe : Redis a déjà tranché ce dilemme

Redis 7.4 a ajouté `HEXPIRE`/`HPEXPIRE`/`HTTL`/`HPERSIST` (TTL par champ de hash) — la même fonctionnalité que ce corollaire vise. Ils **n'ont pas** transformé chaque champ en objet pleinement adressable/indépendant dans leur structure hash principale (`listpack`/`hashtable`) : ils ont ajouté une **structure d'expiration séparée et compacte** (`ebuckets`/`mstr` — un mini index dédié uniquement aux champs qui ont effectivement un TTL), justement pour **ne pas payer l'overhead sur les champs qui n'en ont pas besoin**. C'est un signal fort que l'approche "tout uniforme dans le même mécanisme lourd" a un coût mémoire jugé trop élevé même par l'équipe qui maîtrise le mieux ce compromis. Ça ne veut pas dire que le sous-arbre ART est une mauvaise idée pour RadixOx (l'architecture est différente, le `Node` à 64B est déjà plus lourd qu'un slot `listpack`), mais ça confirme la réserve du §3.2 : **réserver le coût du "champ = Node complet" aux champs qui en ont réellement besoin**, pas à la totalité d'un hash dès qu'il dépasse le seuil `Small`.

---

## 6. Risques transverses (au-delà du choix par-conteneur)

- **Éviction active partagée** : `evict_expired()` échantillonne des nœuds "tagged" (ceux qui ont un TTL) dans **un seul pool global**. Si des millions de champs de hash acquièrent chacun un TTL, le pool de nœuds tagués explose en population et en proportion par rapport aux clés top-level — l'échantillonnage reste correct statistiquement (proportionnel = comportement Redis-like), mais il faut vérifier que le **budget CPU par tick d'éviction** (actuellement dimensionné pour un keyspace top-level) ne devient pas insuffisant pour purger à temps un keyspace 100-1000× plus dense en entrées tagués. À benchmarker isolément, pas supposé.
- **Croissance du slab partagé** (risque n°1 du document, §6.7) : aujourd'hui HASH_SLAB/SET_SLAB/ZSET_SLAB/LIST_SLAB sont **séparés** (`HiSlab::new(0, 100_000_000)` chacun). Unifier "champ = Node" fait retomber toute cette pression sur **le même slab** que les clés top-level (`TaggedHiSlab::new(20000, 25000000)` dans `lib.rs`, capacité actuellement dimensionnée pour 25M — pas 100M×4). Un `HSET` massif (1M champs d'un coup) grossirait directement le slab principal au lieu d'un slab de conteneur isolé — même diagnostic que le document (§6.7), mais l'impact change d'échelle une fois qu'il n'y a plus de cloisonnement entre "clés" et "champs".
- **Persistance (undo-log)** : § 3 de `ARCHITECTURE.md` n'existe pas encore en code — donc l'argument "un seul type à sérialiser" (§6.5.3) est un bénéfice **futur conditionnel**, pas un gain immédiat. Ne pas le compter dans le calcul coût/bénéfice à court terme.
- **Compatibilité de tests existants** : `oxidart/src/test_structures.rs` (60 tests, mentionné dans `CLAUDE.md`) couvre déjà Hash/Set/ZSet avec préfixes communs et isolation inter-clés — toute migration doit repasser cette suite avant tout export, et l'ordre d'itération (§4.2) doit être vérifié explicitement puisque c'est le genre de régression qu'une suite de tests peut ne pas détecter si elle ne teste pas l'ordre.

---

## 7. Recommandation priorisée

1. **Ne pas faire un big-bang "tout ART"**. Traiter chaque conteneur séparément selon §4 :
   - **Set d'abord** (le plus simple, corrige aussi l'absence de variante `Small` actuelle).
   - **Hash ensuite**, avec un flag explicite "ce hash peut avoir des TTL de champ" plutôt qu'une promotion automatique au seuil `THRESHOLD` — pour ne pas payer 64B/champ sur des hash qui n'utiliseront jamais `HEXPIRE`.
   - **ZSet** seulement après avoir conçu explicitement le deuxième sous-arbre (score-ordonné) — ce n'est pas un sous-produit gratuit du premier.
   - **List exclue** de cette migration ; traiter son risque de resize séparément (VecDeque a un profil de croissance moins dangereux, à confirmer par mesure plutôt que supposé).
2. **Benchmark isolé avant tout code** (reprend le risque n°1 du document, mais élargi à l'échelle du slab partagé, cf. §6) : mesurer l'insertion de 1M champs dans un même hash avec le slab **partagé** (pas un slab dédié comme aujourd'hui) et vérifier qu'aucune extension de slab / demande de huge page ne stalle la boucle mono-thread.
3. **Chiffrer le vrai besoin produit** avant d'investir : est-ce que le cas d'usage visé (TTL par champ) est un besoin client réel identifié, ou une extrapolation de "on pourrait" ? Le doc externe présente ça comme un bénéfice structurel automatique — en pratique c'est un **budget mémoire à payer maintenant pour une fonctionnalité RESP qui n'existe pas encore côté serveur** (pas de `HEXPIRE` implémenté dans `resp.rs` actuellement). Vérifier l'ordre des priorités avec la persistance (§3, non implémentée) qui est probablement plus bloquante pour un usage en production que le TTL de champ.

---

## 8. Suite de la discussion (2026-07-29, même jour) — trois pivots

Après le §1-7, la discussion a bifurqué sur trois points que l'utilisateur veut acter comme
notes de travail. Ils sont indépendants du choix "tout dans l'ART" mais s'y articulent.

### 8.1 Recadrer la comparaison mémoire du §3 : le bon adversaire n'est pas "17B idéal", c'est Redis réel

Le §3 comparait "64B/champ (Node ART)" contre "~16-24B/champ (HashMap actuel)". Objection
correcte de l'utilisateur : **le 16-24B actuel est lui-même un chiffre triché/temporaire**
— il vient d'un `HashMap` std qui paye le vrai coût ailleurs : resize O(n) synchrone en pic
de latence, jamais amorti proprement. Ce coût caché n'apparaît pas dans un calcul statique
d'octets/entrée, mais il est bien réel (c'est tout le §6 de `ARCHITECTURE.md`).

Et Redis, qui a *l'air* de faire aussi bien avec son `dict` (hashtable), ne le fait pas
gratuitement non plus : `dictEntry` = 24B (next ptr + key ptr + val union) **+ overhead
`sds` par clé/valeur** (header 3-11B + arrondi à la classe de taille jemalloc, typiquement
16/32/64B) **+ table double pendant le rehash incrémental** (deux tables coexistent tout
le temps du rehash, soit jusqu'à 2× la table primaire momentanément). Le vrai coût "Redis
moyen amorti, pics inclus" est donc nettement au-dessus de 17B — probablement du même
ordre de grandeur que 64B une fois qu'on compte le pic de rehash, juste **caché dans une
moyenne** au lieu d'être visible comme un coût fixe.

**Conclusion révisée** : la comparaison honnête n'est pas "64B fixe" vs "17B en moyenne",
c'est "64B fixe, **jamais de pic**" vs "17B en moyenne **mais avec un pic périodique
équivalent à payer 2× la structure pendant le rehash**, plus la latence de queue que ça
implique". Sous cet angle le compromis ART est beaucoup plus défendable : on paye un peu
plus tout le temps pour ne jamais payer un pic — exactement la philosophie déjà actée dans
`ARCHITECTURE.md` §1.2 ("Cible de perf : p99 < 900µs... toute proposition qui risque un
spike est à rejeter"). **Le sous-arbre ART n'est donc pas seulement défendable pour le TTL
par champ — il l'est déjà pour la seule raison "zéro pic de rehash", indépendamment du TTL.**
Ça change la priorité : le TTL par champ devient un bonus, pas la justification principale.

### 8.2 Pivot : abandonner le modèle 2-thread (I/O + Data), revenir mono-thread pur

Décision en cours de l'utilisateur (à confirmer avant implémentation, mais le raisonnement
est solide) : **abandonner** [[project_2thread_pipeline]] (rings SPSC, `TaggedCmd`,
`BorrowedByte`, thread Data séparé).

**Pourquoi** :
- Trop complexe pour le gain visé — l'utilisateur anticipe ne jamais finir ce chantier
  correctement ("je le ferai jamais trop de taff trop complexe").
- Le coût le plus concret : **`OwnedByte`** (`radixox-lib/src/shared_byte.rs:197`) existe
  **uniquement** pour rendre `Cmd` `Send`-safe à travers la frontière I/O-thread → Data-thread
  (`unsafe impl Send for OwnedByte {}`, invariant `rc == 1`). Chaque commande parsée fait
  aujourd'hui un `OwnedByte::from_slice` par argument — **alloc + memcpy systématique**,
  y compris pour un `GET` d'une clé de 5 octets. En mono-thread pur, ce besoin disparaît :
  on peut revenir à des slices empruntées directement dans `read_buf` (le zero-copy que
  documentait déjà `CLAUDE.md` pour l'ancien `resp.rs` : `decode_bytes_mut()` → `Bytes`
  slices sans alloc), comme le faisait le serveur RESP avant le refactor parseur custom.
- C'est un **gain plus large et plus universel** que la question Hash/Set du §1-7 : il
  s'applique à **chaque commande, chaque argument**, pas seulement aux conteneurs. Probable
  candidat n°1 en rapport gain/effort par rapport au reste de cette note.

**Ce qui tombe avec l'abandon** : `ring_cmd`/`ring_res`/`ring_free` (3 SPSC rings),
`TaggedCmd`, `BorrowedByte` + son protocole de rc différé, le free-batching sort+compact,
le thread Data séparé (`art_thread.rs`). Le parseur (`radixox-lib/src/cmd.rs`, `Cmd` enum)
reste probablement une bonne structure de dispatch, mais ses variantes doivent redevenir
des slices empruntées (`&[u8]` / un type `BorrowedByte`-like *non-Send*, propre au
mono-thread) plutôt que `OwnedByte`.

**Action de suivi recommandée** : mettre à jour/marquer comme remis en question la mémoire
[[project_2thread_pipeline]] — c'est un changement de cap, pas un détail d'implémentation.

### 8.3 Argument de vente n°1 (plus fort que le TTL) : sauvegarde progressive unifiée

Corollaire du "tout dans l'ART" que l'utilisateur identifie comme l'argument le plus solide
face à Redis : si les valeurs de Hash/Set/ZSet vivent **dans le même ART** que les clés
top-level, le futur mécanisme de save (undo-log, §3 de `ARCHITECTURE.md`, non encore codé)
les traverse et les sérialise **par le même mécanisme, sans encodeur dédié par type**.

Redis, lui, doit maintenir **un encodeur RDB par encodage interne** : `listpack` (petit
hash/liste/zset), `hashtable` (gros hash), `quicklist` (liste), `skiplist` (gros zset),
`intset` (petit set d'entiers) — chacun avec sa propre logique de (dé)sérialisation et ses
propres cas limites de compatibilité inter-versions. Unifier le stockage dans un seul ART
fait disparaître cette multiplicité : **un seul traversal, un seul format, un seul chemin
undo-log** pour tout le keyspace, conteneurs compris. C'est un vrai avantage structurel,
indépendant du TTL par champ, et probablement **l'argument commercial le plus solide** de
toute cette migration — à mettre en avant plus que le TTL fin dans toute présentation du
projet. (Rappel : ce bénéfice reste conditionnel à l'implémentation réelle du save, qui
n'existe pas encore — voir §6 "Persistance" ci-dessus.)

### 8.4 Piste à étudier (pas tranchée) : nœud-feuille compact sans `Childs`

Idée soulevée : quand un `Node` n'a **aucun enfant** (feuille pure), les 31B de `Childs`
+ `overflow_idx` (4B) sont un pur gaspillage — ils pourraient à la place stocker la valeur
inline (plus de marge avant fallback heap) ou toute autre donnée utile à une feuille.

C'est exactement la distinction que fait l'ART original (papier Leis et al.) : un nœud
**Leaf** est un type structurel séparé des nœuds internes (Node4/16/48/256), sans tableau
d'enfants du tout. `oxidart` a délibérément **unifié** value-bearing et has-children dans
un seul `Node` de taille fixe (`align(64)`), précisément pour garder un `HiSlab` trivial
(slot uniforme, alloc/free O(1) sans classes de taille). Réintroduire une distinction
feuille/interne casserait cette simplicité : il faudrait soit deux tailles de slot (deux
`HiSlab` ou une union de tailles, ce que le "two-tier Childs + HugeOverflow" actuel
évitait déjà en remplaçant le Node4/16/48/256 classique), soit un flag + union interne
(`repr` avec deux formes possibles dans les mêmes 64B — probablement l'option la plus
compatible avec l'existant, à la `ValUnion`/`Tag` déjà en place).

**À ne pas coder maintenant** — noté comme piste pour une session dédiée, avec benchmark
avant/après sur un keyspace à forte proportion de feuilles pures (cas courant : beaucoup
de clés courtes sans enfants, typique d'un cache clé→valeur simple).

### 8.5 Ordre de priorité suggéré pour la suite (à valider, pas décidé)

1. **Pivot mono-thread** (§8.2) — gain universel, complexité en moins, débloque tout le reste
   sans dette technique du modèle 2-thread.
2. **Persistance undo-log sur le radix principal seul** (déjà dans le plan `ARCHITECTURE.md`
   §6.10 étape 1) — valide le mécanisme avant de le généraliser aux conteneurs.
3. **Migration Set → sous-arbre ART** (§4.1, le plus simple, corrige aussi l'absence de
   variante `Small`).
4. **Benchmark de croissance du slab partagé** (§6.7 / §6 transverse) avant toute
   généralisation Hash/ZSet.
5. Nœud-feuille compact (§8.4) et Hash/ZSet en sous-arbre : après, une fois 1-4 validés
   empiriquement — pas de big-bang.

---

## 9. Décision actée (2026-07-29, suite) — mono-thread définitif + parsing zero-alloc

Le §8.2 passe de "pivot envisagé" à **décision ferme, assumée durablement** ("à tout jamais") :
**mono-thread pur**, abandon définitif du modèle 2-thread I/O+Data ([[project_2thread_pipeline]]).
Corollaire direct : plus besoin de `Send` sur les types qui traversent le parsing → plus
besoin d'`OwnedByte` pour ça. La conséquence va plus loin qu'un simple "retour au zero-copy
de l'ancien resp.rs" — c'est un principe général à appliquer partout où c'est possible :
**emprunter par défaut, posséder seulement quand la donnée doit survivre à la commande**.

### 9.1 Règle de distinction owned/borrowed par rôle de la donnée, pas par type de commande

Ce n'est pas "toutes les commandes empruntent" ou "toutes possèdent" — ça dépend de **ce
que devient chaque champ** :

- **Une clé de commande** (`GET key`, `SET key val`, `HGET key field`, …) sert uniquement à
  **traverser l'ART** pour trouver/créer un nœud. Elle n'a jamais besoin de survivre après
  le retour de la commande (pas de `.await` entre parse et dispatch — invariant déjà acté
  dans `ARCHITECTURE.md` §1.2 : commandes sync = exécution d'un tenant). Donc : **`&[u8]`
  emprunté directement dans `read_buf`, zéro alloc.**
- **Une valeur qui doit être stockée** (le `val` d'un `SET`, d'un `HSET`, …) doit survivre
  bien au-delà de la commande courante (relue par de futurs `GET`) → elle **doit** devenir
  une allocation possédée et partagée : `SharedByte` (refcount, déjà l'implémentation
  existante dans `radixox-lib/src/shared_byte.rs`). Pas de gain possible ici, c'est
  la nature de la donnée qui l'exige, pas une limite du modèle de threads.

### 9.2 Exemple concret — avant / après sur `GET` et `SET`

| Commande | Aujourd'hui (modèle 2-thread) | Après le pivot |
|---|---|---|
| `GET key` | `Cmd::Get(OwnedByte)` → 1 alloc+memcpy pour une clé qui ne sert qu'à un lookup immédiat, jamais stockée | `Cmd::Get(&[u8])` emprunté dans `read_buf` → **0 alloc** |
| `SET key val` | `Cmd::Set { key: OwnedByte, val: OwnedByte }` → 2 allocs | `Cmd::Set { key: &[u8], val: SharedByte }` → **1 alloc** (uniquement la valeur stockée) |

C'est un changement de flux réel, pas cosmétique : `GET` (la commande la plus fréquente
sur la plupart des workloads — cf. `bench_memtier.sh`, workloads caching 1:10 GET:SET) passe
de "1 alloc systématique" à "0 alloc", ce qui touche directement les p99/p99.9 déjà
documentés dans `BENCH_ANALYSIS.md` (le rehash n'est pas le seul générateur de queue
latency — l'allocateur mimalloc en prend aussi sa part sous forte charge).

### 9.3 Deuxième temps (pas immédiat) : pousser `&[u8]` à l'intérieur de l'ART lui-même

Idée notée pour plus tard, pas pour maintenant : réduire l'usage de `SharedByte` *dans les
structures internes de l'ART* (`CompactPath`, chemins de compression, `Childs`) au profit de
`&[u8]` là où c'est possible, dans un objectif de **recyclage** (réutiliser des buffers/slots
plutôt que réallouer à chaque insertion). Contrainte à respecter avant de s'y attaquer :
l'ART **doit** posséder ce qu'il stocke durablement (une clé de nœud vit potentiellement
indéfiniment, bien après le retour de la commande qui l'a créée) — donc ce chantier ne peut
porter que sur la **réutilisation d'allocations déjà possédées** (pooling, recyclage de
slots libérés), pas sur un emprunt qui violerait la survie au-delà du call synchrone. À
creuser après le pivot mono-thread et la simplification du parsing (§9.1-9.2), pas en
parallèle — trop de surface de changement simultanée sinon.

---

## 10. `ValNode` 128B — inline Hash/Set + pivot vers un B-tree séparé (2026-07-30)

Discussion distincte de §1-9, sur le format concret d'un nœud qui stocke Hash/Set
**inline** (pas via sous-arbre ART par champ comme discuté en §3-4 — l'idée a évolué
vers "tout doit rester dans des slots HiSlab uniformes" pour préserver le mécanisme
COW déjà architecturé, cf. §9). Notes de travail, rien d'implémenté.

### 10.1 Point de départ — `ValNode` à 128B (double du `Node` actuel à 64B)

```rust
struct ValNode {
    compression: CompactPath,      // 8
    parent_idx: u32,               // 4
    len: u8,
    tag: Tag,
    exp_and_radix: ExpAndRadix,   // 8
    value: Option<SharedByte>,    // 8 (niche NonNull — vérifié, pas de coût caché)
    data: NodeData,
}
union NodeData {
    hash: ManuallyDrop<[(ExpAndRadix, SharedByte, SharedByte); 4]>,  // 96
    set:  ManuallyDrop<[(ExpAndRadix, SharedByte); 6]>,               // 96
    childs: ([u32; 18], [u8; 18]),                                    // ~92
}
```

En `repr(Rust)` (packing auto), ça tombe pile à 128B. **En `repr(C)`** (nécessaire vu
l'unsafe/allocation manuelle par slab), il faut revérifier avec `size_of` — le padding
d'alignement (8B pour `CompactPath`/`ExpAndRadix`/`Option<SharedByte>`) n'est plus
optimisé automatiquement, le total peut dépasser 128B sans réordonnancement manuel des
champs.

### 10.2 Problème identifié — `childs` XOR `hash`/`set` dans l'union

Un `ValNode` avec `tag = Hash` ne peut pas *aussi* avoir des `childs` (branchement ART).
C'est exactement le bug historique `split_node`/`ensure_key` (cf. section bug-fix plus
haut dans ce fichier / `CLAUDE.md` racine) : une clé Hash/Set peut être préfixe d'une
autre clé insérée après (`user:1` avant `user:10`), et le nœud doit alors porter les
deux simultanément. Pas résolu dans ce document — noté comme risque à couvrir avant
d'écrire du code, probablement par un niveau d'indirection supplémentaire plutôt que par
retrait de `childs` de l'union.

### 10.3 Débordement de la capacité inline (4 Hash / 6 Set) — chiffrage

Décision actée : **pas de fallback vers une structure séparée** (pas de retour à
`HashMap`/`BTreeSet`) — tout doit rester allouable en `HiSlab` pour garder le mécanisme
COW/save déjà conçu ailleurs. Plusieurs pistes explorées, dans l'ordre :

1. **Débordement vers `childs` (branchement ART classique)** — rejeté après chiffrage.
   Exemple simulé : `SADD myset user:0..user:9` (10 membres, capacité inline Set = 6).
   Les 6 premiers tiennent inline (~16B/membre en heap mimalloc pour le `SharedByte`
   du membre, soit ~96B total). Les 4 derniers (`user:6..9`) partagent le même premier
   byte `'u'` → collision sur le radix byte → il faut un nœud intermédiaire (radix `u`,
   compression `"ser:"`) + 4 nœuds feuilles → **5 nœuds × 128B = 640B**. Total ~736B
   pour 10 membres, avec un facteur ~8× entre régime inline et régime `childs`. Rejeté :
   trop coûteux dès qu'il n'y a pas de compression naturelle entre les membres (cas
   fréquent — champs/membres sans préfixe commun).

2. **Chaînage de nœuds de même forme** (`next: u32` vers un autre `ValNode` même `Tag`,
   même array de 6 entrées) — teste bien mieux en RAM (10 membres = 2 nœuds × 128B =
   256B, contre 640B en (1)), reste 100% `HiSlab`-uniforme. **Rejeté à son tour** : le
   lookup redevient linéaire (scan des blocs, O(n/6)) — pour un Set qui grossit
   légitimement (dizaines de milliers de membres), `SISMEMBER` deviendrait O(n).

3. **B-tree classique séparé du mécanisme ART** (direction retenue) — cf. §10.4.

### 10.4 Direction retenue : `BNode` — type distinct, propre `HiSlab<BNode>`

Constat clé : essayer de faire porter la sémantique B-tree (routage par clé, split par
promotion de médiane) par le même `union NodeData`/`Tag` que `ValNode` mélange deux
algorithmes incompatibles (radix byte-jump vs comparaison de clé triée). Direction
retenue : **type séparé**.

```
ValNode (Hash/Set débordé) --overflow_root_idx: u32--> HiSlab<BNode>
```

Même pattern déjà en place pour `overflow_arena: OverflowArena` (séparé du slab
principal `Node`) — donc pas un nouveau principe, une extension du même.

**Ce qui reste à trancher avant d'écrire du code** (rien décidé, juste identifié) :
- Capacité réelle d'un `BNode` de 128B : entrées `(clé, val, exp)` + séparateurs +
  pointeurs enfants — à chiffrer précisément, pas encore fait.
- Split = promotion de médiane vers le parent (algo B-tree standard) — le parent (le
  `ValNode` racine du Hash/Set) doit pouvoir porter des séparateurs au premier split,
  pas juste un `overflow_root_idx` brut.
- Merge au delete (vrai B-tree, rétrécit) vs jamais de merge (cohérent avec le style
  append-only déjà en place ailleurs, plus simple, mais grossit sans jamais reculer) —
  **pas tranché**.

### 10.5 Suite

Rien d'implémenté à ce stade — discussion mise en pause pour reprise ultérieure.
Prochaine étape suggérée : chiffrer la capacité d'un `BNode` 128B avant de trancher
split/merge.

---

## 11. `ValArt` — pivot vers un radix hybride flat+branchement, code indépendant (2026-08-02)

Suite de la discussion §10 (le `BNode`/B-tree séparé n'a pas été retenu). Nouvelle
direction : `ValArt`, une structure **propre, pas de réutilisation du code
`Childs`/`Overflow`/`HugeOverflow` existant** ("on se met des contraintes de con" —
le mécanisme actuel est une table de dispatch radix-byte plafonnée à 127 slots/nœud,
pas une liste plate générique, et ses invariants ne conviennent pas tels quels).

### 11.1 Pourquoi pas de sous-arbre ART pur par champ (retour sur §3-4)

Confirmé : les champs d'un même Hash/membres d'un même Set n'ont généralement **aucun
préfixe commun** entre eux (contrairement au keyspace top-level où `user:1`/`user:2`
partagent une structure réelle). Un dispatch radix-byte classique dégénère sur ce cas
(cf. §10.3.1, `SADD user:0..9` → 640B pour 10 membres, ~8× l'overhead d'un tableau
inline). `ValArt` doit donc privilégier le stockage **flat** pour le cas courant
(petit hash/set, champs sans préfixe partagé) et ne router vers du branchement que
pour le cas extrême (beaucoup d'entrées collisionnant sur le même premier octet).

### 11.2 Layout retenu — nœud 128B, deux zones toujours présentes (pas d'union)

```
ValNode (128B) :
  ├─ radix[N]   — N bytes, un octet de dispatch par slot de branchement
  ├─ childs[N]  — N × u32, idx vers un autre ValNode (même forme, récursif)
  └─ data       — 128 - (N + 4N) bytes restants : records flat (key, val, exp)
```

Point clé par rapport à la version §10.1 (`union NodeData { hash | set | childs }`) :
ici `childs` et `data` sont **deux zones séparées et simultanément présentes** dans
le même struct, pas un union. Ça résout le blocage identifié en §10.2 (`childs XOR
hash/set` — un nœud ne pouvait pas porter une valeur ET brancher en même temps,
exactement le bug-class `split_node`/`ensure_key` historique). Avec deux zones
distinctes, un `ValNode` peut toujours stocker des entrées flat **et** avoir des
enfants simultanément, sans conflit structurel.

**N (nombre de slots radix/childs) : à déterminer par la contrainte mémoire réelle**,
pas figé ici. Le calcul est un arbitrage direct contre la zone `data` : chaque slot
coûte 5B (1 radix + 4 idx) pris sur la capacité flat. Éléments déjà discutés pour
trancher N plus tard :
- Alphabet du domaine (décimal, hexa, alphabétique libre) compte **moins qu'il n'y
  paraît** — la collision de premier octet se résorbe par récursion (un enfant
  gère à son tour ses propres sous-entrées via sa propre zone flat+branchement),
  pas par largeur inline. Donc pas la peine de sur-dimensionner N pour couvrir
  exactement un alphabet (ex: N=16 pour hexa coûterait 80B sur 128, bien trop cher
  pour un bénéfice qui ne joue que sur les gros containers denses en premier-octet).
- Benchmark réel à faire une fois le reste posé, pas de choix a priori.

### 11.3 Flag TTL — bit volé dans le byte de radix, zéro coût structurel

> ⚠️ **Caduc depuis le 2026-10-12** : les clés sont désormais **binaires** (tous les octets
> 0x00–0xFF, palier dense de 256 enfants, `assert!(radix != 0)` et `is_ascii` retirés,
> tests `oxidart/src/test_binary.rs`). Le bit 7 d'un octet de clé n'est plus libre : ce
> flag ne peut pas être volé dans le radix. Un flag TTL doit prendre un vrai bit ailleurs
> (octet de tag du record, ou bit de longueur). Le §11.4 (« plafonné par l'espace ASCII
> (127) ») est caduc pour la même raison. Texte d'origine conservé ci-dessous.

Les clés/champs sont garanties ASCII par construction dans RadixOx (contrainte déjà
implicite dans le code existant — `HUGE_OVERFLOW_CAPACITY = ASCII_MAX_CHAR(127) -
LIGHT_OVERFLOW_SIZE` n'a de sens que si le radix byte ne dépasse jamais 127). Le bit
`1<<7` d'un byte de clé ASCII est donc toujours à 0 → utilisable comme flag "ce champ
a un TTL" sans byte de tag dédié. Layout des records dans la zone `data`, sélectionné
par ce bit :
- Sans TTL : `key, val` (~9B)
- Avec TTL : `key, val, exp` (~17B, +8B timestamp)

Contrainte : ce flag ne s'applique qu'aux **clés** (garanties ASCII). Les **valeurs**
restent libres/binary-safe, aucune contrainte dessus (confirmé : `b'\0'` etc. sont
valides côté valeur, seul le radix byte dérivé de la clé est concerné).

Écarté : bit-packing complet (flag + timestamp à la granularité du bit, désaligné).
Argument perf initial (accès désaligné coûteux) concédé comme faux — la lecture/
écriture n'a lieu qu'une fois la valeur trouvée post-traversal, pas dans une boucle
chaude, donc le coût est négligeable. Argument retenu à la place : le gain de densité
du bit-packing complet vs un flag byte-aligné (comme ci-dessus) est marginal (<1
byte/entrée), pour un risque de bug d'offset silencieux (corruption non-crashante,
dur à fuzzer) qui ne vaut pas le coup sur un code appelé à vivre longtemps.

### 11.4 Overflow — deux structures séparées, formes différentes

Au-delà de la capacité d'un `ValNode` (N slots de branchement pleins, ou zone `data`
pleine), deux mécanismes distincts plutôt qu'un seul générique :
- **Overflow de branchement** : même besoin que `radix[N]`/`childs[N]` (couple
  radix+idx), plafonné naturellement par l'espace ASCII (127) — mais code propre à
  `ValArt`, pas de réutilisation d'`Overflow`/`HugeOverflow`.
- **Overflow de valeurs** : pas de radix du tout, juste de la place brute pour des
  records `key,val,exp` supplémentaires quand `data` est pleine mais qu'il n'y a pas
  besoin de brancher. Pas de raison de le plafonner à 127 — un arena qui grossit
  (façon `OverflowArena::slots: Vec<..>` déjà existant côté tree principal, mais en
  version indépendante) convient.

### 11.5 Non résolu — ZSet

L'ordre par score n'est pas donné par ce layout (lookup par clé seulement, comme
identifié en §4.3). Reste à décider : deuxième `ValArt` gardé trié par score (même
principe double-index que `ZSetInner` actuel), ou autre approche — pas tranché.

### 11.6 Suite

Vision actée, rien d'implémenté. Prochaines étapes suggérées : chiffrer N par
benchmark mémoire réel (pas de choix a priori), régler la stratégie de croissance
"au-delà de N childs" (chaînage vers un autre `ValNode` même forme, probable choix
par défaut pour rester simple), et concevoir l'ordre par score pour ZSet (§11.5).

### 11.7 Granularité du COW pendant une save — avantage direct de l'adressage par slab index

Point noté (2026-08-02) : parce que l'adressage entre `ValNode`s se fait par index de
slab stable (pas par pointeur/structure fonctionnelle persistante à la Clojure), un
`SET`/`HSET` qui ajoute un champ neuf pendant une save en cours peut avoir un coût
COW **beaucoup plus fin que "toute la clé/tout le container"** — contrairement à un
modèle où la valeur serait un blob monolithique (`HashMap`/`BTreeMap` actuel) qu'il
faudrait dupliquer en entier au moindre write pendant un snapshot.

**Cas courant (pas d'overflow)** : le champ tient dans la zone `data` déjà allouée
du `ValNode` terminal de la chaîne key→val. Seul ce slot est modifié — son image
"avant" capturée pour le save suffit, aucun autre nœud de la chaîne n'a besoin
d'être touché puisque les index qui y mènent ne changent pas.

**Cas overflow (nuance)** : si la zone `data` du nœud terminal est pleine et que
l'insertion doit créer un nouveau `ValNode` accroché via `radix[N]`/`childs[N]`,
alors le nœud **parent** est modifié aussi (son tableau `childs` gagne une entrée)
— son image "avant" doit être capturée en plus. Donc la granularité réelle est :
*nœud terminal seul dans le cas courant, nœud terminal + parent immédiat dans le
cas d'overflow* — jamais toute la chaîne, mais pas strictement "1 seul nœud" dans
tous les cas.

Ce point rejoint l'argument §8.3 (save unifiée via un seul mécanisme de traversal)
mais le précise à l'échelle du coût par écriture concurrente à une save, pas juste
à l'échelle du format de sérialisation.

---

## 12. Scope de la branche `custom_parser` — Set/Hash d'abord, ZSet skip (2026-08-03)

Décision actée : cette branche implémente `ValArt` pour **Set et Hash uniquement**.
ZSet est explicitement mis de côté (voir §12.4) — trop de mécanismes non triviaux
en même temps pour un seul chantier.

### 12.1 Mécanique de split flat+branchement — exemple concret

Contrairement à oxidart, les records de la zone `data` d'un `ValNode` stockent la
**clé complète en `SharedByte`** (pointeur heap), pas une compression de chemin
inline. Donc déplacer un record d'un nœud à un autre coûte **toujours la même
chose** (copie d'un tuple fixe de pointeurs, ~17-24B), peu importe la longueur de
la clé — la notion de "suffixe plus court = moins cher à copier" (héritée du
raisonnement oxidart) **ne s'applique pas** ici. Le critère de choix "qui reste,
qui bouge" lors d'un split est donc arbitraire/implémentation (ex: convention
"le nouvel entrant migre"), pas un calcul d'octets.

Déroulé de référence (`user:12`, `user:137`, puis `user:1` insérés dans cet ordre) :

1. `SADD`/`HSET` sur `user:12` → record flat dans la zone `data` du nœud `user:`
   (suffixe stocké tel quel dans le `SharedByte`, pas de compression).
2. Insertion de `user:137` → collision détectée (`12` et `137` partagent le
   préfixe `"1"` au-delà de `user:`). Nouveau `ValNode` alloué pour `user:1`,
   **seul `user:137` y migre** ; `user:12` reste en place dans `user:` (choix de
   convention, pas d'optimisation de coût réel — voir ci-dessus). État :
   `user:{12}`, `childs['1']→user:1:{137}`.
3. Insertion de `user:1` comme vraie clé (valeur propre) → `user:1` ne peut plus
   être à la fois valeur et rester un simple flat record ailleurs : `user:12`
   doit migrer à son tour dans `user:1:`. État final : `user:{childs['1']→user:1}`,
   `user:1:{data: [137, 12], val: <valeur de user:1>}`.

Ce dernier point ne casse rien grâce au layout §11.2 : `data` (flat) et
`childs[N]` (branchement) sont des zones **séparées et simultanées**, donc
`user:1` peut porter sa propre valeur ET brancher vers ses enfants en même temps
(c'est précisément le problème qu'un design en union — §10.2 — ne permettait pas).

### 12.2 Suppression — swap-remove dans la zone `data`, pas de décalage

La zone `data` est un tableau flat **non ordonné** (aucune sémantique d'ordre à
préserver) → suppression par swap-remove classique : le dernier record valide du
tableau est recopié à la place du record supprimé, O(1), coût fixe (même
argument qu'en §12.1 : copier un tuple de pointeurs ne dépend pas de la longueur
des clés qu'ils référencent).

Distinct de la **recompression inter-nœuds** (si un nœud enfant devient trivial
après un delete, faut-il le réabsorber dans le parent ?) — même principe que
l'auto-recompression déjà en place côté oxidart (cf. `CLAUDE.md` racine), à
reconcevoir séparément pour `ValArt`, pas encore fait.

### 12.3 Encodage f64 → u64 ordonné (utile le jour où ZSet sera repris)

Pour trier des scores IEEE754 comme des entiers non signés (nécessaire pour tout
schéma big-endian de type clé `score ++ member`) :

```rust
fn f64_to_ordered_u64(f: f64) -> u64 {
    let bits = f.to_bits();
    let mask = if (bits as i64) < 0 { u64::MAX } else { 1u64 << 63 };
    bits ^ mask
}
```

Flipper *seulement* le bit de signe ne suffit pas : ça ordonne correctement les
positifs entre eux, mais pas les négatifs entre eux (une magnitude de bits plus
grande = valeur *plus petite* côté négatif — il faut inverser tous les bits, pas
juste le signe, pour ce cas). `NaN` n'a pas besoin d'être géré : le parsing RESP
du score doit rejeter `"nan"` à l'entrée (comme Redis), donc aucun NaN ne peut
apparaître via ZADD ; ZINCRBY ne fait qu'additionner des floats déjà validés,
donc ne peut pas en produire spontanément non plus.

### 12.4 ZSet — pourquoi c'est mis de côté sur cette branche

Le score-tree d'un ZSet a un profil **inverse** de celui de Hash/Set : des scores
big-endian numériquement proches partagent un préfixe d'octets réel (c'est le but
de l'encodage), contrairement aux champs/membres Hash/Set qui n'ont généralement
aucun préfixe commun (§11.1). `ValArt` a été conçu pour le second cas ; le
score-tree relève plutôt d'un radix-trie classique façon oxidart, y compris pour
gérer un nombre de ties (membres à score identique) non borné — un sous-arbre
oxidart absorbe ça nativement via `HugeOverflow`/`OverflowArena` (branchement à
cardinalité arbitraire sous un préfixe compressé), sans mécanisme dédié à
inventer.

**Direction pressentie pour une session future** (pas commencée) :
- **score-tree** : sous-arbre/instance oxidart classique, clé = `big_endian(score)
  ++ member` (§12.3 pour l'encodage).
- **member-tree (lookup)** : `ValArt` (même mécanisme que Hash/Set).
- **Cross-référence bidirectionnelle** entre les deux (idx du nœud member-tree
  stocké côté score-tree et vice-versa) pour éviter la double traversée sur
  update (ZADD sur membre existant, ZINCRBY) — gain réel vs. l'implémentation
  Redis actuelle (skiplist sans back-pointer vers le dict). Coût : chaque
  split/swap-remove dans un des deux arbres doit propager la mise à jour du
  pointeur croisé dans l'autre, sinon corruption logique silencieuse (même
  classe de bug que `split_node`/`push_child_idx` historique) — nécessite une
  suite de tests dédiée à cette cohérence avant d'être considéré fiable.
- **Cache min/max** (idx direct vers l'extrémité du score-tree) pour ZPOPMIN/MAX
  en O(1).
- **Overlap de traversée A/B via prefetch** (pattern AMAC — Asynchronous Memory
  Access Chaining) pour les opérations où les deux traversées sont indépendantes
  (ZSCORE, insert neuf) ; ne s'applique pas au cas dépendant (update d'un membre
  existant, qui a besoin de l'ancien score avant de pouvoir naviguer le
  score-tree) — c'est justement ce cas dépendant que la cross-référence résout.
- Pari optimiste "insert neuf par défaut, corriger si membre existant déjà" —
  rentable seulement si le workload réel est dominé par des inserts plutôt que
  des re-scores de membres existants ; à trancher selon l'usage visé, pas un
  défaut universel.

## 13. ZSet — architecture retenue (2026-10-26, pas implémenté)

Remplace le design du 2026-10-07 (texte précédent : `git show 18218a1:ANALYSE_ART_UNIFIE.md`,
§13). Décidée avec l'utilisateur le 2026-10-26, après l'allocateur sans jemalloc
(MEMORY_ARCH.md §18). Repose sur les objets à poignée (MEMORY_ARCH.md §19).

### 13.0 Pourquoi refaire

Le ZSet actuel (`InnerZCommand` : `SmallVec` trié ≤ 16, puis `BTreeSet<(score, membre)>` +
`HashMap`) :
- **membres épinglés** : partagés par les deux structures (`rc = 2`), donc convertis en
  externe (§16.9 de MEMORY_ARCH) ; le GC ne peut pas les déplacer ;
- **hors de l'allocateur** : `BTreeSet` et `HashMap` dans glibc (~59 o/membre, invisibles
  dans `INFO`) ;
- **la save bloque** : hors barrière, écrit d'un bloc au `begin` (340 ms par million de
  membres) ;
- **ZRANGE par rang en O(n)** (`iter().skip(start)`), comme ZRANK, ZCOUNT, ZREMRANGEBYRANK.

Mesuré (2026-10-26, membres de 15 o, scores entiers, RSS par membre, clé comprise) :

| | grands (1000 × 1000) | petits (100 000 × 10) |
|---|---|---|
| Valkey 9.1.2 (skiplist + table ; listpack) | 93 o | 27 o |
| RadixOx actuel | 99 o | 62 o |
| **objectif** | **≤ 70 o** | **≤ 27 o** |

### 13.1 Quelle structure sert quelle commande

Comme Redis (`struct zset { dict *dict; zskiplist *zsl; }`), l'index membre → score n'a
**pas besoin d'ordre** : tout ce qui est ordonné passe par l'arbre.

| commandes | structure |
|---|---|
| ZSCORE, ZMSCORE, existence dans ZADD, ancien score dans ZREM / ZINCRBY, ZINTER / ZUNION / ZDIFF (appartenance) | index (valart, haché) |
| ZRANGE (rang, BYSCORE, BYLEX), ZRANK, ZCOUNT, ZLEXCOUNT, ZPOPMIN/MAX, ZREMRANGEBY* | B+tree compté |
| ZRANDMEMBER | B+tree : rang tiré au hasard puis sélection en O(log n) (uniforme) |
| ZSCAN | index (ordre valart stable sous mutation, même garantie qu'un curseur de table) |
| ZCARD | compteur de l'en-tête |

### 13.2 Deux formes

- **Petite** (§13.3) : au plus **128 membres** et chaque membre d'au plus **64 o** (seuils
  de Redis, `zset-max-listpack-entries` / `-value`). Un seul bloc, membres en ligne.
- **Grande** (§13.4) : au-delà. Index + B+tree compté + poignées.
- Passage petite → grande quand un seuil est dépassé ; **pas de retour** (Redis non plus).
- Seuils à re-mesurer (§13.9) : coût d'un ZSCORE / ZADD dans la petite forme à 32, 64 et
  128 membres contre la descente de la grande.

### 13.3 Petite forme : bloc en colonnes

```text
[n u8][largeur u8][cap u8][pad][scores : largeur × cap][longueurs : u8 × cap][octets des membres]
```

- **Scores à largeur fixe par bloc**, triés croissants : entiers signés sur 2 ou 4 o si
  tous les scores du bloc sont entiers et y tiennent, sinon 8 o (`f64` en `u64` ordonné,
  §13.4). La largeur ne fait que grandir (réécriture du bloc, une fois) ; jamais de
  rétrécissement en v1. Permet la **recherche dichotomique par score** (ZRANGEBYSCORE,
  position d'insertion), que le listpack (longueurs variables) ne permet pas.
- **Longueurs dans leur colonne** : ZSCORE / ZADD balaient les longueurs (SIMD), puis ne
  comparent les octets que des candidats de la bonne longueur. Décalage d'un membre = somme
  des longueurs avant lui.
- Ordre : `(score, membre)` ; à score égal, ordre lexicographique des octets.
- Insertion / suppression : `memmove` des trois colonnes (≤ ~3 Ko à 128 × 15 o).
- **Un seul propriétaire** (la valeur du nœud oxidart, `Tag::ZSetSmall`, `ValUnion::ptr`) :
  même chemin que `HashSmall` / `SetSmall` (`oxidart/src/small.rs`) — le GC déplace le bloc
  d'un coup (`gc::visit_val`, `v.page`), l'image de save en garde une copie profonde, la
  modification passe par `value_mut` (barrière). Aucune poignée, aucun `SharedByte`
  intérieur. Bloc alloué dans le pool (objet de structure aligné sur 8).
- Estimation : 10 membres de 15 o, scores sur 4 o : (4 + 1 + 15) = 20 o/membre + en-tête.

### 13.4 Grande forme

```text
index (valart)        entrée = {ctrl, poignée u32, score u64}      ← ZSCORE lit le score ici
B+tree compté (HiSlab) feuille = scores [u64], poignées [u32]       ← ordre, rangs, plages
table des poignées     poignée → adresse (8 o)                      ← GC, lecture des octets
membre (pool)          {longueur, poignée, octets}
```

**Score.** `f64` → `u64` ordonné (bijection exacte sur les bits, ordre de `total_cmp`) ;
`-0.0` normalisé en `0.0` avant (Valkey : même score, départagés par le nom) ; NaN refusé
avant la structure (`ERR value is not a valid float`). Le score vit à deux endroits,
l'entrée d'index et l'entrée de feuille ; une mise à jour touche les deux, qui sont lues de
toute façon pour la faire. La table ne contient que des adresses : ni ZSCORE ni une mise à
jour ne l'écrivent. Écarté : `vals[]` / `scores[]` séparés dans la table (ZSCORE vérifie le
membre, donc lit l'adresse **et** le score : deux défauts au lieu d'un ; et le GC ne balaie
pas la table, §19 de MEMORY_ARCH, l'argument « table moins polluée » ne tient plus).

**Membres.** Objets à poignée (MEMORY_ARCH.md §19) : une seule copie des octets, deux
structures qui ne gardent que la poignée (4 o au lieu d'un pointeur de 8), déplacement par
le GC = une case de la table réécrite. Un membre de plus de 16 Kio va dans l'allocateur des
grands objets (§18), que le GC ne déplace pas.

**Index membre → score.** Variante du `HashNode` de valart, entrée `{poignée u32, score
u64}` + octet de contrôle (empreinte du hash). Recherche : hash du membre → candidats dont
l'empreinte correspond → case de la table → octets comparés. Pas d'ordre.

**B+tree compté.** Nœuds dans une `HiSlab` à lui (idx u32), pour la barrière de save.
- Feuille (~512 o) : `n`, `prev` / `next` (feuilles chaînées), `scores [u64; L]`,
  `poignées [u32; L]`, en colonnes (comparaisons SIMD sur les scores) ; L ≈ 41.
- Interne (~512 o) : séparateurs `(score u64, poignée u32)` (clé complète, pour les
  ex-aequo), `enfants [u32]`, `comptes [u32]` (nombre de membres de chaque sous-arbre) ;
  ~25 enfants.
- En-tête du ZSet (objet de structure du pool, seul propriétaire : la valeur oxidart,
  `Tag::ZSet`) : racine, première et dernière feuille, hauteur, nombre de membres, nœud
  racine de l'index.
- Clé `(score, membre)` : `u64` d'abord, octets du membre (via la poignée) seulement à
  score égal.
- Hauteur ~5 pour 1M membres.

**Algorithmes (v1).**
- **Insertion** d'un membre nouveau (absent de l'index, donc réussite certaine) : descente
  depuis la racine avec `comptes[enfant] += 1` à chaque niveau (nœuds déjà en cache),
  insertion dans la feuille. Feuille pleine : split ; le séparateur et le compte du
  nouveau frère (= sa longueur) remontent par la **pile de la descente** (pas de pointeur
  parent). **Split 90/10 quand on insère tout à droite de l'arbre** (scores croissants :
  files à délai, horodatages, `_indices` de YCSB), 50/50 sinon : sans ça, chaque feuille
  reste à moitié vide sur une insertion séquentielle.
- **Suppression** (présence connue par l'index) : descente avec `−1`. Pas de
  rééquilibrage classique (emprunt / fusion) : une feuille vide est rendue, deux voisines
  sous ¼ chacune sont fusionnées ; racine à un seul enfant : hauteur −1. Un arbre parfois
  sous-rempli reste correct.
- **Mise à jour de score** (ZADD d'un membre existant, ZINCRBY) : sur place si le nouveau
  score reste entre ses voisins de feuille (aucun compte ne change) ; sinon suppression +
  insertion.
- **Rang** : en descendant, somme des comptes à gauche du chemin. **Sélection par rang** :
  on descend en soustrayant. ZCOUNT = rang(max) − rang(min).
- **Plages** (ZRANGE & co) : une descente (rang ou score), puis les feuilles chaînées.

Estimation (membre de 15 o) : membre 22 o (15 + en-tête 6, arrondi) + case 8 + entrée
d'index ~19 (13 o ÷ remplissage valart, à mesurer) + entrée de feuille ~17 (12 o ÷ 69 %)
+ internes ~1 = **~67 o/membre** (~81 au pire, remplissage 50 %).

### 13.5 GC

- Petite forme : le bloc, d'un coup (§13.3).
- Grande forme : les membres s'évacuent **en parcourant le bloc** (en-tête → poignée → case
  réécrite, MEMORY_ARCH.md §19), sans parcourir ni le B+tree ni l'index : une passe ne
  dépend pas de la taille des ZSet. En-tête : objet de structure, déplacé par la visite de
  la valeur oxidart. Nœuds du B+tree (HiSlab) et de l'index (slab valart) : compactage de
  slab (MEMORY_ARCH.md §17.6), plus tard.

### 13.6 AMAC

- **v1 : préchargement seul**, sur le modèle de `amac_fields` (phase 1 bis) : rien n'est
  gardé, l'exécution refait ses descentes, cache chaud. Correct par construction ; **pas de
  pierre tombale** au niveau du ZSet (une suppression, un split, un passage petite →
  grande plus tôt dans la fenêtre sont vus par la redescente). La clé du ZSet elle-même
  reste couverte par oxidart (`DEL z` puis `ZADD z` dans la même fenêtre, TTL).
- Chaînes de la phase de préchargement :

| commande | chaîne |
|---|---|
| ZSCORE, ZMSCORE | clé (oxidart) → index → candidat → case → octets |
| ZADD membre nouveau | clé → index (absence) **et** descente du B+tree vers `(score, membre)`, en parallèle dans la même voie |
| ZADD mise à jour, ZINCRBY, ZREM, ZRANK | clé → index → ancien score → descente du B+tree vers l'ancienne entrée (enchaînées) |
| petite forme | clé → lignes du bloc |

- **Plages** : pas d'AMAC entre commandes mais un pipeline dans la commande : pendant
  l'émission de la feuille `i`, préchargement de la feuille `i+1`, puis des cases de ses
  poignées, puis des octets.
- **v2** (après mesure) : garder les indices des lectures ; pour les écritures, règle « ZSet
  modifié plus tôt dans la fenêtre → redescente » (le ZSet vient d'être écrit, il est en
  cache). Toujours pas de pierre tombale (une entrée morte dans une feuille casserait les
  rangs).

### 13.7 Save

- Nœuds du B+tree derrière la barrière (images, comme valart) ; index : valart, déjà
  derrière ; en-tête et petite forme : modifiés via `value_mut` (image = copie profonde).
- Membres : lus par leur poignée au moment de l'écriture ; un membre libéré pendant une
  save attend la fin de la save (liste différée de poignées, comme la liste différée de
  valart, SAVE_DESIGN.md §2.B), pour que les images gardent des poignées valides.
- Format : par ZSet, cardinalité T0 puis `(score, membre)` dans l'ordre de l'arbre ; le
  rechargement construit les feuilles pleines de gauche à droite (pas d'insertion une à
  une).

### 13.8 Ordre de travail

Chaque couche seule, avec ses tests, avant la suivante :
1. **Objets à poignée** (MEMORY_ARCH.md §19) : table, curseur dédié du pool, évacuation par
   parcours de bloc ; tests contre un modèle (allouer, libérer, évacuer, chaque case pointe
   vers son objet, chaque en-tête vers sa case).
2. **B+tree compté**, générique sur la clé, seul : vérificateur d'invariants (comptes,
   ordre, remplissage, chaînage, hauteur) ; tests contre un `BTreeSet` + rangs calculés.
3. **Petite forme** seule : tests contre un modèle trié.
4. **ZSet assemblé** (petite + grande, passage) : test différentiel contre le ZSet actuel,
   gardé comme oracle (comme `oxidart-legacy` pour l'arbre).
5. Branchement des commandes ; ZRANGE par rang en O(log n) vérifié.
6. AMAC (préchargement seul).
7. Save (barrière, liste différée, format, rechargement).
8. Mesures : mémoire contre Valkey (objectifs du §13.0), ZADD / ZSCORE / ZRANGE, ex-aequo
   massifs (tout à 0), seuils de la petite forme.

### 13.9 Mesures prévues

- Mémoire par membre, grands et petits, scores entiers et flottants, contre Valkey.
- ZADD séquentiel (scores croissants) : remplissage des feuilles (split 90/10).
- ZSCORE / ZADD dans la petite forme à 32, 64, 128 membres contre la grande.
- Ex-aequo massifs : coût des comparaisons d'octets dans la descente.
- Remplissage réel de l'index valart (le nombre le moins sûr de l'estimation).

### 13.10 Options écartées (ne pas reproposer)

| option | raison |
|---|---|
| trie radix (oxidart) pour l'ordre | pas de rangs (O(n)), nœuds de 64 o et plus |
| `BTreeMap` std / à deux niveaux | pas de comptes, rangs en O(n) |
| valart pour ordonner les ex-aequo | ordre par hash ; Redis exige l'ordre lexicographique |
| scores `f32` | entiers exacts seulement jusqu'à 2^24 : casse les horodatages en ms |
| membre partagé `rc = 2` entre les deux structures | épinglé, le GC ne le déplace pas |
| deux copies du membre (une par structure) | remplacé par la poignée : une copie, déplacement en une écriture |
| `vals[]` / `scores[]` séparés dans la table | deux défauts par ZSCORE (§13.4) |
| petite forme à pointeurs | en-tête et arrondi par membre ; en ligne, un seul propriétaire, rien à suivre |
| pierres tombales dans le ZSet pour AMAC | v1 sans indice gardé ; en v2, « ZSet modifié → redescente » |
| rééquilibrage complet à la suppression | partie la plus délicate d'un B+tree, inutile à la correction |
| retour grande → petite forme | Redis ne le fait pas ; complexité sans cas d'usage |

### 13.11 Pour plus tard (après mesure)

- **Préfixe de 8 o du membre** à côté du score dans les feuilles et séparateurs : départage
  les ex-aequo par comparaison d'entiers, sans déréférencement (+~12 o/membre ; inutile si
  les membres partagent un long début, `job:000…`).
- Indices AMAC gardés (§13.6, v2).
- Indice de feuille dans l'entrée d'index (ZREM / mise à jour sans redescente).
- Variante « clé = score seul + seaux d'ex-aequo comptés » (§13.8 de l'ancien texte), si les
  ex-aequo massifs le demandent.
- List : même problème de save (hors barrière) et même principe (structure en slab, objets
  à poignée si plusieurs références).

### 13.12 Implémentation (2026-10-27) : étapes 1 à 5 faites

Code : `radixox-lib/src/handle.rs` (poignées), `oxidart/src/zset/` (`btree.rs`, `index.rs`,
`small.rs`, `large.rs`, `mod.rs`), commandes dans `oxidart/src/zcommand.rs`. L'ancien ZSet
(`InnerZCommand`, `zset_inner.rs`) n'est plus compilé qu'en test, comme oracle.

Écarts avec le texte ci-dessus, et pourquoi :
- **Index : trie haché à seaux, pas une variante du `HashNode` de valart.** Le `HashNode`
  est lié aux `SharedByte` partout (drop, GC, débordement, save) ; la variante aurait
  dupliqué ~2 500 lignes. `index.rs` : 4 bits du hash par niveau, seau de 128 o = 9 entrées
  `{poignée, score}` + empreinte, répertoire de 16 cases où un seau couvre une plage
  alignée (hachage extensible dans le répertoire). Coupe locale d'un seau plein, jamais de
  redimensionnement d'ensemble (pas de rehash O(n)) ; fusion de jumeaux peu remplis ;
  chaînes au-delà des 64 bits. Mesuré : remplissage 69 %, **23,4 o/entrée** (dont 2,9 o de
  répertoires), contre ~19 estimés. Piste si besoin : répertoires de 64 o.
- **En-tête dans la slab des ZSet** (`HiSlab<ZSet>`, `Tag::ZSet` + idx), pas un objet de
  structure du pool : même nombre d'accès (une case de slab au lieu d'un pointeur),
  aucune nouvelle `Tag`. La petite forme est aussi derrière cet idx (`ZSet::Small`, bloc
  dans le pool, visité par la phase `ZSets` du GC).
- **Séparateur = plus petite clé de son sous-arbre droit** (invariant vérifié). Un
  séparateur périmé gardait la poignée d'un membre retiré ; la poignée, réutilisée,
  faussait l'ordre des ex-aequo (trouvé par le différentiel). Retirer la clé d'un
  séparateur le remplace par la suivante ; la mise à jour sur place ne touche jamais la
  première entrée d'une feuille.
- NaN refusé au parseur (`arg_f64`) et en résultat de ZINCRBY (`ERR resulting score is
  not a number (NaN)`), comme Redis.

**Étapes 6 et 7 faites (2026-10-27).**
- AMAC (`zset/amac.rs`) : chaînes de la phase 1 bis comme au §13.6 ; `exec::hint_field`
  rend le membre et ce qui suit l'index (`ZProbe` : index seul, ancienne entrée, ou
  `(score, membre)` d'un ZADD). ZRANGE : pipeline dans la commande (`large::Range`).
  Différentiels oxidart (`amac_matches_plain_tree`) et serveur (`tests/amac.rs`) étendus à
  la grande forme.
- Save : SAVE_DESIGN.md §D (barrières en-têtes + feuilles, format v3 en ligne, listes
  différées). Test `amac_and_gc_during_save_keep_t0` : fenêtres AMAC et passes du GC entre
  chaque tranche, dump = vue T0.
- DEL / écrasement / expiration d'un grand ZSet (> 512 membres) : machine à état
  (`zset/lazy.rs`), la clé disparaît tout de suite, membres, feuilles, nœuds puis index
  rendus par tranches en fond (~4 K unités par tranche), gelée pendant une save. Mesuré :
  DEL d'1 M membres 0,03 ms, tout rendu en moins de 2 s, PING max 0,21 ms.

**Commandes (2026-10-27).** ZADD (NX XX GT LT CH INCR), ZRANGE (BYSCORE BYLEX REV
LIMIT WITHSCORES), ZREVRANGE, ZRANGEBYSCORE, ZREVRANGEBYSCORE, ZRANGEBYLEX,
ZREVRANGEBYLEX, ZRANK / ZREVRANK (WITHSCORE), ZCOUNT, ZLEXCOUNT, ZPOPMIN / ZPOPMAX,
ZREMRANGEBYRANK / BYSCORE / BYLEX, ZMSCORE, ZRANDMEMBER. Plages résolues en deux rangs
(`lower_bound`, O(log n)) puis parcours des feuilles. Différentiel contre Valkey 9.1.2
(`scripts/zset_vs_valkey.py`) : 0 écart sur 8 × ~29 000 commandes, grande forme comprise.
Écart voulu : `LIMIT` à offset négatif rend toujours vide (Valkey : vide ou dernier élément
selon l'encodage du ZSet, non documenté). ZREMRANGEBY* et ZPOP retirent membre par membre
(O(k log n), synchrone).

**Mesures (étape 8, 2026-10-27, `scripts/bench_zset.sh`, ~3 min).** Contre Valkey 9.1.2,
deux serveurs mono-thread épinglés sur le CPU 0, client sur 2–5, profil `performance`.
Mémoire : RSS par membre, clé comprise, membres de 15 o, 1 M membres, état stable (+12 s) :

| cas | RadixOx | Valkey | objectif §13.0 |
|---|---|---|---|
| grands (1000 × 1000), entiers | 78 o | 92 o | ≤ 70 |
| grands, flottants | 76 o | 92 o | |
| 1 × 1 M, scores croissants | 82 o | 87 o | |
| 1 × 1 M, tous à 0 | 82 o | 88 o | |
| petits (100 000 × 10) | 31 o | 23 o | ≤ 27 |

- Grands : structures seules 66,1 o/membre (test `memory_breakdown`, `--ignored`) =
  objets 20,0 + table 8,0 + B+tree 15,9 (feuilles remplies à 87 %) + index 22,3 (seaux à
  71 %) ; le reste est de la RSS hors structures (pages du pool, slabs).
- Petits : 92 o/membre avant correctif (chaque croissance du bloc laissait un trou dans le
  pool jusqu'au GC) ; un ZADD à plusieurs membres pré-dimensionne maintenant le bloc
  (`ZSmall::reserve`). Reste au-dessus de Valkey : bloc 19,2 o/membre, en-tête dans la slab
  des ZSet (32 o par ZSet = 3,2 o/membre ici), réserve fixe du pool (2 pages = 4 Mo, 4
  o/membre à ce volume), clé.

Débit (pipeline 64, 50 connexions, 2 M requêtes, ZSet d'1 M membres) :

| commande | RadixOx | Valkey |
|---|---|---|
| ZSCORE | 2,66 M/s (serveur à 74 % : minorant) | 1,60 M/s |
| ZADD (mise à jour ou ajout) | 0,79–0,89 M/s | 0,44 M/s |
| ZINCRBY | 0,89 M/s | 0,67 M/s |
| ZRANK | 1,33 M/s | 0,80 M/s |
| ZRANGE 500000 500009 WITHSCORES | 1,33 M/s | 1,14 M/s |
| ZRANGEBYSCORE x +inf LIMIT 0 10 | 0,89 M/s | 0,36 M/s |
| ZADD sur 100 000 petits ZSet | 2,00 M/s | 1,14 M/s |

ZRANGE WITHSCORES était à 0,89 M/s (sous Valkey) : seconde descente de la clé et score
formaté par `String` ; corrigé (`format_score`, entiers sans allocation).

Puis (7977c4b, ec84134 ; débit compté côté serveur, `total_commands_processed`) : ZADD
**1,19 M/s**, ZINCRBY **1,34 M/s**, ZRANK **2,00 M/s**, ZRANGEBYSCORE 0,88 M/s, petits ZSet
2,30 M/s. ZADD passait de 11 200 à 7 100 instructions : 3 descentes du B+tree au lieu de 2
(`update` = `find` + `remove`, qui refaisait `locate`), balayage des 24 séparateurs resté
scalaire (somme de booléens ; masque `u64` + `popcnt` le vectorise), deux recherches dans
l'index (`score` puis `add` → `upsert`), et l'entrée connue `(s, h)` retrouvée dans sa
feuille par égalité de poignée au lieu d'un départage par membre (deux défauts de cache par
ex-aequo). Mesures annexes : balayage SIMD contre dichotomie neutre en AVX-512 comme en AVX2
(`x86-64-v3`) ; feuilles non triées ≤ 1 % (décalages) ; préfixe de 8 o du membre dans la
feuille écarté (41 → 24 entrées par feuille, inutile sur des membres à préfixe commun).
Reste dans ZADD : `tie` à l'insertion (~11 % des cycles, ex-aequo à placer par membre).

Contre l'ancien ZSet `std` (`BTreeSet` + `HashMap`, 337909f), mêmes conditions : ZSCORE
2,24 → 3,03 M/s, ZADD 0,85 → 1,12, ZINCRBY 0,95 → 1,31, ZRANGE 0 9 1,15 → 1,61, ZRANGE au
rang 500 000 **2 155/s → 1,47 M/s** (rang en O(n) → O(log n)), petits ZSet 1,44 → 2,01 ;
RSS 100–107 → 76–86 o/membre. L'ancien exécutait moins d'instructions (ZADD 5 000) mais
plus de cycles (5 840 contre 4 390).

Leviers restants (mémoire) : petite forme directement dans la valeur oxidart
(`Tag::ZSetSmall`, spec §13.3 : −3,2 o/membre sur les petits), répertoires de l'index en
64 o (~−1,5 o), remplissage des seaux.

Reste : ZSCAN, ZUNION / ZINTER / ZDIFF (STORE), BZPOP* (service direct par `ZADD` / `ZINCRBY`, même mécanisme que les listes : §14.3).

## 14. List — architecture retenue (2026-10-27, pas implémentée)

### 14.0 Pourquoi, et pour quels usages

Aujourd'hui une List est un `VecDeque<SharedByte>` dans une HiSlab, sans aucune commande.
Le tampon du `VecDeque` vit dans le tas Rust (immobile, recopié en entier à chaque
doublement), et la save le sérialise d'un coup au `begin` (bloquant).

Usages visés, par ordre d'importance :
- **files de travaux** (`LPUSH` + `BRPOP` / `BLMOVE`) : opérations aux deux bouts ; la liste
  se vide par ses éléments les plus anciens ;
- **file fiable** (`LMOVE` vers une liste « en cours », puis `LREM` à la fin du travail) :
  le `LREM` porte sur une liste courte (le nombre de travaux en cours) ;
- **derniers N** (`LPUSH` + `LTRIM 0 N-1`) : bornées par construction.

**Décision (utilisateur, 2026-10-27)** : complexités de Redis (bouts en O(1), milieu et
positions en O(n)), pas d'index par valeur en v1 (option plus tard, §14.8) ; **blocs
chaînés** (un saut par bloc, pas par valeur) et **pierres tombales** pour les retraits,
rares : un retrait trouvé coûte O(1), la case morte part quand un pop l'atteint.

### 14.1 Structure

```text
en-tête (slab des listes)          blocs (HiSlab<Block>, 256 o, alignés sur 64)
┌───────────────────────┐   ┌──────────────────────┐   ┌──────────────────────┐
│ first, last : u32     │──▶│ prev, next : u32     │◀─▶│ prev, next : u32     │
│ len : u32 (vivants)   │   │ lo, hi : u8          │   │ lo, hi : u8          │
│ blocks : u32          │   │ dead : u32 (masque)  │   │ dead : u32           │
└───────────────────────┘   │ slots : [SharedByte; 30]  │ slots : [SharedByte; 30]
                            └──────────────────────┘   └──────────────────────┘
```

- **Bloc** : 16 o d'en-tête + 30 cases de 8 o = 256 o (4 lignes de cache). Cases occupées
  `[lo, hi)` ; bit `i` de `dead` = case `i` morte (pierre tombale). Vivants du bloc =
  `hi − lo − popcnt(dead)`. Coût : 256 o / 30 ≈ **8,5 o par élément** en plus de la valeur
  (Redis, listpack : ~2 à 11 o, valeur en ligne).
- **`push_back`** : case `hi` du dernier bloc s'il reste de la place, sinon un bloc neuf
  (`lo = hi = 0`). **`push_front`** : case `lo − 1` du premier bloc, sinon un bloc neuf
  rempli par la fin (`lo = hi = 30`). O(1).
- **`pop_front` / `pop_back`** : saute les cases mortes du bout (et efface leurs bits),
  prend la première vivante ; bloc vide → décroché et rendu. O(1) amorti : une pierre
  tombale n'est sautée qu'une fois.
- **Retrait d'une case trouvée** (`LREM`) : bit mort, valeur rendue (différée pendant une
  save, §14.4), `len −= 1`. Au bout du bloc, `lo` / `hi` reculent tout de suite (avec les
  morts voisines). Bloc sans vivant → décroché. Bloc à moins de ¼ de vivants dont un voisin
  a la place → fusion (≤ 30 copies) : la mémoire reste bornée même si les retraits
  touchent le milieu (sans fusion, pire cas 256 o par élément).
- **Insertion au milieu** (`LINSERT`) : dans le bloc, réécriture compacte (morts retirées,
  ≤ 30 copies) ; bloc plein → coupé en deux. O(position) pour trouver, O(1) ensuite.
- **Position `i`** (`LINDEX`, `LSET`, début de `LRANGE`, `LTRIM`) : on part du bout le plus
  proche en sautant des blocs entiers par leurs vivants, puis le rang dans le bloc se lit
  par le masque. O(distance / 30).

### 14.2 Commandes

| commandes | coût |
|---|---|
| `LPUSH` `RPUSH` `LPUSHX` `RPUSHX` | O(1) par élément |
| `LPOP` `RPOP` (`count`) | O(1) par élément |
| `LMOVE` `RPOPLPUSH` (même liste : rotation) | O(1) |
| `LLEN` | O(1) |
| `LINDEX` `LSET` | O(distance au bout / 30) |
| `LRANGE` | O(début / 30 + nombre rendu) |
| `LTRIM` | O(retirés), blocs entiers d'un coup |
| `LINSERT` `LREM` `LPOS` (`RANK` `COUNT` `MAXLEN`) | O(n) de parcours, comme Redis ; chaque retrait O(1) |
| `BLPOP` `BRPOP` `BLMOVE` `BRPOPLPUSH` | §14.3 |

`LMPOP` / `BLMPOP` : plus tard.

### 14.3 Commandes bloquantes : service direct (principe proactif)

**Règle (utilisateur, 2026-10-27, très importante)** : celui qui produit sert. Un push sur
une clé où des clients attendent **leur donne les éléments directement**, puis seulement le
reste entre dans la liste ; rien n'est inséré pour être aussitôt retiré. Même règle pour
les ZSet (`BZPOPMIN` / `BZPOPMAX` servis par `ZADD`, `ZINCRBY`). Pas d'événement, pas de
réveil : le producteur fait la transition et écrit la réponse (connexions parquées,
RUNTIME_ARCH.md §7).

**Où vérifier, sans rien payer sur le chemin chaud** : un client n'attend que sur une clé
**absente** (sur une clé non vide il est servi tout de suite, et chaque push sert les
attentes aussitôt). Une liste ou un ZSet existant n'a donc jamais d'attente : la
vérification n'a lieu **qu'à la création de la clé** (un compteur global d'attentes à zéro
la saute tout à fait). Tout chemin qui crée une List ou un ZSet passe par ce point de
service : `LPUSH` `RPUSH` `LMOVE` / `BLMOVE` (destination) `LINSERT` (non : clé requise)
`ZADD` `ZINCRBY`, et plus tard `RENAME`, `RESTORE`, `*STORE`.

**Sémantique de Redis, à l'identique** (vérifiée par le différentiel) :
- attentes par clé en FIFO (ordre d'arrivée) ; un client qui attend sur plusieurs clés
  (`BLPOP k1 k2 0`) est retiré de toutes dès qu'il est servi ;
- les éléments servis sont ceux que Redis donnerait **après** le push complet : la clé
  était vide, donc après `LPUSH k a b c` la liste serait `[c, b, a]` ; un `BLPOP` prend
  `c`, un `BRPOP` prend `a`, le suivant dans la file prend l'élément suivant de son bout ;
  le reste (éventuellement rien : aucun bloc alloué) devient la liste ;
- réponse du push : la longueur **après le push, avant le service** (`LPUSH k a b c` → 3),
  comme Redis ;
- ZSet : la clé était vide, les membres ajoutés sont triés en mémoire ; `BZPOPMIN` prend le
  plus petit `(score, membre)`, `BZPOPMAX` le plus grand, le reste est inséré ; réponse du
  `ZADD` inchangée (nombre ajoutés, ou score avec `INCR`) ; `XX` sur clé absente : rien ;
- `BLMOVE` / `BRPOPLPUSH` servi : l'élément est poussé dans la destination, qui peut à son
  tour servir ses attentes (en chaîne, itératif, jamais récursif) ;
- journal (AOF, réplication) : le push tel qu'envoyé, puis un `LPOP` / `RPOP` / `ZPOPMIN`
  / `LMOVE` par client servi, comme Redis le propage ; le réplica rejoue donc le même état.

**Attente** : clé absente → la connexion se parque, inscription `(slot, epoch)` dans la
file de la clé, timer du timeout ; timeout ou connexion morte : désinscription, rien au
journal. Le client servi reçoit sa réponse et sa connexion est rejouée au tour suivant
(§7) : une commande rejouée peut reparquer.

### 14.4 Save, GC, AMAC

- **Save en CoW, sans aucun arrêt**, comme les feuilles du B+tree (SAVE_DESIGN.md §D) : au
  `begin`, seulement la copie des bitmaps d'occupation (un `memcpy`) ; un bloc est copié à
  sa première modification pendant la save (au plus une fois). En-têtes en `SaveBarrier`
  (image : `first`, `len`), blocs en `SaveBarrier<Block>`,
  points de passage `block_mut` / `free_block` ; la save suit le chaînage T0 des blocs et
  écrit `len` puis les éléments vivants, en flux (budget 1 par bloc + 1 par élément).
  Valeurs retirées pendant la save → liste différée. Format de l'enregistrement : celui de
  la List v1 (cardinal puis éléments), relu tel quel. Remplace la sérialisation bloquante
  au `begin`.
- **GC** : la phase `Lists` visite les `SharedByte` des cases vivantes, blocs dans l'ordre
  de la slab (parcours linéaire, comme aujourd'hui).
- **Compactage de la slab des blocs** : facile ici (contrairement au B+tree) : un bloc
  déplacé n'est référencé que par ses deux voisins (`prev.next`, `next.prev`) ou par
  l'en-tête (`first` / `last`), atteignables depuis le bloc lui-même. Pas en v1 ; même
  mécanisme que la slab des nœuds ensuite.
- **AMAC** : en-tête → premier ou dernier bloc selon la commande ; pour un pop, les octets
  de la valeur rendue.

### 14.5 Ordre de travail

1. blocs + en-tête seuls, tests contre `VecDeque` (pushs, pops, retraits, insertions,
   positions, fusions) ;
2. branchement : remplace `HiSlab<VecDeque<SharedByte>>` (valeur, digest, GC, chargement) ;
3. commandes non bloquantes, différentiel contre Valkey (comme `zset_vs_valkey.py`) ;
4. commandes bloquantes (parquage, timers, journal) ;
5. save progressive ;
6. AMAC ;
7. mesures contre Valkey (`LPUSH`/`RPOP` avec et sans pipeline, `BRPOP`, `LRANGE`,
   mémoire par élément, petites listes).

### 14.6 Points ouverts

- **Aucune commande sans borne** : `LTRIM`, `LREM` ou `DEL` d'une très grosse liste ne
  doivent pas tenir la boucle ; au-delà d'un seuil, en job par tranches (comme KEYS,
  UNLINK, le DEL des grands ZSet). Même règle à appliquer à `ZREMRANGEBY*` (§13.12).

- **Petites listes** : une liste d'un élément coûte un bloc de 256 o, contre quelques
  dizaines d'octets chez Valkey (listpack). Si les mesures le montrent : premier bloc plus
  petit, ou petite forme (comme `HashSmall`).
- **Taille de bloc** : 30 cases (256 o) en v1 ; 62 (512 o) divise par deux les sauts des
  parcours au prix des petites listes. À mesurer.

### 14.7 Options écartées (ne pas reproposer)

| option | pourquoi |
|---|---|
| un nœud par élément (liste doublement chaînée) | un saut par valeur, ~16 o de liens par élément |
| `VecDeque` | tampon immobile dans le tas, recopie entière en grandissant, save bloquante |
| index par valeur en v1 | ~40 o par élément et un coût à chaque push pour un usage rare (§14.0) |

### 14.8 Plus tard : index par valeur (option)

Index haché valeur → occurrences (comme l'index du ZSet), pour `LREM` / `LPOS` / `LINSERT`
en O(clé + retirés). Les pierres tombales gardent les positions `(bloc, case)` stables, ce
qui le rend possible ; seules la fusion et la coupe de blocs devraient mettre à jour les
entrées déplacées (≤ 30). Créé à la demande sur une liste, jamais pour une file pure.
