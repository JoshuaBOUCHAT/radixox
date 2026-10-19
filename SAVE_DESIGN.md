# SAVE_DESIGN — Save progressive (snapshot COW applicatif)

> État : conception validée le 2026-10-09, implémentation en cours.
> Lire avant de toucher à la persistance, à `oxidart/src/lib.rs` (mutations),
> à `valart/` ou à `radixox-lib/src/save_barrier.rs`.
> Remplace l'ancien format par nœud et l'undo-log de l'ancienne version d'`ARCHITECTURE.md`
> (avant le 2026-10-19) ; principes de fichier et de recovery : `ARCHITECTURE.md` §3.

## Reprise — état au 2026-10-10 (à lire en premier)

**Fait** (branche `custom_parser`) : étapes 1 à 8. `BGSAVE` écrit `RADIXOX_DIR/dump.rxd` (défaut `.`) : DFS par tranches de 512 nœuds, double buffer de 2 Mo, fichier unique avec pied crc32 (§5 « Fichier », §5quater « Étape 7 »). Commits : `ffe6d63` (fix famine du yield monoio), `886d792` (étape 7), `7536903` (fix SETNX/HINCRBY, hors save). Tous les tests passent (`cargo test --release -p oxidart`, `-p radixox-lib`, `-p radixox`).

**Étape 8 faite (2026-10-10)** : reload en masse au démarrage, voir §5 « Contiguïté des préfixes et reload en masse » et §5quater « Étape 8 ». Fix au passage : `persist` ressuscitait une clé expirée pas encore nettoyée.

**Ensuite, pour clore la save** : ~~commande `SAVE` synchrone~~ (fait, §5bis) ; ~~vidage par tranches de la liste différée valart~~ (fait, §2.B) ; ~~reload en flux~~ (fait, §5quater « Reload par fragments ») ; `rename` final via `std::fs` sur le thread de service (bloquant, à passer en io_uring) ; mémoire des images et de la liste différée dans `INFO` ; `hislab` en dépendance `path` (`oxidart/Cargo.toml`) à repasser sur une version publiée. Compression zstd par tranches faite (§5 « Compression », `RADIXOX_SAVE_ZSTD`) ; reste à la mesurer sur des valeurs réalistes.

**Reporté jusqu'à la boucle io_uring maison (décision 2026-10-11)** : saves automatiques (BGSAVE toutes les N s si ≥ M changements) et AOF (optionnel, fsync chaque seconde, base = snapshot + fin de journal ; voir l'analyse coût snapshot ∝ taille de la base / AOF ∝ débit d'écriture). Raison : sous monoio, ça ajouterait encore des tâches minuterie et des boucles d'attente, de la dette pour la migration. À porter vers la boucle maison, déjà dépendant de monoio : la tâche BGSAVE (`yield_now` entre tranches de DFS, de compression et de vidage), `write_loop` (écritures `write_all_at` en vol), la surveillance du signal d'arrêt (`resp_cmd/shutdown.rs`, sondage toutes les 20 ms), l'attente de fin de BGSAVE avant la save d'arrêt, le `SaveDone` d'UNLINK. Dans la boucle maison, tout ça devient des travaux avancés d'une tranche par tour (save, compression, vidage, évicteur), le signal via `signalfd` lu par io_uring.

**Pièges connus**
- **Yield sous monoio** : ne jamais écrire un `yield` à base de `cx.waker().wake_by_ref()` ; monoio remet alors la tâche en tête de file et elle affame les connexions. Utiliser `oxidart::async_command::yield_now` (réveil par micro-tâche). Détail et mesures : §5quater « BGSAVE minimal ».
- **Le curseur de save** se libère **avant** `finish_save` (il peut contenir des valeurs possédées).
- **`SET n 42` stocke une chaîne** ; seul `INCR`/`INCRBY` crée un `Value::Int`.

**Benchs** (`scripts/stress_save.sh`)
- Variables : `MODES="nosave save"`, `RUN_TIME` (s), `PIPELINE` (défaut 1), `RATIO` (défaut `1:4`), `SERVER=valkey`, `OUT_DIR` (le dump y est écrit ; prendre un tmpfs pour comparer à Valkey). Serveur sur CPU 0-1 sous `systemd-run --scope -p MemoryMax=4G`, memtier sur CPU 2-7, 1M clés de 100 B.
- Durée ≈ 2 × (load + `RUN_TIME` + ~2 s) ; viser < 30 s par lancement, l'annoncer avant. Écrire la sortie dans un fichier (`systemd-run` lancé dans un pipe a renvoyé des 143).
- Mémoire : le pic du scope (`journalctl --user | grep "run-p.*memory peak"`) compte parent + fils de fork une seule fois, **et le fichier dump s'il est en tmpfs**.
- Sans pipeline, 93 % du CPU serveur est du temps noyau (réseau) et memtier (4 threads) est probablement la limite : pour mesurer un gain serveur, utiliser `PIPELINE=32` ou un client plus fort.
- Les mesures sans pipeline faites avant `ffe6d63` sont faussées par la famine du yield.
- À faire un jour : comparaison avec Valkey sur 3–5M clés en forte écriture (CoW du fork), en relevant `MemoryMax`.

## 0. Vocabulaire

| Terme | Sens |
|---|---|
| **T0** | Instant du `begin` de la save. La **vue T0** est l'état de la base à cet instant, et c'est ce que le dump contient. |
| **Barrière** (`before_write`) | Appel placé juste avant toute modification ou libération d'un nœud. Si le nœud n'a pas encore été sérialisé, elle en fait une copie T0. |
| **Image** | Copie T0 d'un nœud, stockée dans une `BTreeMap<idx, image>`. Oxidart : copie de capture qui possède un clone de la valeur et une copie de la compression (voir §2.A). Valart : copie bit à bit sans refcount. |
| **Capture** | Création d'une image. Au plus une par nœud et par save. |
| **Bitset `intact`** | Copie du bitmap de présence HiSlab au T0 (`copy_occupancy`). Bit à 1 = présent à T0, ni modifié ni sérialisé. Sert aussi de dirty flag. |
| **Point de passage** | Seule fonction autorisée à donner un `&mut` sur un nœud, ou à le libérer. La barrière y est placée. |
| **Liste différée** | Pendant une save, **toute** clé/valeur qui sort d'un nœud valart et les **conteneurs Hash/Set supprimés** y sont mis de côté au lieu d'être détruits. Vidée à la fin de la save. |

## 1. Ce qui peut changer pendant une save

### Oxidart — `TaggedHiSlab<Node>` (64 B)
- Champs écrits dans le dump : `compression` (`CompactPath`, inline ≤ 7 octets sinon heap), `childs` (6 inline), `tag`+`val`, l'expiration dans `exp_and_radix`, `overflow_idx`.
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

**Oxidart** — `NodeImage` autonome (`oxidart/src/save.rs`) :
```rust
struct NodeImage {
    compression: CompactPath,   // copie profonde
    tag: Tag,
    val: ValUnion,             // Bytes : clone (rc+1), droppé avec l'image ; Int : copie ; conteneurs : idx
    exp_and_radix: ExpAndRadix,
    childs_start: u32,         // enfants (inline + overflow + huge) aplatis
    childs_len: u32,           //   dans OxidSave::childs: Vec<(u8, u32)>
}
struct OxidSave { barrier: SaveBarrier<NodeImage>, childs: Vec<(u8, u32)>, t0: u64 }
```
- Règle : *modifier les enfants de X, c'est modifier X* (`push_child_idx` / `remove_child` passent par `node_mut(X)` avant de toucher l'overflow ; `remove_node` avant de libérer l'overflow). L'`OverflowArena` et la `HugeOverflow` n'ont ni bitset ni COW à elles : pour un nœud intact, la save les lit dans l'arena vivante (inchangée depuis T0).
- Enfants aplatis plutôt que copies d'`Overflow`/`HugeOverflow` avec index réécrits (décision 2026-10-10) : plus simple, pas d'allocation par capture (un seul `Vec` réutilisé, vidé à `finish`), lecture triviale côté parcours.
- **Drop d'une image oxidart** : jamais converti en `Node` (son `Drop` détruirait le conteneur). `NodeImage::drop` : si `tag == Bytes` → drop du clone ; la compression se droppe seule ; tout autre tag → rien.

**Valart** — `ManuallyDrop<ValNode>` bit à bit, **sans toucher aux refcounts** (jusqu'à 6 entrées inline + un bloc overflow de 255, trop de sauts mémoire). Le bloc overflow est copié (`mi_malloc` + memcpy, allocation propre car c'est un pointeur et sa taille varie) et l'image pointe sur la copie ; la copie est libérée avec l'image. La save utilise les mêmes accesseurs (`key`, `val`, `child`) sur l'image que sur un nœud vivant. Implémenté : `enum ValNodeImage { Hash(ManuallyDrop<HashNode>), Set(ManuallyDrop<SetNode>) }`, `HashNode::image_copy` / `free_image`, `HashOverflow::shallow_copy` / `free_shallow` (idem Set).

`SaveBarrier::before_write(idx, || image)` reçoit une closure de construction.

### B. Liste différée (valart) et drop différé des conteneurs
Côté valart (images sans refcount), pendant une save, **rien de ce qui sort d'un nœud n'est détruit** :
```rust
deferred_bytes: Vec<SharedByte>,        // via ValTrees::dispose (HDEL, SREM, SPOP, HSET qui écrase)
deferred_drops: Vec<(NodeType, u32)>,   // hash_drop / set_drop pendant une save
```
- **Pas de bitset `captured`** (décision 2026-10-10, prouvée sous ASan) : la règle « on ne diffère que ce qui sort d'un nœud capturé » est fausse. HDEL/SREM font remonter une entrée d'une feuille (capturée, image qui la référence) vers un ancêtre déjà sérialisé (donc non capturé) ; une suppression depuis l'ancêtre la détruirait alors que l'image de la feuille n'est pas encore lue. Test `valart::hash::tests::save_entries_moved_up_then_removed` (use-after-free sous ASan avec l'ancienne règle). Coût : 8 octets par entrée supprimée pendant la save.
- Les nœuds libérés hors `hash_drop`/`set_drop` sont toujours vides (ni entrée, ni overflow) : la barrière les capture, rien d'autre à différer.
- **Un `take` consomme le nœud en entier** : le parcours copie la liste de tous ses enfants (idx), le pointeur de valeur et la compression. Ensuite le nœud ne concerne plus la save.
- **Aucun pointeur brut ne survit à une pause** : avant de rendre la main, le parcours termine le nœud en cours (sérialise sa valeur). Seuls des idx restent dans la pile, valides grâce au COW.
- La liste différée est vidée **après** la save, par tranches (fait 2026-10-10) : `end_save` ferme la barrière, puis `drain_deferred(budget)` détruit les valeurs et les conteneurs différés (destruction de conteneur reprenable, `*_drop_step` avec une pile d'idx ; coût 1 par nœud + 1 par entrée, un nœud toujours en entier). Côté serveur, la tâche BGSAVE vide la liste avec `SAVE_TRANCHE_BUDGET` et un `yield` entre deux tranches, avant de libérer l'emplacement de save : un nouveau BGSAVE attend la fin du vidage (`begin_save` vide quand même d'un coup ce qui resterait, par sécurité). `finish_save` = `end_save` + vidage complet, pour les tests et les appels synchrones.
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
Copiés intégralement au T0 (rarissimes), **en profondeur** (clones rc+1) : ils sont modifiés et droppés sans barrière ni liste différée. Fait : `CollisionBuckets::snapshot_t0` au `begin_save`, `get_t0` dans les parcours T0, `clear_t0` à `finish_save`.

### F. UNLINK
- Avant chaque `free_chunk`, `deln_async` regarde si une save est active ; si oui, il **attend un waker** enregistré dans la barrière. `finish` réveille tous les wakers et les libérations reprennent.
- La coupe (`remove_child` sur le parent) passe par la barrière comme toute mutation : une seule capture. Coupe pendant la save → le sous-arbre reste visible via l'image du parent et il est dumpé (il existait à T0). Coupe avant T0 → inaccessible depuis la racine T0, jamais parcouru.
- Les nœuds du sous-arbre ne sont pas touchés tant que la save tourne : pas de capture en masse, pas d'époque, pas de `forget`.
- Coût accepté : un UNLINK lancé pendant une save répond à la fin de la save (le compte se fait pendant la libération).

## 3. Barrières

| Slab | Barrière | Points de passage |
|---|---|---|
| Oxidart `Node` | `SaveBarrier<NodeImage>` | `node_mut(idx)` / `try_node_mut(idx)`, `remove_node(idx)` (modifier l'overflow d'un nœud = appeler `node_mut` sur lui avant ; `remove_node` avant de libérer son overflow) |
| Valart `ValNode` | `SaveBarrier<ValNodeImage>` (une seule pour Hash et Set) | `hash_node_mut`, `set_node_mut`, `free_hash_node`, `free_set_node` |
| Collisions valart | aucune | copie au T0 |
| ZSet, List | aucune | sérialisés au `begin` |

Audit : plus aucun `map.get_mut`, `map.remove` ou `nodes.get_unchecked_mut` hors des points de passage (sauf `set_parent_idx` et `map.tag`/`untag`, non sérialisés). Fait côté oxidart (étape 1).

## 4. Parcours

- **DFS** (jamais BFS : la frontière d'un BFS peut contenir des millions de feuilles).
- **Un seul DFS classique** : une pile, un buffer de chemin (préfixe de la clé courante), un buffer de sortie. Pas de voies multiples ni de vol de travail (décision 2026-10-10, voir §5ter).
- **Visite d'un nœud** (`save_node`) : pop d'une frame → `barrier.take(idx)` → lire le nœud (slab ou image) → **prefetch de la valeur** (bloc `SharedByte`, ou racine valart d'un Hash/Set) → étendre le chemin avec la compression → empiler les enfants → **prefetch du nouveau haut de pile** → encoder l'en-tête de l'enregistrement et le **mettre en attente** (pas encore écrit). Voir §4bis.
- **Écriture différée** (`emit_oldest`) : un enregistrement est écrit `PIPE_DEPTH = 2` enregistrements après sa visite, quand sa valeur est en cache. Pour un conteneur, la visite intermédiaire prefetch ses entrées.
- Le bloc overflow d'oxidart et une compression heap peuvent encore fauter (lus juste après la visite) : accepté.
- **La pile DFS ne contient que des nœuds oxidart** : `Frame { idx, depth, radix }`. Pas de pile polymorphique.
- **Conteneurs : sérialiseurs monomorphes** `hash_ser_begin/step` et `set_ser_begin/step` (valart), un curseur `ValSer` (pile d'idx valart) partagé. Le DFS garde au plus un conteneur en cours (`Container::{None, Hash, Set}`) : le choix Hash/Set se fait une fois par démarrage ou reprise, jamais par entrée. `*_ser_begin` écrit la cardinalité T0 (lue par `SaveBarrier::peek`, sans consommer la racine) ; `*_ser_step` traite des nœuds entiers jusqu'à épuiser le budget (par nœud : prefetch des entrées, enfants empilés, prefetch du haut de pile, écriture). Un gros conteneur s'étale sur plusieurs tranches dans le même flux, sans découpage.
- **Un nœud (oxidart ou valart) est traité en entier dans une tranche** : le budget peut déborder d'un nœud. Après une pause, seuls des idx restent dans les deux piles.
- Ordre des frères indifférent : seule la contiguïté des sous-arbres compte (voir §5).
- **Tranches** : la save est une task monoio ; elle fait une tranche puis `yield_now().await` (comme `deln_async`). Elle rend la main quand un budget de travail est épuisé, pas après N nœuds. Budget partagé entre le DFS et le sérialiseur de conteneur. Coûts : 1 par nœud lu (oxidart ou valart), 1 par bloc annexe lu (overflow, compression heap, bucket de collision), 1 par valeur/champ/membre écrit, +1 par 4 KB copiés pour une grosse valeur. Constante `SAVE_TRANCHE_BUDGET = 1024` (à créer côté serveur), à ajuster sur le p99.99 de memtier pendant une save.
- Clés expirées au T0 : non écrites.

Mesures (10M nœuds, idx mélangés, parcours seul sans sérialisation) : linéaire 3,9 ns/nœud ; DFS strict + prefetch des enfants 13,4 ; 16 voies 11,1 ; 32 voies 10,6 ; plancher matériel aléatoire ~7 ns/ligne. L'écart DFS/voies (~3 ns/nœud, ~30 ms pour 10M) devient marginal une fois la sérialisation ajoutée, et la save tourne par tranches : sa durée totale compte peu.

## 4bis. Pipeline d'écriture : algorithme et invariants

Code : `oxidart/src/save.rs` (`save_step`, `save_node`, `emit_oldest`, `SaveCursor`, `Pending`, `PVal`). Mesure : −25 % sur les String, −42 % sur les Hash à 1 champ (1M clés).

### Pourquoi
Sans pipeline, un enregistrement est écrit à la fin de sa propre visite. Pour une String, le prefetch de la valeur n'a que l'empilement des enfants pour se masquer. Pour un Hash, c'est pire : la racine valart, puis le champ et la valeur (dont on ne connaît l'adresse qu'après avoir lu la racine) sont trois accès dépendants, payés en série.

### Algorithme
`SaveCursor::pending` est une file (FIFO) d'au plus `PIPE_DEPTH + 1 = 3` enregistrements visités mais pas encore écrits. Chaque enregistrement en attente (`Pending`) contient :
- `head` : l'en-tête déjà encodé (tag, lcp, longueur du suffixe, TTL, suffixe), buffer recyclé ;
- `val` (`PVal`) : de quoi écrire la valeur plus tard (`Bytes`, `Int`, racine `Hash`/`Set`, idx `ZSet`/`List`).

```
visite k     : take, prefetch valeur (SharedByte ou racine valart), enfants, en-tête → file
               si la file a ≥ 2 éléments : warm(file[n-2])   // conteneur visité à k-1 :
                                                             // racine en cache → prefetch entrées
               si la file a > 2 éléments : emit_oldest()     // écrit l'enregistrement de k-2
pile vide    : warm(file[1]) puis emit_oldest(), jusqu'à vider la file ; puis T_END
conteneur    : emit_oldest d'un Hash/Set écrit l'en-tête + la cardinalité et démarre son
               sérialiseur ; save_step le termine (sur plusieurs tranches s'il le faut)
               avant toute autre écriture ou visite
```

### Invariants (à respecter par toute modification de ce code)

**I1 — Ordre.** Les enregistrements sont écrits exactement dans l'ordre de leur visite DFS : la file est une FIFO, et un conteneur démarré bloque la file jusqu'à sa fin (`cur.container` est traité en tête de boucle, avant toute visite ou écriture). C'est ce qui rend le **lcp calculé à la visite** valide à l'écriture : `prev_len` et `min_trunc` sont mis à jour à la visite, jamais à l'écriture.

**I2 — Aucune mutation pendant une tranche.** `save_step` tient `&mut OxidArt` du début à la fin ; valart (global) n'est touché que par `take`/`peek`/sérialiseurs. Toutes les mutations de la base ont lieu **entre** deux appels à `save_step`. C'est l'hypothèse qui rend I3 sûr. Ne jamais appeler une commande, un `yield`, ou quoi que ce soit qui mute l'arbre à l'intérieur d'une tranche.

**I3 — Copie sans refcount = durée de vie d'une tranche.** `PVal::Bytes { owned: false }` est une copie bit à bit de la `SharedByte` d'un nœud lu dans le **slab** (`T0::Slab`). Elle ne possède rien : elle reste valide seulement tant que le nœud n'est pas modifié, c'est-à-dire jusqu'à la fin de la tranche (I2). Après le `take`, le nœud n'est plus intact : une mutation ultérieure ne le capturerait pas et pourrait libérer la valeur. D'où I5.

**I4 — Valeur lue dans une image : clone immédiat.** Une image n'existe que jusqu'au `take` suivant (`SaveBarrier::last`). La `SharedByte` d'un nœud lu en `T0::Image` est donc clonée (`owned: true`) dès la visite.

**I5 — `own_pending()` avant toute pause.** Chaque `return false` de `save_step` (budget épuisé, conteneur pas fini) appelle `cur.own_pending()`, qui clone les copies sans refcount. Entre deux tranches, `pending` ne contient que des valeurs possédées, des entiers et des idx. **Tout nouveau point de sortie de `save_step` doit faire de même.** Contrôle négatif : sans `own_pending`, use-after-free sous ASan dans `test_save`.

**I6 — Conteneur en attente = un idx de racine, consommé seulement à l'écriture.** `PVal::Hash/Set(root)` ne garde que l'idx. La racine reste dans la vue T0 (intacte ou en image) jusqu'à son `take` par `*_ser_step` : `warm` et `*_ser_begin` utilisent `peek`, jamais `take`. L'idx reste valide entre les tranches grâce à la barrière valart et au drop différé des conteneurs (§2.B).

**I7 — Le prefetch ne lit que la vue T0.** `prefetch_root_entries` passe par `peek` : il lit l'image si le nœud a été modifié, sinon le slab. Il ne fait que des prefetch (aucune donnée lue n'est utilisée), mais il déréférence des pointeurs d'entrées : il doit donc lire une version valide du nœud, jamais le vivant d'un nœud capturé.

**I8 — Fin.** `T_END` n'est écrit que si la pile est vide, la file vide et aucun conteneur en cours.

**I9 — Granularité.** Une tranche ne s'arrête jamais au milieu d'un nœud (oxidart ou valart) ni d'un enregistrement : le budget peut être dépassé d'un nœud plus un enregistrement. Entre deux tranches : seulement des idx dans les piles, le chemin courant, et la file (I5).

**I10 — Abandon.** Dropper un `SaveCursor` entre deux tranches est sûr (valeurs possédées, `PVal::drop`). `finish_save` libère les images et la liste différée.

## 5. Format et fichiers

Varints à préfixe façon UTF-8 (la longueur se lit dans le premier octet).

```
enregistrement :
  u8     tag        bits 0-3 : type (String, Int, Hash, Set, ZSet, List) ; bit 7 : TTL présent
  varint lcp        préfixe commun avec la clé précédente (0 pour la première clé)
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

### Fichier (implémenté 2026-10-10, remplace dossier + MANIFEST)
```
dump.rxd      en-tête | section ZSet/List | enregistrements | T_END | pied
pied          u32 LE crc32 (IEEE, crc32fast) de tout ce qui précède | "RXDE"
```
- Un seul fichier, puisque depuis l'étape 5 tout est dans un seul flux : plus de `zl.rxd` ni de `MANIFEST`. Le pied remplace le MANIFEST : `check_footer` détecte la troncature (pied absent) et la corruption (crc). `parse_dump` s'arrête à `T_END` et ignore le pied.
- Écrit dans `dump.rxd.tmp`, puis fsync du fichier, `rename` sur `dump.rxd` (atomique, remplace l'ancien), fsync du dossier. **Le dump est valide à partir du rename.** Dossier : `RADIXOX_DIR` (défaut `.`).
- crc32 IEEE (crc32fast, accéléré matériellement) au lieu de crc32c : crc32c n'était pas disponible hors ligne, même rôle.

### Contiguïté des préfixes et reload en masse
- **Pas de tri entre frères.** Le DFS garantit qu'un sous-arbre est écrit d'un bloc : toutes les clés sous un préfixe sont contiguës (`user:1…` juste après `user:`), quel que soit l'ordre `user:b, user:c, user:a`. C'est tout ce qu'il faut : dans oxidart, l'ordre entre frères n'a pas d'importance.
- **Reload en masse** : clés codées par rapport à la précédente (`lcp` + suffixe) ; le constructeur garde la pile des nœuds ouverts, remonte à la profondeur `lcp` et ajoute la suite comme nouvel enfant. Pas de recherche ni de split par clé, nœuds alloués dans l'ordre DFS. Le fichier est un seul flux depuis la racine.
- **Zéro split, construction ascendante (implémenté 2026-10-10, `oxidart/src/load.rs`)** : l'idée initiale (lire un enregistrement d'avance, nœud de la clé à `max(lcp_préc, lcp_suiv)`) est **fausse** : avec `abcd, abcx, ab_` (ordre DFS valide), rien n'annonce le branchement à la profondeur 2 quand on crée `abc`, il faudrait le couper ensuite. À la place : la pile contient les nœuds *ouverts* (profondeur de fin, valeur, enfants déjà construits dans un `Vec` partagé) ; une clé de lcp `l` ferme les nœuds ouverts de fin `> l`. Un nœud n'est alloué qu'à sa fermeture, quand son début `max(fin du nœud en dessous, l)` est connu, donc avec sa compression définitive ; si `l` le coupe, un nœud de branchement sans valeur est ouvert à `l` et devient son parent. Aucune lecture d'avance, aucun split. Nœuds alloués en post-ordre, sous-arbres contigus dans le HiSlab ; seul retour arrière : `parent_idx` écrit sur les enfants à l'allocation du parent.
- **Clés expirées au reload** (`t0 + ttl < now`, même règle que `is_expired`) : on les saute sans casser le chaînage, car en ordre DFS `lcp(A, C) = min(lcp(A, B), lcp(B, C))` (profondeur de l'ancêtre commun) ; le constructeur garde ce minimum à travers les clés sautées. Le chemin construit (`kept`) ne contient que les clés gardées (la clé courante, sautée ou non, est reconstruite à part par le lecteur).
- Comparaison Redis : chargement par `dictAdd` (taille pré-annoncée par `RESIZEDB`, mais un accès aléatoire au tableau de buckets par clé, donc un défaut de cache par clé dès qu'il dépasse le cache). Ici : parent en haut de pile, nœuds alloués à la suite dans le HiSlab, sous-arbres contigus en mémoire après le reload. Coût dominant attendu : l'allocation des valeurs (`mi_malloc`, ~20–50 ns par valeur). À mesurer à l'étape 8.
- Le T0 (et l'unité du TTL) est dans l'en-tête de `dump.rxd`.
- Écriture : buffers de 256 KB–1 MB soumis à io_uring dès qu'ils sont pleins ; ordre temp → fsync → rename.

### Compression (implémentée 2026-10-11, remplace la décision du 2026-10-10)
- **Tout sur la boucle de service, pas de thread** (règle de l'utilisateur : architecture mono-boucle). La compression zstd avance par tranches entre deux `yield`, comme le DFS. Code : `oxidart/src/compress.rs` (`ZstdStream`, `ZstdReader`), `radixox/src/bin/resp_cmd/bgsave.rs`.
- **Paramètres** : `RADIXOX_SAVE_ZSTD=<niveau>` (**défaut 3**, choix de l'utilisateur 2026-10-11 : dump compact, bon compromis général ; 0 : dump brut) ; `RADIXOX_SAVE_BACKLOG_MB` (défaut 64) = brut en attente de compression au plus. Le tampon couvre les deux modes proposés le 2026-10-10 : **grand** → le DFS finit vite, la barrière se ferme tôt et la compression continue depuis la RAM (le mode « dump brut sur tmpfs puis compression forte », sans fichier intermédiaire ni double écriture) ; **petit** → le DFS attend le compresseur (compression à la volée, barrière ouverte plus longtemps, pour les charges peu écrivantes).
- **Pipeline d'un tour** : une tranche de DFS dans `raw` (crc du fichier brut au fil de l'eau, pied ajouté dans `raw` à la fin du DFS) → une tranche de compression (`COMPRESS_SLICE` = 150 µs, au moins un bloc) de `raw` vers `fill` → écriture de `fill` par le double buffer existant. La barrière se ferme et la liste différée se vide à la fin du DFS ; l'emplacement de save n'est libéré qu'après la fin de la frame et la finalisation du fichier.
- **Format : une seule frame zstd standard** contenant le fichier brut (flux + pied) ; `zstd -d` le redonne. Même nom `dump.rxd` ; au reload, `load_file` reconnaît le magic zstd et décompresse au fil de la lecture (`ZstdReader`, un `Read`), le constructeur ne change pas. Le crc du pied porte sur le brut.
- **Pourquoi pas les morceaux indépendants de 1–4 MB prévus avant** (mesuré, `oxidart/examples/zstd_tranche.rs`) : à partir du niveau 12, chaque début de frame coûte 3–6 ms d'un bloc (initialisation des tables) ; une frame unique ne le paie qu'une fois et gagne ~3 % de ratio. Les morceaux indépendants servaient à décompresser en parallèle (inutile en mono-thread) et à garder un morceau brut (zstd le fait déjà par bloc).
- **Durée d'une tranche** : zstd compresse par bloc, un appel qui complète un bloc le compresse en entier. Taille max de bloc (`ZSTD_c_maxBlockSize`, expérimental, libzstd 1.5.7) selon le niveau : ≤ 5 → 32 KB, 6–11 → 8 KB, 12–15 → 2 KB, ≥ 16 → 1 KB. Et `windowLog`/`hashLog`/`chainLog` plafonnés à 20 (fenêtre 1 MB) : au niveau 19 les tables par défaut (~80 MB) sont remises à zéro au premier appel (8,6 ms d'un bloc) ; plafonnées, premier appel ~1 ms, ratio −1,7 %, débit ×2.
- Mesures `zstd_tranche` (dump 8 MB, valeurs façon JSON, une frame, blocs selon le niveau) : niveau 1–3 ratio 4,2, ~400 MB/s, appel p99 ~0,1 ms ; niveau 9 ratio 4,6, ~80 MB/s, p99 0,16 ms ; niveau 12 ratio 4,4, ~37 MB/s ; niveau 19 (plafonné) ratio 5,1, ~6 MB/s, p99 0,55 ms, max 0,9 ms.
- Mesures sous charge (`stress_save.sh`, pipeline 32, 1:4, run 8 s, même session ; **valeurs memtier très compressibles : ratio ~1000×, vitesse de compression non représentative**) :

  | | GET p99 | p99.9 | p99.99 | save | tranche de compression max |
  |--|--|--|--|--|--|
  | sans save | 1,175 ms | 1,367 ms | 1,647 ms | | |
  | save brute | 1,159 | 1,935 | 2,223 | 118 ms | |
  | zstd 3 | 1,159 | 1,719 | 2,039 | 111 ms (compression 9,7 ms) | 0,21 ms |
  | zstd 19 | 1,327 | 5,791 | 6,559 | 321 ms (compression 198 ms) | 1,37 ms |

  zstd 3 ne coûte rien de visible. zstd 19 reste lourd sur la boucle (tranches ~1 ms, p99.9 ×3) : à réserver aux charges qui l'acceptent ; avant le plafond des tables, tranche max 9,5 ms et p99.99 12,5 ms.
- À faire : bench avec des valeurs réalistes (ratio ~4–5, compression 10–100× plus longue : au niveau 19, ~17 s de compression pour 105 MB, étalée sur la boucle) ; régler `COMPRESS_SLICE` et le tampon ; avec la boucle io_uring maison, ne compresser que dans les temps morts au-delà d'un budget minimal par tour.
- Écarté : compression par valeur façon Redis (allonge les tranches, mauvais ratio sur petites valeurs) ; dictionnaire zstd ; thread de compression ou de décompression (architecture mono-boucle).

## 5bis. Comportement côté serveur (valeurs par défaut)

- **`SAVE`** (implémentée 2026-10-11, `resp_cmd/bgsave.rs::save_sync`) : **complètement bloquante**, la plus rapide possible : ni tranche ni `yield`, le DFS tourne d'une traite (`save_step` par pas de 65 536 nœuds, seulement pour borner le buffer), écriture par `write` bloquants de 8 MB (copie dans le cache du noyau, qui écrit sur disque en parallèle du DFS), puis fsync + rename + fsync du dossier. Aucune mutation possible pendant la save, donc ni image ni liste différée. Brut par défaut (`RADIXOX_SAVE_SYNC_ZSTD=<niveau>` pour compresser). Refusée pendant un BGSAVE. Mesure 10M clés (JSON ~85 B, SSD SATA) : brut 2,03 s dont DFS 0,36 s (le reste = disque, ~520 MB/s), 831 MB ; zstd 1 2,38 s, 171 MB ; zstd 3 2,56 s, 177 MB ; Valkey `SAVE` 20,4 s, 934 MB. Sur ce disque le brut gagne de peu ; sur un disque plus lent, zstd 1 gagnerait (5× moins d'octets).
- **Arrêt avec save** (`resp_cmd/shutdown.rs`, opt-in `RADIXOX_SAVE_ON_EXIT=1`) : SIGINT/SIGTERM lèvent un drapeau (seule chose faite dans le gestionnaire), une tâche le vérifie toutes les 20 ms, attend la fin d'un BGSAVE en cours, fait une `SAVE` puis `exit(0)` (`exit(1)` si la save échoue). Un second signal quitte immédiatement (`_exit(130)`), sans save. Opt-in pour qu'un Ctrl-C en développement ne laisse pas un dump rechargé au démarrage suivant.
- **`BGSAVE`** répond tout de suite. Une seconde demande pendant une save → erreur `Background save already in progress`. Saves automatiques : plus tard, avec l'AOF.
- **Fichier** : `RADIXOX_DIR/dump.rxd` (nom fixe en v1). Au démarrage, on recharge le dump existant **avant** d'accepter des connexions.
- **FLUSHDB et `deln` synchrones pendant une save** : correct sans règle spéciale (chaque libération passe par la barrière), mais capture tout le sous-arbre. Pour éviter cette dérive mémoire, les faire passer par le chemin async (waker).
- **Mémoire** : images + liste différée exposées dans `INFO`, sans limite en v1.
- **Erreur disque** (ENOSPC, fsync) : abandon de la save (le DFS s'arrête, la barrière se ferme), suppression du fichier temporaire, le dump précédent reste intact ; la liste différée est vidée normalement.
- **TTL** : oxidart stocke des secondes ; unité « secondes » dans l'en-tête.
- **Une seule save à la fois** (assertion, y compris côté valart qui est un `static` global).

## 5ter. Options écartées (ne pas reproposer)

| Option | Raison |
|---|---|
| `fork()` + COW du noyau | Pages de 2 MB (THP) copiées sur une écriture d'un octet ; oxidart est une lib in-process (ARCHITECTURE.md §3). |
| Parcours linéaire du slab | Dumpe les sous-arbres coupés par un UNLINK en cours (orphelins), et obligeait à bloquer UNLINK pendant la save. |
| BFS | La frontière peut contenir des millions de feuilles. |
| Format par nœud avec ids | Lié au layout interne ; inutile avec un DFS. |
| Clé complète par enregistrement | Remplacée par `lcp` + suffixe (DFS = sous-arbres contigus). |
| Blocs `[longueur][nb_clés][données][checksum]` et bit « suite de conteneur » | Venaient des fichiers par voie ; avec un seul flux séquentiel, un conteneur s'étale sur plusieurs tranches sans découpage. |
| Pile DFS polymorphique (frames Ox/Val/Coll), puis `ContainerSer` unique | Test de type dans la boucle ; remplacés par la pile DFS oxidart seule + `HashSer`/`SetSer` monomorphes. |
| Tri des frères / fusion triée | La contiguïté des préfixes suffit au reload en masse. |
| TTL en date absolue u64 | Écart à T0 en varint : 1 à 3 octets. |
| Astuce « premier octet ≠ 0 = pas de TTL » | Une clé peut commencer par `0x00` (Redis accepte tous les octets) ; le bit 7 du tag est sans ambiguïté. |
| `Vec<bool>` comme bitset | 1 octet par élément en Rust (pas de spécialisation comme `std::vector<bool>`). |
| `BTreeMap` dans un bump allocator | `BTreeMap::new_in` reste instable même en 1.100 (seuls Box/Vec sont stabilisés). |
| Bump allocator pour l'undo-log | Inutile en v1 ; on mesure d'abord avec la `BTreeMap` std. |
| Free différé dans l'allocateur (hook dans `SharedByte::drop`) | Coûte hors save ; remplacé par rc+1 (oxidart) et la liste différée ciblée (valart). |
| rc+1 sur les entrées valart à la capture | Jusqu'à des centaines de sauts mémoire par capture. |
| Bloquer UNLINK pendant la save (verrou lecteurs/écrivain) | Remplacé par un waker dans la barrière, attendu avant chaque `free_chunk`. |
| Époque de coupe + `forget` pour les UNLINK commencés avant T0 | Inutile : toute libération en cours attend le waker, le sous-arbre coupé n'est pas parcouru. |
| Parcours à K voies + vol de travail (`StealStack`, segments, un fichier par voie) | Gain mesuré ~20 % sur le parcours seul, marginal avec la sérialisation et le découpage en tranches ; complexité (vol, segments, reload par segment, K fichiers) non justifiée. DFS simple + prefetch du haut de pile. À ne reconsidérer que si un bench de bout en bout le justifie. |
| Stores non-temporels pour la copie du bitmap | ×1,5 seulement ; si la pause du `begin` gêne, passer à la copie paresseuse par bloc de 512 slots. |
| Clones de `SharedByte` gardés par le parcours | Salit les lignes de cache et impose une 2e passe aléatoire ; on sérialise directement. |

## 5quater. État du code (2026-10-09, branche `custom_parser`, commit « valart branché dans oxidart, fix mimalloc, SaveBarrier + SAVE_DESIGN.md »)

- `radixox-lib/src/save_barrier.rs` : `BitSet` (`get`/`set`/`clear`/`next_set`/`words_mut`) et `SaveBarrier<T>` (`begin_with(snapshot)`, `before_write(idx, &T)` qui fait un `ptr::read`, `forget`, `is_intact`, `unsafe take(idx) -> T0::{Slab, Image(&T)}`, `epoch`, `finish`). 6 tests sur un arbre jouet, propres sous ASan. *(Historique : remplacé à l'étape 3, voir plus bas.)*
- `~/rust/hislab` : `copy_occupancy(&self, out: &mut Vec<u64>) -> u32` sur `HiSlab` et `TaggedHiSlab` (+ test). Oxidart et valart en dépendance `path = "../../hislab"` le temps du chantier (repasser sur une version publiée ensuite). Commité dans hislab (ed38da3, non publié), avec la croissance eager de lvl2/lvl3 dans `bitmap_tree.rs`.
- Fix mimalloc : `SharedByte`, `CompactPath` et les blocs overflow valart appellent `mi_malloc`/`mi_zalloc`/`mi_free` (crate `libmimalloc-sys`) au lieu de `MiMalloc.alloc`, qui passe par `mi_malloc_aligned` et sur-alloue (−8 % de RSS à 100 B).

### Étape 1 faite (2026-10-10)
- `oxidart/src/lib.rs` : points de passage `node_mut` / `try_node_mut` / `remove_node` / `set_parent_idx` (exception sans barrière) ; `get_node_mut` supprimé. `create_node_with_val` passe par `push_child_idx`. `free_subtree_iterative` et `free_chunk` (UNLINK) retirent le nœud **avant** de libérer son overflow.
- Champ `saving: bool` (remplacé par la `SaveBarrier` à l'étape 3) : `get_idx` ne supprime plus une clé expirée pendant une save, `evict_expired` renvoie 0. `set_saving` en `cfg(test)`.
- Lectures sans capture : `get_ref(key) -> NodeValRef` (tag + `&ValUnion`) remplace `get_mut` ; `hash_root`, `set_root`, `cmd_zcard/zrange/zscore/zrem` passent par là. `get_or_create_hash/set` et `get_zset_mut` lisent d'abord et n'appellent `node_mut` (via `install_value_raw`) que pour créer. `NodeValMut` ne sert plus qu'à `incr`.
- `del(key) -> bool` (ne ramène pas la valeur, ne compte pas une clé expirée) ; `take(key) -> Option<Value>` pour GETDEL.
- Corrections au passage : `get_zset_mut` ne libérait pas une valeur expirée et héritait de son TTL ; un Hash/Set/ZSet créé avec TTL est maintenant tagué pour l'évicteur (le Hash/Set ne l'était pas) ; `deln("")` / `deln_async("")` vident vraiment la valeur de la racine (`deln_async` ne la supprimait pas) ; DEL d'une clé expirée renvoie 0.

### Étape 2 faite (2026-10-10)
- valart : seuls accès mutables au slab = `hash_node_mut` / `set_node_mut`, seules libérations = `free_hash_node` / `free_set_node` (maintenant `&mut self`, pour la barrière).
- `ValTrees::dispose(node, bytes)` : seul point où une clé/valeur sortie d'un nœud est détruite (HDEL, SREM, SPOP, valeur écrasée par HSET). Pendant une save : liste différée pour tout (étape 3, voir §2.B). Les entrées *déplacées* entre nœuds (remontée de feuille, relocation overflow, refill) ne passent pas par là : rien n'est détruit.
- `hash_set -> bool` (champ nouveau), `hash_remove -> bool` ; `hash_get_mut` supprimé (HINCRBY passe par `hash_set`). `set_pop` renvoie un clone, l'original passe par `dispose`.
- Buckets de collision : modifiés et droppés sans barrière ; la copie au T0 (§2.E) devra donc être **profonde** (clone rc+1 des `SharedByte`), pas bit à bit.

### Étape 3 faite (+ liste différée valart de l'étape 4), 2026-10-10
- `SaveBarrier<I>` : `before_write(idx, || image)`, images possédées droppées au `take` suivant ou à `finish`, `take` sûr ; plus d'`epoch` ni de `forget`.
- Oxidart : `NodeImage` + `OxidSave` (`save.rs`), barrière dans `try_node_mut` / `remove_node` ; `begin_save` / `finish_save` (démarrent aussi valart) ; `is_saving()` remplace le booléen de l'étape 1.
- Valart : `ValNodeImage`, barrière dans `hash_node_mut` / `set_node_mut` / `free_*_node`, `dispose` différé pendant une save, `hash_drop` / `set_drop` différés (`*_drop_now` à `finish_save`).
- Tests : DFS T0 synchrone de référence (`save.rs::reference`, `hash_for_each_t0` / `set_for_each_t0`) ; `test_save.rs` (mutations aléatoires SET/DEL/TTL/INCR/HSET/HDEL/SADD/SREM/SPOP/deln entrelacées, FLUSHDB pendant une save) ; contrôles négatifs (barrière oxidart coupée → échec ; `dispose` sans différé → use-after-free ASan).
- `radixox_lib::raw_alloc` + `--cfg radixox_sysalloc` pour ASan.
- Bug préexistant vu sous ASan, hors save, corrigé (0082b1a) : `SmallVecIterator` lisait l'ancien emplacement du vec sur `into_iter` en mode inline.


### Étape 4 faite (2026-10-10)
- UNLINK : `SaveDone` (future dans `async_command.rs`) attendu avant chaque `free_chunk` ; les wakers sont dans `OxidSave::waiters`, réveillés par `finish_save`. Test `deln_async_waits_for_save` (coupe immédiate, rien libéré pendant la save, vue T0 complète).
- Serveur : FLUSHDB pendant une save passe par `deln_async("")` (sinon capture de tout l'arbre) ; hors save, `deln` synchrone comme avant.
- Buckets de collision copiés en profondeur au T0 (test `save_collision_buckets_t0`).
- Reste en TODO : vider la liste différée par tranches (aujourd'hui d'un coup à `finish_save`).

### Étape 5 faite (+ format de l'étape 7 et tranches de l'étape 6), 2026-10-10
- Le DFS écrit directement le binaire (pas de couche intermédiaire) : `OxidArt::save_start(out) -> SaveCursor` (fige T0, écrit en-tête + section ZSet/List), `save_step(cur, out, budget) -> bool` (tranche), `finish_save()`. Un pas : `take` T0, prefetch de la valeur, chemin, enfants empilés, prefetch du nouveau haut de pile, puis écriture de l'enregistrement. `lcp = min(min_trunc, prev_len)` en O(1).
- Hash/Set : `ValSer` + `hash_ser_begin/step`, `set_ser_begin/step` (monomorphes, reprenables, prefetch des entrées et du haut de pile ; cardinalité T0 lue par `SaveBarrier::peek`).
- Format v1 dans `save.rs::format` : un seul flux `en-tête | section ZSet/List | enregistrements | T_END` (la section ZSet/List est dans le même flux, plus de `zl.rxd`). Varints à préfixe : `radixox_lib::varint`.
- `parse_dump(buf) -> Dump` (servira au reload). Le DFS de référence de test et `*_for_each_t0` sont supprimés : tous les tests passent par le chemin binaire (`save.rs::reference::{save_all, expected, decoded}`).
- Tests : aller-retour avec tranches de budget aléatoire et mutations entre tranches (TTL compris), compression lcp, rejet des dumps corrompus.
- Mesure (`oxidart/examples/save_bench.rs`, 1M insertions ≈ 880k clés, valeurs 100 B, sans I/O) : String 149 ms / 93 MB, Hash 1 champ 276 ms / 100 MB.
- Reste : fichier + I/O io_uring + crc32c + MANIFEST + rename (étape 7), BGSAVE/SAVE côté serveur, reload (étape 8), vidage par tranches de la liste différée.

### Pipeline d'écriture (2026-10-10, mergé)
- Voir §4bis (algorithme et invariants I1–I10). Mesure (1M insertions) : String 149–162 → 118–122 ms, Hash 1 champ 256 → 147 ms.

### BGSAVE minimal + test de stress (2026-10-10)
- `BGSAVE` (parser `Cmd::BgSave`, `resp.rs`, `resp_cmd/delayed.rs::bgsave_task`) : task monoio, tranches de `SAVE_TRANCHE_BUDGET = 512` (1024 au départ), `yield` entre deux tranches ; le dump est sérialisé en mémoire puis **jeté** (pas d'I/O, étape 7). Log serveur : `[bgsave] X MB en T, N tranches, tranche max M`. monoio repasse par io_uring après `2 × nb_tâches` tâches (`max_round`), donc la save ne peut pas affamer les connexions.
- Script `scripts/stress_save.sh` (~45 s au total) : load 1M clés 100 B, puis 15 s de 80 % GET / 20 % SET (memtier 4×20, serveur CPU 0-1), sans puis avec BGSAVE lancé 1 s après le début du run.
- Script `scripts/stress_save.sh` : `MODES="nosave save"` et `RUN_TIME` (s) en variables. Bug corrigé : le `wait` attendait aussi le serveur lancé en `&`, donc le script ne rendait jamais la main ; il n'attend plus que le `redis-cli BGSAVE`.
- Résultats (1M clés 100 B ; sans save : run 15 s ; avec save : run 8 s, BGSAVE à t+1 s) :

  | | sans save | avec save |
  |--|--|--|
  | load | 185 882 SET/s | 187 205 SET/s |
  | run | 187 644 ops/s | 195 076 ops/s |
  | GET p99 | 0,575 ms | 0,471 ms |
  | GET p99.9 | 0,847 ms | 0,671 ms |
  | GET p99.99 | 1,351 ms | **9,727 ms** |

  BGSAVE sous charge : 105 MB en 79 ms, 2050 tranches, tranche max 4,3 ms. Pic mémoire du scope serveur 193,5 MB, aucun OOM kill (journal noyau sur 7 jours).
- Re-run même environnement, 2 × 8 s de run (32 s au total) : sans save 165 936 ops/s, GET p99 0,783 / p99.9 1,047 / p99.99 2,143 ms ; avec save 168 494 ops/s, GET p99 0,695 / p99.9 1,055 / p99.99 **5,279 ms**. BGSAVE 105 MB en 87 ms, 2050 tranches, tranche max 688 µs. Pics mémoire du scope 183,5 / 192,7 MB. Machine globalement plus lente que le premier run (load 169k contre 187k SET/s).
- Même test sur Valkey 9.1.2 (`SERVER=valkey`, 1 thread, CPU 0-1, RDB sur tmpfs) : sans save 152 680 ops/s, GET p99 1,023 / p99.9 1,127 / p99.99 1,479 ms ; avec save 149 962 ops/s, GET p99 1,047 / p99.9 1,183 / p99.99 1,735 ms. BGSAVE par fork : ~2,1 s, RDB 22,9 MB (LZF, les valeurs memtier se compressent bien), CoW 8 MB, pic mémoire du scope 183,4 MB. Le fork coûte peu à 1M clés ; RadixOx sérialise 25× plus vite (87 ms) mais en tranches dans le thread de service, d'où le p99.99 plus touché (+3 ms contre +0,25 ms).
- `SAVE_TRANCHE_BUDGET = 512` (au lieu de 1024) : sans save 169 660 ops/s, GET p99 0,735 / p99.9 0,959 / p99.99 2,511 ms. Avec save, run 1 : p99.99 43 ms (blocage ponctuel non reproduit, tranche max 614 µs, `TCP_NODELAY` actif) ; run 2 : 169 728 ops/s, GET p99 0,719 / p99.9 0,975 / p99.99 **2,783 ms**. BGSAVE 82 ms, 4100 tranches, tranche max 667 µs. À 512, le surcoût de la save sur le p99.99 tombe à ~+0,3 ms (contre ~+3 ms à 1024), sans coût mesurable sur la durée de la save.
- **Bug de famine trouvé en pipeline** (`--pipeline=32`) : p99.99 = durée exacte de la save (48,9 ms pour 48,9 ms de save, 71 ms pour 71 ms). Instrumentation : 70,4 ms sur 71,5 ms passés *dans* les tranches. Cause : monoio remet en tête de file (`push_front`, `scheduler.rs`) une tâche qui se réveille pendant son propre poll ; le `YieldNow` par `wake_by_ref` repassait donc avant les connexions à chaque tour, jusqu'à épuiser `max_round`, et les connexions réveillées par les CQE restaient derrière jusqu'à la fin de la save. Même défaut dans `oxidart::async_command::yield_now` (KEYS, UNLINK). Fix : réveil via une micro-tâche (`monoio::spawn(async move { waker.wake() })`), rangé en queue ; `yield_now` d'oxidart est maintenant public et partagé par la save. Les mesures sans pipeline ci-dessus datent d'avant ce fix.
- Après fix, `--pipeline=32`, run 5 s, `SAVE_TRANCHE_BUDGET = 512` :

  | | 1:4 sans save | 1:4 avec save | 1:1 sans save | 1:1 avec save |
  |--|--|--|--|--|
  | run | 2 888 397 ops/s | 2 734 550 ops/s | 2 616 697 ops/s | 2 614 162 ops/s |
  | GET p99 | 1,279 ms | 1,495 ms | 1,503 ms | 1,463 ms |
  | GET p99.9 | 1,447 ms | 2,287 ms | 1,839 ms | 2,223 ms |
  | GET p99.99 | 1,703 ms | 2,863 ms | 3,183 ms | 2,831 ms |

  BGSAVE : 136 ms (1:4) / 134 ms (1:1) dont ~48–52 ms de calcul, 4100 tranches, tranche max 595–723 µs, plus long écart sans la main 1,0 ms. Avant fix : p99.99 48–49 ms en pipeline.
- Valkey, même config pipeline 32 / run 5 s : 1:4 sans save 1 820 114 ops/s, GET p99.99 3,183 ms ; avec save 1 792 116 ops/s, p99.99 4,095 ms. 1:1 sans save 1 678 484 ops/s, p99.99 3,871 ms ; avec save 1 667 792 ops/s, p99.99 3,999 ms. Fork 2,0–2,3 ms (`latest_fork_usec`), BGSAVE ~2 s, CoW 48 MB (1:4) / 79 MB (1:1).
- Mémoire (pic cgroup du scope = parent + fils, pages partagées comptées une fois ; **le RDB de 22,9 MB sur tmpfs y est compté**) : Valkey 154 → 224 MB (1:4) et 154 → 255 MB (1:1), soit CoW + fichier ; RadixOx 184 → 193 MB dans les deux ratios (+9 MB dont le tampon de sortie de 4 MB ; dump jeté, rien d'écrit). Correction du run sans pipeline plus haut : les +29 MB de Valkey étaient ~8 MB de CoW + ~23 MB de fichier.
- Lecture : débit et p99/p99.9 intacts (les écarts sont du bruit entre deux runs) ; seul le p99.99 GET prend ~8 ms, cohérent avec la tranche max de 4,3 ms plus la file accumulée derrière. Piste : budget de tranche en temps plutôt qu'en nœuds, ou budget plus petit (1024 nœuds Hash/valeurs longues → tranches inégales).
- Attention, lancer `systemd-run --scope` dans un pipe a renvoyé des 143 : écrire dans un fichier.

### Étape 7 faite (2026-10-10)
- `radixox/src/bin/resp_cmd/bgsave.rs` (remplace `delayed::bgsave_task`). `SaveFile` partagé (`Rc<RefCell>`) : fichier monoio, offset, `fill` (rempli par `save_step`), `spare`, crc, `in_flight`, `walk_done`, erreur.
- Double buffer, modèle proactif : quand `fill` ≥ 2 MB et rien en vol, la tâche de save échange les buffers et lance `write_loop`. Celle-ci, son `write_all_at` fini, réécrit tout ce qui s'est accumulé dans `fill` ; sinon elle s'arrête, ou finalise si le DFS est terminé. Si une écriture est en vol et `fill` ≥ 2 MB, le DFS ne produit plus et cède seulement (backpressure).
- La barrière se ferme à la fin du DFS, sans attendre le disque ; `save_running()` (thread-local) refuse un nouveau BGSAVE jusqu'à la finalisation. Le pied est ajouté à la fin du DFS (crc des morceaux déjà pris + reste de `fill`).
- Erreur d'écriture : le DFS s'arrête au tour suivant (curseur libéré **avant** `finish_save`), le fichier temporaire est supprimé, l'emplacement de save est libéré.
- Tests : `oxidart` `file_footer_detects_truncation_and_corruption` ; `radixox/tests/bgsave.rs` (30 000 clés + Int/Hash/Set/TTL, fichier relu par `check_footer` + `parse_dump` ; dossier absent → l'emplacement de save est libéré).
- Mesure (pipeline 32, 1:4, run 5 s, fichier sur tmpfs) : sans save 3 019 203 ops/s, GET p99.99 1,535 ms ; avec save 2 828 976 ops/s, GET p99 1,303 / p99.9 2,159 / p99.99 2,575 ms. BGSAVE : 105 MB en 128 ms (DFS 124 ms, fsync + rename 2,2 ms), 4100 tranches, tranche max 82 µs, 51 écritures, écriture max 2,9 ms. Pic mémoire du scope 184,5 → 289,4 MB, soit +105 MB = le fichier lui-même sur tmpfs : aucun surcoût RAM visible au pic.
- Reste : `SAVE` synchrone ; `rename` et ouverture du dossier passent par `std::fs` / monoio sur le thread de service (rapides, mais bloquants pour `rename`) ; vidage par tranches de la liste différée ; reload (étape 8).

### Étape 8 faite (2026-10-10)
- `oxidart/src/load.rs` : `DumpReader` (lecteur séquentiel : en-tête + section ZSet/List, puis `next_head` / `value` / `raw_value`) ; `parse_dump` passe par lui. `OxidArt::load_dump(&[u8]) -> Result<LoadStats, DumpError>` : arbre vide exigé, pas de save en cours, construction ascendante (§5), Hash/Set insérés directement dans valart (`hash_create`/`hash_set`, `set_create`/`set_add`, sans `Vec` intermédiaire), nœuds à TTL alloués par `insert_tagged`. Valeurs en attente dans un `RawVal` (libéré par `Drop` si le dump est illisible en cours de route). La seed valart du dump n'est pas reprise (inutile avec `hash_set`/`set_add`).
- Serveur : `resp_cmd/bgsave.rs::load_at_startup`, appelé dans `main` **avant** le `bind`. Fichier absent → démarrage à vide ; illisible, tronqué (`check_footer`) ou invalide → `main` renvoie l'erreur, le serveur ne démarre pas et le fichier n'est pas touché. Log `[load] … clés (… expirées sautées), … MB, lecture …, total …`. `std::fs::read` bloquant : acceptable au démarrage (rien n'est servi).
- Tests : `test_save.rs` `reload_roundtrip_random` (30 rounds : contenu identique, nombre de nœuds ≤ l'original, puis 300 mutations identiques sur les deux arbres comparées à chaque pas, SPOP coupé car aléatoire), `reload_shapes` (`abcd, abcx, ab_`, clé vide, préfixes, fan-out 90 → overflow léger + huge), `reload_skips_expired_and_evicts` (expirées sautées, TTL restant recalculé, l'évicteur retrouve les clés taguées), `reload_rejects_non_empty_tree_and_bad_dump` ; propres sous ASan. `radixox/tests/bgsave.rs` : `restart_reloads_the_dump` (BGSAVE, arrêt, redémarrage, relecture + écriture qui splitte un nœud rechargé), `corrupted_dump_prevents_startup`.
- Le test aléatoire a trouvé un bug préexistant : `persist` sur une clé expirée mais pas encore nettoyée lui retirait son TTL et la ressuscitait (`expire` vérifiait déjà). Corrigé.
- Mesure (`oxidart/examples/save_bench.rs`, 1M insertions ≈ 884k clés, valeurs 100 B, profil performance, sans I/O) : reload String **50 ms (57 ns/clé)**, Hash 1 champ **77 ms (87 ns/clé)**, même nombre de nœuds que l'arbre d'origine (1 107 758). Pour comparaison, l'insertion par `set` des mêmes clés (ordre aléatoire) prend ~290 ms ; la save ~100 ms (String) / ~125 ms (Hash).
- Reste (hors étape 8) : lecture du fichier par io_uring / mmap si le temps de lecture compte sur de gros dumps (non mesuré à froid).

### Reload par fragments (2026-10-11)
- `DumpReader<R: Read>` (`oxidart/src/load.rs`) : la source est lue par morceaux de 1 MB (`READ_CHUNK`) dans une fenêtre allouée une fois (`buf[pos..end]` à lire) ; seul l'enregistrement en cours doit y tenir (une valeur plus grande l'agrandit ; longueur > 4 GB = dump rejeté). Décodage direct dans la fenêtre (`get_varint` / `get_bytes`), recharge seulement si ça déborde (`#[cold]`). La mémoire du reload ne dépend plus de la taille du fichier.
- `OxidArt::load_file(R: Read)` : fichier complet (flux + pied), crc calculé au fil de la lecture sur les octets sortis de la fenêtre, pied vérifié après `T_END`, puis fin de source exigée. `load_dump(&[u8])` : flux brut sans pied. Erreur : `LoadError { Io, Dump }`. `parse_dump` passe par le même lecteur.
- Serveur : `load_at_startup` ouvre le fichier et appelle `load_file` (plus de `std::fs::read` du fichier entier ni de `check_footer` séparé).
- Tests : `reload_file_by_fragments` (source qui rend 1 à 13 octets par lecture, valeur de 3 MB > fenêtre), `reload_file_rejects_truncation_corruption_and_trailing_bytes` (coupes, octet modifié vu par le crc seul, octet en trop) ; propres sous ASan.
- Piège évité : agrandir la fenêtre de 1 MB (`resize`, donc mise à zéro) à chaque recharge rend le reload quadratique avec une source à petites lectures (47 s pour le test, 0,03 s après).
- Mesure (même bench que l'étape 8) : en mémoire String 55 ms / Hash 80 ms (contre 50 / 77 ms avec le lecteur sur tranche, l'écart = copie dans la fenêtre + tests de bord) ; depuis un fichier sur tmpfs, crc compris : 58–61 ms / 92–93 ms.
- Compression : branchée comme un `Read` (`ZstdReader`), voir §5 « Compression » ; pas de thread de décompression (architecture mono-boucle), la décompression se fait au fil de la lecture au démarrage.

### Nœuds 128 o à feuilles inline (2026-10-17)
- L'arbre passe aux nœuds v2 (`oxidart/src/node.rs`) : une clé sans descendant est une **feuille inline** dans le nœud parent (radix + suffixe + valeur + TTL éventuel), plus un nœud.
- **Format de dump inchangé** (enregistrements lcp + suffixe) : un dump de l'ancien arbre se recharge dans le nouveau et inversement (testé dans `test_diff.rs`).
- **Image** (`NodeImage`) : préfixe copié, self-val possédée, enfants aplatis dans `OxidSave::childs`, feuilles (radix, valeur possédée, échéance, suffixe copié) aplaties dans `OxidSave::leaves`. Modifier la page de débordement d'un nœud, c'est modifier le nœud : elle n'est accessible que par `node_mut`.
- **Visite** (`save_node`) : un enregistrement pour la self-val puis un par feuille, enfants empilés ; le nœud est toujours traité en entier (I9), la file `pending` peut donc dépasser `PIPE_DEPTH + 1` le temps d'écrire ses feuilles. Après chaque feuille, le chemin est retronqué à la clé du nœud (`min_trunc`), ce qui garde le lcp exact (I1).
- **Reload** : un nœud ouvert qui se ferme sans enfant devient une feuille inline de son parent ; les autres sont alloués comme avant.
- Oracle des tests : l'ancien arbre est gardé dans le crate `oxidart-legacy` (non publié, dépendance de test).

## 5quinquies. Méthode de travail

- **Tests** : mono-thread obligatoire (`RUST_TEST_THREADS=1` dans `.cargo/config.toml`, statics globaux). `cargo test -p radixox-lib -p oxidart -p valart`.
- **ASan** (nightly installé) : `ASAN_OPTIONS=detect_leaks=0 RUSTFLAGS="-Zsanitizer=address -C target-cpu=native --cfg radixox_sysalloc" cargo +nightly test -p <crate> --target x86_64-unknown-linux-gnu --target-dir <scratch>`. `--cfg radixox_sysalloc` bascule `radixox_lib::raw_alloc` (SharedByte, CompactPath, overflows valart) sur malloc/free : sans lui ASan ne voit pas les blocs mimalloc. `detect_leaks=0` : les tests monoio d'oxidart fuient leur runtime (préexistant). Pour chaque test de COW, faire un **contrôle négatif** (désactiver la protection, vérifier qu'ASan signale un use-after-free).
- **Test de référence de l'étape 5** : mutations aléatoires sur un vrai `OxidArt` (SET, DEL, HSET, HDEL, SADD, SREM, UNLINK, EXPIRE, expirations) entrelacées avec les pas de la save ; le dump doit être identique à un `getn("")` + contenu des conteneurs pris à T0.
- **Benchs** : profil `powerprofilesctl set performance` avant toute mesure ; runs courts (`TEST_TIME=10 LOAD_TIME=5 ./bench_memtier.sh`, `MT_PIPELINE=1` puis `256`), ordres de grandeur suffisants. Memtier avant/après l'étape 1 (le refactor touche le hot path), puis pendant une save (p99.99) pour régler `SAVE_TRANCHE_BUDGET`.

## 6. Ordre de réalisation

1. ✅ Points de passage oxidart, lectures sans `NodeValMut` dans les commandes de lecture, signatures sans valeur de retour, lectures sans modification pendant une save. Refactor pur : tous les tests existants passent.
2. ✅ Points de passage valart, même principe.
3. ✅ `SaveBarrier` avec closure de capture, `NodeImage` / `ValNodeImage`, barrières branchées.
4. ✅ Liste différée, drop différé des conteneurs, pause de l'évicteur, waker UNLINK, copie profonde des buckets au T0 (reste : vidage par tranches).
5. ✅ DFS (synchrone d'abord) et test aléatoire sur un vrai `OxidArt` (SET, DEL, HSET, SADD, UNLINK, expirations mélangés aux pas de la save) comparé à l'état T0, sous ASan.
6. DFS par tranches + prefetch (valeur, compression, overflow, haut de pile), testé contre le DFS synchrone ; bench ns/clé.
7. ✅ Format, fichier de données, crc, section ZSet/List, I/O (fichier unique avec pied, plus de MANIFEST).
8. ✅ Reload en masse depuis le fichier, et test aller-retour.
8bis. ✅ Vidage par tranches de la liste différée valart.
9. ✅ Reload en flux (lecture par morceaux).
10. ✅ Compression zstd par tranches (§5 « Compression ») ; reste : bench sur valeurs réalistes.

## Fin de save sans pic : feuilles des images libérées par tranches (2026-10-19)

À la fin du DFS, toutes les images ont été consommées (`take`), mais le tableau aplati de
leurs feuilles (`OxidSave::leaves`, une copie de valeur par feuille) restait entier :
`end_save` le libérait d'un bloc (8,8 ms pour ~480 000 feuilles, 1 M clés sous écriture,
p99.99 du serveur à 10 ms pendant une save). Désormais `end_save` le déplace dans
`OxidSave::leftover`, et `OxidArt::drain_deferred(budget)` le libère par tranches avant la
liste différée valart (le BGSAVE l'appelle déjà tranche par tranche). `finish_save` et
`begin_save` vident le reste d'un coup. Test : `image_leaves_are_freed_by_slices`.

## Plafond de CPU du BGSAVE (2026-10-18)

Mesure : sans plafond, la save prenait ~60 % du thread pendant ~100 ms (1M
clés) ; les tranches étaient courtes (≤ 62 µs) mais les clients faisaient la
queue : p99.9 0,46 → 0,94 ms. `RADIXOX_SAVE_CPU_SHARE` (0,1 par défaut) : la
tâche ne travaille que si temps de tranches + compression ≤ part × temps
écoulé, sinon elle cède. Résultat : ~550–690 ms sous charge, p99.9 au niveau
de la référence (0,46–0,74 ms selon le run, contre 0,79–0,86 pour Valkey).

Signal d'inactivité : aucune lecture réseau depuis 1 ms (`note_read`, un
compteur par lecture) → pas de plafond (~50 ms sur serveur inactif). Écarté :
« le `yield` est revenu tout de suite » — des requêtes déjà arrivées dans le
noyau attendent que la boucle les relève ; la save comblait ces trous et le
p99.9 remontait à 0,87 ms.

