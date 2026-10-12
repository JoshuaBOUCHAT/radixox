# TX_DESIGN — Transactions atomiques à conditions (`MULTI ATOMIC`)

> **Statut : design, rien d'implémenté.** Décisions de la session du 2026-10-12.
> Dépend de la boucle io_uring maison (`RUNTIME_ARCH.md`) pour les connexions parquées et
> le commit groupé, et du flux d'effets (AOF / réplication) pour la durabilité. Les étapes 1
> à 4 (§12) n'en dépendent pas.

---

## 0. Résumé

```
MULTI ATOMIC
GET user:10:role             ← lecture = condition : vide → abandon, rien n'est écrit
DELPREFIX user:12:           ← suppression d'un sous-arbre en O(profondeur), annulable
SET audit:42 "…"
EXEC [DURABLE]
→ succès  : tableau des résultats (valeurs lues comprises), un seul aller-retour
→ abandon : -TXABORT 0 GET user:10:role : clé absente     (rien n'est appliqué)
```

| Propriété | Comment |
|---|---|
| Isolation | gratuite : un seul thread, une transaction s'exécute d'un bloc dans un tour |
| Atomicité | exécution dans l'ordre + journal d'annulation logique (par clé, pas par nœud) |
| Conditions | toute lecture qui revient vide fait échouer la transaction ; `GET?` lit sans conditionner |
| Diagnostic | l'abandon nomme la commande fautive (index, commande, raison) |
| Durabilité | option `DURABLE` : un enregistrement BEGIN…COMMIT dans le flux d'effets, `fsync` groupé par tour |
| Compatibilité | `MULTI` / `EXEC` sans `ATOMIC` gardent exactement la sémantique Redis |

---

## 1. Pourquoi

Ce que les utilisateurs de Redis écrivent aujourd'hui en Lua ou avec une boucle `WATCH` :

| Besoin | Aujourd'hui (Redis) | Ici |
|---|---|---|
| Agir seulement si une condition tient (droits, session valide, référence existante) | script Lua `if redis.call('GET', …) then … end` | la lecture *est* la condition |
| Idempotence (« seulement si pas encore traité ») | Lua, ou `SET NX` + logique client | `NOTEXISTS payment:xyz:done` |
| Compare-and-set sur plusieurs clés | `WATCH` + retry sous contention | `EXPECT k v` dans la transaction |
| Supprimer tout un namespace (cache d'un utilisateur, d'un tenant) | `SCAN` + `DEL` : lent, non atomique | `DELPREFIX p`, atomique, O(profondeur) |
| Tout ou rien si une commande échoue | impossible : les autres commandes s'appliquent | rollback, commande fautive nommée |
| Durabilité d'une opération précise | `appendfsync always` pour **tout** le serveur | `DURABLE` au cas par cas, `fsync` groupé |

Classement de la valeur réelle attendue (discussion du 2026-10-12) : `DELPREFIX` et les
lectures-conditions d'abord (elles remplacent le plus de Lua en prod), puis `EXPECT`, puis
`DURABLE` (utile à ceux qui utilisent le serveur comme base principale), puis le rollback sur
erreur d'exécution (souvent un bug, mais gratuit une fois le reste en place).

---

## 2. Syntaxe

### 2.1 Bloc

```
MULTI ATOMIC          ouvre une transaction atomique (MULTI seul = sémantique Redis)
<commandes…>          mises en file, réponse +QUEUED (erreur de parse → EXECABORT, comme Redis)
EXEC                  exécute ; EXEC DURABLE : répond après fsync (§8)
DISCARD               abandonne sans rien exécuter
```

### 2.2 Lectures-conditions

Dans un bloc `ATOMIC`, **une lecture qui revient vide fait échouer la transaction**.

| Commande | « Vide » (= échec) |
|---|---|
| `GET k`, `HGET k f`, `ZSCORE k m` | absente (ou expirée) |
| `MGET k…`, `HMGET k f…` | au moins une absente |
| `EXISTS k…` | au moins une absente |
| `SISMEMBER k m` | 0 |
| `KRANGE p from n` | aucune entrée |
| `HGETALL k`, `SMEMBERS k` | clé absente |

Un mauvais type (`GET` sur un hash) est une erreur d'exécution : abandon aussi (§4.4).

**Lecture sans condition** : suffixe `?` sur le nom de la commande (`GET? k`, `HGET? k f`,
`KRANGE? …`). Renvoie `nil` / vide sans faire échouer la transaction.

### 2.3 Conditions explicites

| Commande | Réussit si | Rôle |
|---|---|---|
| `NOTEXISTS k…` | toutes absentes | idempotence, création exclusive |
| `EXPECT k v` | la valeur string de `k` vaut `v` | compare-and-set |
| `EXPECT k f v` | le champ `f` du hash `k` vaut `v` | idem sur un hash |

Plus tard, si le besoin se confirme : `EXPECT k TYPE t`, comparaisons numériques
(`EXPECTGT k n`…), `EXPECT k TTL>…`.

### 2.4 `DELPREFIX`

`DELPREFIX p` supprime toutes les clés qui commencent par `p` (tous types). Utilisable aussi
hors transaction. Réponse : `1` si un sous-arbre a été détaché, `0` sinon (le compte exact
demanderait de parcourir le sous-arbre, voir §7.3). Dans un bloc `ATOMIC`, ce n'est pas une
condition : `0` ne fait pas échouer la transaction.

---

## 3. Réponses

- **Succès** : tableau des résultats, comme `EXEC` (valeurs des lectures comprises). Un seul
  aller-retour donne les données, la vérification et l'action.
- **Abandon** : une erreur, rien n'est appliqué (et rien n'est écrit dans le flux d'effets) :
  ```
  -TXABORT <index> <commande> : <raison>
  -TXABORT 0 GET user:10:role : clé absente
  -TXABORT 2 EXPECT user:10:role : "user" ≠ "admin"
  -TXABORT 3 INCR stock:7 : WRONGTYPE
  ```
  L'index est celui de la commande dans le bloc (base 0). Les arguments ne sont pas recopiés
  dans le message au-delà du nom de clé (pas de fuite de valeurs dans les logs).

---

## 4. Sémantique

### 4.1 Isolation

Une transaction s'exécute **d'un bloc**, dans un seul tour de boucle, sans rendre la main :
aucune autre commande ne s'intercale. Sérialisable par construction.

### 4.2 Atomicité

Exécution dans l'ordre. Au premier échec (condition ou erreur), le journal d'annulation est
rejoué à l'envers (§6), puis la réponse d'abandon est envoyée.

**Cas courant sans aucun rollback** : les conditions sont presque toujours placées avant les
écritures. Un échec survient alors avant toute modification : le journal est vide, l'abandon
ne coûte que les lectures faites.

### 4.3 Lecture de ses propres écritures

Une commande voit les effets des commandes précédentes de la même transaction (exécution
réelle, pas de simulation) : `SET k x` puis `GET k` renvoie `x` ; `DELPREFIX user:12:` puis
`GET user:12:name` est vide (et fait donc échouer la transaction, sauf `GET?`).

### 4.4 Erreurs

| Erreur | Moment | Effet |
|---|---|---|
| Parse, commande inconnue, mauvais nombre d'arguments | mise en file | `EXECABORT` au `EXEC`, comme Redis |
| Condition non remplie | exécution | rollback, `TXABORT` |
| Erreur d'exécution (`WRONGTYPE`, entier invalide, dépassement) | exécution | rollback, `TXABORT` |
| Commande interdite dans `ATOMIC` (§4.6) | mise en file | `EXECABORT` |

### 4.5 Expiration

- `now` est figé pour toute la transaction (une lecture d'horloge par tour, `RUNTIME_ARCH.md`
  §9) : une clé ne peut pas expirer au milieu d'une transaction.
- Une clé expirée est **absente** pour les conditions.
- Pas de suppression à la volée pendant la transaction : les expirées croisées sont notées et
  purgées **après** le commit ou le rollback (même mécanisme que `mget_ref` +
  `purge_expired`, commit 128f393). Un rollback n'a donc jamais à « ressusciter » une clé
  purgée.

### 4.6 Commandes interdites dans `ATOMIC`

- **Commandes parquées** (KEYS, UNLINK, BLPOP, SAVE, BGSAVE…) : elles ne tiennent pas dans
  un tour.
- **FLUSHDB** : v1 interdit. (Faisable plus tard comme un `DELPREFIX ""`, détachement de la
  racine.)
- **SUBSCRIBE** et assimilés.
- **PUBLISH** est permis mais **différé** : les messages ne partent qu'au commit, jamais sur un
  abandon. Idem pour les futures notifications par préfixe.

### 4.7 Taille

Une transaction bloque la boucle le temps de son exécution, comme dans Redis. Plafond
configurable du nombre de commandes par transaction (`tx-max-commands`, défaut 10 000).
`DELPREFIX` reste en O(profondeur) quelle que soit la taille du sous-arbre (§7).

---

## 5. Exécution

```
exec_atomic(tx):
    undo = []                      // journal, réutilisé d'une transaction à l'autre
    expired = []                   // expirées croisées, purgées à la fin
    for (i, cmd) in tx:
        match run(cmd, &mut undo, &mut expired):
            Ok(résultat)  → résultats.push(résultat)
            Err(raison)   → rollback(undo); purge(expired); return TXABORT(i, cmd, raison)
    commit(undo)                   // libère ce qui était gardé pour un éventuel rollback
    purge(expired)
    flux d'effets ← BEGIN, effets, COMMIT   (§8)
    return résultats
```

- `run` est le chemin d'exécution normal de la commande, avec un crochet qui pousse l'entrée
  d'annulation **avant** chaque mutation.
- **Écarté : passe de validation séparée** (lecture seule + table « clé → état après la
  transaction »). Inutile en v1 : l'exécution dans l'ordre abandonne déjà avant toute écriture
  dans le cas courant, et la lecture de ses propres écritures vient gratuitement.

### 5.1 Préchargement de tout le bloc

Au `EXEC`, **toutes les clés du bloc sont connues** avant la première exécution : c'est le
cas idéal pour le préchargement, mieux qu'un pipeline (dont la fenêtre se découvre en lisant).

```
exec_atomic(tx):
    clés = toutes les clés et tous les préfixes du bloc (MGET, EXISTS : chaque clé ;
           DELPREFIX : le chemin du préfixe ; SET d'une clé neuve : son chemin)
    found = descend_batch(clés)        // miss superposés, valeurs préchargées
    époque0 = art.structure_epoch
    … exécution dans l'ordre (§5) …
```

- **Une seule descente froide pour tout le bloc** : les chemins se chevauchent au lieu de
  s'enchaîner. Un bloc `EXPECT` + `DELPREFIX` + `SET` coûte à peu près une descente froide
  plus du calcul en cache, donc **moins qu'un `GET` suivi d'un `SET` séparés** (deux
  descentes froides l'une après l'autre). Ordre de grandeur attendu, d'après MGET (commit
  128f393, ×2,5 à ×3,5 par clé en lot) : une transaction de 3–4 commandes à peine plus chère
  qu'un GET seul. À mesurer.
- **Clé neuve** (`SET` d'une clé absente) : `descend_batch` s'arrête au dernier nœud existant
  du chemin et le précharge ; c'est justement le nœud que l'insertion va découper ou compléter.
- **Réutilisation des idx** : tant qu'aucune mutation structurelle n'a eu lieu
  (`structure_epoch == époque0`, compteur de `RUNTIME_ARCH.md` §17.2), une lecture utilise
  directement l'idx trouvé, sans seconde descente. Les conditions étant en tête de bloc, c'est
  le cas courant. Après une mutation structurelle : descente normale, dans un cache chaud.
- **Abandon précoce** : si une condition en tête échoue, le préchargement des clés suivantes
  est perdu. Coût accepté (des prefetchs, aucune écriture), et l'abandon reste bien plus
  rapide qu'un aller-retour client pour vérifier la condition.
- Même mécanisme pour un `MULTI` / `EXEC` classique (sans `ATOMIC`) : le gain de vitesse ne
  dépend pas du rollback.

---

## 6. Journal d'annulation (logique, par clé)

### 6.1 Principe

Avant chaque mutation, on note **l'état antérieur de ce qui va changer**, au niveau des clés
et des champs, pas des nœuds. Le rollback rejoue ces états à l'envers avec les opérations
normales (`set`, `del`, `hset`…). La forme de l'arbre redevient identique : la forme
canonique ne dépend que du contenu (test `purge_nested_expired_recompresses`, commit 128f393,
avec `assert_canonical` : même nombre de nœuds qu'un arbre neuf).

Coût proportionnel à ce que la transaction modifie, jamais à la taille de la base. Pas
d'`unsafe`. Fonctionne pendant une save : les écritures du rollback passent par la barrière
comme toutes les autres.

### 6.2 Entrées

```rust
enum Undo {
    /// État complet d'une clé avant mutation : None = absente.
    Key { key: SharedByte, old: Option<(Value, Option<u64>)> },   // valeur clonée (rc+1) + échéance absolue
    /// TTL seul (EXPIRE, PERSIST) : la valeur n'a pas bougé.
    Ttl { key: SharedByte, old_exp: Option<u64> },
    /// Champ de hash : None = absent.
    HashField { key: SharedByte, field: SharedByte, old: Option<SharedByte> },
    /// Appartenance à un set : true = était présent.
    SetMember { key: SharedByte, member: SharedByte, was: bool },
    /// Score d'un membre de zset : None = absent.
    ZMember { key: SharedByte, member: SharedByte, old: Option<f64> },
    /// Conteneur détaché par DEL (ou écrasé par SET) : gardé vivant, pas détruit.
    Container { key: SharedByte, tag: Tag, idx: u32, exp: Option<u64> },
    /// Sous-arbre détaché par DELPREFIX.
    Subtree { prefix: SharedByte, root: u32 },
}
```

| Commande | Entrée | Rollback |
|---|---|---|
| `SET`, `SETNX`, `SETEX`, `MSET`, `INCR*`, `DECR*` | `Key` | restaurer la valeur et l'échéance, ou `del` |
| `DEL` d'une string | `Key` | `set` avec l'échéance d'origine |
| `DEL` d'un hash / set / zset | `Container` | réinstaller le conteneur (son idx), sans copie |
| `EXPIRE`, `PEXPIRE`, `PERSIST` | `Ttl` | remettre l'échéance |
| `HSET`, `HDEL`, `HINCRBY` | `HashField` par champ | `hset` de l'ancienne valeur ou `hdel` |
| `SADD`, `SREM`, `SPOP` | `SetMember` par membre | `sadd` / `srem` inverse |
| `ZADD`, `ZREM`, `ZINCRBY` | `ZMember` par membre | remettre le score ou retirer |
| Création d'un conteneur par `HSET`/`SADD`/`ZADD` | `Key { old: None }` en plus | `del` du conteneur entier |
| `DELPREFIX` | `Subtree` | greffe (§7.2) |

**Détacher au lieu de détruire** : un `DEL` sur un hash d'un million de champs ne copie rien.
La valeur (idx du conteneur) est retirée du nœud et gardée dans le journal ; elle n'est
détruite qu'au commit (par tranches si elle est grosse, comme UNLINK). Le rollback la
réinstalle telle quelle.

### 6.3 Commit

Le journal est vidé : les clones (rc+1) sont relâchés, les conteneurs et sous-arbres détachés
partent en libération par tranches (job de fond, `RUNTIME_ARCH.md` §8). Coût du commit hors
libérations : proportionnel au nombre d'entrées.

---

## 7. `DELPREFIX` : détacher, libérer plus tard, greffer en cas de rollback

### 7.1 Détachement

1. Trouver la racine du sous-arbre des clés qui commencent par `p` (`locate_prefix`, déjà
   écrit pour `krange`). Absente → réponse `0`.
2. La couper de son parent (`remove_child`, via la barrière) puis `try_recompress` sur le
   parent : l'arbre principal est aussitôt dans sa forme canonique sans le sous-arbre.
3. Hors transaction : le sous-arbre part à la libération par tranches (machinerie d'UNLINK,
   `free_chunk`). Dans une transaction : entrée `Subtree` dans le journal, libération au
   commit.

Coût : O(longueur du préfixe), quelle que soit la taille du sous-arbre.

Pendant une save : mêmes règles qu'UNLINK (`SAVE_DESIGN.md` §2.F). La coupe passe par la
barrière (une capture du parent) ; le sous-arbre reste visible dans la vue T0 et il est
dumpé ; sa libération attend la fin de la save.

### 7.2 Greffe (rollback)

Le rollback rejoue le journal à l'envers : au moment de la greffe, l'arbre est revenu à son
état **juste après le détachement**. Mais `try_recompress` a pu fusionner le parent avec son
seul enfant restant : on ne peut pas remettre un pointeur à sa place.

La greffe est donc une vraie insertion de sous-arbre à une clé :
1. descendre vers la clé complète de la racine détachée (préfixe + sa compression) ;
2. comme une insertion : découper le nœud où le chemin diverge (`split_node`) si besoin ;
3. rattacher la racine détachée comme enfant au bon radix, en ajustant sa compression à la
   nouvelle frontière ;
4. remettre `parent_idx` / `parent_radix` de la racine.

Point dur, à tester en premier (§11) : préfixes qui tombent au milieu d'une compression,
préfixe égal à une clé existante, parent racine.

### 7.3 Compte exact

`DELPREFIX p COUNT` : compte les valeurs du sous-arbre avant de répondre (parcours
O(sous-arbre), à réserver aux petits préfixes). Sans `COUNT`, la réponse est `0`/`1`. Le
compte exact est aussi disponible après coup, en fin de libération (comme UNLINK), pour les
logs.

---

## 8. Durabilité (`EXEC DURABLE`)

Dépend du **flux d'effets** (le même pour l'AOF et la réplication, voir la discussion
réplication du 2026-10-12).

- Une transaction commitée s'écrit comme **un seul enregistrement** :
  `BEGIN <id>`, effets, `COMMIT <id>`. Une transaction abandonnée n'écrit rien.
- **Effets, pas commandes** : TTL relatifs réécrits en échéances absolues (`PEXPIREAT`),
  `SPOP` → `SREM` des membres tirés, `INCRBYFLOAT` → `SET` du résultat. `DELPREFIX` reste
  `DELPREFIX` (déterministe). Les lectures et conditions ne s'écrivent pas.
- **Relecture** : un enregistrement sans `COMMIT` (fichier tronqué) est ignoré en entier.
  La durabilité est atomique elle aussi.
- **Commit groupé** : la connexion d'une transaction `DURABLE` est parquée
  (`RUNTIME_ARCH.md` §7) ; **un seul `fsync` par tour** pour toutes les transactions durables
  du tour ; au CQE du `fsync`, les réponses partent. Latence = un `fsync` (des dizaines de µs
  sur NVMe) ; débit = des centaines de transactions par `fsync`.
- Sans `DURABLE` : l'enregistrement va dans le flux (si l'AOF est actif) sans attendre.

---

## 9. Réplication

L'enregistrement BEGIN…COMMIT part tel quel vers les répliques. Une réplique met les effets
en tampon jusqu'au `COMMIT` et les applique d'un bloc : jamais d'état intermédiaire visible,
même si le maître meurt au milieu de l'envoi.

---

## 10. API bibliothèque (oxidart embarqué)

```rust
let res = art.transaction(|tx| {
    let role = tx.get(b"user:10:role")?;              // vide → Err(Abort::Missing)
    if role != "admin" { return Err(Abort::user("pas admin")); }
    tx.delprefix(b"user:12:")?;
    tx.set(b"audit:42", "…")?;
    Ok(())
});
// Err → tout est annulé, avec l'étape fautive
```

- Même moteur : journal d'annulation, détachement, purge des expirées à la fin.
- `tx.get` rend une référence valable pendant la transaction (`RangeVal`), ou une valeur
  possédée sur demande.
- Une panique dans la closure : à trancher (rollback via un garde `Drop`, ou état déclaré
  corrompu). Défaut proposé : garde `Drop` qui annule.

---

## 11. Tests

- **Modèle de référence** : transactions aléatoires (mélange de lectures-conditions,
  écritures, `DELPREFIX`, erreurs forcées) contre une `BTreeMap` ; après un abandon, l'arbre
  doit être **identique à l'état d'avant** : listing complet (`krange`), nombre de nœuds,
  `assert_canonical`.
- **Greffe** : `DELPREFIX` puis rollback sur des préfixes au milieu d'une compression, égaux à
  une clé, sous la racine, avec recompression du parent ; comparer à l'arbre d'avant.
- **Conteneurs** : `DEL` d'un gros hash puis rollback (aucune copie : même idx réinstallé).
- **Pendant une save** : transactions commitées et abandonnées pendant un BGSAVE ; le dump
  reste la vue T0 exacte.
- **Expiration** : conditions sur des clés qui expirent pile au `now` de la transaction ;
  purge après abandon.
- **Durabilité** : flux tronqué au milieu d'un enregistrement → transaction ignorée à la
  relecture.
- ASan sur les tests de rollback (conteneurs détachés, sous-arbres greffés).

---

## 12. Ordre de réalisation

1. **`DELPREFIX` hors transaction** : détachement + libération par tranches. Utile seul,
   faisable sur le serveur monoio actuel.
2. **`MULTI` / `EXEC` / `DISCARD` Redis classiques** (compatibilité clients), avec le
   préchargement de tout le bloc au `EXEC` (§5.1) dès cette étape.
3. **`ATOMIC` sur les strings** : lectures-conditions, `GET?`, `NOTEXISTS`, `EXPECT`,
   journal `Key` / `Ttl`, `TXABORT`.
4. **Greffe** (rollback de `DELPREFIX`), puis journal des conteneurs (`HashField`,
   `SetMember`, `ZMember`, `Container`). API bibliothèque `art.transaction`.
5. Après la boucle maison et le flux d'effets : `EXEC DURABLE` avec commit groupé.
6. Avec la réplication : enregistrements BEGIN…COMMIT transmis et appliqués d'un bloc.

---

## 13. Écarté

| Option | Raison |
|---|---|
| Rollback par la barrière COW de la save (images de nœuds) | `begin` copie le bitmap de tout le slab (trop cher par transaction) ; restaurer une image demande de reconstruire `Childs`/`Overflow`, `parent_idx`, tags TTL ; valart sans refcount dans les images (risque d'use-after-free, cf. SAVE_DESIGN §2.B) ; ZSet/List sans barrière ; une seule barrière, en conflit avec BGSAVE. La barrière reste l'outil des **vues cohérentes** (snapshot, SCAN cohérent, synchro de réplique). |
| Passe de validation séparée avant application | inutile en v1 (§5) |
| `WATCH` comme mécanisme principal | retry optimiste pénible sous contention ; les conditions dans la transaction le remplacent (`WATCH` reste à implémenter pour la compatibilité) |
| Scripts Lua | gros chantier, et ce design couvre l'essentiel de leurs usages réels |
| Changer la sémantique de `MULTI` / `EXEC` par défaut | casserait les clients qui comptent sur la sémantique Redis |

---

## 14. Questions ouvertes

- Syntaxe de la lecture sans condition : suffixe `?` (`GET? k`) ou préfixe `OPTIONAL GET k` ?
  Le suffixe est plus court ; vérifier qu'aucun client ne refuse un nom de commande avec `?`.
- `DELPREFIX` : réponse `0`/`1` par défaut, ou compte exact quand le sous-arbre est petit ?
- Plafond `tx-max-commands` : valeur par défaut, et plafond en octets de journal ?
- Panique dans `art.transaction` : rollback automatique ou état déclaré corrompu ?
- `FLUSHDB` atomique (détachement de la racine) : utile ?
