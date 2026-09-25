# radixox / oxidart — Contexte d'architecture

> Document de référence pour agents IA travaillant sur ce projet.
> Contient : modèle d'exécution, invariants de sûreté, persistance, et le
> chantier en cours (data-as-value → data-as-ART-node).
> **Lire en entier avant de proposer du code.** Les décisions ici ne sont pas
> des préférences : ce sont des invariants dont dépend la correction du système.

---

## 0. TL;DR pour un agent

- **oxidart** = le graphe / radix tree (ART) cœur, adressé par index de slot dans un slab.
- **radixox** = le service redis-like bâti dessus (~60 commandes, pipeline, plusieurs M req/s).
- **Mono-thread async. AUCUN lock n'est permis. Jamais.** Ne propose jamais `Mutex`, `RwLock`, `Arc`, atomics de synchro, ni `fork()`. La sûreté est structurelle.
- **Invariant central** : rien de daté (borrow, index physique de slab, curseur, hypothèse de fraîcheur) ne traverse une barrière `.await` en étant réutilisé après sans re-résolution.
- **Cible de perf** : p99 < 900µs, p99.9 ~1100µs. Toute proposition qui risque un spike (stop-the-world, resize O(n), gros memcpy synchrone) est à rejeter ou à rendre incrémentale.
- **Chantier en cours** : migrer les types conteneurs (Hash/Set/ZSet/List) de « valeurs std (HashMap, BTreeSet…) rangées dans un slab » vers « sous-arbres ART natifs ».

---

## 1. Modèle d'exécution

Runtime **mono-thread async**. Un seul thread, ordonnancement coopératif, la concurrence n'existe **qu'aux points de `.await`**.

### 1.1 Règle d'or (l'invariant qui génère tout le reste)
> Rien de daté ne survit à une barrière async pour être réutilisé sans re-validation.

« Daté » = tout ce dont la validité dépend de l'état du monde à un instant : un `Ref`/`RefMut`, un **index de slab**, un curseur de parcours, un pointeur, une hypothèse (« j'ai déjà vérifié que X existe »).

### 1.2 Déclinaisons concrètes (toutes correctes par construction)
- **Commandes sync** (la grande majorité) : reçoivent `&RefCell<Oxidart>`, empruntent localement, s'exécutent **d'un tenant** (pas de `.await`), le borrow est droppé au retour par typage. Pas de yield → pas d'entrelacement → atomiques. Double-borrow entre sync = **impossible**.
- **Commandes async** (il y en a exactement 2, hors du hot path) : accèdent à l'arbre **uniquement** via `with_borrow(|art| { ... })` où la closure est **sync**. Conséquence : impossible de tenir un borrow à travers un `.await` (on ne peut pas mettre `.await` dans une closure sync → ne compile pas). Elles yield **entre** les sections, jamais dedans.
- **Connexions dynamiques (modèle acteur, inversion de contrôle)** : pas d'attente-avec-ressource-en-main. Un acteur qui trouve une connexion déjà prise **dépose une continuation logique** dans un slot d'attente et **se détruit**. Le détenteur actuel, en finissant, **respawn** un acteur depuis ce paquet. → le `hold-and-wait` est cassé → **pas de deadlock possible**.

### 1.3 Règle sur les continuations d'acteur
Le paquet déposé dans un slot d'attente doit être **100% intention logique re-résolvable** : commande + arguments + **clé-curseur** (jamais un index de slab, jamais un pointeur, jamais un résultat de validation antérieure). Il doit survivre à un délai arbitraire entre dépôt et respawn. Au réveil, le nouvel acteur **re-résout tout** dans le contexte présent.

### 1.4 Ce qu'un agent ne doit JAMAIS proposer
- Un lock, quel qu'il soit (le modèle l'interdit ; toute exclusion vient du mono-thread).
- Un borrow (`Ref`/`RefMut`) tenu à travers un `.await`.
- Un index de slab transporté à travers un `.await` ou stocké dans une continuation puis réutilisé sans re-`find`.
- `fork()` (voir §3.5).
- Une opération O(n) synchrone sur une grande structure sur le hot path (voir §5).

---

## 2. Structure de données : slab + ART

- Les nœuds vivent dans un **slab** (arène), adressés par **index de slot** (`u32`).
- **Contrainte dure : un slot = 64 octets, PLEINS. Zéro bit libre.** Aucune métadonnée ne peut être ajoutée in-slot. Tout marquage auxiliaire est **aside** (structures externes indexées par l'index de slot).
- Un `find` immuable traduit une clé en index de slot. Les commandes appellent `find` puis opèrent sur l'index (`prendre → opérer → rendre`, jamais de borrow persistant). **Pas de lien parent stocké** in-slot (à confirmer selon impl — conditionne le path-copying à la volée).
- Radix / ART : **ordonné lexicographiquement** (octets bruts). Fan-out via nœuds adaptatifs / compression de préfixe + chaînage pour la liste d'enfants. Allocation via slab.

### 2.1 Conséquence de l'ordre lexicographique
`field:10 < field:2` en ordre binaire. Pour tout usage où l'ordre sémantique compte (ranges, scan), **encoder les clés numériques** en big-endian / padding pour que l'ordre binaire coïncide avec l'ordre attendu.

---

## 3. Persistance (RDB + AOF)

Principe directeur : **le hot path n'est jamais dévié ; c'est le save qui est sacrifié.** Le save est rare, en fond, chunké/yieldé — sa latence n'a aucune importance. Le hot path (M req/s) doit rester intact.

### 3.1 AOF (obligatoire)
- Append-only log de mutations, O(1) par write. Durabilité continue.
- Régime **`everysec`-like** : fsync périodique. Perte bornée au crash (au pire la fenêtre non-fsyncée). **Assumé et documenté.**
- `fsync`-per-write (`always`) = incompatible avec plusieurs M req/s (chaque write attend le disque). Offert éventuellement en option, jamais le défaut. Rappel : **durable ≠ instantané** — la « définitivité » n'existe que si l'ack client est **après** le fsync.

### 3.2 RDB (snapshot)
- Déclencheur : temps écoulé **OU** taille d'AOF dépassée. Le « personne connecté / creux d'activité » est un **bonus opportuniste**, jamais la condition dure (sinon rejeu non borné si la nuit est chargée).
- Nécessaire **en plus** de l'AOF pour deux raisons : (a) borne le temps de rejeu à la récupération ; (b) fournit un **point de reprise sain** — un log de deltas propage les erreurs en cascade, un snapshot matérialisé est autonome et ne peut hériter d'une corruption antérieure.

### 3.2bis Format de sérialisation : node-level, pas memcpy brut, pas réinsertion logique

Ni dump brut du slab (memcpy), ni réinsertion via `ensure_key`/`split_node` (trop lent, refait la logique métier). Format retenu :

- **Itération linéaire du slab** (nœuds initialisés, filtrée par le bitmap HiSlab) → écriture séquentielle sur disque. Cache-friendly, et minimise la durée du save donc la fenêtre CoW (cf. §3.5 — plus le save dure, plus de nœuds risquent d'être déviés).
- Chaque nœud dumpé comme `node_id: { data sérialisée, childs[…] → node_id }` : topologie préservée par **référence d'id** (pas par pointeur mémoire), valeur sérialisée explicitement (pas de memcpy).
- **Pourquoi pas memcpy brut** : `Value::Hash/Set/ZSet` utilise (aujourd'hui, avant migration §6) `HashMap`/`BTreeSet` std qui contiennent des **pointeurs heap absolus**, pas des offsets relogeables. Un memcpy + reload à une autre adresse corromprait ces valeurs. Sérialisation explicite du contenu logique obligatoire tant que ces conteneurs std existent (et reste la voie la plus simple même après migration vers sous-arbres ART, §6).
- **Reload** = reconstruction directe du graphe depuis les enregistrements (place les nœuds aux bons index, rebranche les `childs` par id), pas de repassage par la logique d'insertion → load rapide.
- **Pas de compaction séparée nécessaire** : HiSlab maintient déjà l'invariant compact (swap last-used/first-free, back-pointers O(1)) → le dump linéaire hérite de cette compacité, pas de trous à défragmenter après coup. Une passe de fond optionnelle (dump→dump compressé) se limite à de la compression pure, jamais de compaction d'index, et ne touche jamais la structure vivante (pas de lock nécessaire côté ART).
- **Overflow arena** (HugeChilds) : slab séparé → soit un second namespace d'id, soit (plus simple) embarqué inline dans l'enregistrement du nœud parent plutôt que par référence croisée.

### 3.3 Mécanisme de snapshot non-bloquant : **UNDO-LOG** (décision clé)
On stocke **l'ancienne version (T0)**, PAS la nouvelle. L'arbre de base est **muté en place normalement** pendant tout le save.

- Le hot path ne consulte aucune structure de déviation → **zéro disjonction save/pas-save, cache locality intacte, vitesse maximale**.
- **Write-barrier** : avant de muter le nœud `i`, si `bitset_dirty[i] == 0` → copier l'état T0 dans l'undo-log + lever le bit. Sinon muter directement (déjà sauvegardé, ou déjà sérialisé).
- Le **save** lit : `bitset[i]==0` → slab (encore T0) ; `bitset[i]==1` → undo-log (récupère T0).
- **Réconciliation = NÉANT.** La base *est déjà* l'état courant. Fin de save = jeter l'undo-log + reset ciblé du bitset + baisser le flag. Rien à reconstituer. ← *c'est ce qui dissout le point le plus effrayant.*

Note : le redo-overlay (stocker la *nouvelle* version) est **rejeté** — il dévie le hot path pendant le save et rend la réconciliation dangereuse. L'undo-log met le coût sur le save (le bon sacrifice).

### 3.4 Structures aside (tout est aside — slots pleins)
| Structure | Rôle | Propriété anti-spike |
|---|---|---|
| `bitset_dirty` (1 bit/slot) | filtre O(1) « ce nœud est-il dévié ? » | grandit AVEC le slab, jamais de resize |
| `HashMap<u32, …>` `with_capacity` | porte les copies T0 des nœuds déviés | dimensionnée sur `write_rate × durée_save` ; `clear()` entre saves (pas de réalloc) ; capacité = high-water mark |
| `Vec<u32>` des index déviés | reset ciblé du bitset + itérateur de fin de save | push amorti, jamais de rehash |

**Reset entre saves : TOUJOURS ciblé** (parcourir `Vec` des déviés), **jamais** un balayage O(nb_slots) du bitset.

**Backpressure** : si la déviation approche la capacité réservée, **forcer la fin/reconciliation du save** plutôt que laisser resizer. La dirty-map pleine est un signal de scheduling, pas un resize.

**Alternative envisagée pour la dirty-map (`node_id → T0`)** : `BTreeMap<u32, OldNode>` sur allocateur **bump/arena** (`bumpalo::Bump` via `Allocator`, `BTreeMap::new_in`, **nightly-only** — `allocator_api`) plutôt qu'un `Vec<Option<Box<OldNode>>>` indexé direct sur toute la capacité du slab.
- Le tableau indexé direct semblait "gratuit" grâce à la null-pointer-optimization de `Option<Box<T>>` (`vec![None; n]` → `alloc_zeroed` → pages mmap demand-zero non commitées tant que non touchées), mais avec un pattern d'accès **pseudo-aléatoire** sur un domaine large (10M slots), même peu d'entrées dirty font fauter une fraction significative des pages (~39% pour 10K/20K pages, calcul type coupon-collector) → coûte en pratique proche du `HashMap` en RSS, plus une indirection pointeur en trop.
- `BTreeMap` regroupe ~11 entrées par nœud (B=6) → footprint mémoire proportionnel au **nombre d'entrées dirty réelles**, pas à la capacité totale du slab.
- Bump allocator = pas de free individuel (le besoin est insert+lookup seulement, destruction en bloc en fin de save via `drop(arena)`), évite la fragmentation heap du `BTreeMap` sur l'allocateur global. `bump.reset()` réutilise le chunk entre deux saves.
- **Non tranché** entre cette option et le `HashMap` `with_capacity`/high-water-mark déjà documenté ci-dessus (qui évite aussi tout resize via capacité fixe + `clear()`) — à départager par benchmark si le sujet redevient actif. Le `HashMap` a l'avantage de rester en stable Rust.

### 3.5 Pourquoi PAS `fork()`
- COW page-level du noyau + **huge pages** = 2 MiB copiés par écriture d'un octet → spike de latence corrélé au load. Rédhibitoire. (On ne peut pas désactiver THP : on *veut* les huge pages pour le TLB / le slab.)
- oxidart est une **lib in-process**, pas un service isolé : forker le process hôte est illégitime (fds dupliqués, threads non copiés) et **stalle la loop de l'hôte** qu'on protège.
- Le COW **applicatif** (granularité nœud, quelques dizaines d'octets) évite les trois problèmes.

### 3.6 Atomicité fichier (ordre SACRÉ)
1. Sérialiser vers fichier **temp**.
2. `fsync` le temp.
3. `rename()` atomique (même FS) → le RDB devient valide.
4. **PUIS SEULEMENT** tronquer l'AOF en amont de l'offset du snapshot.
- Capturer **l'offset AOF dans le même tick sync** que la racine (cohérence gratuite en mono-thread). Stocker l'offset **dans le header du RDB** (un seul fichier atomique, pas deux à synchroniser).
- Garder le **RDB N-1** jusqu'à validation du nouveau. **Checksum par entrée** (AOF + RDB) → détecter la corruption et s'arrêter au dernier point sain au lieu de propager.

### 3.7 Recovery (toujours identique, déterministe)
Charger le dernier RDB valide (checksum OK) → rejouer l'AOF **depuis l'offset du RDB** → si entrée AOF corrompue, stop au dernier point sain. Pipeline unique, jamais « l'un ou l'autre ».

---

## 4. Avantages structurels du modèle radix

### 4.1 UNLINK natif (`user:*`)
`user:` est un **vrai sous-arbre**. `UNLINK user:*` = détacher un pointeur (null), **O(1)**, atomique en mono-thread. Le sous-arbre devient **inatteignable par le live** → isolation par topologie (pas par lock). Nettoyage réel en **background task stateful** (elle est seule à y avoir accès → peut tenir curseurs/ptr sans risque de course).

**Deux fils à border** :
- Valeurs partagées par refcount → la task **décrémente**, ne `free` pas aveuglément (une valeur sous `user:*` peut être référencée ailleurs).
- **Save concurrent** : le save vit dans T0 où `user:*` existait → la **libération mémoire** doit respecter le gel-pendant-save (le détachement est immédiat ; le reclaim des slots attend la fin du save). Brancher le nettoyage sur le flag de save.

### 4.2 SCAN supérieur à Redis
L'ordre natif de l'ART transforme le curseur opaque de Redis (reverse-binary-iteration des buckets, doublons possibles, non reprenable à une clé) en **position sémantique** :
- Curseur = **la clé** (`user:32`). Reprise = successor query O(K) : « plus petite clé > `user:32` dans le sous-arbre ». Robuste sous mutation (une clé est toujours une question valide, même si la clé a disparu entre deux tours).
- Restreint au **sous-arbre du préfixe** (pas de scan du keyspace entier + filtre après coup comme Redis).
- Ordonné, sans doublon, reprenable — sémantique strictement meilleure au même coût.

**RÈGLE ABSOLUE** : le curseur transporté est **la clé, JAMAIS l'index de slab** (l'index périme sous mutation/reclaim → saut de clés, doublons, lecture de mémoire réaffectée).

Handle-par-connexion (`1 → user:32`) accepté pour économiser le réseau **parce que** le state par connexion existe déjà dans le modèle acteur — mais il mappe vers **la clé**, ce qui préserve la robustesse. À gérer : purge des handles à la fermeture **et à la réaffectation** de connexion (cas « connexion vivante réutilisée » = source de handle fantôme).

---

## 5. Anti-spike : croissance des structures

Le tueur de p99.9 = tout O(n) synchrone caché (rehash, resize, gros memcpy).

- **Rehash de hashmap** → **rehash incrémental** (double table + `rehashidx`, migration de N buckets par op, borné par buckets ET par entrées déplacées). En mono-thread : pas de synchro à gérer, mais **migration = transaction sync sans `.await`**.
- **Croissance de slab** → doit être **extension par blocs** (pas de réalloc qui copie/déplace). Avec huge pages : vérifier que la demande de nouvelles pages ne stalle pas.
- Coordination avec le save : geler le rehash pendant la fenêtre de save (même flag que le reclaim), ou snapshoter les deux tables + `rehashidx` de façon cohérente. Le plus simple : **geler**.

---

## 6. CHANTIER EN COURS : data-as-value → data-as-ART-node

### 6.1 État actuel du code

```rust
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    None = 0, Int = 1, Bytes = 2, Hash = 3, Set = 4, ZSet = 5, List = 6,
}

// Un slab std par type conteneur, valeurs std à l'intérieur :
static mut HASH_SLAB: MaybeUninit<HiSlab<InnerHCommand>> = MaybeUninit::uninit();
static mut SET_SLAB:  MaybeUninit<HiSlab<BTreeSet<SharedByte>>> = MaybeUninit::uninit();
static mut ZSET_SLAB: MaybeUninit<HiSlab<InnerZCommand>> = MaybeUninit::uninit();
static mut LIST_SLAB: MaybeUninit<HiSlab<VecDeque<SharedByte>>> = MaybeUninit::uninit();

#[derive(Clone, Debug, PartialEq)]
pub enum InnerHCommand {
    Small(SmallVec<2, (SharedByte, SharedByte)>),   // petit hash : inline
    Large(HashMap<SharedByte, SharedByte>),          // gros hash : std HashMap  <-- PROBLÈME
}
```

### 6.2 Le problème que ça résout
`HSET` sur un gros hash → `HashMap::Large` **resize O(n) synchrone** en plein hot path. Un `HSET` de 1M champs = catastrophe p99. **Les commandes Hash/Set/ZSet/List ne sont pas shippables en prod tant que ce n'est pas résolu au niveau arch.** Même problème potentiel pour tous les conteneurs à variante « Large » basée sur une structure std qui resize.

### 6.3 La cible
Remplacer la variante `Large(HashMap<…>)` (et équivalents Set/ZSet/List) par : **discriminant + inline small OU sous-arbre ART enfant** (« data as ART node »). Le gros conteneur devient un sous-arbre oxidart, adressé par index de slot, exactement comme la structure principale.

Forme visée (indicative) :
```rust
pub enum InnerHCommand {
    Small(SmallVec<2, (SharedByte, SharedByte)>),   // inchangé : petits hash restent inline
    Node(SlabIndex),                                 // gros hash = sous-arbre ART (index de slot)
}
```
(idem pour Set/ZSet/List : `Small(...)` | `Node(SlabIndex)`.)

### 6.4 Pourquoi c'est faisable sans refonte totale
L'indirection **existe déjà** : les valeurs sont **soit <8 octets inline, soit flaguées + pointeur vers slabs multiples**. Passer à un sous-arbre ART = **changer la cible d'une indirection déjà présente**, pas créer une couche. Slab, flag (`Tag`), indirection : déjà là. Le `find` par walk existe déjà (interface de parcours).

### 6.5 Bénéfices
1. **Tue le rehash** : un ART n'a pas de resize global (splits locaux à la place). Débloque les gros conteneurs.
2. **Ranges + scan ordonné sur les champs** (gratuit via l'ordre de l'ART) — surensemble fonctionnel de la hashmap. Permet aussi des fonctions custom (range queries sur champs de hash, etc.).
3. **Unification** : un seul allocateur / COW / reclaim / chemin de sérialisation. Save **uniforme** (un seul type à traiter au lieu d'un par conteneur).

### 6.6 Attentes CORRECTES (ne pas survendre)
- **Ne réduit PAS les peaks à néant** : déplace la pression du rehash vers les **splits de nœuds** et surtout la **croissance du slab** (`HSET` 1M = 1M d'allocs de slot dans un court intervalle).
- **Latence `HGET` unitaire possiblement en hausse** (trie = k sauts vs hashmap = 1-2 accès), compensée par la prévisibilité. Validé empiriquement : le radix principal tient déjà p99 < 900µs → le walk n'est pas trop lourd.
- **Le save reste non-trivial**, juste uniforme.

### 6.7 RISQUE N°1 À LEVER AVANT DE CODER
**Le chemin de croissance du slab sous insertion massive.** Benchmark obligatoire, 3 questions :
- (a) alloc de slot O(1) sans réorganisation même quand l'arène se remplit ?
- (b) agrandissement du slab = extension par blocs (OK) ou réalloc qui copie/déplace (SPIKE) ?
- (c) huge pages : l'extension demande de nouvelles huge pages → peut-elle staller la loop ?

Si les 3 = « pas de spike » → foncer. Si une spike → **c'est le vrai boss final**, à régler AVANT la migration (sinon on réécrit le modèle pour découvrir que `HSET` 1M spike encore, ailleurs).

### 6.8 Points de conception pour data-as-ART-node
- **Valeurs immuables + refcount, JAMAIS de copie de valeur lourde.** Modifier une valeur/sous-structure pendant un save = **nouvelle allocation + rebranchement de pointeur** (pas de mutation in-place), et l'undo-log sauve **pointeur T0 + incrément refcount** (pas une copie). Le save suit le pointeur → valeur maintenue vivante par le refcount. Empêche les valeurs lourdes de réintroduire un spike hot-path.
- **Reclaim différé à deux niveaux** (nœuds + valeurs), piloté par le flag save.
- **Topologie dans l'undo-log** : les writes en fenêtre de save peuvent faire **split/merge** (pas que des updates de valeur). L'undo-log capture « tout état T0 détruit » : nœuds supprimés (contenu + **slot gelé** jusqu'à fin de save), anciens pointeurs parent. Save parcourt la structure T0 reconstituée : propre→slab, dévié→undo-log, supprimé→undo-log (slot gelé), créé-après-T0→ignoré.
- **Ordre** : encoder les champs numériques (big-endian/padding) pour que l'ordre binaire = ordre sémantique.

### 6.9 QUESTION OUVERTE qui conditionne l'ampleur du chantier
**Les valeurs (`SharedByte`, valeurs de champs) sont-elles mutées en place ou réallouées à chaque modification ?**
- Réallouées (immuables) → undo-log = pointeurs + refcount, **zéro copie lourde**, switch propre.
- Mutées en place → il faut copy-on-write-de-valeur pendant le save ; pour les valeurs lourdes, c'est LE point à border. → basculer ces valeurs en immuable-pendant-le-save.

*À trancher avant d'implémenter la partie « données » du switch.*

### 6.10 Migration progressive (NE PAS big-bang)
1. **Persistance d'abord sur le radix principal** (déjà éprouvé, p99 < 900µs) : undo-log + AOF, sans toucher aux conteneurs. Valide le mécanisme de save sur une structure maîtrisée.
2. **Benchmark de croissance du slab** (§6.7), isolément, avant migration.
3. **Migration des conteneurs** (Hash puis Set/ZSet/List) vers sous-arbres ART, une fois persistance solide + allocateur validé. Ranges gagnés au passage.

Chaque étape shippable et réversible.

---

## 7. Checklist de revue pour un agent IA

Avant de valider une proposition de code, vérifier :
- [ ] Aucun lock / atomic de synchro / `Arc` / `fork()`.
- [ ] Aucun borrow tenu à travers un `.await`.
- [ ] Aucun index de slab transporté à travers un `.await` ou stocké dans une continuation sans re-résolution.
- [ ] Aucun curseur de scan = index de slab (doit être une clé).
- [ ] Aucune opération O(n) synchrone sur grande structure sur le hot path (rehash, resize, memcpy massif) — sinon rendre incrémentale/chunkée/yieldée.
- [ ] Les mutations sync s'exécutent d'un tenant ; les 2 async passent par `with_borrow` sync-only.
- [ ] Toute libération mémoire (reclaim de slots/valeurs) respecte le flag « save actif ».
- [ ] Ordre RDB/AOF respecté : temp → fsync → rename → puis truncate AOF.
- [ ] Reset de bitset ciblé (via `Vec` des déviés), jamais un balayage complet.
- [ ] Pour les valeurs partagées : décrément refcount, jamais free aveugle.
