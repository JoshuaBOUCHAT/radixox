# Analyse — "Tout dans l'ART" et TTL par valeur (Hash/Set/ZSet fields)

> Contre-analyse de `ARCHITECTURE.md` (produit par un IA sans accès au code) confronté
> à l'état réel du code sur la branche `custom_parser`, + étude de faisabilité du
> corollaire discuté : éliminer HashMap/BTreeMap/BTreeSet/VecDeque des conteneurs et
> tout stocker comme sous-arbres ART natifs, pour obtenir **TTL par champ de hash /
> par membre de set** avec la même mécanique que le TTL par clé top-level.
>
> Date : 2026-07-29. Basé sur lecture directe de `oxidart/src/{lib,value,hcommand,
> scommand,zset_inner,node_childs,compact_str}.rs` et `radixox-lib/src/shared_byte.rs`.

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
      compression: CompactStr,   // 8 (union ptr/inline)
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
| **Champ = `Node` ART complet (proposition)** | **64 B fixes** (le `Node` entier) + alloc heap champ si >7B (sinon inline dans `CompactStr`) + alloc heap valeur si Bytes non-inline | Le nœud lui-même remplace l'entrée `HashMap`, mais son overhead fixe (`Childs` 31B, `overflow_idx`, `parent_idx`, `exp_and_radix`) est payé **même pour un champ qui n'a jamais besoin de TTL ni d'enfants** |

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
structures internes de l'ART* (`CompactStr`, chemins de compression, `Childs`) au profit de
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
    compression: CompactStr,      // 8
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
d'alignement (8B pour `CompactStr`/`ExpAndRadix`/`Option<SharedByte>`) n'est plus
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
