# radixox / oxidart — Architecture, point par point

> Point d'entrée de la documentation d'architecture. Chaque sujet tient ici en
> quelques lignes (ce qui est vrai **aujourd'hui** dans le code, et pourquoi), puis
> renvoie au document de détail. Les décisions ci-dessous ne sont pas des
> préférences : ce sont des invariants dont dépend la correction du système.
>
> Réécrit le 2026-10-19 (l'ancienne version décrivait des nœuds de 64 o, un
> undo-log et un chantier de conteneurs depuis réalisés ; historique :
> `git log -- ARCHITECTURE.md`).

---

## 0. Carte des documents

| Document | Sujet | État |
|---|---|---|
| **ARCHITECTURE.md** (ici) | vue d'ensemble, invariants, checklist de revue | à jour |
| [`README.md`](README.md) | présentation, commandes supportées, configuration (`RADIXOX_*`), benchs de référence, layout du dépôt | à jour (public, en anglais) |
| [`RUNTIME_ARCH.md`](RUNTIME_ARCH.md) | boucle io_uring maison qui remplacera monoio : connexions, réception, émission, lot, équité, jobs, jamais bloquant, déploiement, migration | conception validée (revue 2026-10-19), **rien d'implémenté** |
| [`SAVE_DESIGN.md`](SAVE_DESIGN.md) | save progressive : COW applicatif par nœud, format du dump, BGSAVE / SAVE / reload | **implémenté** (étapes 1–8) ; restent saves auto + AOF, reportés à la boucle maison |
| [`SCAN_DESIGN.md`](SCAN_DESIGN.md) | SCAN à curseur côté serveur (table de curseurs, LRU, COUNT exact) | **implémenté** |
| [`TX_DESIGN.md`](TX_DESIGN.md) | transactions atomiques à conditions (`MULTI ATOMIC`, `DELPREFIX`, `EXEC DURABLE`) | conception, rien d'implémenté ; dépend de la boucle maison |
| [`ANALYSE_ART_UNIFIE.md`](ANALYSE_ART_UNIFIE.md) | journal de décisions « tout dans l'ART » : TTL par champ (abandonné), valart (§11–12), design ZSet retenu (§13) | journal ; §13 = référence du futur ZSet |
| [`protocol-spec.md`](protocol-spec.md) | spécification RESP de Redis (copie de référence) | référence externe |

Ordre de lecture conseillé : ce fichier → le document du sujet touché. Avant de toucher
au serveur réseau : RUNTIME_ARCH.md. Avant de toucher à une mutation d'`oxidart` ou de
`valart` : SAVE_DESIGN.md (toute mutation passe par un point de passage avec barrière).

---

## 1. Modèle d'exécution

**Un processus = un thread = une base.** Pas de lock, pas d'atomique de synchronisation,
pas d'`Arc`, pas de `fork()`. L'exclusion vient du mono-thread. Le multi-cœur passe par
plusieurs processus (sharding par namespace, qui préserve les scans par préfixe), jamais
par des threads qui partagent l'état. `SELECT n ≠ 0` n'a pas de sens (à transformer en
erreur, RUNTIME_ARCH §16).

**Aujourd'hui (monoio)** — `radixox/src/bin/resp.rs` :
- une tâche par connexion, arbre partagé en `Rc<RefCell<OxidArt>>` ;
- lecture double tampon, `read_and_flush` (écriture et lecture soumises ensemble) ;
- parse par fenêtres de 8 commandes, clés préchargées ensemble (`RADIXOX_PREFETCH`) ;
- commandes sync exécutées d'un tenant (atomiques) ; KEYS, UNLINK, FLUSHDB-pendant-save
  et BGSAVE découpés en tranches avec `oxidart::async_command::yield_now` ;
- Pub/Sub : writer task + `SubRegistry` (`radixox/src/bin/utils/mod.rs`).

**Demain (boucle maison)** — RUNTIME_ARCH.md : plus de futures ni de tâches ; files de
frames par connexion, tirage par tours, jobs à curseur, connexions parquées.

### 1.1 Règle d'or

> **Rien de daté ne survit à un point de rendu de main sans être re-résolu.**

« Daté » = un borrow (`Ref`/`RefMut`), un **idx de slab**, un pointeur, un curseur de
parcours, une hypothèse (« X existe »). Le point de rendu de main est un `.await` sous
monoio, **la fin d'un tour de boucle** dans la boucle maison.

Conséquences :
- un job ou une continuation garde une **clé** re-résolvable, jamais un idx de slab
  (exception : la save, protégée par sa barrière COW) ;
- un curseur de SCAN est rattaché à une clé, jamais à un idx ;
- aucun borrow de l'arbre n'est tenu à travers un `.await` ;
- dans la boucle maison, aucune référence vers l'anneau de réception ne survit au tour
  autrement que par une `FrameRef` comptée (RUNTIME_ARCH §5.4).

### 1.2 Jamais bloquant

Toute commande peut prendre du temps, aucune ne bloque les autres : les travaux longs
(KEYS, UNLINK, save, libérations lourdes) sont découpés en tranches. Aujourd'hui par
`yield_now` ; demain par des jobs à curseur, avec seuil « devient un job » et verrou de
lecture par clé pour l'atomicité (RUNTIME_ARCH §8.1).

---

## 2. Données

### 2.1 OxidArt — l'arbre des clés (`oxidart/`)

- **Nœuds de 128 o** (`oxidart/src/node.rs`, assertion de taille) dans un `HiSlab`
  (crate `hislab`, dépôt séparé `~/rust/hislab`) : `mmap` anonyme + `MADV_HUGEPAGE`,
  adresses stables, idx `u32` au lieu de pointeurs.
- **Feuilles inline** : une clé sans descendant n'est pas un nœud, c'est une entrée de son
  parent (radix, suffixe, valeur, TTL). Le dernier étage coûte quelques octets par clé.
- **Adressage par rang** : entrées triées, bitmaps (enfant / TTL / suffixe), popcount pour
  l'offset. Quatre formes : 8 entrées inline (recherche SWAR), +8 en page d'overflow,
  jusqu'à 32, jusqu'à 256 (bitmap de 256 bits).
- **Compression de chemin** (`CompactPath`, ≤ 7 o inline) et **recompression automatique**
  après suppression : forme canonique pour un contenu donné (`assert_canonical` en test).
- **Descentes entrelacées** (`prefetch_batch.rs`, `descend_batch`) : K clés avancent
  étage par étage, les miss mémoire se superposent. MGET / EXISTS réutilisent l'idx
  trouvé ; MGET et KRANGE encodent les valeurs par référence (pas de clone de refcount).
- **Ordre lexicographique** des octets bruts : `getn` / `deln` par préfixe, `getn_glob`
  (DFA), KRANGE / KREVRANGE, SCAN. Conséquence : encoder les nombres en big-endian / avec
  padding quand l'ordre numérique compte.
- **TTL** : horloge murale en ms (`tick()` lit `SystemTime`), expiration paresseuse à la
  lecture + évicteur actif par échantillonnage (`evict_expired`). Pendant une save, les
  lectures ne suppriment pas et l'évicteur est en pause.
- Oracle de test : `oxidart-legacy` (ancien arbre 64 o), tests différentiels `test_diff.rs`.

### 2.2 Valeurs

| Type | Représentation |
|---|---|
| String | `SharedByte` : buffer à refcount mono-thread de 8 o (pas d'atomique), mimalloc |
| Entier | `i64` (seuls INCR / INCRBY en créent ; `SET n 42` stocke une chaîne), formaté à la lecture |
| Hash, Set | ≤ 8 entrées : bloc compact de `SharedByte` ; au-delà : **valart** (`valart/`), un radix compact par conteneur, slab unique partagé (nœuds de 128 o) |
| ZSet | `BTreeSet<(score, membre)>` + `HashMap` membre → score ; design cible (B+tree à compteurs) : ANALYSE_ART_UNIFIE §13 |
| List | pas de commandes (LPUSH… à faire) |

Pourquoi valart plutôt que `HashMap` : pas de rehash O(n) en plein chemin chaud, ordre
naturel, même mécanique de save que l'arbre principal (ANALYSE_ART_UNIFIE §10–12).

### 2.3 Commandes (`radixox-lib/src/cmd.rs`)

`Cmd<'a>` : les clés empruntent le tampon de lecture (zéro allocation), les données
stockées durablement (valeurs, membres, arguments de commandes longues) sont des
`SharedByte` possédés. `Cmd::parse` = une passe, sans retour arrière. Limites et
découpage à trois issues (complet / incomplet / invalide) : à faire (RUNTIME_ARCH §5.3).

---

## 3. Persistance

**Principe : le chemin chaud n'est jamais dévié ; c'est la save qui paie.**

- **Fait** (SAVE_DESIGN.md) : snapshot par **COW applicatif au niveau du nœud**. Au T0,
  une barrière s'ouvre ; avant toute modification ou libération d'un nœud pas encore
  sérialisé, son image T0 est copiée (au plus une fois). Le DFS par tranches lit le nœud
  vivant ou son image. Valart : liste différée (rien de ce qui sort d'un nœud n'est
  détruit pendant la save). UNLINK attend la fin de la save pour libérer.
- **Commandes** : BGSAVE (tranches, compression zstd par tranches, ≤ 10 % du thread sous
  charge), SAVE (bloquante), save d'arrêt (`RADIXOX_SAVE_ON_EXIT`), reload en masse au
  démarrage (`RADIXOX_DIR/dump.rxd`, crc32 vérifié ; dump illisible = refus de démarrer).
- **Pas de `fork()`** : le COW du noyau copie 2 Mo par écriture d'un octet avec les huge
  pages (pic de latence corrélé à la charge), et oxidart est aussi une bibliothèque
  embarquée : forker le processus hôte n'est pas légitime.
- **Pas encore fait** (reporté à la boucle maison) : saves automatiques (toutes les N s si
  ≥ M écritures) et AOF. Principes retenus pour l'AOF :
  - `everysec` par défaut (fsync groupé par tour), `always` en option seulement ; un client
    n'est « durable » que si l'ack part après le fsync ;
  - un seul flux d'effets partagé avec la réplication (collecteur d'effets,
    RUNTIME_ARCH §18) ;
  - **ordre sacré** : écrire le snapshot dans un fichier temporaire → fsync → `rename` →
    fsync du dossier → **puis seulement** tronquer l'AOF avant l'offset du snapshot
    (offset stocké dans l'en-tête du dump) ;
  - recovery déterministe : dernier dump valide → rejeu de l'AOF depuis son offset →
    arrêt au dernier enregistrement sain (checksum par entrée).

---

## 4. Ce que le radix apporte

- **UNLINK / DELPREFIX** : `user:` est un vrai sous-arbre ; le détacher rend toutes ses
  clés invisibles d'un coup, la libération se fait ensuite par tranches. Pendant une save,
  le sous-arbre détaché reste visible via l'image T0 de son parent.
- **SCAN** (SCAN_DESIGN.md) : curseur côté serveur rattaché à la dernière clé rendue ;
  ordonné, sans doublon, reprenable, restreint au préfixe, COUNT exact.
- **KRANGE / KREVRANGE** : pages ordonnées par préfixe, nœuds frères préchargés.
- **Reload rapide** : le dump est un parcours DFS, rechargé en masse sans repasser par
  l'insertion (×8 plus rapide que Valkey sur 10 M clés).

---

## 5. Anti-pic de latence

Le tueur de p99.9, c'est un O(n) synchrone caché : rehash, resize, gros memcpy, parcours
complet.
- Pas de structure std qui se redimensionne d'un coup sur le chemin chaud (d'où valart).
- Slabs : réservation virtuelle d'un coup (`mmap`), pas de realloc qui copie ; THP pour le
  TLB, pre-fault minimal.
- Travaux longs : par tranches (§1.2), avec une part de CPU plafonnée sous charge.
- Compaction des slabs et GC (prévus) : gelés pendant une save (ils déplacent des idx).

---

## 6. Checklist de revue

Avant de valider du code :
- [ ] Aucun lock, atomique de synchronisation, `Arc`, thread partagé ni `fork()`.
- [ ] Rien de daté (borrow, idx de slab, pointeur, référence vers l'anneau) ne survit à un
      `.await` ou à la fin d'un tour sans re-résolution (§1.1).
- [ ] Les curseurs (SCAN, jobs) sont des clés, jamais des idx.
- [ ] Toute mutation ou libération d'un nœud oxidart / valart passe par son point de
      passage (barrière de save, SAVE_DESIGN §3) ; aucune libération pendant une save hors
      liste différée.
- [ ] Aucune opération O(n) synchrone sur une grande structure (§5) : sinon tranches.
- [ ] Pour une valeur partagée : décrément de refcount, jamais de libération aveugle.
- [ ] Fichiers : temporaire → fsync → `rename` → fsync du dossier.
- [ ] Entrées réseau : toute longueur lue est bornée et vérifiée sans débordement
      (`checked_*`) ; une donnée client ne doit jamais pouvoir faire paniquer
      (`panic = "abort"` en release : un panic arrête le serveur).
- [ ] Tests : `RUST_TEST_THREADS=1` (statics globaux, `.cargo/config.toml`) ; benchs et
      tests lourds sous `systemd-run --scope -p MemoryMax=…`.
