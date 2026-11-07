# Bitset : type natif, B-tree de nœuds de 64 o sur slab global

Document de conception (2026-11-03). Code : `radixox-lib/src/bitset.rs`, forme clairsemée
faite le 2026-11-07 (§9) ; valeur oxidart à deux formes et commandes faites le 2026-11-07
(§10). Lire d'abord `CLAUDE.md` et `ARCHITECTURE.md` ; mémoire et
GC : `MEMORY_ARCH.md` ; save : `SAVE_DESIGN.md`.

## 0. Résumé des décisions

| Sujet | Décision |
|---|---|
| Sémantique | **type à part, pas une string Redis** : `GET` & co. → `WRONGTYPE`. Remplace COMMANDS_PLAN §5 (« des strings ») |
| Forme clairsemée | B-tree des **offsets des bits à 1** (`u32`) ; tout bit absent vaut 0 |
| Nœud | 64 o, `#[repr(C)]`, en-tête `u32` (`len` 3 bits, `is_leaf` 1 bit, `id` de l'arbre 28 bits), 7 clés, 8 enfants |
| Index | `BtreeNodeIdx(u32)` = n° de bloc (28 bits) + case dans le bloc (4 bits), **deux valeurs sentinelles** ; double indirection gardée (§3) |
| Slab | **global à tous les bitsets** : blocs de 1 Kio (16 nœuds), une table `ptrs`, masque d'occupation `u16` par bloc, bitset des blocs non pleins, allocation dans le bloc le plus plein |
| Racine | table globale `arbres[id] = (racine, hauteur, nb de bits à 1)` ; la valeur oxidart ne porte que l'`id` |
| GC | marquage des blocs peu remplis + balayage linéaire ; parent retrouvé en **redescendant depuis la racine de l'`id`** avec `keys[0]` (§5) |
| Clairsemé / dense | forme **choisie feuille par feuille** (liste d'offsets ou 512 bits), seuil unique, sans arrêt du monde (§6) |
| Insertion | **sans branche** : AVX2 (deux moitiés de 32 o), AVX-512 (le nœud entier en un `vpermt2d`) ; repli scalaire en `cmov` (§7) |
| Pas maintenant | compteurs de bits par sous-arbre (BITCOUNT sur plage / k-ième bit) : *keep it simple* (§8) |

## 1. Sémantique

- Un bitset est un **type de valeur à part** (comme Hash, Set, ZSet, List). On casse
  volontairement la compatibilité Redis, où un bitmap est une string : pas de `GET`,
  `GETRANGE`, `APPEND`, `SETRANGE`, `STRLEN` dessus (`WRONGTYPE`), donc **jamais de
  conversion vers une string**.
- Offsets : `u32` complet (`0 ..= u32::MAX`). **`u32::MAX` est un offset valide** : il ne
  peut pas servir de sentinelle de clé vide (les cases vides se déduisent de `len`).
- Noms et liste des commandes : à fixer (reprendre `SETBIT` / `GETBIT` / `BITCOUNT` /
  `BITPOS` / `BITOP` sur ce type, ou des noms à nous). `BITFIELD` n'a pas de sens naturel
  ici. COMMANDS_PLAN §5 est à revoir en conséquence.
- Le nombre total de bits à 1 est tenu dans `arbres[id]` : `BITCOUNT` sans plage en O(1).

## 2. Le nœud de 64 o

```rust
#[repr(C, align(64))]
struct BtreeNode {
    header: u32,                 // bits 0..3 : len (≤ 7) ; bit 3 : is_leaf ; bits 4..32 : id de l'arbre
    keys: [u32; 7],              // octets 4..32, triées, valides sur 0..len
    child_idx: [BtreeNodeIdx; 8],// octets 32..64, valides sur 0..=len (nœud interne)
}
```

- **`#[repr(C)]` obligatoire** : sans lui Rust peut réordonner les champs, et l'insertion
  vectorielle (§7) suppose l'en-tête en lane 0, les clés en lanes 1..8, les enfants dans
  la deuxième moitié.
- `len` dans les **bits bas** de l'en-tête : l'insertion fait `len += 1` par une addition
  sur la lane 0.
- **`is_leaf`** : peut disparaître si la hauteur est rangée dans `arbres[id]` (toutes les
  feuilles sont à la même profondeur, la descente compte ses niveaux) ; l'`id` passe alors
  à 29 bits. À trancher à l'implémentation.
- **`id` de l'arbre (28 bits, 268 M bitsets)** : fixé à l'allocation, **ne change jamais**
  (une coupe le recopie, RENAME ne touche que la valeur oxidart). C'est ce qui permet au GC
  de retrouver le parent sans pointeur vers le parent (§5).
- Ordre 7 clés / 8 enfants : nombre impair de clés → médiane nette → **coupe préventive à
  la descente** (on coupe tout nœud plein rencontré), donc pas besoin de remonter.
- **À trancher : feuille à 15 clés.** Les feuilles sont ~7/8 des nœuds et leurs 32 o
  d'enfants ne servent à rien. Une forme feuille `header + [u32; 15]` double la densité.
  Coût : deux formes de nœud, donc deux routines d'insertion (§7).
- **À trancher : B-tree (ébauche actuelle) ou B+tree** (clés seulement dans les feuilles,
  séparateurs dans les nœuds internes). Le B+tree simplifie beaucoup la suppression (pas
  d'emprunt/fusion avec remontée de clé du nœud interne). Pas de chaînage des feuilles (pas
  de place) : les parcours dans l'ordre reprennent par redescente avec le dernier offset.

## 3. Index et slab global

```rust
// 28 bits : n° de bloc (index dans `ptrs`), 4 bits : case dans le bloc (16 nœuds = 1 Kio)
struct BtreeNodeIdx(u32);
```

- **Une seule table `ptrs: Vec<NonNull<[BtreeNode; 16]>>` pour tous les bitsets.** Une
  table par arbre donnerait à chaque petit bitset son bloc de 16 nœuds pour 1 utilisé
  (960 o perdus) : exactement les trous qu'on veut éviter.
- **Double indirection gardée (choix du 2026-11-03)** : `ptrs[n° de bloc]` puis la case.
  Raison : ne pas multiplier les réservations de mémoire virtuelle (mmap) par structure ;
  avec AMAC, la lecture de `ptrs` se recouvre dans le pipeline.
- **Deux sentinelles** dans l'espace des index (`u32::MAX` et `u32::MAX - 1`, par exemple
  « pas d'enfant » et l'autre usage) : avec 2²⁸ blocs au plus, `BtreeNodeIdx::new` doit
  vérifier qu'il ne les produit jamais.
- **Occupation** : un masque `u16` par bloc (case vivante ou non), et un bitset global
  « bloc non plein ». **Allocation dans le bloc le plus plein** d'abord, pour laisser les
  blocs peu remplis se vider.
- **Rendre la mémoire au système** : un bloc de 1 Kio vide ne se rend pas seul (pages de
  4 Kio). Les blocs sont découpés dans les blocs de 64 Kio du pool (MEMORY_ARCH.md), qui
  apportent la comptabilité `maxmemory` ; `MADV_DONTNEED` quand une page entière est vide.

## 4. Table des arbres

`arbres[id] = (racine: BtreeNodeIdx, hauteur, nb de bits à 1)`. La valeur oxidart d'un
bitset ne contient que l'`id`.

- Déplacer la racine (GC, coupe de la racine) ne met à jour que `arbres[id]`, jamais oxidart.
- RENAME / MOVE déplacent la valeur oxidart : l'`id` reste le même, rien à toucher dans les nœuds.
- DEL : libérer tous les nœuds (en tranches si gros, comme les écritures bornées) puis l'`id`.
- `id` libres : free list ; la table est elle-même un tableau plat (16 o par entrée environ).

## 5. GC : évacuation par balayage linéaire

Le problème : déplacer un nœud oblige à corriger le seul endroit qui pointe vers lui (case
du parent, ou `arbres[id]` pour une racine).

1. **Déclenchement** : remplissage global (nœuds vivants / cases des blocs alloués) sous un
   seuil, par exemple 70 %. Se lit dans les masques, sans toucher aux arbres.
2. **Marquage** : les blocs à ≤ 4 nœuds vivants sur 16 entrent dans un bitset `à_vider`.
   **On n'alloue plus dedans.**
3. **Balayage linéaire** des blocs marqués, par tranches (~50 µs, comme l'évicteur TTL et la
   tranche GC). Pour chaque case vivante (masque) :
   1. lire l'`id` dans l'en-tête → `arbres[id].racine` ;
   2. **redescendre avec `keys[0]`** : n'importe quelle clé du nœud mène à lui ; s'arrêter
      quand un enfant vaut l'idx du nœud → c'est le parent (si le nœud est la racine,
      c'est `arbres[id]`) ;
   3. recopier le nœud dans une case d'un bloc non marqué, corriger la case du parent,
      libérer l'ancienne case.

   Coût : une descente de `hauteur` niveaux par nœud déplacé, pas de parcours d'arbre entier.
4. **Évacuation au passage** : une descente d'**écriture** (SETBIT et compagnie) qui
   rencontre un enfant dans un bloc marqué le déplace (parent et nœud déjà en cache). Pas
   dans les lectures : elles restent de pures lectures.
5. Un bloc dont le masque tombe à 0 est libéré et démarqué.
6. **Pendant un BGSAVE : évacuation suspendue** (elle déplace des idx, comme le compactage
   des slabs, ARCHITECTURE.md). Plus tard éventuellement : passer par la barrière de SAVE_DESIGN.

### Écarté

- **Pointeur vers le parent dans le nœud** : 4 o, et chaque coupe réécrit le parent des
  enfants passés dans la nouvelle moitié (4 accès au hasard). L'`id` d'arbre, lui, ne
  change jamais.
- **Fusion de blocs à masques disjoints** (allocateur *Mesh*, Powers et al., PLDI 2019 :
  `ptrs[B] = ptrs[A]` sans bouger aucun index) : rien à corriger chez les parents, mais deux
  masques par bloc (logique / physique) et une allocation compliquée. Écarté pour garder
  l'algorithme simple.
- **Parcours d'arbre complet pour retrouver les nœuds d'un bloc** : inutile depuis que le
  nœud porte son `id`.

## 6. Clairsemé / dense, feuille par feuille

Coût de la forme clairsemée : 4 o par bit à 1 au minimum, ~6 o avec des feuilles remplies
à 70 %, contre 1 bit en dense. **Le dense gagne dès ~2 % de bits à 1.**

- **Choix de la forme par feuille, pas pour tout le bitset** (à la Roaring) : une feuille
  couvre un morceau d'offsets et est soit une **liste d'offsets** (clairsemée), soit un
  **bitmap de 512 bits** (dense). Les deux font 64 o : une seule classe de slab.
- La bascule ne touche qu'**un nœud de 64 o** : pas d'arrêt du monde, pas de bascule
  incrémentale à écrire, et une oscillation autour du seuil ne coûte que 64 o recopiés →
  **un seuil unique est acceptable** (sans hystérésis).
- Clé d'un morceau dense dans le parent : `offset >> 9`.

**Point ouvert : une feuille dense de 512 bits remplit les 64 o, il ne reste pas de place
pour l'en-tête** (`id` pour le GC, forme de la feuille). Options :
- 480 bits utiles + en-tête de 4 o : morceaux de 480 bits, division au lieu d'un décalage ;
- 256 bits + en-tête : simple, mais densité divisée par deux ;
- **en-tête déporté** : table parallèle aux cases (`id` + forme, 4 o par case, écrite à
  l'allocation seulement, lue par le GC) ; les 64 o restent entièrement utiles et la forme
  peut aussi venir du parent. Probablement la meilleure option, à confirmer.

La variante `Dense` de l'ébauche (bitset entier en dense) n'est plus nécessaire si les
feuilles denses existent ; à garder éventuellement pour un bitset presque plein partout.

## 7. Insertion sans branche

La position d'insertion tombe au hasard : une boucle scalaire + `copy_within` (qui devient
un `memmove`) se paie en mauvaises prédictions. La disposition du §2 se prête au vectoriel :
**en-tête + 7 clés = 32 o, 8 enfants = 32 o**, et le nœud entier = un registre de 512 bits.

**Recherche et insertion partagent le calcul** : `lt` = masque des clés `< x` ;
`pos = 1 + popcnt(lt)` est la lane d'insertion, et en descente `popcnt` donne l'enfant à
suivre. Une clé insérée en lane `pos` (indice de clé `pos - 1`) met son nouvel enfant de
droite à l'**indice d'enfant `pos`** : les mêmes masques servent aux deux moitiés.

On n'insère jamais dans un nœud plein (coupe préventive) : `k6` et `c7` sont libres, le
décalage peut écraser la dernière lane de chaque moitié.

### AVX2 (binaire portable x86-64-v3)

Esquisse, non compilée.

```rust
#[target_feature(enable = "avx2")]
unsafe fn insert_key(node: *mut BtreeNode, x: u32, len: u32) -> Option<i32> {
    use core::arch::x86_64::*;
    let p = node as *mut __m256i;                 // [en-tête, k0..k6]
    let v = _mm256_load_si256(p);
    let lane = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
    let xv = _mm256_set1_epi32(x as i32);
    let valid = _mm256_and_si256(                 // lanes 1..=len
        _mm256_cmpgt_epi32(lane, _mm256_setzero_si256()),
        _mm256_cmpgt_epi32(_mm256_set1_epi32(len as i32 + 1), lane),
    );
    let eq = _mm256_and_si256(_mm256_cmpeq_epi32(v, xv), valid);
    if _mm256_testz_si256(eq, eq) == 0 { return None; } // déjà à 1
    // v < x non signé (AVX2 n'a pas de cmpgt non signé) : min(v, x) == v, v != x exclu plus haut
    let lt = _mm256_and_si256(_mm256_cmpeq_epi32(_mm256_min_epu32(v, xv), v), valid);
    let pos = 1 + (_mm256_movemask_ps(_mm256_castsi256_ps(lt)) as u32).count_ones() as i32;
    let pv = _mm256_set1_epi32(pos);
    let after = _mm256_cmpgt_epi32(lane, pv);
    let at = _mm256_cmpeq_epi32(lane, pv);
    let shifted = _mm256_permutevar8x32_epi32(v, _mm256_setr_epi32(0, 0, 1, 2, 3, 4, 5, 6));
    let mut out = _mm256_blendv_epi8(v, shifted, after);
    out = _mm256_blendv_epi8(out, xv, at);
    out = _mm256_add_epi32(out, _mm256_setr_epi32(1, 0, 0, 0, 0, 0, 0, 0)); // len += 1
    _mm256_store_si256(p, out);
    Some(pos)
}
```

Nœud interne : charger les octets 32..64, même `vpermd`, mêmes `after` / `at` avec le
nouvel enfant, réécrire. Dans une feuille, on peut le faire quand même (lanes inutiles) ou
tester `is_leaf` (branche très prévisible).

### AVX-512 (natif, 7600X) : le nœud entier en une permutation

Clés et enfants dans un seul `zmm` (lanes 0..8 = en-tête + clés, 8..16 = enfants). Une
seule `vpermt2d` (`_mm512_permutex2var_epi32`) déplace tout : la table d'indices vaut
`lane`, sauf `lane - 1` sur les lanes à décaler, `16` (= `x`) en `pos` et `17` (= nouvel
enfant) en `8 + pos`. Les masques se calculent sur des entiers, sans table :

- trous : `(1 << pos) * 0x0101` ;
- lanes décalées : `((0xFF << (pos + 1)) & 0xFF) * 0x0101`.

Esquisse, non compilée.

```rust
#[target_feature(enable = "avx512f")]
unsafe fn insert_node(node: *mut BtreeNode, x: u32, child: u32, len: u32) -> bool {
    use core::arch::x86_64::*;
    let v = _mm512_load_si512(node.cast());
    let lane = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let xv = _mm512_set1_epi32(x as i32);
    let valid: __mmask16 = ((1u16 << len) - 1) << 1;             // lanes 1..=len
    if _mm512_mask_cmpeq_epu32_mask(valid, v, xv) != 0 { return false; }
    let pos = 1 + _mm512_mask_cmplt_epu32_mask(valid, v, xv).count_ones();
    let hole: u16 = (1u16 << pos) * 0x0101;
    let shift: u16 = ((0xFFu16 << (pos + 1)) & 0xFF) * 0x0101;
    let mut idx = _mm512_mask_sub_epi32(lane, shift, lane, _mm512_set1_epi32(1));
    idx = _mm512_mask_mov_epi32(idx, hole & 0x00FF, _mm512_set1_epi32(16));
    idx = _mm512_mask_mov_epi32(idx, hole & 0xFF00, _mm512_set1_epi32(17));
    let extra = _mm512_castsi128_si512(_mm_setr_epi32(x as i32, child as i32, 0, 0));
    let mut out = _mm512_permutex2var_epi32(v, idx, extra);   // tout le nœud d'un coup
    out = _mm512_mask_add_epi32(out, 1, out, _mm512_set1_epi32(1)); // len += 1
    _mm512_store_si512(node.cast(), out);
    true
}
```

Vérification des lanes (`pos` ∈ 1..=7) : lanes `< pos` inchangées, `pos` ← `x`,
`pos+1..8` ← clé précédente (la lane 7 reçoit `k5`), `8..8+pos` inchangées (`c0..c(pos-1)`),
`8+pos` ← nouvel enfant, au-delà ← enfant précédent (la lane 15 reçoit `c6`). Avec
`pos = 7`, `shift` vaut 0 côté clés et côté enfants.

Variante : `_mm512_mask_expand_epi32` pose aussi les éléments dans les lanes d'un masque,
mais la frontière clés/enfants décale les enfants d'une lane de trop ; il faut d'abord
compresser la lane 7, donc deux instructions. `vpermt2d` reste la plus simple.

### Repli et mesure

- Sans AVX2 : boucle scalaire déroulée de 7 tours,
  `keys[j] = if j > i { keys[j - 1] } else { keys[j] }`, que le compilateur transforme en
  `cmov` (sans branche non plus).
- Choix à l'exécution ou à la compilation selon le binaire (`target-cpu=native` contre la
  v3 portable, CLAUDE.md).
- **Mesurer avant de garder** : `perf stat` (instructions, mauvaises prédictions par
  SETBIT) contre la version scalaire, bench court. Le gain ne compte que si AMAC recouvre
  l'accès DRAM au nœud.
- La feuille à 15 clés (§2), si retenue, a sa propre routine : 16 lanes = en-tête + 15
  clés, même principe sans la moitié enfants.

## 8. Pas maintenant

- **Compteurs de bits à 1 par sous-arbre** (BITCOUNT sur plage en O(log n), k-ième bit à
  1) : 24 bits débordent pour un gros arbre, et il faudrait les compteurs de chaque enfant
  pour que ce soit vraiment utile, ce qui ne rentre pas dans 64 o. *Keep it simple* :
  BITCOUNT avec plage parcourt les feuilles de la plage. Ajoutable plus tard dans les nœuds
  internes sans changer les feuilles.
- Époque de save dans l'en-tête (4 bits libres à côté de l'`id`) : seulement si la save
  CoW en a besoin (SAVE_DESIGN).

## 9. État du code `radixox-lib/src/bitset.rs` (2026-11-07)

Forme clairsemée faite et testée (v1 simple, scalaire) :

- **B+tree retenu** (§2) : offsets dans les feuilles, séparateurs dans les nœuds internes
  (l'enfant `i` couvre `[keys[i-1], keys[i])`). Un séparateur peut survivre à la
  suppression de sa clé : il reste une borne valide, rien à corriger.
- **`is_leaf` retiré** : hauteur dans `trees[id]`, en-tête = `len` (3 bits) + `id` (29 bits).
  Feuilles à 7 clés (la feuille à 15 clés reste à trancher, avec la forme dense).
- Insertion : coupe préventive à la descente (feuille 4 | 3, séparateur recopié ; interne
  3 | médiane | 3). Suppression : réparation préventive (`len ≤ MIN = 3` → emprunt au
  voisin gauche, sinon droit, sinon fusion) ; la racine interne vidée par une fusion
  disparaît (hauteur − 1). Jamais de remontée.
- `route` / `lower_bound` : 7 comparaisons sans branche dépendante des données ; décalages
  en `copy_within` (le SIMD du §7 viendra les remplacer, à mesurer).
- **Slab global** : blocs de 1 Kio alloués à l'allocateur global (pas encore découpés dans
  les blocs du pool), masque `u16` par bloc, **15 listes doublement chaînées par
  occupation** → allocation dans le bloc le plus plein en O(1) ; bloc vide rendu, son n°
  réutilisé. `NodeIdx` exclut le dernier n° de bloc pour ne jamais produire `u32::MAX`.
- Table `trees[id] = (racine, hauteur, count)`, `id` libres en pile.
- API (`BitsetId`, `Copy`, sans `Drop`) : `create`, `destroy` (récursif, d'un bloc),
  `get`, `set` / `clear` (rendent l'ancienne valeur), `count` en O(1), `next_set(from)`
  (redescente avec le plus petit séparateur `> from` si la feuille est épuisée : base de
  BITPOS et des parcours), `stats()`.
- Tests : différentiels contre `BTreeSet` (domaine étroit → emprunts/fusions, domaine
  `u32` complet), offsets extrêmes (`u32::MAX`), 200 bitsets partageant le slab ;
  vérificateur d'invariants (tri, bornes, `MIN`, profondeur, `id`, `count`) et cohérence
  des masques / listes du slab.

**Construction en bloc et parcours (2026-11-07).**
- `from_sorted(&[u32])` et `from_words(&[u64])` (bitset classique : bit `i` = bit `i % 64`
  du mot `i / 64`) construisent l'arbre de bas en haut : feuilles remplies à la suite,
  réparties à égalité à ≤ 6 clés (une case libre : le premier `set` ne coupe pas), puis
  chaque niveau prend la première clé de chaque enfant comme séparateur, ≤ 7 enfants ; la
  répartition égale garantit `MIN` sans rééquilibrage. Ni descente ni coupe. Servira à la
  conversion classique → compact, au rechargement d'un dump et au résultat de `BITOP`.
- `WordBits` : bits à 1 de mots de 64 bits dans l'ordre (8 mots nuls sautés d'un coup,
  `tzcnt` + `blsr`) ; `popcount_words` (vectorisé par LLVM) donne la taille d'avance.
- `for_each_leaf` : parcours dans l'ordre par pile explicite, sans redescente, feuille
  sœur préchargée ; `fill_words` (compact → classique), `last`.

Mesures (`bitset_bench`, `#[ignore]`, release, 7600X, profil `performance`, 4 M offsets
à un bit sur 16 en moyenne, 7,8 Mio en classique) :

| Opération | ns/offset |
|---|---|
| `from_sorted` à froid (blocs neufs) | 8,4 |
| `from_sorted` à chaud (mémoire réutilisée) | 2,7 |
| `from_words` (à froid) | 9,2 |
| `set` un par un, croissant / aléatoire | 74 / 214 |
| `for_each_leaf` | 0,57 |
| `fill_words` | 1,2 |
| `WordBits` | 1,2 |
| `popcount_words` | 0,02 (0,09 ns/mot) |

À froid, les deux tiers du temps sont des défauts de page sur les blocs neufs. La
construction en bloc va 27× plus vite que les insertions croissantes et 80× plus vite
que les aléatoires. Coût d'une conversion classique → compact sous un plafond de 64 Kio
(512 K bits), à ~9 ns/offset à froid : elle n'a lieu que quand le bitset devient
clairsemé (seuil ~2 %, §6), soit ≤ ~10 K offsets → ~0,1 ms. Le pire cas théorique (les
512 K bits à 1) coûterait ~4,6 ms, mais il ne déclenche pas cette conversion. À confirmer
quand le plafond et le seuil seront fixés.

Ajouté avec l'intégration (§10) : `for_each_from(from, f)` (parcours dans l'ordre depuis
un offset, arrêt anticipé : BITCOUNT / BITPOS / save / bascule), `destroy_lazy` +
`free_step(budget)` (libération par tranches, `id` réservé jusqu'à la fin : le GC à venir
lit encore l'`id` des nœuds condamnés).

Reste : blocs dans le pool (`maxmemory`), GC §5, feuilles denses §6, SIMD §7.

## 10. Valeur oxidart : deux formes, bascule tranche par tranche (2026-11-07)

Code : `oxidart/src/bitset/` (valeur, migration, save), `oxidart/src/bcommand.rs`
(commandes). `Tag::Bitset`, `ValUnion::idx` = case d'un en-tête dans un HiSlab (comme les
List) ; `TYPE` répond `bitset`.

### 10.1 Deux formes pour tout le bitset

- **Classique** : `Vec<u64>` couvrant `[0, 64·n)`, bit `i` = bit `i % 64` du mot `i / 64`.
- **Compacte** : le B+tree de `radixox-lib/src/bitset.rs` (`BitsetId`).
- Coûts estimés : classique `8·⌈ext/64⌉` o, compacte `max(64, 12·count)` o (~10,7 o par
  offset construit en bloc, ~13 en insertions au hasard). **Hystérésis ×4** : on passe en
  classique quand elle coûte au plus la moitié de la compacte (densité ≳ 2 %), en compacte
  quand celle-ci coûte au plus la moitié de la classique (densité ≲ 0,5 %). Entre les deux,
  on ne bouge pas : pas d'oscillation.
- Évalué à chaque écriture, en O(1) (`count` et `hi` = dernier offset à 1 + 1 tenus dans
  l'en-tête). Un `SETBIT` classique au-delà de la fin regarde la règle **avant** d'agrandir :
  un offset lointain fait passer en compacte au lieu d'allouer des Mio de zéros.
- Création : la forme la moins chère pour le premier offset (classique sous ~256).
- Clé supprimée quand le dernier bit à 1 part (comme une List vide) ; `SETBIT k o 0` sur une
  clé absente ne la crée pas.

### 10.2 Bascule par tranches, les deux formes vivantes

Changer de forme ne fige pas la boucle : on garde **l'ancienne et la nouvelle forme en
même temps** et on déplace les offsets tranche par tranche, dans l'ordre croissant, derrière
un curseur `c`. L'ancienne forme a une étendue `ext` (`64·n` en classique, 2³² en
compacte).

- **Territoires** : l'ancienne forme fait foi sur `[c, ext)`, la nouvelle sur
  `[0, c) ∪ [ext, 2³²)`. Chaque requête est **routée** vers la forme dont le territoire
  contient l'offset. Une seule règle pour les deux sens : en classique → compacte, un
  `SETBIT` au-delà de la fin du tableau va directement dans la compacte.
- **Pas d'effacement dans l'ancienne forme** pendant la bascule : ce qui est sous `c` y
  reste, périmé, hors de son territoire, donc jamais lu. L'ancienne forme est libérée d'un
  coup à la fin (tableau : un `free` ; arbre : libération par tranches). Le compte total est
  tenu dans l'en-tête, pas dans les formes.
- **Parcours dans l'ordre** (BITCOUNT, BITPOS, save, BITOP) : trois segments consécutifs,
  `[0, c)` dans la nouvelle, `[c, ext)` dans l'ancienne, `[ext, 2³²)` dans la nouvelle. Pas
  de fusion de deux flux.
- **Qui avance** : chaque écriture sur le bitset fait une tranche (budget fixe), et une
  liste des bitsets en bascule est avancée par la boucle quand elle est libre (même crochet
  que la libération des grands ZSet). Rien pendant une save (une tranche capturerait l'image
  du bitset).
- Le pic de mémoire est la somme des deux formes ; au point de bascule elles sont du même
  ordre (facteur 2), donc au plus ~3× la forme finale.
- Une tranche démarre tout de suite au déclenchement : une petite bascule finit dans
  l'écriture qui l'a déclenchée, sans état double visible.

### 10.3 Save, reload, condensé

- Enregistrement `T_BITSET` (7) : `count` puis les offsets en deltas varint, quelle que soit
  la forme (format indépendant de la représentation).
- **v1** : barrière par bitset entier (SaveBarrier sur le HiSlab des en-têtes) ; l'image T0
  est une copie (mots ou offsets, la plus petite) prise à la première écriture pendant la
  save. O(taille) une fois par bitset et par save : à remplacer par un journal de
  différences (bits posés / effacés pendant la save, appliqués à la fin par tranches) si ça
  se voit. Sérialisation en flux par tranches avec `peek` (lit l'image si elle est apparue
  entre deux tranches), suppression pendant la save différée (la case reste occupée).
- Condensé : `seq_term(clé, T_BITSET, exp, (Σ crc(offset), count))`.

### 10.4 Commandes (noms de Redis, type à nous)

`SETBIT`, `GETBIT`, `BITCOUNT [start end [BYTE|BIT]]`, `BITPOS bit [start [end
[BYTE|BIT]]]` : mêmes numéros de bits que Redis (le bit `i` de notre bitset est le bit `i`
de la string Redis), donc mêmes réponses, sauf que **la « longueur » est déduite du
contenu** (`⌈(dernier bit à 1 + 1) / 8⌉` octets) : un `SETBIT k 100 0` ne l'allonge pas.
`GET` & co. → `WRONGTYPE`. `BITOP` ensuite (bornée, résultat construit en bloc).
`BITCOUNT key start` sans `end` accepté (`end` = -1), comme Valkey 8+. Erreurs vérifiées
contre `valkey-server` 9.1.

### 10.5 État (2026-11-07)

Fait : §10.1 à §10.4 sauf `BITOP`. Tests : `oxidart/src/bitset/test.rs` (différentiel
contre `BTreeSet` avec bascules dans les deux sens entrelacées, tranches de 24 en test ;
commandes contre un modèle de Redis), opérations bitset dans `random_op` des tests de save
(vue T0, condensé, reload sous mutations et bascules), `radixox/tests/bitset.rs`
(réponses vérifiées contre Valkey), reload dans `tests/bgsave.rs`.

Limites connues de la v1 :
- lectures sur plage (`BITCOUNT` / `BITPOS` avec plage sur un gros bitset compact, gros
  `popcount` classique) d'un bloc : ~1 ns par offset, à passer en tranches au-delà d'un
  seuil (`exec/jobs.rs`) ;
- image de save = copie entière à la première écriture (§10.3) ;
- en-tête de ~100 o par bitset (deux `Form`), à resserrer si les petits bitsets comptent ;
- agrandir un tableau classique est un `Vec::resize` (copie si la réallocation déplace) :
  borné par la règle de forme, pas par une tranche.

