# SAVE_DESIGN — Save progressive (snapshot COW applicatif)

> État : conception validée le 2026-10-09, implémentation en cours.
> Lire avant de toucher à la persistance, à `oxidart/src/lib.rs` (mutations),
> à `valart/` ou à `radixox-lib/src/save_barrier.rs`.
> Remplace §3.2bis (format par nœud) d'`ARCHITECTURE.md` ; le reste du §3
> (undo-log, atomicité fichier, recovery) reste valable.

## 0. Vocabulaire

| Terme | Sens |
|---|---|
| **T0** | Instant du `begin` de la save. La **vue T0** est l'état de la base à cet instant, et c'est ce que le dump contient. |
| **Barrière** (`before_write`) | Appel placé juste avant toute modification ou libération d'un nœud. Si le nœud n'a pas encore été sérialisé, elle en fait une copie T0. |
| **Image** | Copie T0 d'un nœud, stockée dans une `BTreeMap<idx, image>`. Oxidart : copie de capture qui possède un clone de la valeur et une copie de la compression (voir §2.A). Valart : copie bit à bit sans refcount. |
| **Capture** | Création d'une image. Au plus une par nœud et par save. |
| **Bitset `intact`** | Copie du bitmap de présence HiSlab au T0 (`copy_occupancy`). Bit à 1 = présent à T0, ni modifié ni sérialisé. Sert aussi de dirty flag. |
| **Point de passage** | Seule fonction autorisée à donner un `&mut` sur un nœud, ou à le libérer. La barrière y est placée. |
| **Liste différée** | Pendant une save, ce qui sort d'un nœud **valart capturé** (nœud retiré, clé/valeur sortie) et les **conteneurs Hash/Set supprimés** y sont déplacés bit à bit au lieu d'être droppés. Vidée par tranches à la fin de la save. |
| **Voie, étage, vol** | Parcours à K voies DFS, prefetch en deux étages, vol de travail entre voies. |
| **Époque** | Numéro de la save, noté par UNLINK au moment de sa coupe. |

## 1. Ce qui peut changer pendant une save

### Oxidart — `TaggedHiSlab<Node>` (64 B)
- Champs écrits dans le dump : `compression` (`CompactStr`, inline ≤ 7 octets sinon heap), `childs` (6 inline), `tag`+`val`, l'expiration dans `exp_and_radix`, `overflow_idx`.
- Champs **non écrits** (pas de barrière) : `parent_idx`, `parent_radix`, et le tag TTL de HiSlab (`map.tag`/`untag`).
- Hors du nœud mais modifiés sur place : `OverflowArena` (Vec + freelist) et `HugeOverflow` (HiSlab `thread_local`).
- Mutations : `set_internal`, `set_cond`, `split_node`, `create_node_with_val`, `push_child_idx`, `remove_child`, `try_recompress`, `del`, `deln`, `free_subtree_iterative`, `delete_node_inline` (expiration paresseuse dans `get`), `delete_node_for_eviction`, `expire`, `persist`, `get_or_create_hash`/`set`, `get_zset_mut`, `incrby` (via `node_value_mut`).
- Libérations (`map.remove`) : `del`, `free_subtree_iterative`, `try_recompress` (enfant absorbé), `delete_node_inline`, `delete_node_for_eviction`, `free_chunk` (UNLINK).
- Piège : `get_mut()` / `node_value_mut()` donnent un `&mut` même aux commandes qui ne font que lire (`hash_root`, `cmd_zcard`, `cmd_zscore`…).

### Valart — un seul `HiSlab<ValNode>` (128 B), partagé par les Hash et les Set
- Dans le nœud : entrées inline (`SharedByte` clé/valeur), bitmap et rang des enfants, pointeur `overflow` (bloc mimalloc modifié sur place : `push`, `remove`, `realloc`, `shrink_if_sparse`), `parent_or_card` (cardinalité sur la racine).
- Points de passage existants : `hash_node_mut`, `set_node_mut`, `free_hash_node`, `free_set_node`. L'allocation n'a pas besoin de barrière.
- Hors du slab : `hash_collisions` / `set_collisions` (`Vec<Vec<T>>`, quasi jamais utilisés).

### ZSet, List — `HiSlab` statiques, modifiés via `zset_mut` / `list_mut`.

### Compaction HiSlab, GC — pas encore implémentés. Règle future : gelés pendant une save (ils déplacent des idx).

## 2. Protection

### A. Images

**Oxidart** — même structure que le vivant, pour lire les enfants avec le même code :
```rust
struct OxidCow {
    images:    BTreeMap<u32, ManuallyDrop<Node>>, // overflow_idx réécrit → index dans `overflows`
    overflows: Vec<Overflow>,                      // big_overflow_idx réécrit → index dans `huges`
    huges:     Vec<HugeOverflow>,                  // copiées élément par élément (SmallVec peut être sur le heap en release)
}
trait ChildArenas { fn overflow(&self, i: u32) -> &Overflow; fn huge(&self, i: u32) -> &HugeOverflow; }
fn for_each_child(node: &Node, arenas: &impl ChildArenas, f: impl FnMut(u8, u32));
// deux impl : vivant (OverflowArena + HUGE_OVF) et COW (overflows + huges)
```
- Règle : *modifier les enfants de X, c'est modifier X* (`push_child_idx` / `remove_child` appellent la barrière de X avant de toucher l'overflow). L'`OverflowArena` et la `HugeOverflow` n'ont ni bitset ni COW à elles : pour un nœud intact, la save les lit dans l'arena vivante (inchangée depuis T0).
- Capture : **valeur `SharedByte` clonée (rc+1)** — l'image en détient une vraie référence, donc pas de liste différée ni de bitset `captured` côté oxidart ; le saut mémoire est de toute façon payé juste après par le drop quand c'est un SET qui écrase. **Compression heap copiée en profondeur.** Idx de conteneur : protégé par le drop différé des conteneurs (B).
- `overflows` et `huges` sont vidés par `clear()` à la fin de la save (capacité conservée).
- **Drop d'une image oxidart** (quand la save la consomme, ou à `finish`) : **jamais `drop(Node)`**, qui appellerait `drop_raw` et donc `hash_drop`/`set_drop`/`free_zset` sur l'idx de conteneur. `NodeImage` a son propre `Drop` : si `tag == Bytes` → drop du clone de `SharedByte` ; drop de la copie de compression ; tout autre tag (Int, Hash, Set, ZSet, List, None) → rien.

Capture oxidart, appelée **avant** toute modification du nœud ou de ses enfants :
```rust
if save.active && intact[idx] {
    intact[idx] = false;
    let mut img = capture_copy(&nodes[idx]);   // bit à bit + clone de la valeur (rc+1)
                                               // + copie profonde de la compression si heap
    if img.overflow_idx != u32::MAX {
        let mut ov = copy_of(&overflow_arena[img.overflow_idx]);
        if let Some(h) = ov.big_overflow_idx {              // à partir de 17 enfants
            ov.big_overflow_idx = Some(cow.huges.len() as u32);
            cow.huges.push(copy_elementwise(&huge_ovf[h]));
        }
        img.overflow_idx = cow.overflows.len() as u32;
        cow.overflows.push(ov);
    }
    cow.images.insert(idx, img);
}
```
Exemple : nœud 4 avec 9 enfants = 6 inline + 3 dans l'`Overflow` léger, pas de `HugeOverflow`. La save lit ensuite l'image via `for_each_child(&img, &cow, …)` et voit les 9 enfants T0, même si l'overflow vivant a été modifié, libéré ou réutilisé.

**Valart** — `ManuallyDrop<ValNode>` bit à bit, **sans toucher aux refcounts** (jusqu'à 6 entrées inline + un bloc overflow de 255, trop de sauts mémoire). Le bloc overflow est copié (`mi_malloc` + memcpy, allocation propre car c'est un pointeur et sa taille varie) et l'image pointe sur la copie ; la copie est libérée avec l'image. La save utilise les mêmes accesseurs (`key`, `val`, `child`) sur l'image que sur un nœud vivant.

`SaveBarrier::before_write(idx, || image)` reçoit une closure de construction.

### B. Liste différée (valart) et drop différé des conteneurs
Côté valart (images sans refcount), pendant une save, ce qui sort d'un nœud **capturé** n'est pas détruit : on le déplace bit à bit dans la liste.
```rust
enum Deferred {
    ValNode(ManuallyDrop<ValNode>),  // nœud valart retiré
    Bytes(ManuallyDrop<SharedByte>), // clé/valeur sortie d'un nœud valart capturé
    Container(Tag, u32),             // Hash/Set supprimé en entier
}
```
- **Seuls les nœuds valart capturés diffèrent** (leur image pointe sur les ressources T0). Bitset `captured` sur le slab valart (1 bit/slot) : mis à 1 à la capture, remis à 0 quand la save consomme l'image, et à `finish` pour les images restantes (via les clés de la map). Règle au point de passage pendant une save : `captured[idx]` → liste différée, sinon drop normal. On diffère tout ce qui sort d'un nœud capturé, même ce qui est né après T0.
- **Un `take` consomme le nœud en entier** : la voie copie la liste de tous ses enfants (idx), le pointeur de valeur et la compression. Ensuite le nœud ne concerne plus la save.
- **Aucun pointeur brut ne survit à une pause** : avant de rendre la main, chaque voie termine son tour (sérialise les valeurs prefetchées en attente). Seuls des idx restent dans les piles, valides grâce au COW.
- La liste différée est vidée **par tranches** en fin de save (même budget que le parcours). Un nouveau `begin` attend qu'elle soit vide.
- Hors save : comportement normal, aucun coût.
- Un conteneur supprimé pendant la save n'est pas détruit, donc ses nœuds valart restent intacts : pas de capture en masse.
- Valart est un `static` global : sa `SaveBarrier<ValNodeImage>` est un champ de `ValTrees`. Deux saves ne se chevauchent jamais (assertion).

À adapter :
1. **Variantes sans valeur de retour** (`del()`, `hash_remove`, `set_remove`…) : DEL/HDEL/SREM n'ont besoin que d'un compteur (on évite de ramener la valeur en cache, même hors save). Côté valart, la valeur sortie d'un nœud capturé part alors directement dans la liste différée. Les commandes qui ont besoin de l'ancienne valeur (GETDEL, SPOP, GETSET) la `clone()`.
2. **`drop_raw` sur un Hash/Set pendant une save** → `Deferred::Container` au lieu de `hash_drop` / `set_drop`.
3. Côté oxidart, rien d'autre : la valeur est tenue par le clone de l'image, la compression par sa copie profonde.

### C. Pendant une save, on limite les modifications
- **Les lectures ne modifient rien** : une clé expirée est renvoyée absente sans supprimer le nœud. Elle sera nettoyée après.
- **L'évicteur TTL est en pause.**
- Les getters gardent `&mut self` : hors save, une lecture qui trouve une clé expirée la supprime et recompresse ; pendant une save, elle renvoie seulement « absent ». Les commandes qui ne font que lire (`hash_root`, `cmd_zcard`, `cmd_zscore`…) lisent la valeur par référence après avoir trouvé le nœud, sans demander de `NodeValMut` (qui passerait par `node_mut` et déclencherait une capture).

### D. ZSet et List (v1)
Sérialisés **intégralement au `begin`**, de façon bloquante, dans une section du fichier indexée par idx de slab. L'enregistrement d'une clé ZSet/List référence cet idx T0. Pas de barrière sur leurs slabs.

### E. Buckets de collision valart
Copiés intégralement au T0 (rarissimes).

### F. UNLINK
- `deln_async` note l'époque à la coupe.
- Coupe faite avant le T0 de la save en cours : la libération continue via `forget` (sans capture), le sous-arbre est inaccessible depuis T0.
- Coupe pendant la save : on compte les clés en lecture, on répond, et la libération physique attend la fin de la save.

## 3. Barrières

| Slab | Barrière | Points de passage |
|---|---|---|
| Oxidart `Node` | `SaveBarrier<NodeImage>` | `node_mut(idx)`, `remove_node(idx)`, `children_mut(idx)` |
| Valart `ValNode` | `SaveBarrier<ValNodeImage>` (une seule pour Hash et Set) | `hash_node_mut`, `set_node_mut`, `free_hash_node`, `free_set_node` |
| Collisions valart | aucune | copie au T0 |
| ZSet, List | aucune | sérialisés au `begin` |

Audit : plus aucun `map.get_mut`, `map.remove` ou `nodes.get_unchecked_mut` hors des points de passage (sauf correction de `parent_idx`).

## 4. Parcours

- **DFS** (jamais BFS : la frontière d'un BFS peut contenir des millions de feuilles).
- **K voies** entrelacées. Chaque voie a sa pile, son buffer de chemin (préfixe de la clé courante) et son bloc de sortie.
- **Deux étages par pas** : terminer le nœud précédent (valeur et compression en cache) → écrire l'enregistrement, empiler les enfants ; puis lire le nœud suivant (prefetché quand son parent a été lu) → `barrier.take(idx)`, copier ce qu'il faut, prefetch de la valeur, de la compression heap, du bloc overflow et des enfants.
- **Vol** : une voie inoccupée prend la frame **du fond** (le plus gros sous-arbre) de la voie dont la frame du fond est **la moins profonde** (plus petit `depth` en `buf[start]` ; la longueur de pile n'est pas le bon critère), et recopie le préfixe `victime.path[..depth]`.
- **`StealStack`** au lieu d'une `VecDeque` : `buf: Vec<Frame>` + `start` (frames `[0, start)` déjà volées). Le propriétaire fait `push`/`pop` à la fin, le voleur prend `buf[start]` et incrémente `start`. Pile vidée → `clear()` et `start = 0`. Capacité réservée une fois ; si le haut est plein et `start > 0`, on récupère la place du début (`copy_within`) au lieu de grossir. Un seul thread : pas d'atomiques.
- **Frames typées** `Ox{idx,depth,radix}` / `Val{idx,level}` / `Coll{bucket}`. Un conteneur est toujours traité par une seule voie (le vol ne prend que des frames `Ox`). Un conteneur plus grand qu'un bloc est découpé en morceaux : le premier morceau du bloc suivant répète la clé (`lcp = 0`, clé complète) avec le **bit 6 du tag** (« suite de conteneur ») ; au reload, ses entrées s'ajoutent au conteneur existant.
- Ordre des frères indifférent : seule la contiguïté des sous-arbres compte (voir §5).
- **Tranches** : la save rend la main quand un budget de travail est épuisé, pas après N nœuds. Coûts : 1 par nœud lu (oxidart ou valart), 1 par bloc annexe lu (overflow, compression heap, bucket de collision), 1 par valeur/champ/membre écrit, +1 par 4 KB copiés pour une grosse valeur. Budget commun aux voies : constante `SAVE_TRANCHE_BUDGET = 1024`, à ajuster sur le p99.99 de memtier pendant une save.
- Clés expirées au T0 : non écrites.

Mesures (10M nœuds, idx mélangés) : linéaire 3,9 ns/nœud ; DFS strict + prefetch des enfants 13,4 ; 16 voies 11,1 ; 32 voies 10,6 ; plancher matériel aléatoire ~7 ns/ligne.

## 5. Format et fichiers

Varints à préfixe façon UTF-8 (la longueur se lit dans le premier octet).

```
enregistrement (fichiers de voie) :
  u8     tag        bits 0-3 : type (String, Int, Hash, Set, ZSet, List) ; bit 6 : suite de conteneur ; bit 7 : TTL présent
  varint lcp        préfixe commun avec la clé précédente (0 en début de segment et de bloc)
  varint len_suffixe
  [varint ttl_restant = expiration − T0 (secondes)]   si bit 7
  suffixe
  valeur :
    String → varint len | octets
    Int    → varint zigzag
    Hash   → varint nb | (varint len | champ | varint len | valeur)*
    Set    → varint nb | (varint len | membre)*
    ZSet/List → varint idx dans la section ZSet/List
```
- TTL : écart par rapport au `T0` de l'en-tête (1 à 3 octets en général). Les clés expirées au T0 ne sont pas écrites, donc l'écart est toujours ≥ 0. Un drapeau d'unité dans l'en-tête permettra de passer aux ms.

### Un fichier par voie
```
dump.tmp-<époque>/
  voie-00.rxd … voie-NN.rxd   ajout à la fin uniquement : blocs [longueur][nb_clés][données][checksum]
  zl.rxd                      section ZSet/List (sérialisée au begin)
  MANIFEST                    écrit en dernier : T0, unité TTL, seed valart, fichiers, nb de clés, checksums
```
- Pas d'écriture concurrente dans un même fichier ; les écritures partielles io_uring restent locales à une voie.
- Fin de save : fsync de chaque fichier, puis MANIFEST, son fsync, et `rename` du dossier. **Le dump est valide à partir de là** (reload par insertions, ordre quelconque).

### Contiguïté des préfixes et reload en masse
- **Pas de tri entre frères.** Le DFS garantit qu'un sous-arbre est écrit d'un bloc : toutes les clés sous un préfixe sont contiguës (`user:1…` juste après `user:`), quel que soit l'ordre `user:b, user:c, user:a`. C'est tout ce qu'il faut : dans oxidart, l'ordre entre frères n'a pas d'importance.
- Chaque voie produit une série de **segments** : un nouveau segment commence à chaque sous-arbre volé (début marqué dans le bloc). La voie volée continue son propre segment après le « trou ».
- **Reload en masse** : clés codées par rapport à la précédente (`lcp` + suffixe) ; le constructeur garde la pile des nœuds ouverts, remonte à la profondeur `lcp` et ajoute la suite comme nouvel enfant. Pas de recherche ni de split par clé, nœuds alloués dans l'ordre DFS. **Une seule recherche par segment** pour le raccrocher.
- **Fusion optionnelle** (plus tard) : regrouper les fichiers de voie en un seul, compresser. Elle ne compare pas de clés ; elle ne touche que des fichiers (thread ou processus séparé, sans verrou), et un plantage pendant la fusion ne perd rien.
- Le T0 (et l'unité du TTL) est dans le `MANIFEST` **et** dans l'en-tête de chaque fichier de voie, pour qu'un fichier se lise seul.
- Compression par bloc (lz4/zstd) plus tard. Écriture des voies : buffers de 256 KB–1 MB soumis à io_uring dès qu'ils sont pleins ; ordre temp → fsync → rename.

## 5bis. Comportement côté serveur (valeurs par défaut)

- **`SAVE`** répond quand la save est terminée (seule la connexion qui l'a lancée attend) ; **`BGSAVE`** répond tout de suite. Une seconde demande pendant une save → erreur `Background save already in progress`. Saves automatiques : plus tard, avec l'AOF.
- **Fichier** : chemin réglable (équivalent `dir` + `dbfilename`), `dump.rxdb` par défaut. Au démarrage, on recharge le dump existant **avant** d'accepter des connexions.
- **FLUSHDB et `deln` synchrones pendant une save** : même règle qu'UNLINK (couper, compter, libérer après la save).
- **Mémoire** : images + liste différée exposées dans `INFO`, sans limite en v1.
- **Erreur disque** (ENOSPC, fsync) : abandon de la save, suppression du dossier temporaire, le dump précédent reste intact ; la liste différée est vidée normalement.
- **TTL** : oxidart stocke des secondes ; unité « secondes » dans l'en-tête.
- **Une seule save à la fois** (assertion, y compris côté valart qui est un `static` global).

## 5ter. Options écartées (ne pas reproposer)

| Option | Raison |
|---|---|
| `fork()` + COW du noyau | Pages de 2 MB (THP) copiées sur une écriture d'un octet ; oxidart est une lib in-process (ARCHITECTURE.md §3.5). |
| Parcours linéaire du slab | Dumpe les sous-arbres coupés par un UNLINK en cours (orphelins), et obligeait à bloquer UNLINK pendant la save. |
| BFS | La frontière peut contenir des millions de feuilles. |
| Format par nœud avec ids | Lié au layout interne ; inutile avec un DFS. |
| Clé complète par enregistrement | Remplacée par `lcp` + suffixe (DFS = sous-arbres contigus). |
| Tri des frères / fusion triée | La contiguïté des préfixes suffit au reload en masse. |
| TTL en date absolue u64 | Écart à T0 en varint : 1 à 3 octets. |
| Astuce « premier octet ≠ 0 = pas de TTL » | Une clé peut commencer par `0x00` (Redis accepte tous les octets) ; le bit 7 du tag est sans ambiguïté. |
| `Vec<bool>` comme bitset | 1 octet par élément en Rust (pas de spécialisation comme `std::vector<bool>`). |
| `BTreeMap` dans un bump allocator | `BTreeMap::new_in` reste instable même en 1.100 (seuls Box/Vec sont stabilisés). |
| Bump allocator pour l'undo-log | Inutile en v1 ; on mesure d'abord avec la `BTreeMap` std. |
| Free différé dans l'allocateur (hook dans `SharedByte::drop`) | Coûte hors save ; remplacé par rc+1 (oxidart) et la liste différée ciblée (valart). |
| rc+1 sur les entrées valart à la capture | Jusqu'à des centaines de sauts mémoire par capture. |
| Bloquer UNLINK pendant la save (verrou lecteurs/écrivain) | Remplacé par l'époque de coupe + libération différée. |
| `VecDeque` pour les piles des voies | `StealStack` (Vec + `start`), pas de croissance par l'avant. |
| Stores non-temporels pour la copie du bitmap | ×1,5 seulement ; si la pause du `begin` gêne, passer à la copie paresseuse par bloc de 512 slots. |
| Clones de `SharedByte` gardés par le parcours | Salit les lignes de cache et impose une 2e passe aléatoire ; on sérialise directement. |

## 5quater. État du code (2026-10-09, branche `custom_parser`, commit « valart branché dans oxidart, fix mimalloc, SaveBarrier + SAVE_DESIGN.md »)

- `radixox-lib/src/save_barrier.rs` : `BitSet` (`get`/`set`/`clear`/`next_set`/`words_mut`) et `SaveBarrier<T>` (`begin_with(snapshot)`, `before_write(idx, &T)` qui fait un `ptr::read`, `forget`, `is_intact`, `unsafe take(idx) -> T0::{Slab, Image(&T)}`, `epoch`, `finish`). 6 tests sur un arbre jouet, propres sous ASan. **À faire à l'étape 3** : capture par closure (`before_write(idx, || image)`), images possédées et droppables (oxidart) vs bit à bit (valart), bitset `captured` pour valart.
- `~/rust/hislab` : `copy_occupancy(&self, out: &mut Vec<u64>) -> u32` sur `HiSlab` et `TaggedHiSlab` (+ test). Oxidart et valart en dépendance `path = "../../hislab"` le temps du chantier (repasser sur une version publiée ensuite). Commité dans hislab (ed38da3, non publié), avec la croissance eager de lvl2/lvl3 dans `bitmap_tree.rs`.
- Fix mimalloc : `SharedByte`, `CompactStr` et les blocs overflow valart appellent `mi_malloc`/`mi_zalloc`/`mi_free` (crate `libmimalloc-sys`) au lieu de `MiMalloc.alloc`, qui passe par `mi_malloc_aligned` et sur-alloue (−8 % de RSS à 100 B).

## 5quinquies. Méthode de travail

- **Tests** : mono-thread obligatoire (`RUST_TEST_THREADS=1` dans `.cargo/config.toml`, statics globaux). `cargo test -p radixox-lib -p oxidart -p valart`.
- **ASan** (nightly installé) : `RUSTFLAGS="-Zsanitizer=address -C target-cpu=native" cargo +nightly test -p <crate> --target x86_64-unknown-linux-gnu --target-dir <scratch>`. Pour chaque test de COW, faire un **contrôle négatif** (désactiver la protection, vérifier qu'ASan signale un use-after-free).
- **Test de référence de l'étape 5** : mutations aléatoires sur un vrai `OxidArt` (SET, DEL, HSET, HDEL, SADD, SREM, UNLINK, EXPIRE, expirations) entrelacées avec les pas de la save ; le dump doit être identique à un `getn("")` + contenu des conteneurs pris à T0.
- **Benchs** : profil `powerprofilesctl set performance` avant toute mesure ; runs courts (`TEST_TIME=10 LOAD_TIME=5 ./bench_memtier.sh`, `MT_PIPELINE=1` puis `256`), ordres de grandeur suffisants. Memtier avant/après l'étape 1 (le refactor touche le hot path), puis pendant une save (p99.99) pour régler `SAVE_TRANCHE_BUDGET`.

## 6. Ordre de réalisation

1. Points de passage oxidart, lectures sans `NodeValMut` dans les commandes de lecture, signatures sans valeur de retour, lectures sans modification pendant une save. Refactor pur : tous les tests existants passent.
2. Points de passage valart, même principe.
3. `SaveBarrier` avec closure de capture, `NodeImage` / `ValNodeImage`, barrières branchées.
4. Liste différée, drop différé des conteneurs, règles UNLINK, pause de l'évicteur.
5. DFS de référence (une voie, synchrone) et test aléatoire sur un vrai `OxidArt` (SET, DEL, HSET, SADD, UNLINK, expirations mélangés aux pas de la save) comparé à l'état T0, sous ASan.
6. K voies + prefetch à deux étages + vol, testées contre le DFS de référence ; bench ns/clé.
7. Format, fichiers de voie, blocs, checksum, section ZSet/List, MANIFEST, I/O.
8. Reload en masse par segment depuis les fichiers de voie, et test aller-retour.
9. (Optionnel, plus tard) Fusion des fichiers de voie, compression.
