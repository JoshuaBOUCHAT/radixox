# Boucle io_uring maison (runtime-less) — Architecture

> Remplace monoio par une boucle io_uring écrite à la main, **sans `Future`,
> sans waker, sans tâche**. Les CQE sont dispatchés directement vers des
> machines à états explicites.
>
> Décisions prises en session le 2026-10-10, le 2026-10-11 et le 2026-10-19 (revue
> complète : modèle par files de frames, équité, accept classique, jamais bloquant,
> déploiement). La version précédente de ce fichier (runtime à futures : `RawTask`,
> `fn_poll`, `syscall_nb`) est **écartée**, voir §14. Historique : `git log -- RUNTIME_ARCH.md`.
>
> Base logicielle : **io-uring 0.7.15**, **noyau ≥ 6.12** (§11). monoio garde sa 0.6.4
> jusqu'à sa suppression (§13) : les deux versions coexistent dans le workspace.
>
> Démarche : *make it work, make it fast, make it adaptable*. Une seule boucle dans
> `radixox/src/bin/uringloop.rs`, qui remplace `resp.rs` une fois stabilisée.

---

## 0. Résumé

```
un processus           une boucle, une base (pas de multi-boucle, pas de SELECT n ≠ 0)
connexions             slot pris dans notre free list ; accept classique + FilesUpdate (§4.1)
                       ConnIn (rx, file de frames) / ConnOut (out, envoi, mode) séparés
user_data              kind << 32 | slot ; registres userland marqués (slot, epoch) (§4.3)
fin de connexion       Closing (flush puis fermeture) ou Draining (erreur) → refs == 0 → CLOSE_DIRECT
lecture                recv multishot + anneau incrémental ; frames complètes → FrameRef
                       dans la file de la conn, refcount par tampon ; frame coupée → rx
lot                    tirage par tours entre connexions (1 commande par conn d'abord),
                       ordre gardé par connexion, barrières, budget de tour (§2.1)
écriture               réponses écrites directement dans out ; un send MSG_WAITALL en vol
                       par connexion ; flush en fin de tour ; backpressure par octets
jamais bloquant        commande lourde → job qui encode sa réponse en octets ; connexion
                       parquée ; verrou de lecture par clé pour l'atomicité (§8.1)
cœur sans I/O          les machines émettent des Op ; une couche mince fait les SQE (§1.2)
délégation             sends en IOSQE_ASYNC : off/on d'abord, auto ensuite (§6.4)
cible finale           thread principal = calcul pur sur des lots, réseau entier au noyau (§17)
attente                enter(GETEVENTS | NO_IOWAIT) ; timeout = prochaine échéance de timer
```

---

## 1. Principes

- **Un seul propriétaire.** `struct Server { ring, art, conns, bufs, pubsub, blockers, jobs, timers }`
  est passé en `&mut` aux handlers. Plus de `Rc<RefCell<OxidArt>>`, plus de borrow à l'exécution.
- **Pas de `.await`.** L'invariant d'`ARCHITECTURE.md` §1.1 (« rien de daté ne traverse un
  `.await` ») devient : **rien de daté ne survit d'un tour de boucle à l'autre**. Un job
  conserve un curseur re-résolvable (clé), jamais un idx de slab ni un pointeur (sauf la
  save, qui a sa barrière COW, cf. `SAVE_DESIGN.md`).
- **Machines à états plates.** Chaque machine a peu d'états. Ses transitions ne viennent
  que d'un CQE ou d'un `step` de job. Une seule imbrication : la connexion porte l'état
  de sa frame en cours (§5.3).

| Machine | États |
|---|---|
| Connexion | `Normal` · `Parked` · `Closing` · `Draining` |
| Réception | armée · affamée (`ENOBUFS`) · suspendue (backpressure) · terminée |
| Envoi | libre · en vol |
| Découpage | frame complète · incomplète (reliquat dans `rx`) · invalide · remplissage de valeur |
| Job | curseur + `step(&mut Server, budget) -> Done \| More` |

- **Une connexion n'a pas de machine à états pendant l'exécution** : elle a une file de
  frames à exécuter (§5.4) et un tampon de sortie. Le lot (§2.1) pioche dans les files ;
  l'exécution écrit dans `out`. Pas d'interior mutability : l'anneau et `inbound` sont
  empruntés en lecture, `art` et `outbound` en écriture, ce sont des champs disjoints de
  `Server`.

### 1.1 Garder la complexité sous contrôle (2026-10-12)

La boucle est un ensemble de machines à états. C'est plus lisible que des futures et des
`select!` (avec monoio, l'état d'une connexion était dispersé entre tâches, channels, wakers
et points d'`await`, avec des invariants implicites jamais vérifiés), **à condition** de
tenir ces cinq règles :

1. **Un `enum` par machine, un `match` exhaustif, des transitions centralisées.** L'état ne
   change que dans quelques fonctions nommées (`park`, `wake`, `drain`…), comme les points de
   passage de la save. Le compilateur force à traiter chaque état : aucun cas oublié.
2. **Un vérificateur d'invariants du serveur** (`Server::check_invariants`), appelé en debug
   à la fin de chaque tour, comme `assert_canonical` pour l'arbre :
   - `refs` d'une connexion = ce qui peut encore produire un CQE pour elle (recv armé,
     send en vol, jobs) ;
   - une connexion parquée a une file de frames vide (tout est dans `rx`) ;
   - au plus un send en vol par connexion, tampon en vol inchangé ;
   - `Σ refs_buf` = nombre de `FrameRef::Ring` dans toutes les files (§5.4).
   Une incohérence explose tout de suite, au bon endroit.
3. **Tests déterministes sur un faux anneau** (§16) : séquences de CQE fabriquées (envoi
   partiel, connexion qui meurt en plein BLPOP, LPUSH et timeout dans le même tour,
   `ENOBUFS`), puis vérification de l'état et des invariants ; aussi tirées au hasard, comme
   les tests de `krange` contre une `BTreeMap`. Les bugs io_uring deviennent reproductibles.
4. **Une boucle mince** : récolte → `dispatch(kind)` → un handler par type d'événement.
   Aucune logique métier dans la boucle ; les commandes restent dans `execute_sync`, inchangé.
5. **Pas d'optimisation prématurée — le mot important est « prématurée ».** On optimise
   tard, pas jamais. Le premier prototype vise un squelette **correct et mesuré** : pas de
   préchargement entre connexions, pas de délégation des envois, pas de modes extrêmes
   (§17). Le tirage par tours (§2.1) n'est pas une optimisation mais la structure même de
   la boucle : il est dans le prototype. Mais le
   design leur garde leur place dès le départ (idx de slot stables, `out` par connexion,
   dispatch par lots possible), pour que les optimiser plus tard ne demande pas de tout
   réécrire. Chaque optimisation arrive ensuite **avec sa mesure avant/après**, comme les
   chantiers de l'arbre (SWAR, prefetch des frères, multi-clés : chaque gain chiffré avant
   commit).

### 1.2 Cœur sans I/O

Les machines à états ne soumettent jamais de SQE elles-mêmes. Elles poussent des `Op`
(`Recv{slot}`, `Send{slot}`, `CancelRecv{slot}`, `CancelSend{slot}`, `Close{slot}`,
`FileWrite{job}`…) dans une file ; une couche mince les traduit en SQE au moment de
l'`enter`. Dans l'autre sens, la couche anneau traduit chaque CQE en `Completion` typée.

Les tests du faux anneau (§16) injectent des `Completion` fabriquées et lisent les `Op`
produites : aucun io_uring à simuler. Les résultats « lents » suivent la même logique
proactive : un job construit sa réponse de son côté et l'injecte à la fin (§8).

---

## 2. La boucle

```rust
loop {
    let now = clocks();                     // murale (TTL) + monotone (timers), §9
    srv.art.set_now(now.wall);

    srv.harvest(BUDGET_CQE);                // 1. CQE → frames dans les files (§5.4)
                                            //    au plus BUDGET_CQE : le reste attend dans la CQ
    srv.run_batch(BUDGET_TOUR);             // 2-4. tirage par tours, parse, exécution (§2.1)
    srv.release_frames();                   // 5. refcount des tampons, restitution à l'anneau
    srv.timers.fire(now.mono, &mut srv);    //    échéances (BLPOP, autosave, balayage, fsync AOF)
    srv.inject_ready();                     //    réponses de jobs finis, connexions à rejouer
    srv.flush_dirty();                      // 6. un send par connexion sale (§6)
    if let Some(job) = srv.jobs.next() {    // 7. une tranche de travail de fond
        enter(to_submit, 0, 0);             //    les réponses du tour partent AVANT la tranche
        srv.run_slice(job, BUDGET_JOB);
    }
    if srv.idle() {                         // 8. soumission + attente
        enter(to_submit, 1, GETEVENTS | EXT_ARG | NO_IOWAIT, timeout = srv.timers.next())
    } else {
        enter(to_submit, 0, GETEVENTS)      // on continue de tourner
    }
}
```

**Setup de l'anneau.** `SINGLE_ISSUER` + `DEFER_TASKRUN` + `TASKRUN_FLAG` (§6.4),
`register_ring_fd`, CQ au moins 4× la SQ (`setup_cqsize`, ex. SQ 4096 / CQ 16384). SQ_POLL
n'est plus une option (§14).

**Budget de tour.** Les réponses ne partent qu'en fin de tour : un tour trop long retarde
tout le monde. On ne vide pas toute la CQ : au plus `BUDGET_CQE` CQE par tour, le reste
attend dans la CQ et ses données restent dans l'anneau (pas encore rendu). Aucune copie.
Le lot a son propre budget, en temps (§2.1).

**Envoi avant la tranche de job.** Les SQE ne partent qu'à un `enter`. Sans l'`enter` de
soumission avant `run_slice`, les réponses du tour attendraient la tranche. Coût : 2
syscalls par tour, seulement quand un job tourne.

**`NO_IOWAIT` (6.15, utilisé si le noyau l'annonce).** Sans lui, l'attente dans `enter` est comptée en iowait et gonfle
`/proc/pressure/io` : alertes de monitoring trompeuses en prod.

**⚠️ `GETEVENTS` obligatoire.** Avec `DEFER_TASKRUN`, les complétions (y compris la
copie des données reçues) ne sont exécutées que pendant un `io_uring_enter` avec
`GETEVENTS`. `Submitter::submit()` ne passe pas ce flag quand `want == 0` : on appelle
`enter` nous-mêmes. Sans ça, une save qui tourne affame les connexions (même effet que
le bug de yield monoio).

**Réglages à mesurer :** `SubmitArgs::min_wait_usec` (regrouper les complétions à faible
charge), `BUDGET_CQE`, `BUDGET_TOUR`, `BUDGET_JOB` (valeurs de départ : §13.1).

### 2.1 Le lot : tirage par tours entre connexions (2026-10-19)

Chaque connexion a une file de frames prêtes (§5.4). Le lot pioche dedans **par tours** :

```
tour 0 : la 1re frame de chaque connexion qui en a une
tour 1 : la 2e frame des connexions qui en ont encore
…        jusqu'à épuiser BUDGET_TOUR ; le point de départ tourne à chaque tour de boucle
```

- **Les connexions sans pipeline passent toutes au tour 0.** Ce sont des allers-retours :
  le client attend chaque réponse, il faut les servir vite. Les pipelines se partagent le
  budget restant ; leur reste attend le tour de boucle suivant, où il sera traité en masse
  (et plus tard délégué en `IOSQE_ASYNC`, §6.4).
- **Ordre gardé à l'intérieur d'une connexion, libre entre connexions.** Réordonner les
  commandes d'un même client casse la causalité vue par les autres : `SET a` puis `SET b`
  exécutés à l'envers, un tiers du même lot peut voir `b` sans `a`. Entre connexions, tout
  ordre est linéarisable (un seul thread). Comme l'ordre par connexion est gardé, les
  réponses vont **directement dans `out`**, sans phase de remise en ordre.
- **Barrières.** Une commande qui change l'interprétation des suivantes (table §2.2 :
  AUTH, HELLO, SUBSCRIBE, UNSUBSCRIBE, QUIT, toute commande qui parque, plus tard MULTI)
  est la **dernière** exécutée pour sa connexion dans ce tour de boucle. Sinon
  `[GET a, KEYS *, GET b]` exécuterait `GET b` avant la réponse de KEYS.
- **Budget en temps** (2026-10-19, pas encore fait : le tour est borné en commandes,
  `RADIXOX_BATCH_CMDS`) : ~200 µs d'exécution par tour, mesurés au TSC (`radixox::tsc`),
  avec 1024 commandes et un plafond d'octets de sortie comme garde-fous. Un budget en nombre
  de commandes ne borne rien sans pipeline : là, le coût d'un tour, ce sont les sends
  payés dans notre `enter` (~3 µs chacun en loopback), pas les commandes. Les commandes
  lourdes ne coûtent presque rien au lot : elles deviennent des jobs (§8.1), qui ont leur
  propre part du CPU.
- **Connexion tuée pendant le lot** (limite d'`out`, plus tard CLIENT KILL) : son mode est
  testé avant chaque exécution, ses frames restantes sont relâchées.
- Plus tard (§8bis, §17) : préchargement par fenêtres **entre connexions** d'un même tour
  de tirage, réutilisation des idx, fusion des lectures identiques.

### 2.2 Table des commandes

Une table de métadonnées par commande, comme la table de commandes de Redis, au lieu
d'informations dispersées entre `main_key` et le `match` du dispatch :

| Champ | Sert à |
|---|---|
| arité (min, max) | rejet au parse |
| barrière | §2.1 |
| écriture / lecture | collecteur d'effets (§18), verrous de lecture (§8.1), WATCH plus tard |
| clé(s) principale(s) | préchargement, verrous |
| coût estimable | seuil « devient un job » (§8.1) |
| autorisée non authentifiée / en Pub/Sub | contrôle d'accès, mode abonné |

Elle donne un chemin clair pour chaque cas particulier (frame à moitié reçue, barrière,
commande lourde) et sert aussi `COMMAND`, plus tard les ACL et les transactions.

### 2.3 Ce que les budgets font au p99 (2026-10-19)

**Sous saturation, la latence est imposée par la loi de Little**, pas par la boucle :
`latence moyenne = requêtes en vol / débit = connexions × profondeur de pipeline / débit`.
80 connexions à 280 K ops/s → 0,29 ms (mesuré : 0,29–0,32 ms). 10 000 connexions actives
sans pipeline → ~36 ms quoi qu'on fasse. Seul le débit la baisse (délégation TCP, lots,
pipeline côté client). Les budgets décident **qui** attend et l'écart entre moyenne et p99.

**Hors saturation**, une requête attend au pire ~2 tours (la fin du tour en cours, puis le
sien). `T ≈ commandes × coût user + sends × coût noyau + tranche de job` :

| Régime | Commandes | Sends | T | p99 ajouté (~2 T) |
|---|---|---|---|---|
| pipeline (peu de conns, gros lots) | 1024 × ~0,25 µs ≈ 250 µs | quelques-uns | ~300–400 µs | ~0,6–0,8 ms |
| sans pipeline, 80 conns | 80 × 0,3 µs ≈ 25 µs | 80 × ~3 µs ≈ 240 µs | ~300 µs | ~0,6 ms (0,41 mesuré sous monoio) |
| sans pipeline, ≥ 1024 conns | ~300 µs | 1024 × ~3 µs ≈ 3 ms | ~3,5 ms | ~7 ms, mais Little impose déjà ≥ 3,6 ms |

- La tranche de job (~50 µs) ajoute au plus ~10 % à un p99 de ~400 µs, seulement pendant
  qu'un job tourne, et ne retarde pas l'envoi des réponses (`enter` avant la tranche).
- Le levier sur un tour long sans pipeline reste la **délégation des sends** (§6.4) : sortis
  de notre `enter`, les 3 ms de la dernière ligne redeviennent du calcul user.
- L'équité protège les petits clients : au tour 0 du tirage, chaque connexion sans
  pipeline passe à chaque tour ; 10 conns × 256 commandes s'étalent sur ~3 tours (~1,2 ms
  pour le lot complet).
- Ordres de grandeur tirés des mesures du 2026-10-19 (3,6 µs de CPU serveur par requête
  sans pipeline, 8 % de user) : à vérifier avec l'histogramme de durée des tours exposé
  dans `INFO` dès l'étape 2.

---

## 3. `user_data`

```
u64 = [ kind : 32 ][ slot : 32 ]
```

| kind | slot | Remarque |
|---|---|---|
| `Accept` | listener | multishot, un seul SQE par listener pour toute la vie du serveur |
| `Install` | conn | `FilesUpdate` du socket accepté dans son slot (§4.1) |
| `Recv` | conn | multishot |
| `Send` | conn | au plus un en vol par conn |
| `Cancel` | conn | `IOSQE_CQE_SKIP_SUCCESS` ; en cas d'erreur, log seulement |
| `Close` | conn | `IOSQE_CQE_SKIP_SUCCESS` ; ne touche jamais la table (le slot peut déjà être redonné) |
| `File` | job | écritures et fsync du dump / de l'AOF |
| `Signal` | — | lecture de la `signalfd` |
| `Timeout` | — | si un timer passe par io_uring plutôt que par le timeout d'`enter` |

**Pas de génération dans `user_data`.** Une CQE ne peut jamais viser une connexion qui a
été remplacée (`refs`, §4.2), donc aucun test d'obsolescence dans le dispatch. La
génération existe, mais pour les registres userland seulement (`epoch`, §4.3).

---

## 4. Connexions

### 4.1 Accept classique, slot choisi par nous (2026-10-19)

On optimise le flux, pas l'ouverture : un client sérieux ouvre un pool au démarrage et le
garde des heures, et même une connexion courte paie surtout la poignée de main TCP. Donc :

1. `AcceptMulti` **sans** `allocate_file_index` : le CQE rend un **fd normal**.
2. Dans le handler : `getpeername` (adresse pour `CLIENT LIST`, logs, futur filtrage par
   IP), `TCP_NODELAY` (indispensable : il avait corrigé les p99.99 de 43 ms en pipeline),
   `TCP_USER_TIMEOUT` (§4.2).
3. Slot pris dans **notre** free list (LIFO : table dense, slots chauds en cache).
4. `FilesUpdate` (opcode, pas de syscall en plus) installe le socket dans ce slot, chaîné
   par `IOSQE_IO_LINK` au premier `RecvMulti` ; `close` du fd au CQE de l'installation.

- `register_files_sparse(MAX_CONN)` à l'init ; le chemin chaud (recv, send) reste en
  fixed file, sans `fget`/`fput` par op. Si la mesure de l'étape 2 (§13) montre que le
  fixed file ne gagne rien, on le retire aussi : fd normal partout.
- Coût : 2–3 syscalls par connexion, payés une fois.
- **Max clients** : quelques slots gardés en réserve. Sous la réserve, la connexion reçoit
  `-ERR max number of clients reached` puis est fermée, comme Redis.
- `ss` voit les sockets (sock_diag) ; seuls `ss -p` et `lsof` ne les rattachent pas au
  processus → les exposer dans `INFO clients`.
- Plusieurs listeners (TCP, socket Unix, plus tard TLS) : `slot` de `user_data` = id du
  listener. kTLS (§16) demande un fd pendant la poignée de main : un argument de plus
  pour l'accept classique.

### 4.2 Fin de connexion : `Closing`, `Draining` et le compteur `refs`

**Règle unique : `refs` compte tout ce qui peut encore produire un CQE pour le slot.
`CLOSE_DIRECT` seulement à `refs == 0`.**

| Source | +1 | −1 |
|---|---|---|
| recv multishot armé | armement | CQE sans `F_MORE` |
| send en vol | soumission | CQE du send |
| job qui vise la conn (KEYS, UNLINK…) | création | fin du job |

Les registres userland (pub/sub, blockers, timers) ne comptent pas dans `refs` : ils sont
marqués `(slot, epoch)` (§4.3).

**Deux façons de finir :**
- **`Closing`** — QUIT, ou EOF du client (un client qui ferme son côté écriture après son
  pipeline : `nc`, scripts, `--pipe` attend quand même ses réponses). Le recv est annulé ;
  les frames déjà reçues sont exécutées ; `out` est envoyé ; puis `Draining`.
- **`Draining`** — erreur, reset, dépassement de limite, protocole invalide (après
  `-ERR Protocol error` envoyé au mieux), timeout. Rien n'est plus exécuté ni soumis.

Passage en `Draining` :
1. On ne soumet plus rien pour la connexion ; ses `FrameRef` sont relâchées tout de suite
   (sinon elles retiennent des tampons de l'anneau).
2. On annule le recv **et le send en vol**. Un send ne finit pas forcément tout seul : un
   client qui arrête de lire sans fermer (process figé, réseau coupé sans RST) remplit la
   fenêtre TCP et le send reste en vol indéfiniment, donc `refs` ne tomberait jamais à 0.
   Filet de sécurité côté noyau : `TCP_USER_TIMEOUT` (ex. 30 s) à l'accept.
3. On la retire tout de suite de Pub/Sub (mémoire, et compte exact renvoyé par PUBLISH).
   Les autres registres (blockers, timers) sont nettoyés paresseusement grâce à l'`epoch`.
4. Chaque CQE restant fait `refs -= 1`. À 0 : `CLOSE_DIRECT`, slot rendu à la free list.

### 4.3 Structure et `epoch`

```rust
struct ConnIn {                       // emprunté en lecture pendant le lot
    queue: VecDeque<FrameRef>,        // frames complètes prêtes à exécuter (§5.4)
    rx: Vec<u8>,                      // reliquat de frame coupée, frames recopiées (§5.4)
    partial: Option<Box<Partial>>,    // remplissage de grosse valeur (§5.3)
}
struct ConnOut {                      // mutable pendant le lot
    mode: Mode,                       // Normal | Parked(Reason) | Closing | Draining
    epoch: u32,                       // monotone à travers les réutilisations du slot
    refs: u8,
    out: Vec<u8>,                     // réponses en cours d'accumulation
    sending: Option<Vec<u8>>,         // tampon en vol (None = pas de send en vol)
    last_active: u32,                 // secondes, pour le balayage d'inactivité (§9)
    client: ClientInfo,               // id global (≠ slot), nom, adresse, authed…
    channels …                        // pour le retrait immédiat de Pub/Sub
}
```

- **`epoch`** est incrémenté à chaque accept et à chaque parquage, **jamais remis à zéro**
  quand le slot est redonné. Timers et inscriptions BLPOP portent `(slot, epoch)` ; une
  entrée dont l'epoch ne correspond plus est ignorée. Remplace `park_seq`, et dispense les
  registres de prendre des `refs`.
- `client.id` reste un compteur global (les curseurs SCAN sont rattachés à lui), jamais le
  slot, qui est réutilisé.
- `sending` : le noyau lit ce tampon par pointeur jusqu'au CQE. Jamais redimensionné ni
  échangé pendant un envoi (vérifié par `check_invariants`).
- Le prototype garde des `Vec` pour `out`/`sending`. Le pool enregistré arrive avec
  `SEND_ZC` (§6.3).

---

## 5. Réception

### 5.1 Recv multishot + anneau incrémental

- `RecvMulti` sur `Fixed(slot)`, groupe de tampons partagé par toutes les connexions.
- Anneau enregistré avec `register_buf_ring_with_flags(..., IOU_PBUF_RING_INC)` (6.12+) :
  le noyau consomme un tampon **par morceaux**. Le CQE porte l'id du tampon
  (`cqueue::buffer_select`), et `cqueue::buffer_more` dit si le noyau continuera d'y
  écrire.
- ~~Quelques gros tampons (4 × 2 Mo)~~ **démenti par la mesure (2026-10-19)** : défaut
  **64 × 32 Ko = 2 Mo** (`RADIXOX_RING_BUFS`, `RADIXOX_RING_BUF_KB`). Un gros tampon reste
  retenu longtemps par une seule connexion (l'anneau s'épuise : `ENOBUFS`, réarmements),
  et les données arrivent dans une zone trop vaste pour rester en cache. Petits tampons :
  rendus vite, réutilisés encore chauds. Mesures : §12.4.
- Les morceaux de **connexions différentes s'entrelacent** dans un même tampon : c'est
  toujours le tampon en tête du groupe qui sert, quel que soit le socket. Interprétation
  du mécanisme, à confirmer par un test dans le prototype. On ne compte donc pas sur la
  contiguïté entre deux morceaux d'une même connexion.

**`ENOBUFS` est un cas normal, pas une anomalie.** Avec `DEFER_TASKRUN`, le noyau n'écrit
dans l'anneau que pendant notre `enter`, mais dans un seul `enter` il peut vider le tampon
de réception de chaque socket (plusieurs Mo avec l'autotuning) : 80 connexions pipelinées
dépassent vite 8 Mo, et les frames en attente (§5.4) retiennent aussi des tampons. Donc :
liste « affamées », réarmées quand des tampons reviennent, compteur dans `INFO`. Nombre
et taille des tampons à mesurer (4 × 2 Mo contre 32 × 256 Ko : un tampon retenu par une
seule frame bloque un quart de l'anneau dans le premier cas). À vérifier sur 7.2 : le
plafond de relances du multishot par invocation, et si `len` sur `RecvMulti` borne le
volume reçu par invocation (équité entre une connexion bavarde et les autres).

### 5.2 Handler

```rust
Kind::Recv => {
    let more = cqueue::more(flags);              // F_MORE absent → recv terminé
    if !more { conn.refs -= 1; }
    match res {
        n if n > 0 => {
            let bid = cqueue::buffer_select(flags).unwrap();
            frame_chunk(conn_in, bufs.chunk(bid, n)); // découpage en frames (§5.4), pas d'exécution
            bufs.mark_done_if(bid, !cqueue::buffer_more(flags));
            if !more { rearm.push(slot) }
        }
        0 => conn.close_gracefully(),            // EOF → Closing (§4.2)
        e if e == -ENOBUFS => starved.push(slot),// réarmé quand des tampons reviennent
        _ => conn.drain(),
    }
}
```

Le handler ne fait que découper : l'exécution a lieu dans le lot (§2.1).

### 5.3 Parse : trois cas

RESP annonce la taille d'une bulk string avant ses données. Le parseur ne devient donc
reprenable que sur un seul état.

| Cas | Copie | Parseur |
|---|---|---|
| 1. frame entière dans le morceau (cas courant) | aucune : `Cmd<'a>` emprunte le tampon | `Cmd::parse` actuel |
| 2. frame coupée, petite (en-têtes, commande, clés) | reliquat de quelques centaines d'octets → `conn.rx` | actuel, sur `rx` |
| 3. frame coupée avec une valeur `$N` au-delà d'un seuil (~16 Ko) | morceaux copiés **directement** dans le `SharedByte` de N octets, alloué dès que N est lu | + état « remplissage de valeur » |

```
on_data(conn, morceau):
  si conn.partial : compléter la valeur ; si pleine → construire le Cmd, exécuter, reprendre
  si conn.rx vide : parser sur place ; frame incomplète → cas 2 ou 3
  sinon           : ajouter à rx, parser rx
```

- Cas 3 : les arguments déjà lus (commande, clé) sont copiés dans `partial`, une allocation
  seulement quand une frame est coupée. L'état ne peut pas vivre sur la pile : la suite
  arrive dans un CQE ultérieur.
- Bénéfices du cas 3 : une seule copie pour les grosses valeurs (celle vers `SharedByte`
  est obligatoire de toute façon), mémoire par connexion bornée (un `SET` de 100 Mo n'a pas
  besoin d'un `rx` de 100 Mo), et une limite de taille de requête facile à appliquer.
- Ordre : le prototype fait les cas 1 et 2, le cas 3 vient avec le chantier des grosses
  valeurs.
- Écarté : le parseur reprenable à n'importe quel octet (un état par élément du protocole).

**Découpage à trois issues : `Complete(n) | Incomplete | Invalid`.** Le `Cmd::parse` actuel
rend `None` à la fois pour « incomplet » et « invalide » : une frame mal formée (ou `*0`)
laisse la connexion attendre indéfiniment pendant que `rx` grossit. `Invalid` →
`-ERR Protocol error`, puis `Draining`.

**Commandes inline acceptées** (2026-10-19), comme Redis : une frame qui ne commence pas
par `*` est une ligne découpée sur les espaces, avec guillemets (`SET k "a b"`) et
échappements. Telnet, `nc` et les scripts shell marchent ; coût nul pour le chemin RESP
(un test sur le premier octet).

**Limites à trois niveaux (2026-10-19).** Une limite par connexion seule ne tient pas :
1 Go × 10 000 connexions = 10 To en théorie.

| Niveau | Limite | Dépassement |
|---|---|---|
| frame | bulk string ≤ 512 Mo (`proto-max-bulk-len`), ≤ 1 048 576 arguments, ligne inline ≤ 64 Ko | `-ERR Protocol error`, `Draining` |
| connexion | reliquat `rx` + valeur en remplissage ≤ 1 Go (`client-query-buffer-limit`) | `Draining` |
| **global** | **budget d'accumulation** : `rx` + valeurs partielles + files recopiées, toutes connexions (1 Go par défaut, plus tard une part de `maxmemory`) | **pas de fermeture** : backpressure sur les plus gros consommateurs (recv suspendu, §7.1), reprise quand le budget redescend |

- Tenu par un compteur mis à jour à chaque copie ou libération dans `rx` : rien sur le
  chemin chaud, qui ne copie pas.
- Cas 3 : la taille annoncée `$N` est **réservée dans le budget avant** d'allouer, et la
  valeur grossit avec les données reçues : un client qui annonce 500 Mo sans les envoyer
  ne prend pas 500 Mo.
- Même logique en sortie : **budget global d'`out`** en plus des limites par connexion
  (§10, §13.1).
- `INFO` expose les deux compteurs et le nombre de connexions suspendues.

Entiers d'en-tête lus avec `checked_mul` / `checked_add` : corrigé le 2026-10-19
(`b975c9d`) ; `$18446744073709551615` faisait passer le test de longueur puis paniquer le
découpage (crash à distance avant AUTH).

### 5.4 Files de frames et refcount des tampons (2026-10-19)

> **Révisé le 2026-10-19 (implémentation de l'étape 2).** La file ne contient plus des
> frames découpées à l'arrivée mais des **spans** : les morceaux reçus tels quels (`bid,
> off, len`, un refcount par morceau ; deux morceaux contigus de la même connexion dans
> le même tampon fusionnent). Le parse a lieu **au lot, fenêtre par fenêtre** : K frames
> parsées (une seule passe, `Cmd::parse` donne la longueur), préchargées, exécutées. C'était
> l'intention (« parser K puis exécuter K ») : découper tout le tour à l'arrivée relisait
> chaque frame froide plus tard (+500 instructions et +14 défauts L1 par commande,
> mesuré). Une frame coupée entre deux morceaux est complétée dans `rx` au moment de
> l'exécuter (par pièces de 4 Ko, l'excédent rendu au morceau) ; un reliquat en fin de
> dernier morceau est recopié dans `rx` pour rendre le tampon. Le texte ci-dessous décrit
> la première version (`FrameRef` par frame) ; ses règles de rétention restent valables.

Le handler du recv découpe ; il n'exécute pas. Chaque frame complète devient une
`FrameRef` dans la file de sa connexion :

```rust
enum FrameRef {
    Ring { buf: u16, off: u32, len: u32 },   // dans l'anneau ; refs_buf[buf] += 1
    Local { off: u32, len: u32 },            // dans conn.rx
}
```

- **Le parse a lieu à l'entrée dans le lot** : `FrameRef` → `Cmd<'tour>`, qui emprunte
  l'anneau (ou `rx`) pour la durée du tour seulement. Aucun `unsafe` de durée de vie. Une
  frame qui attend plusieurs tours est découpée une fois et parsée une fois, à son
  exécution.
- **Refcount par tampon** : `refs_buf[buf]` = nombre de `FrameRef::Ring` qui le visent.
  Un tampon est rendu à l'anneau quand `refs_buf == 0` **et** que son dernier morceau ne
  porte plus `F_BUF_MORE` (le noyau a fini d'y écrire).
- **Frame coupée en bout de morceau** → recopiée dans `conn.rx` (`Local`). Si le morceau
  suivant de la même connexion est **adjacent** dans le même tampon (même `bid`, `off` =
  fin du précédent), on étend sans copie ; sinon on copie. Un test gratuit, en
  optimisation de secours.
- `rx` est en ajout seul tant que la file contient des `Local` ; compacté quand elle est
  vide.
- **Ce qui vit longtemps possède ses données** : clés et arguments des jobs et des
  inscriptions BLPOP sont des `SharedByte`, jamais des `FrameRef`.
- **Ne pas retenir l'anneau longtemps** :
  - connexion parquée : au parquage, sa file est recopiée dans `rx`, et ce qu'elle reçoit
    ensuite y va directement (un `BLPOP 0` ne retient jamais l'anneau) ;
  - connexion en backpressure (§6.2) : même chose si le blocage dure plus de quelques
    tours, puis recv annulé ;
  - seuil bas : si l'anneau libre passe sous X %, les frames qui retiennent le tampon le
    plus ancien sont recopiées dans leur `rx`.
- Connexion qui meurt : ses `FrameRef` sont relâchées tout de suite.
- Invariant (`check_invariants`) : `Σ refs_buf` = nombre de `FrameRef::Ring` dans toutes
  les files.

---

## 6. Émission

### 6.1 Règles

- **Au plus un send en vol par connexion.** C'est l'ordre des octets, et c'est requis par
  la délégation io-wq, qui ne sérialise pas les envois sur un socket.
- **Flush en fin de tour.** Les connexions qui ont produit des réponses pendant le tour
  vont dans une liste « sales ». En fin de tour : un send par connexion sale. Toutes les
  réponses d'un tour sont regroupées, et `read_and_flush` disparaît (le recv reste armé).
- **Le noyau fait les relances.** `EAGAIN` n'apparaît jamais dans un CQE : io_uring arme un
  poll interne et réessaie quand le socket redevient écrivable. Avec **`MSG_WAITALL`**, il
  enchaîne aussi les envois partiels et ne poste qu'un CQE, une fois tout envoyé ou en
  erreur : plus de champ `sent` ni de reprise chez nous (à vérifier sur 7.2 avec un client
  qui lit lentement). `MSG_NOSIGNAL` en plus, par sécurité.
- Au CQE du send : si `out` n'est pas vide, on échange les tampons et on renvoie.
- Note : avec `DEFER_TASKRUN`, ces relances tournent quand même sur **notre** thread,
  pendant notre `enter`. Pour qu'elles tournent ailleurs : §6.4.

### 6.2 Flush anticipé et backpressure

```
après chaque commande :
  si out.len() ≥ SEUIL et aucun send en vol  → send tout de suite
  si out.len() ≥ LIMITE (send déjà en vol)   → arrêter de parser cette conn,
                                               garder le reste dans rx,
                                               reprendre au CQE du send
```

- Le seuil se compte en **octets de sortie**, pas en commandes. Vers 64 Ko par défaut,
  plus bas (16 Ko ?) quand les sends partent sur io-wq.
- Découper par nombre de commandes est perdant sur notre cœur : un send coûte ~3 µs de
  noyau, une commande ~0,1 µs (§12). Le découpage ne paie que pour une grosse sortie ou
  quand le send est délégué.
- La limite donne de la backpressure : un client qui pipeline plus vite qu'il ne lit
  sort du tirage (§2.1). Ça règle le TODO « buffers bornés » pour toutes les connexions.
  Si ça dure : file recopiée dans `rx` et recv annulé (§5.4, §7.1).
- L'autocorking TCP du noyau fusionne de petits sends rapprochés : un seuil un peu bas
  coûte moins cher que prévu.

### 6.3 Ce qui est zero-copy, et ce qui ne l'est pas

« Fixed » désigne trois mécanismes différents dans io_uring :

| Mécanisme | Ce que c'est | Pour nous |
|---|---|---|
| **Fixed files** (fichiers enregistrés) | un slot dans une table au lieu d'un fd : pas de `fget`/`fput` par op | oui, sur tout le flux (§4.1) |
| **Provided buffers** (anneau de tampons) | le noyau choisit où écrire chaque réception | oui, indispensable au recv multishot (§5.1) |
| **Registered buffers** (`register_buffers`, `*_FIXED`) | mémoire épinglée une fois, lue par le noyau sans copie | plus tard : `SEND_ZC` et dump en `O_DIRECT`. Rien à gagner pour un send qui copie, et ça interdirait les `Vec` qui grandissent |

| Op | Copie noyau | Remarque |
|---|---|---|
| `SEND` / `WRITE` | oui | |
| `WRITE_FIXED` sur un socket | **oui** | évite seulement l'import du tampon ; pas zero-copy |
| `SEND_ZC` (+ `IORING_RECVSEND_FIXED_BUF`) | non | 2e CQE de notification ; rentable au-delà de quelques Ko |
| `SEND_ZC` en **loopback** | oui | le noyau recopie à la livraison locale : aucun gain |
| recv (anneau ou `READ_FIXED`) | oui | |
| `RecvZc` (zcrx, 6.15+) | non | carte compatible, header split, file RX dédiée ; pas de loopback |

Plan : un **pool enregistré** (`register_buffers`, une seule région découpée en blocs,
pris par un send et rendu à son CQE) pour `SEND_ZC` au-delà d'un seuil (~16 Ko, à mesurer
sur une vraie carte), et pour le dump en `O_DIRECT` + `WRITE_FIXED`. Le pool est épinglé
et compte dans `RLIMIT_MEMLOCK` (défaut systemd : 8 Mo) : rester en dessous ou documenter
`LimitMEMLOCK`.

### 6.4 Délégation des envois (`IOSQE_ASYNC`)

**Cadrage (2026-10-19) : io-wq est un chemin de secours détourné.** Il sert normalement
aux opérations qui ne savent pas se faire sans bloquer (fichiers bufferisés…) ; pour un
socket, io_uring ne l'utilise jamais (poll interne + relance sur notre thread). Avec
`IOSQE_ASYNC`, on le détourne en pool de calcul pour la pile TCP. Sa règle de création
(un worker dès qu'une op attend, sous le plafond) est bonne pour le disque, où un worker
attend sans CPU ; pour nous, un worker brûle du CPU du début à la fin et n'aide que s'il
y a un cœur libre. Plafond par défaut ≈ `RLIMIT_NPROC` : **un plafond fixe est
obligatoire**. Le noyau ne connaît ni notre quota, ni SMT, ni le coût de l'éclatement, ni
notre goulot. Plafond et affinité se changent à chaud (`register_iowq_max_workers`,
`register_iowq_aff`) ; on ne peut ni imposer un minimum ni pré-créer les workers ; en
baissant le plafond, le worker en trop s'arrête au plus tard après ~5 s d'inactivité.

**Modèle de gain : un pipeline, pas Amdahl.** Thread principal et workers tournent en
même temps ; `débit max ∝ 1 / max(part série, part parallélisable / N)`. Part série réelle
≈ 25–30 % (code RadixOx ~8 % sans pipeline, recv dans notre `task_work`, gestion io_uring,
passage de relais) :

| Workers TCP | Étage limitant | Gain théorique |
|---|---|---|
| 1 (≈ SQ_POLL) | TCP, 0,75 | ×1,33 |
| 2 | TCP, 0,375 | ×2,7 |
| 3 | série = TCP, 0,25 | ×4 (plafond) |

Réaliste : ×2–3 avec 2–3 workers (le passage d'un cœur à l'autre a un coût, cf. SQ_POLL
§12.3 ; Valkey io-threads, même modèle : ~200 K → ~500 K sur 4 threads). La pile TCP se
parallélise bien entre connexions (verrou par socket). En pipeline, la part user monte à
70–80 % : la délégation rapporte peu, les lots (§8bis, §17) prennent le relais.

Par défaut, le noyau exécute le send sur notre thread pendant `enter`. `IOSQE_ASYNC`
l'envoie aux workers io-wq. Justification chiffrée (§12) : sans pipeline, le noyau prend
~80 % de notre cœur ; en pipeline 64, ~34 %. Mesures loopback, donc gonflées : à
revalider sur un vrai réseau.

**Fonctionnement d'io-wq (ce qu'on subit) :**
- Un pool de threads noyau (`iou-wrk-<pid>`) par tâche qui soumet, threads de notre
  processus. Deux classes : *bounded* (disque) et *unbounded* (réseau).
- Une seule file par pool : le premier worker libre prend l'op. **On ne choisit pas le
  worker.** Le noyau réveille de préférence celui qui vient de finir (cache chaud).
- **Création à la demande** : un worker naît quand une op entre dans la file alors que
  tous les workers existants sont occupés, sous le plafond. Sous débit soutenu, on monte
  vite au plafond. Un worker inactif quelques secondes (~5 s) s'arrête.
- Pour un socket, un worker **ne bloque jamais** : il tente sans bloquer et, sur
  `EAGAIN`, s'abonne aux événements du socket. Le nombre de workers actifs = travail CPU
  de TCP en parallèle.
- Les sockets ne sont pas sérialisés (contrairement aux écritures sur un même fichier) :
  deux sends sur une connexion peuvent tourner en même temps → **un send en vol par
  connexion** (§6.1).
- Avec `DEFER_TASKRUN`, le CQE d'une op finie par un worker n'est visible qu'au prochain
  `enter(GETEVENTS)`.

**Ce qu'on pilote :**
1. **Quoi** : `IOSQE_ASYNC` op par op.
2. **Combien** : `register_iowq_max_workers([bounded, unbounded])`, rappelable à chaud.
   Le noyau monte jusqu'au plafond ; c'est la boucle qui déplace le plafond.
3. **Où** : `register_iowq_aff(cpuset)`, commun à tous les workers. Ça ne réserve pas
   les cœurs : pour une vraie réservation, `cpuset` cgroup ou `isolcpus`.

**Ce que coûte la délégation, de notre côté :**
- sous charge : une mise en file (quelques centaines de ns), le worker est déjà réveillé ;
  on économise les ~2–3 µs de pile TCP du send ;
- à faible charge : réveil d'un worker endormi (1–2 µs) et passage de cache d'un cœur à
  l'autre. Mais à faible charge, on a du CPU libre.

Le coût suit donc la charge dans le bon sens : argument pour une règle simple.

**Pièges (2026-10-19), du plus important au moins important :**
1. **Quota du conteneur.** Les workers io-wq sont des threads de notre processus : leur
   CPU compte dans notre cgroup. Avec une limite à 1 CPU, déléguer ne libère rien et
   déclenche du throttling. Au démarrage, lire `cpu.max` et le cpuset : **en dessous de 2
   CPU de quota, ASYNC désactivé**.
2. **SMT en cloud.** 2 vCPU = souvent les deux threads SMT d'un seul cœur physique : le
   worker partage nos unités d'exécution, gain réel ~+30 %, pas ×2. Le bench cloud doit
   dire quels vCPU sont frères (`lscpu -e`).
3. **Où tournent les relances.** Sur `EAGAIN` (fenêtre du client pleine), la relance passe
   par un poll. Probable : le drapeau ASYNC est conservé (`REQ_F_FORCE_ASYNC`) et la
   relance repart sur io-wq. **À vérifier dans le source 7.2** : sinon les clients lents
   ramènent leur coût sur notre cœur.
4. **Verrou du socket.** Le worker qui envoie sur X et notre thread qui reçoit sur X
   (multishot) prennent le même `lock_sock`. Contention brève, à surveiller au profil.
5. **Effet sur les lots.** Déléguer raccourcit nos tours → moins de commandes par lot →
   préchargement moins efficace (§17). Les deux se mesurent ensemble.
6. **Limites d'environnement.** `pids.max` serré : un worker peut ne pas naître, le travail
   attend un worker existant (sans gravité avec un plafond de 1–2). L'affinité io-wq doit
   rester dans le cpuset du conteneur.

**Ce qui reste en direct :** le recv. Le multishot tourne dans notre `task_work` ; sur une
vraie carte, le gros de la réception (softirq) tourne sur le cœur de l'interruption, pas
forcément le nôtre (§16, IRQ).

**Le recv multishot ne peut pas aller sur io-wq (vérifié, source 7.3-rc, 2026-10-20).**
`io_wq_submit_work` (`io_uring.c`) : « Don't allow any multishot execution from io-wq » —
avec `DEFER_TASKRUN`, seuls nous pouvons poster ses CQE ; sur un socket non bloquant, le
worker arme un poll et rend la main, chaque arrivée s'exécute ensuite dans notre
`task_work` (`io_poll_issue`). Un recv **simple** en ASYNC part bien sur io-wq (réveil du
poll → `io_req_task_submit` → `io_queue_iowq`), mais chaque lecture repasse par nous
(relais, mise en file, CQE) et coûte un SQE : contre ~1 500–1 700 cycles/cmd de recv
économisés, gain net probable faible. Non retenu. **À rouvrir (2026-10-22)** : avec les envois
délégués, `io_recv` pèse 25 % du thread de la boucle (§12.7) ; le critère n'est plus le
débit maximal mais le CPU libéré sur la boucle à débit fixé (pour le métier, la save, le
GC). Prototype prévu : `RADIXOX_RECV_ASYNC=1`, statique comme `SEND_ASYNC`, un recv en vol
par connexion, buffer select sur notre anneau ; c'est alors le worker qui copie dans nos
tampons. Le type de buffer (`READ_FIXED`, enregistré) ne change pas qui copie. Ce qui reste à notre thread dans la
réception : la copie socket → tampon de l'anneau et le CQE, au moment où **nous** entrons
(le softirq TCP tourne ailleurs).

**Prototype mesuré (2026-10-24) : `RADIXOX_RECV_ASYNC=1`, perte nette, désactivé par
défaut.** Recv simple + `BUFFER_SELECT` sur notre anneau + `IOSQE_ASYNC` ; le cœur n'a pas
changé (un CQE `more = false` avec données réarme, comme un multishot terminé). Loopback,
Ryzen 7600X, `scripts/bench_zipf.sh`, Zipf 0,99, sans pipeline, 80 conns, sends délégués :

| | multishot | `RECV_ASYNC` |
|---|---|---|
| rx-3c (boucle + 2 workers) | 592 K, boucle 82 %, workers 100 % | 502 K, boucle 69 %, workers 99 % |
| rx-3c + `MIN_WAIT` 5/8 | 630 K (client saturé) | 445 K |
| rx-4c (boucle + 3 workers), client saturé | 469 K, boucle 69 %, workers ~53 % ; 2,28 cœurs | 471 K, boucle **86 %**, workers **~98 %** ; **3,79 cœurs** |

- `io_recv` disparaît bien du thread de la boucle (profil : 33 % de son temps avant), mais
  à débit égal la boucle coûte **plus** : un SQE de plus par commande (`io_submit_sqes`
  9 → 15 %), le réveil d'un worker par recv (`io_wq_enqueue`), des CQE qui arrivent un par
  un (sommeil / réveil 13 → 17 %).
- Sans pipeline, la socket est **toujours vide** quand le recv part (le client attend la
  réponse) : chaque recv paie la tentative à vide et l'armement d'un poll sur le worker
  (`io_arm_poll_handler`), puis la relance. Le relais n'est évité que si les données sont
  déjà là, c'est-à-dire en pipeline, où la boucle n'a pas besoin d'aide.
- **Tampons** : un worker io-wq qui choisit un tampon de l'anneau le réserve **en entier**
  dès le choix (sélection hors `uring_lock`), et le garde tant que le recv attend des
  données. Avec 64 × 32 Ko et 80 conns en pipeline 64 : `ENOBUFS` en boucle (le compteur
  `available` ne voit pas ces réservations), puis blocage même en attendant qu'un tampon
  revienne (cause non établie). Le mode met donc l'anneau à 1 024 × 4 Ko et ne réarme
  plus sur `ENOBUFS` (`Config::recv_oneshot`) : **au moins un tampon par connexion
  active** ; au-delà de ~1 000 conns actives, à revoir.
- Avec SQPOLL (§6.4 bis) en plus, rx-4s : sans pipeline 467 K (multishot) contre 372 K,
  pipeline 64 3,61 M contre 3,25 M. Le passage de main ne coûte plus rien à la boucle, mais
  les workers s'usent davantage (93–95 % contre 81–84 %) : perte aussi.
- Conclusion : io-wq n'est pas le bon outil pour la réception. Pour décharger le recv, il
  faut que chaque thread réseau garde un multishot sur **son propre anneau** (piste 4 du
  §17.6, threads réseau + SPSC). Lire la CQ sans `enter` (anneau sans `DEFER_TASKRUN`,
  CQE postés par les workers) ne change rien à ce coût et n'a pas été tenté.

**Ordre de mise en place, sans optimisation prématurée :**
1. `RADIXOX_SEND_ASYNC=off|on`, statique. Mesures à charge faible, moyenne et saturée,
   sur 2 vCPU, en loopback puis en cloud.
2. Seulement si les courbes se croisent vraiment : mode `auto`, règle ci-dessous.
3. Raffinements, chacun mesuré : seuil de taille (ne déléguer que les gros sends),
   plafond io-wq piloté à chaud.

**Règle `auto` (étape 2).** Signal de saturation gratuit : le drapeau `IORING_SQ_TASKRUN`
(option `TASKRUN_FLAG`), levé par le noyau quand des complétions attendent qu'on entre.
Avec `DEFER_TASKRUN`, regarder la CQ ne sert à rien (rien n'y est publié entre deux
`enter`).

```
fin de tour :
  si sq.taskrun()   → tours_charges += 1, tours_libres = 0
  sinon             → tours_libres += 1, tours_charges = 0
sends en ASYNC  si tours_charges ≥ 4  ou travail de fond en attente (save, GC, compression)
sends en direct si tours_libres  ≥ 16 et pas de travail de fond
plafond io-wq   : 1 par défaut ; +1 si on reste saturé alors que les sends sont délégués
                  et que leur délai soumission → CQE monte ; −1 quand ça retombe
```

- Seuils asymétriques (4 / 16, à régler) : on bascule vite, on revient lentement.
- Config de départ : 1 worker unbounded épinglé sur un cœur physique réservé (pas le
  frère SMT du nôtre) quand la machine le permet.
- Alternative écartée : notre propre thread d'envoi alimenté par `IORING_OP_MSG_RING`
  (contrôle total, mais un deuxième thread userland, contraire au mono-thread).

### 6.4 bis Mode SQPOLL (`RADIXOX_SQPOLL_CPU`, 2026-10-24)

Option pour qui accepte de brûler des cœurs : `RADIXOX_SQPOLL_CPU=<cpu>` crée l'anneau en
SQPOLL (thread noyau épinglé, `RADIXOX_SQPOLL_IDLE_MS`, défaut 1000), **sans
`DEFER_TASKRUN` ni `TASKRUN_FLAG`** (incompatibles). À combiner avec `RADIXOX_SEND_ASYNC` :
- le thread SQPOLL lit la SQ et exécute le travail des complétions de nos requêtes (le
  recv multishot, copie comprise, tourne dans **son** contexte) ; il écrit les CQE ;
- les workers io-wq font les envois ;
- la boucle ne fait **aucun appel système** tant qu'elle a du travail : elle écrit des SQE
  et lit la CQ en mémoire. Au repos, elle surveille la CQ pendant `RADIXOX_SQPOLL_SPIN_US`
  (défaut 1000) puis dort dans `enter(GETEVENTS)` (avec `SQ_WAKEUP` si le thread s'est
  endormi).
- Ce qui change dans le code (`uringloop.rs`, rien dans le cœur) : `build_ring`, le signal
  du travail au repos (`work_waiting` : CQ non vide au lieu d'`IORING_SQ_TASKRUN`), et
  `wait`. Testé : grosses valeurs, pipeline, BGSAVE.

Mesures (loopback, Ryzen 7600X, `scripts/bench_zipf.sh`, Zipf 0,99, 80 conns ; `rx-Ns` =
boucle, SQPOLL, N−2 workers d'envoi) :

| | rx-3c (boucle + 2 workers) | rx-3s (boucle + SQPOLL + 1 worker) |
|---|---|---|
| pipeline 64 | 3,62 M, p99 1,91 ms ; boucle 91 % dont noyau 15 | **4,00 M, p99 1,52 ms ; boucle 100 % user, 0 noyau** |
| sans pipeline | **646 K** (client saturé) | 299–310 K : le seul worker d'envoi sature (~300 K envois/s par worker) |

À 4 cœurs sans pipeline (client sur 4 CPU, saturé ou presque) : rx-4c 467 K, rx-4s 413 K,
rx-3c 405 K. L'occupation ne dit rien en SQPOLL (boucle et thread noyau en attente
active) ; au profil, le thread SQPOLL fait ~60 % de travail utile à 429 K ops/s
(`io_sq_tw` : recv, complétions ; le reste tourne à vide dans `io_sq_thread`) →
**plafond estimé ~700 K sans pipeline** pour un thread SQPOLL sur ce CPU (non mesuré
directement : le client sature avant).

Lecture : en pipeline, SQPOLL retire tout le noyau de la boucle (+10 % de débit, latence
meilleure). Sans pipeline, il ne parallélise pas la réception : un seul thread la fait
pour toutes les connexions. Le million sans pipeline passe par plusieurs threads réseau
(§17.6, piste 4). La ligne SQ_POLL du §14 reste vraie pour le mode par défaut.

**Coût par opération, recv contre send** (rx-4s sans pipeline, 440 K ops/s, `perf record -p`
4 s, 2026-10-24) :

| thread | CPU | rôle | coût / op |
|---|---|---|---|
| workers io-wq (×2) | 1,69 | send | **~3,8 µs** |
| SQPOLL | 1,03 (≈ 0,58 utile) | recv multishot en task_work, soumission | ~1,3 µs |
| boucle | 1,0 (attente active) | métier | — |

Le send coûte trois fois le recv **à cause du loopback** : le send exécute aussi toute la
réception du pair (`net_rx_action` → `process_backlog` → `tcp_v4_rcv`, 40 % du temps des
workers) et netfilter deux fois (`nf_hook_slow`, 26 % : conntrack de firewalld sur `lo`).
Symétriquement, le send de memtier paie notre traitement TCP de réception, et notre recv
n'est plus qu'une copie depuis la file. Sur une vraie carte, le send s'arrête au pilote et
la réception TCP passe en softirq sur le CPU de l'IRQ (RSS) : la répartition change, à
mesurer à deux machines. Multishot sous SQPOLL : la requête appartient au thread SQPOLL,
le réveil du poll y pose le task_work (`io_sq_tw` → `io_poll_task_func` → `io_recv`), la
requête reste armée ; elle ne peut pas aller sur io-wq (la version simple + ASYNC perd,
§6.4).

### 6.5 Signaux de diagnostic (exposés dans `INFO`)

| Signal | Mesure |
|---|---|
| occupation de la boucle | temps de traitement / temps total (attente dans `enter` = repos) |
| part noyau de notre cœur | durée des `enter` sans attente, ou `getrusage(RUSAGE_THREAD)` toutes les 100 ms |
| commandes par recv | compteur du parseur = profondeur réelle du pipeline client |
| sends délégués | part en ASYNC, délai soumission → CQE, plafond io-wq courant |
| backpressure | connexions bloquées sur la limite d'`out` |

| Occupation | Noyau | Cmd/recv | Diagnostic | Action |
|---|---|---|---|---|
| faible | — | — | tranquille | sends en direct |
| ~100 % | ≥ 60 % | ~1 | clients sans pipeline, limite = TCP | délégation io-wq ; conseiller l'auto-pipelining côté client |
| ~100 % | faible | élevé | pipeline, limite = ART + mémoire | préchargement par lots (§8bis) ; déléguer ne sert presque à rien |
| ~100 % | élevé | élevé | les deux | délégation + préchargement |

Le serveur peut écrire ce diagnostic dans ses logs (« le noyau prend 80 % du cœur, clients
sans pipeline : activer l'auto-pipelining côté client ou la délégation des envois »).

---

## 7. Connexions parquées

Un mécanisme unique pour KEYS, UNLINK, FLUSHDB pendant une save, et BLPOP / BRPOP plus
tard.

- La connexion passe en `Mode::Parked(reason)`, `epoch += 1`. Sa file de frames est
  recopiée dans `rx` (§5.4) : un parquage ne retient jamais l'anneau.
- Le recv reste armé : on détecte le FIN, et les commandes suivantes sont **copiées** dans
  `rx` sans être exécutées (même sémantique que Redis : un QUIT envoyé pendant un BLPOP
  attend le réveil).
- Celui qui termine écrit la réponse (ou la pousse dans `ready`, §8), repasse la connexion
  en `Normal` et la met dans la liste des connexions **à rejouer au tour suivant** — jamais
  dans le handler : une commande rejouée peut reparquer la connexion. Le job à la fin de
  sa dernière tranche, LPUSH pour BLPOP, le timer pour un timeout.
- Si la connexion meurt pendant le parquage : `Draining`. Un job qui la vise garde sa
  référence (`refs`) jusqu'à sa fin et n'écrit rien.

Remplace le « transfert complet + respawn de task » pour BLPOP de l'ancien `CONN_DESIGN.md`
(supprimé le 2026-10-19, voir git), et les commandes async de `delayed.rs`. Même esprit (proactif, sans waker, sans oneshot),
sans aucune tâche.

### 7.1 Multishot et parquage : pas de `select!` (2026-10-12)

Avec monoio, une connexion en BLPOP attendait trois choses à la fois (réveil, timeout,
lecture pour QUIT / FIN) : d'où le `select!`. Ici ce sont trois événements qui arrivent
seuls, et **celui qui produit l'événement fait la transition** :

| Événement | Source | Transition |
|---|---|---|
| octets reçus | CQE du recv multishot (toujours armé) | copiés dans `rx`, non exécutés |
| fin de connexion | CQE recv `res == 0` / erreur | `Draining` |
| LPUSH sur la clé | le LPUSH, même thread | réponse dans `out`, `Normal`, `rx` rejoué |
| échéance | tas de timers | réponse `nil`, `Normal`, `rx` rejoué |

Le multishot reste : c'est la moitié noyau de la logique proactive (la lecture est toujours
en vol), les machines à états la moitié userland.

**Pièges et parades :**
1. **Le multishot lit pendant le parquage** : un client parqué qui continue d'envoyer fait
   grossir `rx` sans fin, et un recv multishot ne se met pas en pause. Au-delà d'une limite
   de `rx` : **annuler le recv** de la connexion (`ASYNC_CANCEL`, elle garde son `refs`),
   réarmer quand elle repart. Ne jamais compter sur `ENOBUFS` pour freiner : l'anneau est
   partagé, ce serait affamer toutes les connexions. Même règle pour la backpressure §6.2 :
   « arrêter de parser » ne suffit pas tant que le recv reste armé.
2. **Slot réutilisé** et 3. **événement d'un parquage précédent** : réglés par la même
   règle (2026-10-19). Timers et inscriptions portent `(slot, epoch)` ; l'epoch du slot est
   monotone à travers les réutilisations (§4.3), donc une entrée qui ne correspond plus
   est ignorée, que le slot ait été redonné ou que la connexion ait été reparquée. Les
   inscriptions ne prennent pas de `refs`. Règle aussi BLPOP multi-clés : inscrit dans
   plusieurs files, le premier réveil gagne, les autres inscriptions deviennent obsolètes
   et sont retirées paresseusement. Le tas de timers est compacté quand plus de la moitié
   de ses entrées sont obsolètes.

---

## 8. Jobs

```rust
enum Job {
    BgSave(SaveState),                  // DFS + compression + vidage (bgsave.rs)
    Keys { slot, cursor, acc },
    Unlink { slot, cursor, count },
    FlushDb(DelnCursor),
    Evict(TagCursor),                   // project-evictor-cursor-scan
    Gc(GcCursor),                       // plus tard
}
```

- `step(&mut Server, budget) -> Done | More`. Une tranche par tour, **tourniquet simple
  en v1** (décision 2026-10-19).
- Plus tard, si les mesures le justifient : **file à rétroaction multi-niveaux** (idée de
  l'utilisateur, 2026-10-19). Un job commence « léger » (ou dans la classe donnée par son
  coût estimé, §8.1), descend en « semi-lourd » au-delà de ~0,5 ms consommée, en « lourd »
  au-delà de ~5 ms ; tourniquet pondéré 4:2:1 entre classes. Approxime « servir d'abord
  ce qui a le moins consommé » : les jobs courts (KEYS sur un petit préfixe) finissent
  vite, les longs ne sont jamais affamés. Avec : classe à part pour le travail de fond
  (save, évicteur, libérations, GC : plafond de CPU, part minimale garantie pour la save),
  héritage de priorité quand des écrivains attendent un verrou de lecture (§8.1), et
  attente par classe dans `INFO`.
- Budget minimal par tour même sous charge (sinon un serveur toujours occupé ne nettoie
  jamais), budget large quand il n'y a ni CQE ni send en attente.
- À réécrire en curseurs sync : `deln_async` / `getn_async` / `SaveDone` d'oxidart
  (`async_command.rs`). `save_step` a déjà la bonne forme.
- I/O fichier de la save : ops `Write` à offset sur un fd classique (kind `File`, slot =
  job), `Fsync` à la fin. Le double tampon de `bgsave.rs` (`fill` / `spare` / `in_flight`)
  se garde tel quel.
- Une chose à la fois : pas de compactage HiSlab pendant une save ; le GC des `SharedByte` continue, en sautant valart (MEMORY_ARCH.md §8.5, 2026-10-22).
- `save.waiters: Vec<Waker>` d'oxidart (`SaveDone`) disparaît : le job UNLINK teste
  `is_saving()` à chaque `step`. oxidart ne dépend plus des Waker.
- Le plafond de CPU de BGSAVE (`RADIXOX_SAVE_CPU_SHARE`) garde sa règle, mais le signal
  d'activité devient exact : un tour sans CQE ni connexion sale = repos (remplace le
  compteur `READS` + 1 ms sans lecture).

**Résultat produit au fil de l'eau, en octets (2026-10-19).** Un job encode sa réponse
dans son propre `Vec<u8>` pendant le parcours, pas dans un `Vec<SharedByte>` reconstitué
à la fin : le memcpy se superpose aux miss du parcours suivant, et on n'écrit pas de
refcount dans des lignes froides.
- **En-tête RESP inconnu** (`*N\r\n` de KEYS) : 23 octets réservés au début, en-tête écrit
  calé à droite à la fin, envoi à partir de l'offset. Pas de recopie. RESP2 n'a pas de
  tableau en flux : `acc` de KEYS reste non borné (comme Redis), ou limite configurable.
- **Injection** : au `Done`, le job pousse `(slot, epoch, buf)` dans `ready`. Traité après
  le lot : si l'`out` de la connexion est vide, échange direct des tampons (cas courant :
  une connexion parquée ne produit rien d'autre) ; sinon ajout à la suite. Epoch obsolète
  → résultat jeté.

### 8.1 Jamais bloquant (2026-10-19)

Ce qu'on reproche à Redis n'est pas d'être 5 % plus lent : c'est le p99 qui explose quand
un KEYS, un DEL de gros hash ou un HGETALL de 1 M de champs fige tout le serveur (d'où
UNLINK, lazyfree, SCAN chez Redis). Principe : **toute commande peut prendre du temps,
aucune ne bloque les autres.** Le chemin rapide des petites commandes ne change pas.

- **Seuil** : une commande dont le coût estimé dépasse ~1 ms (~10 K éléments, réglable)
  devient un job. Le coût est connu avant de commencer (HLEN, nombre de clés d'un MGET,
  `count` de KRANGE, table §2.2).
- **Atomicité : verrou de lecture par clé.** Un HGETALL produit en tranches pourrait voir un
  HSET au milieu (réponse à moitié ancienne). Le job déclare ses clés ; une écriture sur
  l'une d'elles **parque l'écrivain** jusqu'à la fin du job (le collecteur d'effets, §18,
  est le point de contrôle : toute écriture y passe). Écritures sur les autres clés et
  lectures : normales. Chez Redis, c'est tout le serveur qui attend.
  KEYS et SCAN restent « au mieux » (comme Redis), sans verrou.
- **Libérations lourdes** : DEL d'une grosse valeur → valeur détachée, réponse immédiate,
  libération par un job. L'équivalent de lazyfree, par défaut.
- **SAVE** = BGSAVE + connexion demandeuse parquée jusqu'à la fin : synchrone pour elle,
  rien de bloqué pour les autres. Seule la save d'arrêt reste bloquante.
- Implémentation après le prototype ; le principe guide dès maintenant la table §2.2 et
  le collecteur d'effets.

---

### 8.2 Travail de fond au repos (2026-10-20)

Le travail de fond (jobs de connexion parquée, libérations, tranches de BGSAVE) avance de
deux façons :

1. **Pendant le tour** (`run_jobs`, inchangé) : une étape de chaque job, et les tranches de
   BGSAVE sous le plafond de CPU (`RADIXOX_SAVE_CPU_SHARE`). C'est le **plancher** quand la
   boucle est saturée.
2. **Au repos** (`idle_work`, binaire) : quand le cœur n'a plus rien pour les clients
   (`quiet` : ni op à soumettre, ni connexion prête, ni réponse à envoyer) et qu'il reste du
   travail de fond (`has_background`), la boucle soumet les réponses du tour puis enchaîne
   des `idle_step()` **sans plafond** tant que :
   - `IORING_SQ_TASKRUN` est à 0 : avec `DEFER_TASKRUN` + `TASKRUN_FLAG`, le noyau le lève
     (`atomic_or` dans `io_req_local_work_add`, `io_uring/tw.c`, vérifié dans le source)
     dès qu'un recv, un send délégué ou une écriture de fichier a du travail pour nous ; le
     travail lui-même ne s'exécute que dans notre `enter(GETEVENTS)`, rien ne nous
     interrompt ;
   - le cœur reste `quiet` (un job fini a une réponse : elle part d'abord) ;
   - ni la prochaine échéance de timer ni `IDLE_WORK_MAX` (1 ms) ne sont passées, lues au
     TSC (`radixox::tsc`, ~7 ns la lecture).

   Ordre d'une étape : un client parqué, puis une libération, puis une tranche de save. Une
   requête qui arrive pendant une étape attend au plus la fin de l'étape (≤ tranche max).

- **Le tour n'est qu'un plancher.** Le travail fait au repos compte dans la part : le tour
  ne fait de tranches de save que si la save a eu moins de `RADIXOX_SAVE_CPU_SHARE` du temps
  écoulé, repos compris. Erreur corrigée le jour même : ne compter que le temps « disputé »
  laissait le tour se croire toujours sous sa part et refaire 250 µs de save à chaque
  rafale de requêtes (1 cœur, 165 K ops/s, save de 105 Mo : p99.9 0,80–0,96 → 0,49–0,51 ms,
  p99.99 1,7–2,4 → 0,87–1,29 ms, durée inchangée).
- Reste sur 1 cœur : quelques tranches de 300–550 µs, toutes avec une écriture de 2 Mo en
  vol. Probable : l'écriture bufferisée ext4 qui doit allouer des blocs part sur un worker
  io-wq qui hérite de notre affinité (`iou-wrk` vu sur le CPU 0 pendant la save) et préempte
  la boucle le temps de copier 2 Mo dans le cache de pages ; le fsync aussi. Pistes :
  morceaux plus petits, `O_DIRECT` (DMA depuis nos tampons, plus de copie CPU), ou workers
  de fichiers épinglés ailleurs quand un cœur est libre.
- **Fait : `RADIXOX_FILE_CPUS`** (optionnel) : écritures du dump en `IOSQE_ASYNC`, un worker
  « bounded » épinglé (`register_iowq_aff`, masque unique du ring = union avec
  `RADIXOX_IOWQ_CPUS`). Vérifié : `iou-wrk` sur le CPU 1 même sous `taskset -c 0`. 1 cœur,
  165 K ops/s, deux runs : tranche max 484–844 → 102–104 µs, p99.99 1,17–1,31 → 0,82–1,02 ms
  (Valkey, fils sur le CPU 1, même session : 1,35–1,41), ~0,3 cœur·s de CPU 1 par save
  (Valkey ~2,5). Reste ~0,3 ms au-dessus du p99.99 sans save.
- Corrigé au passage : `idle()` voyait « la save veut une tranche » sans voir le plafond,
  la boucle tournait sans dormir ni avancer (`submit` non bloquants) quand la save avait
  atteint sa part ; et `wants_slice` restait vrai quand le DFS attendait le disque.
- Faux noyau : `Fake::idle_work` (moitié des graines de l'oracle aléatoire, test
  `idle_work_runs_jobs_until_someone_waits`).

**Mesure** (p1, 78 conns, 1:4, 1 M clés de 100 o, BGSAVE à t = 3 s, boucle sur CPU 0,
2 workers sur 1-2 ; un run par point, deux ordres) :

| | save | CPU save | tranche max | débit | p99 / p99.9 / p99.99 |
|---|---|---|---|---|---|
| avant | 877–896 ms | 88–90 ms | 326–328 µs | 565–572 K | 0,207–0,231 / 0,239–0,271 / 0,68–0,76 ms |
| **après** | **158–159 ms** | 64–65 ms | 311–417 µs | 517–568 K | 0,207 / 0,247–0,263 / 0,64–0,73 ms |

Save ÷5,6, latence inchangée. Mono-cœur sans délégation (boucle saturée) : 683 → 712 ms,
attendu (pas de temps libre). Débit : l'écart de ±10 % suit la position du run, pas le
binaire (runs sans save alternés : 570 / 569 / 516 / 513 K). Le dump fait 0,1 Mo (valeurs
memtier constantes, compressées par zstd) : pas d'attente disque dans cette mesure.

## 8bis. Préchargement par lots et chantiers de l'arbre

Constat (§12) : en accès aléatoire, RadixOx passe 84–88 % de ses cycles user à attendre la
mémoire ; le calcul d'un GET est quasi nul (47 ns tout en cache, 423–474 ns en aléatoire,
mêmes 894 instructions).

**Fait (commit 7dac760, serveur monoio).** `OxidArt::prefetch_keys`
(`oxidart/src/prefetch_batch.rs`) : K clés descendent étage par étage, chacune précharge
son nœud suivant, puis l'overflow, puis la valeur. Rien n'est gardé ni écrit. `handle_buffer`
parse par fenêtres de K commandes, précharge `Cmd::main_key`, exécute dans l'ordre.
`RADIXOX_PREFETCH` (défaut **8**, 0 = off). Au-delà de 8–10 clés, ça se dégrade (cause non
établie : THP déjà actif, TLB non confirmée).

**Dans la boucle maison :**
- Le lot porte sur **toutes les connexions du tour**, pas seulement sur le pipeline d'une
  connexion : l'ordre est gardé par connexion, les connexions différentes sont concurrentes.
  Ça profite donc aussi aux clients sans pipeline.
- Les `Cmd<'tour>` du lot empruntent l'anneau pendant le tour ; d'un tour à l'autre,
  seules des `FrameRef` comptées retiennent les tampons (§5.4).
- Équité : tirage par tours (§2.1) ; le préchargement se fait par fenêtres à l'intérieur
  d'un tour de tirage, entre connexions.

**Suite (par ordre) :**
1. Étendre `main_key` aux commandes multi-clés (MGET, MSET, DEL, EXISTS) et aux Set/ZSet.
   **Fait autrement (128f393)** : MGET et EXISTS font leur propre descente entrelacée
   (`descend_batch`, paquets de 64) ; DEL et MSET appellent `prefetch_keys` par paquet.
   `main_key` reste mono-clé. Set/ZSet : pas encore.
2. GET par référence : le clone de la valeur écrit le refcount dans une ligne froide
   (~24 % des miss DRAM). **Fait pour MGET et KRANGE** (`mget_ref`, `krange` : `&SharedByte`
   encodé pendant l'emprunt, conversion possédée sur place si besoin) ; GET simple : pas encore.
3. **Réutilisation des idx + `invalidate(idx)`** : un seul parcours qui garde l'idx final.
   **Fait pour MGET / EXISTS (128f393)** : `descend_batch` rend l'idx, lecture pure, les
   expirées croisées sont purgées après relâche des références (`purge_expired`). Reste à
   l'appliquer au lot du pipeline (§17.2).
   Signalé par `remove_node`, `delete_node_inline`, `try_recompress` (la valeur de l'enfant
   passe à l'idx du parent), `split_node`, plus tard compactage et GC. Scan linéaire de la
   fenêtre (8 × u32), pas de tri, exécution dans l'ordre. Entrée invalidée → descente
   normale, dans un cache chaud. Le parcours est en lecture seule pure (pas de suppression
   d'expiré). Mode debug qui revérifie chaque idx par une vraie descente. À mesurer contre
   le préchargement seul : la seconde descente coûte du calcul, pas des miss (lignes encore
   en L1).
4. `prefetchnta` pour les données des valeurs (lues une fois) : sur Zen, L3 est un victim
   cache, une valeur évincée de L2 y est installée et chasse les nœuds du haut de l'arbre.
5. Recherche d'enfant sans branchement (SWAR sur les 6 radix inline, SSE2 sur l'overflow) :
   rentable une fois les miss superposés. **Fait (aa93749)** : SWAR partout (inline, overflow
   léger, groupes `[[u8;8][u32;8]]` du huge), SSE2 inutile ; +8,8 % en pipeline 64.

**Chantiers de layout (prototype + bench avant code définitif) :**
- **Feuille fusionnée avec sa valeur** : l'entrée d'enfant (u32, bit de tag) désigne
  directement un bloc `[exp?|suffixe de clé|rc|len|données]`. Un miss de moins par GET,
  ~64 o de moins par clé (−600 Mo sur la base de 10 M). Demande des valeurs adressables sur
  32 bits → allocateur bump. Points durs : promotion feuille → nœud au split, rétrogradation,
  évicteur TTL (le tag est aujourd'hui sur les nœuds), réécriture par le GC. Étape
  intermédiaire possible : slab de feuilles compactes de 16–24 o.
- **Nœuds de 128 o à 16 enfants inline** : plus d'overflow pour les clés décimales ou hex
  (un saut dépendant de moins par étage) ; même mémoire que 64 o + overflow sur les IDs
  numériques, mais double coût sur les nœuds à faible fan-out. À trancher sur 3 formes de
  clés (IDs numériques, clés hiérarchiques, tokens/UUID).
- Fait : descente sans bitmap d'occupation (`child_node`, −3 %).

---

## 9. Temps, timers, signaux

- **Deux horloges, lues une fois par tour** (vDSO, quelques dizaines de ns amorties) :
  - **murale** pour le TTL → `art.set_now`. C'est déjà le cas (`tick()` lit `SystemTime`),
    et c'est ce qu'il faut : les échéances du dump sont absolues, comme chez Redis. Un saut
    d'horloge en arrière prolonge des TTL, sans casser quoi que ce soit ;
  - **monotone** pour les timers internes.
  Plus de tâche ticker.
- **Budgets de tranche** mesurés au TSC (`radixox::tsc` : `rdtsc` étalonné au démarrage,
  ~7 ns la lecture ; repli sur `Instant` sans TSC invariant). Fait pour les tranches de
  BGSAVE et le travail de fond au repos (§8.2). `*_COARSE` (3 ns) n'a qu'un tick de
  résolution (1–4 ms) : bon pour le TTL et l'inactivité, pas pour un budget.
- **Timers** : petit tas binaire (deadlines BLPOP, autosave, fsync AOF chaque seconde,
  balayage), suppression paresseuse par `(slot, epoch)`. Le timeout de l'`enter` bloquant =
  la prochaine échéance, sinon l'horloge gèlerait au repos.
- **Inactivité et clients lents à envoyer** : **pas un timer par connexion** (64 K entrées
  mises à jour à chaque commande). `last_active` en secondes par connexion, et un job de
  balayage incrémental, comme le `clientsCron` de Redis, qui coupe aussi les frames
  partielles qui n'avancent plus.
- **Évicteur TTL** : job à curseur sur le bitmap tagué (remplace le tirage statistique).
- **Signaux** : `signalfd` lue par io_uring (kind `Signal`). Remplace le sondage toutes les
  20 ms de `shutdown.rs`. ⚠️ Pendant la save d'arrêt (bloquante), la boucle ne lit plus la
  signalfd : avant de la lancer, débloquer les signaux et réinstaller le gestionnaire
  `_exit` actuel, pour qu'un second signal sorte tout de suite.

---

## 10. Pub/Sub

- PUBLISH encode le message une fois, l'ajoute à l'`out` de chaque abonné et le marque
  sale. Pas de writer task, de channel ni de `CancelationFutur`.
- **Limite d'`out` par abonné** → `Draining` (équivalent de `client-output-buffer-limit
  pubsub` : 32 Mo dur, 8 Mo pendant 60 s). Règle le TODO OOM sur abonné lent. Le budget
  global d'`out` (§5.3) s'applique aussi.
- Plus tard : un bloc partagé par refcount + `sendmsg` multi-iovec, au lieu de N memcpy.

---

## 11. Versions noyau

**Décision (2026-10-19) : noyau ≥ 6.12, anneau incrémental obligatoire, pas de mode
dégradé.** Quelqu'un qui déploie un serveur io_uring en 2026 accepte un noyau de 2024.

| Fonction | Noyau min. |
|---|---|
| recv multishot, anneau de tampons | 6.0 / 5.19 |
| accept multishot | 5.19 |
| `DEFER_TASKRUN` | 6.1 |
| bundles | 6.10 |
| anneau incrémental (`IOU_PBUF_RING_INC`) | **6.12 — minimum retenu** |
| `NO_IOWAIT` (optionnel) | 6.15 |
| zcrx (`RecvZc`, optionnel) | 6.15 |
| Machine de dev | 7.2-zen ✓ |

Distributions (à revérifier au moment du README) : Ubuntu 24.04 sort en 6.8, mais ses
noyaux HWE suivent (24.04.2 → 6.11, 24.04.3 → 6.14) et les images cloud aussi ; Ubuntu
26.04, Debian 13 (6.12), RHEL 10 (6.12), Amazon Linux 2023 (6.12 disponible) passent.
Debian 12 et RHEL 9 non.

**io_uring ou rien.** Bloqué par le seccomp par défaut de Docker, ou par
`kernel.io_uring_disabled` (GKE/COS, noyaux durcis) : détection au démarrage, message
clair (« lancez avec `--security-opt seccomp=…` »), une section dans le README. Pas de
repli epoll.

---

## 12. Référence perf (profil du 2026-10-11, monoio)

memtier 4 threads × 20 conns, ratio 1:4, 200 K clés, serveur `taskset -c 0`,
`perf record -g -p`. Pipeline 1 : 228 K ops/s. Pipeline 32 : 3,76 M ops/s.

| Poste | p1 | p32 |
|---|---|---|
| send : pile TCP/IP | 26 % | 15 % |
| send : réception TCP du client (loopback) | 26 % | 16 % |
| netfilter (firewalld) | 23 % | 13 % |
| io_uring (soumission, CQE, task work) | ~12 % | ~8 % |
| recv | ~8 % | ~5 % |
| copies | 2,5 % | 2,5 % |
| monoio (user) | 2,5 % | 1,6 % |
| RadixOx (user) | 2,9 % | 34 % |

Lecture :
- Le coût est par paquet, pas par octet.
- La boucle maison gagne directement monoio et une partie d'io_uring (~5–10 % en p1).
- Le gros levier sur notre cœur est la délégation du send (§6.4).
- En pipeline, le goulot devient l'ART (`get_idx`).

Le prototype doit battre ce tableau poste par poste. Biais du loopback : la réception du
client et netfilter sont facturées au serveur. Le bench final se fera sur deux VM cloud
dans la même zone.

### 12.1 Méthode de bench (leçons du 2026-10-11)

- **memtier : toujours `--distinct-client-seed --randomize`.** Sinon tous les clients tirent
  la même suite de clés : localité artificielle, débit pipeliné surestimé ×3 (3,6 M contre
  1,16 M ops/s sur 1 M clés). Les chiffres memtier antérieurs, y compris contre Valkey, sont
  à refaire. `scripts/bench_vs_valkey.sh` passe l'option ; les anciens scripts à la racine
  (`bench_*.sh`) non.
- **memtier 2.4.2 : `--key-pattern=Z` est inutilisable** (quasi uniforme quel que soit
  l'exposant). Zipf réaliste : YCSB (scrambled zipfian) ou générateur maison.
- **Base de 10 M clés réutilisable** : `user:10000000..19999999`, valeurs 100–300 o,
  `target/bench_dump/dump.rxd` (2 Go), reload 0,78 s avec `RADIXOX_DIR=target/bench_dump`.
- Préciser comment l'arbre a été construit : un reload ou un load séquentiel donne des
  nœuds contigus (ordre DFS), des SET aléatoires des nœuds dispersés.
- **Topologie CPU** (Ryzen 7600X : cœur physique n = CPU n et n+6) : serveur épinglé
  (`RADIXOX_CPU_PIN`), client sur d'autres cœurs physiques, jamais sur le frère SMT du
  serveur.
- **`bench_compare.sh` (YCSB A, 2026-10-22)** : tient le profil `performance` pendant le
  bench (`powerprofilesctl launch`, `PROFILE=keep` pour ne pas y toucher ; −15 % d'horloge et
  −10 % de débit en `power-saver`, boost coupé) ; centiles et max du run calculés sans les
  `WARMUP=2` premières secondes (chauffe de la JVM) à partir des histogrammes par seconde ;
  occupation par CPU depuis `/proc/stat` (softirq compris), user/noyau par CPU, ops par
  seconde-CPU, fréquence tenue (`cpuinfo_avg_freq`) du serveur et du client, avertissement
  quand YCSB dépasse 60 % de ses CPU logiques (`CLIENT_SATURATED`). À 3 cœurs par serveur,
  **YCSB est la limite** (Java, 6 CPU logiques sur 3 cœurs) : comparer alors l'efficacité par
  CPU, pas le débit ; pour la capacité, memtier ou deux machines. Les variables `RADIXOX_*`
  du shell passent à RadixOx. Résultats de référence : §12.7.
- **Clients Node.js** (ioredis) : les pauses du GC de V8 (~40 ms) produisent des rafales
  et un p99.9 élevé, chez Valkey comme chez nous. Pour la latence : memtier, YCSB ou un
  client Rust.
- **Valkey io-threads** : n'aide pas sous ~50 connexions (coût de passage de relais), aide
  nettement à partir de ~80. Comparer à ressources égales et préciser le nombre de
  connexions.
- (Leçons reprises de l'ancien `BENCH_ANALYSIS.md` du 2026-06, supprimé le 2026-10-19 ;
  ses chiffres étaient biaisés par l'absence de `--distinct-client-seed`.)

### 12.2 Comparaison Valkey 9.1.2 (base 10 M, uniforme, 1:4, 1 cœur chacun)

| | p1 (80 conns) | p32 | p256 | RSS | rechargement |
|---|---|---|---|---|---|
| RadixOx avant préchargement | — | ~935 K | 1,21 M | 3,33 Go | 0,78 s |
| RadixOx préchargement 8 | 196 K (82 % noyau) | 1,09–1,21 M | **1,81 M** | 3,33 Go | 0,78 s |
| Valkey 1 thread | — | **1,32 M** | 1,69 M | **2,58 Go** | 6,5 s (SAVE 18 s) |
| Valkey io-threads 2 (2 cœurs) | — | 2,06 M | 2,81 M | — | — |

Points faibles : mémoire (~333 contre ~258 o/clé), pipeline court. Points forts : pipeline
profond, persistance. Écart sans pipeline / pipeline 64 : ×7,4.

### 12.3 SQ_POLL sans pipeline (2026-10-19, monoio, loopback)

80 conns (memtier 8 threads × 10 sur CPU 2-5,8-11), 1:4, 1 M clés, 10 s par run, deux
passes, profil `performance`. Serveur `RADIXOX_CPU_PIN=0` ; SQ_POLL épinglé sur le CPU 1.
Occupation par CPU lue dans `/proc/stat` (softirq compris, non compté dans le processus).

| | ops/s | latence moy. | p99 | CPU 0 (serveur) | CPU 1 |
|---|---|---|---|---|---|
| sans SQ_POLL | 281 K / 278 K | 0,316–0,319 ms | 0,41–0,43 ms | 91 % : user 8, noyau 50, softirq 33 | libre |
| SQ_POLL | 277 K / 280 K | 0,286–0,289 ms | 0,375–0,391 ms | 16 % (user 13) | 100 % : noyau 61, softirq 39 |

Client à 76 % inactif dans les deux cas. Lecture :
- Le partage est **~90:10** noyau/user une fois le softirq compté, pas 80:20.
- SQ_POLL **déplace** tout le TCP sur un seul thread noyau, il ne le **partage** pas : le
  débit plafonne au même niveau, le thread principal attend (84 % inactif). Seul gain :
  −10 % de latence moyenne, −13 % au p99, au prix d'un cœur qui tourne en boucle (son
  100 % inclut de l'attente active).
- Conclusion : pour gagner du débit sans pipeline, il faut **paralléliser** le TCP
  (io-wq ≥ 2 workers, §6.4) ; confirme l'abandon de SQ_POLL (§14).

### 12.4 Étape 2 : radixox-uring contre monoio (2026-10-19, loopback, 1 cœur)

80 conns (memtier 8 × 10, CPU 2-5,8-11), 1:4, 1 M clés, 100 o, graines distinctes, 8 s,
profil `performance`, un run par point (bruit ±5 % ; un run monoio p256 à 3,23 M dans une
autre série n'a pas été reproduit : 2,54 M dans la même série).

| | p1 | p32 | p256 |
|---|---|---|---|
| monoio | 241 K, moy. 0,333 ms | 2,31 M | 2,54 M |
| radixox-uring | **265 K, moy. 0,302 ms** | 2,32 M | **2,84 M (+12 %)** |

Compteurs par commande en p256 (`perf stat`, sous perf) :

| | instr. user | cycles user | défauts L1 | cycles noyau |
|---|---|---|---|---|
| monoio | 3 917 | 1 765 | 29,4 | 296 |
| uring, découpage à l'arrivée, anneau 8 Mo | 4 425 | 2 055 | 43,4 | 269 |
| uring, une passe au lot, anneau 8 Mo | 4 139 | 2 000 | 40,8 | 406 |
| **uring, une passe au lot, anneau 2 Mo (64 × 32 Ko)** | **3 563** | **1 621** | 33,8 | 287 |

Leçons : (1) parser au moment d'exécuter, par fenêtres (§5.4) ; (2) l'anneau doit être
petit et ses tampons rendus vite (§5.1) ; (3) les défauts L1 de l'arbre sont identiques
entre les deux binaires : l'écart venait entièrement de la plomberie.

**YCSB workload A** (⚠ phase *run* sur 1 000 clés seulement : `workloada` fixe `recordcount=1000` et `bench_compare.sh` ne le remplaçait pas pour le run — corrigé le 2026-10-19, à refaire ; même biais pour les deux serveurs) (`bench_compare.sh`, 5 M hashes de 100 o, 10 M ops 50/50, 100
threads, RadixOx sur CPU 0-1, un run chacun, lancé par l'utilisateur le 2026-10-19).
Les deux runs n'ont pas le même profil d'énergie : comparer l'écart à Valkey du même run.

| | monoio vs Valkey (power-saver) | radixox-uring vs Valkey (performance) |
|---|---|---|
| load | 91 K vs 68 K (+34 %), p99 1 335 vs 2 841 µs | **115 K vs 76 K (+51 %)**, p99 1 216 vs 2 535 µs |
| run | 204 K vs 173 K (+18 %) | **258 K vs 199 K (+30 %)** |
| lecture moy. / p99 | 488 / 633 vs 574 / 1 121 µs | **386 / 570** vs 500 / 971 µs |
| lecture p99.9 / p99.99 | 715 / 1 029 vs 1 235 / 1 382 µs | 660 / 1 007 vs 892 / 1 065 µs |
| RSS | 1 555 vs 2 054 Mo | 1 557 vs 2 053 Mo |

La boucle creuse l'écart en débit et en latence moyenne ; les queues extrêmes restent
devant Valkey avec moins de marge qu'avec monoio (un run : à surveiller).

**Avant bascule : queues de latence et BGSAVE (2026-10-19, p32 / p256, 1 cœur).**
Mesures du critère « p99.99 sous BGSAVE » ; chaque ligne corrige un défaut trouvé.

| | p32 sans save : débit, p99, p99.9 | p32 + BGSAVE : p99, p99.9, p99.99 | p256 : débit, p99 |
|---|---|---|---|
| monoio | 2,06 M, 1,94, 2,22 ms | 2,06–2,10, 2,53–2,58, 10,8–10,9 ms | 2,54 M, 8,8 ms |
| uring, tours de 1024 cmd, tranche de save après l'`enter` | 2,47 M, 2,37, 3,01 | 3,18, 5,12, 14,0 | 2,84 M, 10,7 |
| + envois du tour avant la tranche de fond | | 2,46, 3,15, 15,9 | |
| + pas d'`enter` en plus quand la tranche est plafonnée | | 2,40, 3,14, 12,4 | |
| + tours de 4096 cmd | 2,20 M, 1,62, 1,87 | 2,03, 2,38, 14,1 | 2,79 M, 12,3 |
| + plusieurs tranches de save par tour (jusqu'à sa part) | | 1,78, 2,11, 9,8 | |
| + une connexion à la fois, envois entre tranches de 2048 | 2,41 M, 1,55 | 1,69, 2,22, 9,0 | 2,58 M, 13,1 |
| **+ ordre d'arrivée, sans rotation** | **2,08 M, 1,46** | (1,69, 2,22, 9,0) | **2,83 M, 9,5** |

Leçons, contraires au design initial (§2.1, §2.3) :
- **des tours courts allongent la queue** : toutes les réponses d'un tour partent ensemble,
  une connexion servie au tour suivant attend un tour de plus. Le tour sert toutes les
  connexions prêtes, par tranches de 2048 commandes entre lesquelles les réponses prêtes
  partent ;
- **servir une connexion d'un bloc** (fenêtres de 8, une place par autre connexion prête)
  plutôt qu'entrelacer 8 par 8 : sinon tous les lots finissent en même temps ;
- **pas de rotation du point de départ** : l'ordre d'arrivée garde à chaque connexion sa
  place d'un tour à l'autre (comme la file FIFO de monoio) ;
- **la tranche de save passe après l'envoi des réponses**, peut se répéter dans un tour
  jusqu'à sa part de CPU (une seule par tour : save deux fois plus longue, plus de copies
  T0 à libérer), et l'`enter` en plus n'a lieu que si une tranche s'exécute.
- ~~Pic restant, commun aux deux binaires : `end_save` libère d'un coup les copies T0~~
  **Résolu (2026-10-19)** : mesuré, ce n'étaient pas les images (toutes consommées par le
  DFS) mais leurs **feuilles** (~480 000 sur 1 M clés sous écriture, une copie de valeur
  chacune, 8,8 ms d'un bloc). `end_save` les met de côté, `drain_deferred` les libère par
  tranches : p32 + BGSAVE, p99.99 10,2 → **3,25 ms** (le niveau sans save), tranche max
  9 ms → 318 µs.

**Délégation des sends, version statique (2026-10-19, §6.4 étape 1).** `RADIXOX_SEND_ASYNC=1`,
`RADIXOX_IOWQ_WORKERS`, `RADIXOX_IOWQ_CPUS`. Boucle sur CPU 0, workers sur le cœur
physique 1 (CPU 1, et 7 son frère SMT), client sur 2-5,8-11 ; un run par point.

| | sans (1 cœur) | 1 worker (CPU 1) | 2 workers (CPU 1+7) |
|---|---|---|---|
| p1 débit | 310 K | 289 K | **526 K (+70 %)** |
| p1 moy. / p99 | 0,295 / 0,447 ms | 0,277 / 0,375 | **0,174 / 0,279 ms** |
| p32 débit / p99 | 2,08–2,41 M / 1,46–1,55 | | 2,59 M / 1,54 |
| p256 débit / p99 | 2,83 M / 9,5 | | 2,76 M / 12,2 |
| occupation p1 (2 workers) | | | CPU 0 75 %, CPU 1 89 %, CPU 7 89 % |

Lecture : sans pipeline, la pile TCP part sur les workers et le débit suit leur nombre ;
un seul worker déplace le TCP sans le paralléliser (même constat que SQ_POLL, §12.3) ;
en pipeline profond, déléguer ne rapporte rien (goulot = l'arbre) et peut coûter → le
mode `auto` (déléguer quand le noyau sature notre cœur) a une raison d'être. Repère :
Valkey 4 io-threads faisait ~500 K en p1 en mai, sur 4 cœurs.

**YCSB A à ressources égales, 3 cœurs physiques chacun** (⚠ même biais : run sur 1 000 clés ; `SERVER_CORES=3 bench_compare.sh`,
un thread par cœur, frères SMT des cœurs serveur inactifs, YCSB sur les 3 autres cœurs ;
RadixOx : boucle + 2 workers io-wq ; Valkey : io-threads=3 ; un run, 2026-10-19).

| | RadixOx (3 cœurs) | Valkey (3 threads) |
|---|---|---|
| load | **223 K**, p99 816 µs | 182 K, p99 1 107 µs |
| run | **411 K (+12 %)** | 366 K |
| lecture moy. | **234 µs** | 271 µs |
| lecture p95 / p99 / p99.9 | 329 / 484 / 727 µs | **319 / 440 / 702 µs** |
| lecture p99.99 | **1 983 µs** | 2 283 µs |
| RSS | **1 556 Mo** | 2 056 Mo |

Débit, moyenne et mémoire pour RadixOx ; **queues p95–p99.9 un peu meilleures chez Valkey**
(3 à 10 %), contrairement au cas à 1 cœur. Hypothèse : variance du passage de relais vers
io-wq (mise en file, réveil d'un worker, complétion rendue au prochain `enter`), là où
Valkey pilote ses propres io-threads. Pistes : mode `auto` et pilotage d'io-wq (§6.4),
mesure du délai soumission → CQE des sends délégués.

**Prochaine optimisation (idée de l'utilisateur, 2026-10-19) : préchargement anticipé.**
Pendant qu'on exécute la fenêtre N, précharger déjà les octets des frames et les clés de
la fenêtre N+1 (distance réglable, comme le préchargement des frères de KRANGE) : les
données reçues reviennent en cache juste avant leur parse.

### 12.5 Coût de la délégation, attente groupée (2026-10-20, loopback, p1)

1:4, 1 M clés de 100 o, graines distinctes, 10 s, profil `performance`, un run par point.
Boucle sur CPU 0 ; `perf stat -t` sur le thread principal seul ; occupation par CPU lue
dans `/proc/stat` (softirq compris).

**Part user / noyau.** 1 cœur, 80 conns : 266 K ; CPU 0 = user 7, sys 50, softirq 40.
Par commande : 4 785 instr. et 2 223 cycles user, ~18 200 cycles noyau (~3,8 µs).

| boucle + workers (CPU) | débit | boucle occupée | cycles user/cmd | cycles noyau/cmd |
|---|---|---|---|---|
| 1 cœur | 266 K | 100 % | 2 223 | 18 165 |
| + 1 worker (1) | 279–304 K | ~70 % | 3 540–4 140 | 6 965–7 590 |
| + 2 workers frères SMT (1, 7) | 450–474 K | 72–77 % | 3 060–3 574 | 5 295–5 505 |
| + 2 workers, cœurs distincts (1, 2) | **540–562 K** | 81–84 % | 2 887–3 065 | 4 710–4 737 |

- Un worker sur un cœur physique coûte **~3,6 µs/cmd** (sys 58, softirq 41 : il paie aussi
  la réception TCP du client, biais du loopback). Les workers saturent dans toutes les
  configs ; client sur 3-5,9-11 à ~88 % : sur le 7600X, impossible de saturer la boucle.
- CPU total : 281 % pour 556 K (5,05 µs/cmd) contre 100 % pour 266 K (3,76 µs/cmd) ;
  délégation = +34 % de CPU par commande pour ×2,1 de débit sur 3 cœurs.
- **Le surcoût user vient des petits lots, pas d'un partage de lignes entre cœurs.** Mêmes
  instructions par commande, ×1,4–1,6 de cycles, ×2,5 de défauts L1, diffus dans tout le
  code (pas concentrés sur les tampons d'envoi). La boucle a de la marge, dort
  (~0,26 changement de contexte/cmd) et sert des tours de quelques commandes :
  `descend_batch` −102 cycles/cmd, chemin unitaire (`get`, `find_value`) +400. Écarté par
  mesure : CC6 (boucle `SCHED_IDLE` sur CPU 0, idle à 0 % : aucun changement).
- **Plus de connexions ne grossit pas les lots** tant que les workers sont le goulot : la
  boucle reçoit au rythme où ils envoient (boucle fermée). 80 → 800 conns : débit plat,
  latence ∝ conns (Little), coût user/cmd inchangé.
- **Horloges** (micro-bench, CPU 0) : `SystemTime::now` 23 ns, `Instant::now` 17 ns,
  `rdtsc` 7 ns, `*_COARSE` 3 ns (résolution 1 tick : 1 ms ici, 4 ms à HZ=250). Une seule
  lecture par tour : −1 %, bruit. Les ~800 cycles/cmd « clock_gettime » du profil sont un
  artefact d'attribution (bord user/noyau) : ce profil exagère tout ce qui touche la
  frontière.

**Attente groupée** (`RADIXOX_MIN_WAIT_US`, `RADIXOX_WAIT_NR`, `min_wait_usec` de
`io_uring_getevents_arg`, noyau 6.12) : au repos, le noyau attend `WAIT_NR` complétions
pendant au plus ce délai avant de nous rendre la main pour une seule.

| cœurs distincts, 2 workers | débit | moy. / p99 / p99.9 | boucle | user/cmd | noyau/cmd |
|---|---|---|---|---|---|
| 78 conns, classique | 556 K | 0,14 / 0,21 / 0,26 ms | 81 % | 2 887 | 4 737 |
| 78 conns, 10 µs / 16 | 540 K | 0,15 / 0,21 / 0,24 | 53 % | 2 285 | 4 130 |
| 318 conns, classique | 562 K | 0,57 / 0,68 / 1,14 | 84 % | 2 973 | 4 710 |
| 318 conns, 10 µs / 16 | 532 K | 0,60 / 0,71 / 1,18 | 56 % | 2 377 | 4 127 |
| 798 conns, classique | 540 K | 1,48 / 1,99 / 5,63 | 82 % | 3 065 | 4 731 |
| 798 conns, 10 µs / 16 | 524 K | 1,52 / 1,85 / 5,66 | 59 % | 2 490 | 4 189 |

- −20 % de cycles user, −12 % de noyau par commande : le coût user revient près du mono-
  thread (lots plus gros par réveil, les changements de contexte ne baissent pas).
- Débit −3 à −5 % (workers saturés ; bruit d'un run), latence égale : inutile pour le
  débit tant que la boucle n'est pas le goulot ; libère ~30 % du cœur (travail de fond).
  À saturation de la boucle, les lots grossissent seuls. Désactivé par défaut.
- Plafond de la boucle, en cycles : ~7 600/cmd classique → **~680 K** ; ~6 400 avec
  `min_wait` → **~810 K** (l'occupation `/proc/stat` en mode `min_wait` est incohérente
  avec les cycles : ~6,5 GHz implicites ; se fier aux cycles). À mesurer sur deux machines.

### 12.6 Contre Valkey 9.1.2, saves sur SSD (2026-10-20, loopback, p1)

1 000 001 clés de 100 o (`--random-data`), 1:4, 78 conns, 12 s, BGSAVE à t = 3 s ; mêmes
CPU pour les deux (1 cœur : CPU 0 ; 3 cœurs : 0-2, RadixOx boucle + 2 workers, Valkey
`--io-threads 3`), fils du BGSAVE de Valkey sur les mêmes CPU (`--bgsave-cpulist`) sauf
mention. Dumps sur le SSD SATA (PNY CS900), fsync compris. RadixOx en dump brut
(`RADIXOX_SAVE_ZSTD=0`) : les valeurs « aléatoires » de memtier se répètent d'une clé à
l'autre, zstd en flux les écrase (11–24 Mo) quand le LZF de Valkey, valeur par valeur, n'y
peut rien (113 Mo) ; brut = même volume à écrire. Un run par point. ⚠ Premiers runs écrits
sur `/tmp` (tmpfs) : fsync gratuit des deux côtés, saves irréalistes (111 ms).

| | RadixOx | Valkey |
|---|---|---|
| capacité 1 cœur | **266 K** | 207 K |
| 1 cœur, 165 K ops/s : CPU | **0,5 cœur** | 0,7 cœur |
| 1 cœur, 165 K : p99 / p99.9 / p99.99 | **0,38 / 0,44 / 0,58 ms** | 0,73 / 0,82 / 0,98 ms |
| + BGSAVE : durée | **0,43 s** | 5,4 s (fils sur un cœur libre : 1,96 s) |
| + BGSAVE : p99 / p99.9 / p99.99 | **0,42–0,44 / 0,49–0,51 / 0,87–1,29** | 2,45 / 2,83 / 3,41 (cœur libre : 0,74 / 0,85 / 1,02) |
| 3 cœurs à fond | **622 K** | 404 K |
| 3 cœurs + BGSAVE : débit, durée | **536 K, 0,36 s** | 379 K, 3,0 s |
| 3 cœurs + BGSAVE : p99 / p99.9 / p99.99 | **0,27 / 0,39** / 2,08 | 0,49 / 2,27 / 3,01 |
| RSS ; copy-on-write pendant la save | **135–142 Mo ; 0** | 190 Mo ; +170–180 Mo |

Le copy-on-write de Valkey est le cas défavorable de `fork` (20 % de SET sur des clés
uniformes touchent presque toutes les pages). Le chiffre 3 cœurs + BGSAVE de RadixOx est
antérieur à la correction du plancher (§8.2).

**Charge partielle 60–80 %, fils de Valkey sur un cœur libre** (avantage donné à Valkey :
son fils de BGSAVE sur le CPU 1, RadixOx seul sur le CPU 0). Débits visés = 60/70/80 % de la
capacité de Valkey (207 K), soit 47–62 % de celle de RadixOx ; le limiteur de memtier livre
±10 % autour (mêmes valeurs pour les deux à chaque paire). BGSAVE dans chaque run, deux
répétitions, ordre alterné ; plages = les deux runs.

| débit | | p50 | p99 | p99.9 | p99.99 | save | CPU |
|---|---|---|---|---|---|---|---|
| ~124 K | RadixOx | 0,29 | **0,42** | **0,50–0,51** | 1,03–1,31 | **348–360 ms** | 0,4 cœur |
| | Valkey | 0,37 | 0,76–0,77 | 0,93–0,94 | 1,10–1,16 | 2 061–2 153 ms | 0,6 + ~2 cœur·s de fils |
| ~145 K | RadixOx | 0,29 | **0,42** | **0,50** | 1,08–1,21 | **376–383 ms** | 0,5 |
| | Valkey | 0,37 | 0,75 | 0,89–0,90 | 1,04–1,16 | 2 150–2 245 ms | 0,6–0,7 + ~2 |
| ~165 K | RadixOx | 0,29 | **0,41–0,44** | **0,48–0,54** | 0,88–1,27 | **408–427 ms** | 0,5–0,6 |
| | Valkey | 0,36–0,38 | 0,73–0,77 | 0,84–0,91 | 1,00–1,07 | 2 149–2 163 ms | 0,7–0,8 + ~2 |
| 213 K (80 % de RadixOx) | RadixOx | 0,29 | 0,38–0,42 | 0,47–0,50 | 1,04–1,26 | 613–671 ms | 0,7 |
| 165 K sans save | RadixOx | 0,29 | 0,42–0,45 | 0,47–0,52 | **0,57–0,73** | — | 0,6 |
| | Valkey | 0,37 | 0,74–0,75 | 0,86 | 0,96–1,02 | — | 0,7–0,8 |

- p50 −20 %, p99 et p99.9 ÷1,8 à toutes les charges, save ÷5–6 ; RadixOx à 80 % de **sa**
  capacité garde un p99 meilleur que Valkey à 60 % de la sienne.
- **p99.99 sous save : à égalité** (0,88–1,31 contre 1,00–1,16 ms) : c'est le seul
  percentile où le cœur supplémentaire de Valkey compense. Coût de RadixOx : +0,3 à +0,6 ms
  sur le p99.99 pendant la save, l'écriture du dump partageant son unique cœur (§8.2).
- Fils de Valkey : ~2 cœur·s par save (CPU 1 à ~100 % pendant les ~2,1 s), contre ~60 ms de
  CPU de tranches pour RadixOx (plus le noyau pour l'écriture). Mémoire : RSS 141–145 contre
  190 Mo ; copy-on-write de Valkey +160–170 Mo pendant la save.

---

### 12.7 Où va le CPU sous charge, `enter`, profil d'énergie (2026-10-21/22, loopback, p1)

Mesures courtes (1 M clés de 100 o aléatoires, memtier 100 conns 1:1 sauf mention ; un run
par point ; un jeu (cs2) tournait pendant les premières, ordre de grandeur seulement).
Scripts jetables, rien dans le dépôt sauf `bench_compare.sh`.

**Le temps compté au thread ment.** `utime+stime` de la boucle seule (1 cœur) : ~61 %. Le
CPU 0 dans `/proc/stat` : user 12 %, sys 51 %, **softirq 37 %**, idle 0 %. Le noyau retire
le softirq (TCP loopback, côté serveur et côté client) du temps du thread ; 2–9 réveils
volontaires/s : la boucle ne dort pas. **Toujours mesurer l'occupation par CPU, softirq
compris** (c'est ce que fait désormais `bench_compare.sh`). La note de mai « thread actif
59 % » souffrait peut-être du même biais (autre config, pas vérifié).

**Délégation des envois, même charge** (boucle CPU 0, 2 workers CPU 1+7) :

| | boucle seule | envois délégués |
|---|---|---|
| débit | 171 K | **398 K** (199 K par cœur physique, contre 171 K) |
| moy. / p99 | 0,58 / 1,50 ms | 0,25 / 0,63 ms |
| CPU 0 sans save | u12 sys51 si37 **idle 0** | u44 sys38 si0 **idle 16** |
| BGSAVE (1 M clés) | 8,5 s, 15 % du CPU | **1,5 s, 47 %** (le repos va à la save, §8.2) |

**Profil de la boucle avec envois délégués** (perf, thread seul) : `io_recv` (multishot,
dans notre `task_work`) **25 %**, dont `tcp_recvmsg` 17 % et la copie `_copy_to_iter` 6 % ;
notre code ~18 % ; gestion io_uring ~15 % ; `Core::tick` 5,5 % → 1,4 % après bbc518c
(`CLOCK_REALTIME_COARSE`). Attributions perf peu fiables aux bords inlinés.

**Nombre d'`enter`** (compté par `LD_PRELOAD` sur `syscall()`, 5 s par point) :

| config | débit | `enter`/s | commandes par `enter` |
|---|---|---|---|
| boucle seule, p1 | 221 K | 5,6 K | ~40 |
| boucle seule, p16 | 1,35 M | 2,1 K | ~630 |
| envois délégués, p1 | 419 K | **317 K** | ~1,3 |
| envois délégués, p16 | 1,98 M | 6,2 K | ~320 |
| délégués + `MIN_WAIT_US=10 WAIT_NR=16`, p1 | 371 K | **46 K** | ~8 |

Une boucle non saturée retourne au noyau pour une ou deux commandes : les cycles qui
devraient être du repos (GC, save) partent en syscalls (chaque entrée paie aussi la
mitigation SRSO sur ce Zen). Sous saturation, les tours grossissent seuls. Les workers io-wq
**ne lisent jamais la SQ** (seuls notre `enter` ou SQPOLL la vident ; un worker ne consomme
que la file interne d'io-wq, puis `schedule_timeout`) ; avec `DEFER_TASKRUN` le CQE d'une op
déléguée n'apparaît qu'à notre `enter` suivant : deux `enter` par op déléguée.

**YCSB A à 3 cœurs, `bench_compare.sh` corrigé** (5 M clés, 10 M ops, profil performance,
~5,2 GHz) : RadixOx 450 K / Valkey 381 K ; par CPU occupé 169 K / 127 K.

| CPU | RadixOx run | Valkey run |
|---|---|---|
| 0 | 69 % (u29 / k40) : la boucle | 100 % (u15 / k85) |
| 1 | 97 % (u1 / k96) : worker | 99 % (u91 / k8) : io-thread en attente active |
| 2 | 98 % (u0 / k98) : worker | 100 % (u14 / k86) |
| client YCSB | 70 % de ses 6 CPU (u18 / k52) | 58 % |

Lecture : la boucle n'est **pas** le goulot (31 % de repos) ; les workers (TCP des envois) et
le client le sont. Côté serveur, ~5 µs de CPU noyau par op contre ~0,64 µs de user dans la
boucle. Plafond estimé de la boucle si on lui retire le noyau qu'on peut retirer (le recv) :
~1–1,2 M ops/s sans pipeline (reste soumission, CQE, surcoût du recv simple) ; mais il
faudrait 4–5 cœurs de workers et un client à part : **à vérifier sur deux machines**
(mémoire `project_cloud_bench_plan`), le loopback mesure surtout la pile TCP.

**Profil d'énergie** (même mini-bench 200 K clés, 3 cœurs) : `power-saver` coupe le boost
(EPP `power`, ~4,5 GHz) ; `performance` ~5,2 GHz : débit +10 % (RadixOx) / +12 % (Valkey),
client et serveur ralentis ensemble, classement inchangé. Le noyau TCP suit l'horloge ; les
attentes mémoire de l'arbre non. `bench_compare.sh` tient désormais le profil `performance`.

**Pièges de mesure trouvés dans `bench_compare.sh` (corrigés 0f6cbbe)** : P99.9/P99.99 lus
dans la dernière ligne d'état YCSB = le dernier intervalle seulement (parfois 2 700 requêtes)
: faux et instables. Et la chauffe de la JVM de YCSB (JIT, connexions, première seconde)
fait ~1 000 requêtes lentes et le max (27–50 ms contre 2,4–5,6 ms ensuite), assez pour
décider du p99.99 sur 10 M ops. Le GC de Java n'y est pour rien en mode normal (pauses ≤
2 ms ; 13–37 ms seulement avec `measurementtype=raw`). ZGC ne change rien.

## 13. Migration

### 13.1 Valeurs par défaut du prototype (2026-10-19)

| Réglage | Valeur |
|---|---|
| connexions max | 10 000 (`maxclients` de Redis), quelques slots réservés pour l'erreur « max clients » |
| sortie par connexion | clients normaux sans limite propre (comme Redis), abonnés 32 Mo dur / 8 Mo 60 s ; budget global d'`out` pour tous |
| entrée | limites à trois niveaux du §5.3 (frame, connexion 1 Go, budget global 1 Go) |
| timeouts | inactivité désactivée (comme Redis), `TCP_USER_TIMEOUT` 60 s, keepalive 300 s |
| anneau de réception | 32 × 256 Ko au départ, à mesurer contre 4 × 2 Mo |
| budgets | `BUDGET_TOUR` ~200 µs (garde-fous : 1024 commandes, octets de sortie), `BUDGET_CQE` 4096, SQ 4096 / CQ 16384, tranche de job ~50 µs |
| préchargement | fenêtre 8 (comme aujourd'hui), 32 à tester à l'étape 7 |
| jobs | tourniquet simple (§8) |
| configuration | variables `RADIXOX_*` (fichier de config plus tard) |

**Étape 0, ce qui a été fait :**
- `Cmd::parse` → `Parse::{Done, Incomplete, Invalid}`, limites du §5.3 (frame), inline,
  `*0` sauté (`Cmd::Empty`) ; dispatch sur le **nom complet** (elle ne regardait que la
  1re lettre et la longueur : `DUMP k` exécutait `DECR k`) ; erreurs typées
  (`Cmd::Invalid` + `CmdError`) ; métadonnées `is_barrier`, `allowed_unauthenticated`,
  `allowed_in_pubsub`, `is_write` (§2.2, sous forme de méthodes plutôt que de table).
- `radixox::exec` (lib) : commandes sync, `execute_sync`, encodeurs, client/AUTH, SCAN.
- `radixox::exec::jobs` : `KeysJob` (reprise sur clé via `scan_keys`, réponse encodée en
  octets, en-tête réservé), `UnlinkJob` (clé exacte supprimée tout de suite, `prefix*`
  détaché d'un coup), `FlushJob` (base détachée, réponse immédiate, `DBSIZE` à 0 aussitôt,
  libération en fond). oxidart : `Detached` remplace `deln_async` / `getn_async` /
  `SaveDone` ; plus de `Waker` dans oxidart.
- Écarts assumés : pendant un `UNLINK prefix*`, `DBSIZE` compte encore les clés pas encore
  libérées ; les globs de KEYS parcourent le sous-arbre de leur préfixe littéral (comme
  SCAN) au lieu du DFA de `getn_glob`. Limite globale d'accumulation : avec la boucle
  maison (monoio : 1 Go par connexion seulement).

**Étape 1, ce qui a été fait** (`radixox/src/uring/`) :
- `Core` reçoit des `Completion` (Accept, Recv, Send) et émet des `Op` (AcceptMulti,
  Install, Reject, RecvMulti, Send, CancelRecv, CancelSend, Close, ProvideBuf) ;
  `bufs.rs` (anneau incrémental, refcount par tampon, tampons disponibles), `conn.rs`
  (`ConnIn` / `ConnOut`, `Item::{Ring, Local, ProtoErr}`, `Mode`), `core.rs` (découpage
  à l'arrivée par `frame_len`, tirage par tours + barrières, flush anticipé et
  backpressure, `Closing` / `Draining`, `epoch`, `check_invariants`).
- `execute_into` partagé avec monoio ; KEYS et UNLINK encore d'une traite, FLUSHDB en
  fond (`run_jobs`) ; Pub/Sub, SAVE, BGSAVE répondent « not supported yet » (étapes 4–5).
- `tests.rs` : faux noyau (anneau incrémental, entrelacement, `ENOBUFS`, envois partiels
  ou bloqués, annulations, EOF) ; 15 tests dont un aléatoire contre un oracle (ordre par
  connexion, `URING_SEEDS=3000` passe), invariants vérifiés à chaque tour.
- **Bug trouvé par les tests** : un morceau sans frame complète rend son tampon aussitôt ;
  si le CQE `ENOBUFS` suivant arrive après, la connexion attendait un retour de tampon
  qui ne viendrait plus. Correctif : sur `ENOBUFS`, réarmer tout de suite si un tampon
  est déjà revenu (`BufPool::any_available`).
- Gardé par sécurité : le champ `sent` (renvoi du reste si un send rend moins que demandé
  malgré `MSG_WAITALL`).

**Étape 3, ce qui a été fait :**
- Cœur : horloge monotone par tour (`set_now`), tas de timers (`next_deadline`,
  `fire_timers`) : évicteur TTL (l'échantillonnage actuel, une passe par seconde, en pause
  pendant une save) et balayage d'inactivité (`RADIXOX_TIMEOUT`, 1024 slots toutes les
  100 ms, `last_active` par connexion). Le binaire attend dans `enter` au plus jusqu'à la
  prochaine échéance.
- Arrêt : avec `RADIXOX_SAVE_ON_EXIT`, SIGINT / SIGTERM bloqués et lus par une `signalfd`
  via io_uring ; à réception, gestionnaire `_exit` réinstallé (second signal = sortie
  immédiate), SAVE bloquante, sortie.
- `SAVE` bloquante dans la lib (`exec::persist::save_sync`), servie par les deux binaires.
- **Reporté** : l'évicteur à curseur (scan du bitmap tagué) est un chantier d'oxidart,
  indépendant de la boucle.
- Tests : `bgsave.rs` choisit aussi son binaire ; sur `radixox-uring`, SAVE, la save
  d'arrêt et le refus d'un dump corrompu passent ; les 4 tests BGSAVE attendent l'étape 4.
  (`cargo test --no-fail-fast` pour voir toutes les suites malgré un échec.)

**Étape 4, ce qui a été fait :**
- **Parquage** (`job_pending`, pas de mode de plus) : KEYS et UNLINK font une première
  tranche ; s'ils n'ont pas fini, la connexion est parquée (époque incrémentée), ses
  morceaux en attente et ce qu'elle reçoit sont recopiés dans `rx` (l'anneau n'est jamais
  retenu), le job avance d'une tranche par tour (`Config::job_budget`), la réponse part et
  la connexion reprend. EOF pendant le parquage : la réponse part avant la fermeture.
  KEYS, UNLINK, BGSAVE, SAVE ferment la fenêtre de leur connexion, comme une barrière.
- **BGSAVE** (`exec::bgsave`, machine synchrone) : DFS par tranches, vidage de la liste
  différée, zstd par tranches, double tampon ; écritures et fsync en ops io_uring
  (`FileWrite`, `FileSync`, une à la fois) ; plafond de 10 % du CPU tant qu'une réception
  date de moins d'1 ms. Différence assumée avec monoio : un échec d'ouverture du fichier
  est répondu tout de suite (`ERR …`) au lieu de « started » puis un échec en fond.
- Arrêt sur signal : un BGSAVE en cours est abandonné avant la save d'arrêt.
- Tests : faux noyau (KEYS long sans bloquer les autres, UNLINK `prefix*` parqué, BGSAVE
  cohérent avec écritures concurrentes, horloge du faux noyau) ; oracle aléatoire avec
  KEYS / UNLINK parqués (budgets 1–3) ; toute la suite d'intégration, `bgsave.rs` compris,
  passe sur les deux binaires.
- Pas encore mesuré : p99.99 sous BGSAVE en pipeline 32 (critère de l'étape).

**Étape 5, ce qui a été fait :**
- Registre canal → (slot, époque) dans le cœur ; PUBLISH encode le message une fois et
  l'ajoute à l'`out` de chaque abonné (compte rendu = abonnés atteints) ; plus de writer
  task ni de channel.
- Limites de sortie des abonnés : 32 Mo dur, 8 Mo pendant 60 s → fermeture (règle le
  TODO OOM sur abonné lent). Désabonnement immédiat à la fermeture.
- Comportement de Redis plutôt que de monoio : SUBSCRIBE en double non recompté,
  UNSUBSCRIBE sans abonnement → `[unsubscribe, nil, 0]`, PING en mode abonné →
  `[pong, msg]`. PUBLISH ferme la fenêtre de sa connexion (réponse dans l'ordre).
- Tests : flux complet et abonné lent sur le faux noyau ; `tests/pubsub.rs` (comportement
  commun) passe sur les deux binaires.

**Organisation du code :** le code commun aux deux serveurs (dispatch, `execute_sync`,
encodage, jobs, table des commandes) vit dans la bibliothèque `radixox/src/lib.rs` ; second
binaire `radixox-uring` (`src/bin/uringloop.rs`) ; les tests d'intégration choisissent leur
binaire (`RADIXOX_TEST_BIN`) pour faire tourner la même suite sur les deux.


*Make it work, make it fast, make it adaptable.* Une seule boucle dans
`radixox/src/bin/uringloop.rs` (io-uring 0.7.15), qui remplace `resp.rs` une fois
stabilisée.

**État (2026-10-19) : étapes 0 à 6 faites — monoio est retiré**, le binaire `radixox` est
la boucle maison. Mesures §12.4. Suite : étape 7+ (optimisations, chacune mesurée).

| # | Étape | Terminé quand |
|---|---|---|
| 0 ✅ | **Sous monoio, sans risque** : sortir dispatch + `execute_sync` dans un module indépendant du runtime ; table des commandes (§2.2) ; découpage à trois issues + limites (§5.3) ; KEYS / UNLINK / FLUSHDB en jobs à curseur qui encodent leur réponse en octets (§8) ; retirer les Waker d'oxidart | tests actuels verts ; monoio ne fait plus qu'appeler `step` + `yield_now` |
| 1 ✅ | **Cœur sans I/O** (§1.2) : files de `FrameRef` + refcount des tampons (§5.4), tirage par tours + barrières (§2.1), `Closing` / `Draining`, `epoch`, `check_invariants` | tests sur faux anneau, scénarios et séquences aléatoires : envoi qui traîne, EOF avec pipeline en cours, `ENOBUFS`, multishot terminé, annulation croisée avec une complétion, frame coupée octet par octet |
| 2 ✅ | **`uringloop.rs`** : accept classique + `FilesUpdate` (§4.1), recv sur anneau incrémental, send `MSG_WAITALL`, toutes les commandes sync, préchargement à l'intérieur d'une connexion comme aujourd'hui | `test_resp.sh` + intégration verts ; A/B court contre monoio en pipeline 1 / 32 / 256 ; mesurés : nombre et taille des tampons, `ENOBUFS`, adjacence, fixed file utile ou non, `MSG_WAITALL` |
| 3 ✅ | Horloges, timers, `signalfd`, ~~évicteur à curseur~~ (reporté), balayage d'inactivité | horloge vivante au repos, arrêt propre avec et sans save |
| 4 ✅ | Parquage + jobs branchés sur `ready`, port de BGSAVE, ~~SAVE = BGSAVE + parquage~~ (SAVE reste bloquante, §8.1 plus tard), save d'arrêt | p99.99 sous BGSAVE en pipeline 32 ≤ monoio |
| 5 ✅ | Pub/Sub avec limite d'`out` | abonné lent → `Draining`, pas d'OOM |
| 6 ✅ | Bascule par défaut, suppression de monoio (`oxidart/monoio.rs`, `async_command.rs`, `#[monoio::test]`, `resp.rs`, `utils/mod.rs`) | critères de bascule (§15) atteints |
| 7+ | Une optimisation à la fois, chacune mesurée : préchargement entre connexions + idx + époque (§17), GET par référence, `IOSQE_ASYNC` off/on puis auto (§6.4), jamais bloquant (§8.1), cas 3 du parseur, saves auto + AOF, pool enregistré + `SEND_ZC`, allocateur bump, GC, listes puis BLPOP | |

---

## 14. Écarté

| Option | Raison |
|---|---|
| Runtime à futures maison (`RawTask`, `fn_poll`, `syscall_nb`, `CURRENT_TASK`) | garde la machinerie des futures sans en avoir besoin ; le multishot y est pénible à modéliser |
| SQ_POLL par défaut | incompatible avec `DEFER_TASKRUN` ; brûle un vCPU en permanence, souvent le frère SMT du nôtre, compté dans le quota du conteneur. Revenu le 2026-10-24 **en option** (`RADIXOX_SQPOLL_CPU`, §6.4 bis) pour les machines dédiées |
| ACCEPT_DIRECT | optimise l'ouverture, pas le flux ; perd l'adresse du client, `setsockopt` classique, le choix des slots, le message « max clients » (§4.1). `TCP_NODELAY` serait hérité du listener, mais ça ne règle qu'un des quatre points |
| `WRITE_FIXED` / tampons enregistrés pour les sends ordinaires | un send copie de toute façon ; réservé à `SEND_ZC` et au dump `O_DIRECT` (§6.3) |
| Anneau non incrémental en repli | noyau ≥ 6.12 imposé (§11) |
| Repli epoll | io_uring ou rien (§11) |
| Plusieurs boucles par processus | 1 processus = 1 boucle = 1 base ; le multi-cœur passe par plusieurs processus (sharding par namespace) |
| Parser à l'arrivée et garder des `Cmd<'a>` d'un tour à l'autre | durée de vie inexprimable sans `unsafe` ; on garde des `FrameRef` comptées et on parse à l'entrée dans le lot (§5.4) |
| Réordonner les commandes d'une même connexion | casse la causalité vue par les autres clients (§2.1) |
| Notre propre thread d'envoi (`MSG_RING`) | contrôle total d'io-wq, mais un deuxième thread userland |
| Dépasser 8–10 clés par lot de préchargement | mesuré : plus lent (§8bis), dans la fenêtre du pipeline. **À re-mesurer** : hors réseau, MGET via `descend_batch` (idx réutilisé, sans seconde descente) va de 155 ns/clé à 8 clés à 105 ns/clé à 32–64 (§17.1) |
| Slotmap / clé générationnelle | l'index de fixed file est stable tant qu'on ne ferme pas (§4.2) |
| `READ_FIXED` | un tampon réservé par connexion, pas de multishot, copie noyau de toute façon |
| Beaucoup de petits tampons dans l'anneau | frontières fréquentes ; le mode incrémental permet quelques gros tampons |
| Contiguïté par connexion dans l'anneau (un groupe par conn) | ids sur 16 bits, mémoire réservée par connexion ; l'adjacence se teste au cas par cas (§5.4) |
| Parseur reprenable à n'importe quel octet | trop d'états ; RESP annonce les tailles (§5.3) |
| Découper un pipeline par nombre de commandes | un send coûte ~30 commandes de CPU ; découpage par octets à la place (§6.2) |
| NAPI busy poll par défaut | brûle du CPU pour quelques µs ; coûteux en cloud sur les instances à crédits ou facturées au CPU. Option possible, pas dans le prototype |
| Client de bench sur le desktop, serveur dans le cloud | RTT internet, Wi-Fi, trafic sortant facturé (~130 $/h à 400 Mo/s) |

---

## 15. À valider

**Décidé le 2026-10-19 :** prototype dans `uringloop.rs` à côté de `resp.rs` ; connexions
parquées comme mécanisme unique (`CONN_DESIGN.md`, `BENCH_ANALYSIS.md` et la section
« Refactor RESP server » du CLAUDE.md, devenus obsolètes, ont été retirés le 2026-10-19) ; accept classique ; SQ_POLL
abandonné ; noyau ≥ 6.12.

**À mesurer ou vérifier dans le prototype :**
- Entrelacement réel des sockets dans l'anneau incrémental (§5.1), adjacence (§5.4).
- Nombre et taille des tampons ; fréquence d'`ENOBUFS` ; plafond de relances du multishot
  et effet de `len` sur `RecvMulti` (§5.1).
- `MSG_WAITALL` : un seul CQE pour un gros envoi à un client lent (§6.1).
- Fixed file : gain réel sur le flux (§4.1).
- `IOSQE_ASYNC` : où tournent les relances après `EAGAIN` (§6.4, piège 3).

**Critères de bascule par défaut (étape 6), décidés le 2026-10-19 :** toute la suite de
tests verte sur les deux binaires ; `scripts/bench_vs_valkey.sh` : débit ≥ monoio − 2 % et
p99 pas pire, sans pipeline et en pipeline 32 ; un run BGSAVE sous charge avec un p99.99
pas pire.

---

## 16. Points pas encore traités (à ne pas oublier)

**Anneau et boucle**
- **CQ pleine** : une rafale de multishot peut déborder la CQ. Surveiller
  `IORING_SQ_CQ_OVERFLOW` et entrer pour vider ; dimensionner la CQ large.
- **SQ pleine en plein tour** (beaucoup de connexions sales) : soumettre tout de suite
  puis continuer, au lieu de perdre des sends.
- **Fin de multishot** : un recv ou un accept multishot peut s'arrêter pour d'autres
  raisons qu'`ENOBUFS` (CQ pleine, `EMFILE` sur l'accept). Règle : tout CQE sans `F_MORE`
  réarme, ou met la connexion (ou l'accept) dans une liste « à réarmer ». Même chemin
  qu'`ENOBUFS`, testé au faux anneau.
- **Mode incrémental** : ne jamais rendre un tampon tant que son dernier morceau porte
  `F_BUF_MORE`.
- **Détection des fonctions noyau au démarrage** (`io_uring_disabled`, version, opcodes
  supportés) avec un message d'erreur clair, plutôt qu'un échec obscur.
- **Arrêt propre** : annuler les multishots, finir les sends en vol, `CLOSE_DIRECT` de
  tous les slots, puis sortir (ou save d'arrêt).

**Connexions et protection**
- Clients lents à envoyer, inactivité : balayage incrémental (§9). Limite de connexions :
  réserve de slots (§4.1). Grosses réponses : jamais bloquant (§8.1).
- `SELECT n` répond OK et ignore `n` : un client qui croit isoler ses données écrit en base
  0. Avec « 1 processus = 1 base » : `SELECT 0` → OK, le reste →
  `-ERR DB index is out of range`.

**Déploiement cloud (2026-10-19)**
- Quota cgroup et SMT : conditionnent la délégation (§6.4).
- **Biais du loopback** : en loopback, notre send fait aussi la réception du client et
  netfilter (la moitié des « 80 % noyau » du §12). Sur une vraie carte, la réception tourne
  sur le cœur de l'interruption (RSS) : régler l'affinité des IRQ (loin de notre cœur, ou
  sur celui d'io-wq) et refaire la ventilation du §12 en cloud avant de chiffrer ASYNC.
- **kTLS** : poignée de main en userland (rustls), puis bascule du socket en TLS noyau ;
  garde le flux io_uring et ouvre l'offload matériel des cartes.

**Plus tard, à garder en tête**
- **Configuration** : les `RADIXOX_*` se multiplient → fichier de config et `CONFIG GET`
  minimal (certaines bibliothèques l'appellent à la connexion).
- **Comptabilité mémoire** : `out`, `rx`, anneau et jobs comptent dans un futur `maxmemory`.

**Performance et déploiement**
- **Sockets Unix** pour les clients sur la même machine (sidecar) : pas de pile TCP ni de
  netfilter, qui représentaient ~75 % du coût sans pipeline en loopback. Gros gain potentiel
  pour peu de code (même boucle, autre type de socket).
- **NUMA** sur les grosses VM multi-sockets : thread, mémoire (mmap HiSlab, anneau) et
  affinité io-wq sur le même nœud.
- **maxmemory et éviction** (LRU/LFU) : indispensable en prod, pas encore conçu (piste :
  1–2 octets LFU, mais le nœud de 64 o est plein).
- **Latence par commande** : histogrammes peu coûteux (TSC) et un slowlog, pour les
  diagnostics.
**Tests**
- **Tests déterministes de la boucle** : le cœur sans I/O (§1.2) est alimenté par des
  `Completion` fabriquées (faux anneau) : send qui traîne, `ENOBUFS`, `ECONNRESET` en plein
  send, annulation qui croise une complétion, CQ pleine. Impossible proprement sous
  monoio, naturel sans runtime.
- **Banc réseau hostile** : clients qui lisent lentement ou plus du tout, resets en plein
  envoi, frames envoyées octet par octet, pipelines énormes.
- **Fuzz du découpage / parseur** (il aurait trouvé le débordement de `parse_uint`, §5.3)
  et différentiel contre Valkey sur le fil.

---

## 17. Cible finale : le thread principal ne fait que calculer

> But ultime du chantier : extraire du cœur tout ce qu'il peut donner. Ce chapitre assemble
> les briques des §2–§8bis en un seul objectif chiffré. Décidé en session le 2026-10-12.

**Pourquoi le CPU par opération, et pas la latence d'une requête.** En prod, le RTT réseau
(des dizaines à des centaines de µs) écrase les quelques µs du serveur : gagner 1 µs sur une
requête isolée ne se voit pas. Ce qui se voit, c'est le **CPU consommé par opération sur le
thread principal** :
- il fixe le **débit maximal par cœur** (1 / CPU par op) ;
- il fixe la **latence sous charge** : quand le thread sature, les requêtes attendent leur
  tour, et c'est là que le p99 explose. Diviser le CPU par op repousse ce mur.

**Partage des rôles :**
- **thread principal** : du calcul pur sur des lots (parse, descentes entrelacées, exécution,
  encodage). Aucun syscall par connexion, aucun waker, un seul `enter` par tour ;
- **noyau** : tout le réseau, via io_uring et ses workers (§17.3).

### 17.1 Le tour par lots

```
loop {
    récolte    : un enter(GETEVENTS) → CQE de toutes les connexions prêtes
    parse      : tirage par tours dans les files (§2.1) → un lot global de Cmd
                 (ordre gardé par connexion)
    descente   : descend_batch sur les clés du lot, par fenêtres de W
                 (miss superposés, valeurs préchargées, idx gardés)
    exécution  : dans l'ordre de chaque connexion ; lectures sur les idx gardés (§17.2)
    encodage   : valeurs par référence, écrites directement dans `out` (ni clone ni Frame)
    soumission : sends des connexions sales + travaux de fond, dans le même enter
}
```

- **L'ordre ne compte qu'à l'intérieur d'une connexion.** Entre connexions, tout ordre est
  linéarisable (un seul thread) : le lot peut être réordonné librement entre connexions.
- **Plus la charge monte, moins une op coûte** : le lot grossit, les miss se chevauchent
  davantage, et le coût fixe du tour (un `enter`) s'amortit sur plus de commandes. Le
  serveur accélère quand il en a le plus besoin.
- **Taille de fenêtre W** : un cœur n'a que ~12–16 miss L1 en vol (line fill buffers) ; le
  gain plafonne au-delà. Mesures hors réseau (MGET, 1M clés aléatoires, 128f393) :

  | clés par lot | ns/clé (lecture + encodage) | vs une par une (~375–450) |
  |---|---|---|
  | 1 | ~400 | ≈ |
  | 8 | ~155 | ×2,5 |
  | 32 | ~106 | ×3,5 |
  | 64 | ~104 | ×3,6 |

  Ça contredit le « au-delà de 8–10, plus lent » de §8bis, mesuré dans la fenêtre du
  pipeline avec seconde descente. **À re-mesurer de bout en bout** ; point de départ W = 32.
- **Équité** : plafond de commandes par connexion et par tour (§8bis).

### 17.2 Ce que le lot permet en plus

1. **Réutilisation des idx pour toutes les lectures du tour** (fait pour MGET / EXISTS) :
   pas de seconde descente. Invalidation, deux options à mesurer :
   - liste `invalidate(idx)` signalée par les mutations structurelles (§8bis.3) : fine,
     mais chaque mutation scanne la fenêtre ;
   - **compteur d'époque structurelle** : `split_node`, `remove_node`, `delete_node_inline`,
     `try_recompress` (plus tard compactage et GC) l'incrémentent ; un idx gardé n'est
     utilisé que si l'époque n'a pas bougé depuis la descente, sinon descente normale (dans
     un cache chaud). Grossier, mais coût de tenue nul. Une mise à jour de valeur sur une
     clé existante n'est pas structurelle et ne casse rien.
   - Lecture pure pendant le lot ; expirées notées puis purgées après l'encodage
     (`purge_expired`), comme MGET.
2. **Fusion des lectures identiques** (clés chaudes) : si N connexions font `GET hot` dans le
   même tour, une seule descente et un seul prefetch, N encodages. Restriction simple pour
   la correction : ne fusionner que les clés qu'aucune commande du lot n'écrit. Gratuit à
   détecter quand les clés du lot sont triées ou hachées pour la descente.
3. **Valeurs par référence partout** (fait pour MGET, KRANGE) : GET, HGET, HMGET, HGETALL
   encodent depuis `&SharedByte`. Aucune écriture de refcount dans une ligne froide.
4. **Préchargement des nœuds frères** pour les parcours ordonnés (fait pour KRANGE, 9620c1a,
   distance 3 : −35 % sur une page de 64).

### 17.3 Tout le réseau au noyau

| Mécanisme | Ce qu'il retire de notre thread | Où |
|---|---|---|
| Accept multishot direct | un SQE par connexion entrante | §4 |
| Recv multishot + anneau incrémental | un SQE et un réarmement par lecture | §5.1 |
| Un `enter` par tour (`SINGLE_ISSUER` + `DEFER_TASKRUN`) | un syscall par opération | §2 |
| Sends délégués (`IOSQE_ASYNC`, io-wq piloté) | la pile TCP d'envoi (~80 % du cœur sans pipeline en loopback) | §6.4 |
| `SEND_ZC` + pool enregistré, au-delà de ~16 Ko | la copie des grosses réponses (pages KRANGE, HGETALL) | §6.3 |
| Sockets Unix pour les clients locaux | pile TCP et netfilter (~75 % du coût sans pipeline) | §16 |
| NAPI busy poll (option) | l'attente des interruptions réseau | §14 : pas par défaut, mode « machine dédiée » |
| zcrx `RecvZc` (6.15+, carte compatible) | la copie à la réception | §6.3 |

### 17.4 Budget cible (ordres de grandeur, à valider)

Côté user, mesuré hors réseau sur 1M clés aléatoires :

| Opération | Seule | En lot |
|---|---|---|
| lecture d'une clé (descente + valeur) | ~400–450 ns | **~105 ns** |
| entrée de KRANGE au-delà de la première | — | **~75 ns** |
| GET tout en cache | ~47 ns (§8bis) | — |

Cible : **~150 ns de CPU user par commande en lot** (descente, exécution, encodage, part du
tour), soit un plafond user de **plusieurs millions d'ops/s sur un cœur**. Au-delà, la limite
devient le noyau (coût par paquet TCP, §12) : d'où la délégation (§17.3) et l'intérêt de
l'auto-pipelining côté client, qui amortit le coût par paquet.

**Mesure de contrôle** (à faire dès le prototype) : `perf` sur le serveur à 80 connexions
sans pipeline puis en pipeline 32, pour séparer arbre / encodage / boucle / noyau, et suivre
la part « arbre » à chaque étape.

### 17.5 Ordre

1. Prototype de la boucle (§13, étapes 1–5).
2. Lot sur tout le tour avec `descend_batch` et réutilisation des idx pour les GET (§17.2.1),
   W = 32, mesure de l'époque contre `invalidate(idx)`.
3. GET / HGET / HMGET par référence (§17.2.3).
4. Délégation réseau mesurée : `IOSQE_ASYNC` off/on à 2 vCPU, en loopback puis en cloud,
   puis mode auto si les courbes se croisent (§6.4).
5. Fusion des lectures identiques (§17.2.2), avec une charge zipfienne pour la justifier.
6. Modes extrêmes sur VM dédiée : NAPI busy poll, `SEND_ZC`, zcrx (§17.3) ; bench cloud.

### 17.6 Principe et pistes chiffrées (2026-10-22)

**Principe (discuté avec l'utilisateur, à graver dans ARCHITECTURE.md s'il le confirme)** :
le thread métier ne fait que ce qui touche l'arbre ; tout le reste se délègue (pile TCP,
crc, zstd, fichiers). Chaque cycle retiré va à ce qui ne se parallélise pas : requêtes,
save, GC. Mesuré : déléguer les envois n'a pas coûté d'efficacité par cœur (199 K contre
171 K ops/s par cœur physique, loopback). Garde-fous : mode `auto` (tout en direct tant que
la boucle a du repos, déléguer quand elle sature), drapeau mono-thread pur, et désactivation
automatique sous 2 CPU de quota (§6.4 piège 1) ; SMT en cloud : ~+30 %, pas ×2.

Pistes, par gain attendu (chacune mesurée seule, jeu fermé, `bench_compare.sh` + CPU par
CPU) :
1. **Attente groupée automatique** quand les envois sont délégués et que la boucle fait
   ~1 `enter` par commande (§12.7 : `enter` ÷7). Existe déjà en option
   (`RADIXOX_MIN_WAIT_US` / `RADIXOX_WAIT_NR`).
2. ~~**Recv simple en `IOSQE_ASYNC`**~~ : mesuré le 2026-10-24 (§6.4), perte nette
   (+66 % de CPU serveur et boucle +25 % à débit égal) ; `RADIXOX_RECV_ASYNC` reste en
   option, désactivé.
3. **Enregistrer le fd de l'anneau** (`IORING_REGISTER_RING_FDS` +
   `IORING_ENTER_REGISTERED_RING`) : ~1 % à 317 K `enter`/s ; vérifier le support du crate
   `io-uring`.
4. **Threads réseau à nous + files SPSC** (si 1–3 laissent encore >10–15 % de noyau sur la
   boucle) : chaque thread réseau a son anneau et ses connexions ; le thread métier lit et
   écrit des files en mémoire partagée sans aucun syscall ; « vérifier avant de dormir »
   (attente active de quelques µs, puis futex). Contraintes : `SharedByte` non `Send` (rc
   non atomique) → octets possédés aux frontières ; allocateur v2 mono-thread ; garder le
   mode mono-thread pur. Proche du design « 2 threads » de mai, écarté alors : demande un
   vrai document de conception.

Écarté de nouveau : SQPOLL (§14 : incompatible avec `DEFER_TASKRUN`, mesuré sans gain de
débit) ; « que les workers io-wq lisent la SQ » (impossible : `SINGLE_ISSUER`, tête de SQ
avancée sans verrou par le seul consommateur).

---

## 18. Effets des écritures (2026-10-19)

> **Remplacé le 2026-10-20 par DURABILITY_DESIGN.md** : on journalise les commandes reçues
> (déterministes à `now` égal), pas un collecteur d'effets passé à chaque commande. Le seul
> point d'appel est la boucle d'exécution du cœur (`exec/journal.rs`, `plan` / `finish`).

Plusieurs mécanismes ont besoin de savoir ce qu'une commande a modifié : AOF, réplication,
WATCH, notifications de keyspace, verrous de lecture des jobs (§8.1), barrière de save.
L'exécuteur du lot est l'endroit naturel pour le savoir.

- Dès le prototype, l'exécution reçoit un **collecteur d'effets** (`&mut Effects`), vide
  au début : les commandes d'écriture (table §2.2) y déclarent leurs clés touchées.
- Ça évite de changer toutes les signatures quand l'AOF arrivera ; la conception de
  l'AOF elle-même n'est pas bloquante aujourd'hui (fsync groupé par tour, kind `File`,
  cf. `TX_DESIGN.md`).
