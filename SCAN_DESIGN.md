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

Curseur envoyé : `gen × max + idx + 1` (`max` fixé au démarrage). Il reste
petit (peu de chiffres décimaux sur le réseau) tant que la table et les
générations le sont, ne vaut jamais 0 et reste ≤ `i64::MAX` (la génération
reboucle avant) pour les clients qui le lisent en signé. Un entrelacement PDEP
donnerait des tailles comparables sans dépendre de BMI2.

Curseur inconnu, terminé ou repris (génération différente) :
`ERR invalid or expired cursor`. Seul écart avec Redis, qui accepte tout
curseur ; repartir de 0 ferait boucler un client lent indéfiniment.

## Configuration

| Variable | Défaut | Effet |
|---|---|---|
| `RADIXOX_SCAN_IDLE_SECS` | `60` | inactivité après laquelle un curseur peut être repris |
| `RADIXOX_SCAN_MAX_CURSORS` | `65536` | curseurs actifs au plus (1 ≤ n < 2³²) |

`INFO` expose `scan_cursors` (curseurs actifs).

## Écarté / reporté

- **BSCAN** (attendre un slot libre) : l'expiration ne produit aucun
  événement, il faudrait file d'attente + minuteur, et l'infrastructure des
  commandes bloquantes n'existe pas encore. Côté client natif : retry avec
  backoff sur l'erreur de plafond. À reconsidérer avec BLPOP.
- Curseur = clé brute envoyée au client : incompatible avec les clients qui
  parsent un entier.
