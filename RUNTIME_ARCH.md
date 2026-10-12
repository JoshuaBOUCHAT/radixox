# Boucle io_uring maison (runtime-less) — Architecture

> Remplace monoio par une boucle io_uring écrite à la main, **sans `Future`,
> sans waker, sans tâche**. Les CQE sont dispatchés directement vers des
> machines à états explicites.
>
> Décisions prises en session le 2026-10-10 et le 2026-10-11. La version
> précédente de ce fichier (runtime à futures : `RawTask`, `fn_poll`,
> `syscall_nb`, SQ_POLL) est **écartée**, voir §14. Historique : `git log -- RUNTIME_ARCH.md`.
>
> Base logicielle : **io-uring 0.7.15** (anneau incrémental, bundles, `SetSockOpt`,
> zcrx, NAPI). monoio garde sa 0.6.4 jusqu'à sa suppression (§13, étape 5) : les
> deux versions coexistent dans le workspace.

---

## 0. Résumé

```
conns: Vec<Conn>       index = slot de fixed file, choisi par le noyau à l'accept
user_data              kind << 32 | slot           (pas de génération)
fin de connexion       Draining → refs == 0 → CLOSE_DIRECT
lecture                recv multishot + anneau incrémental de quelques gros tampons
                       aucune référence vers l'anneau ne survit au handler du CQE
écriture               au plus un send en vol par connexion, flush en fin de tour,
                       flush anticipé au-delà d'un seuil d'octets, backpressure
travaux longs          jobs à curseur, une tranche par tour (save, KEYS, UNLINK, évicteur, GC)
commandes « lentes »   la connexion est parquée ; celui qui termine répond et la reprend
lots de recherches     clés de toutes les connexions du tour préchargées par fenêtres de 8
délégation            sends en IOSQE_ASYNC quand IORING_SQ_TASKRUN reste levé ; plafond io-wq piloté
cible finale           thread principal = calcul pur sur des lots, réseau entier au noyau (§17)
attente                enter(GETEVENTS) ; timeout = prochaine échéance de timer
```

---

## 1. Principes

- **Un seul propriétaire.** `struct Server { ring, art, conns, bufs, pubsub, blockers, jobs, timers }`
  est passé en `&mut` aux handlers. Plus de `Rc<RefCell<OxidArt>>`, plus de borrow à l'exécution.
- **Pas de `.await`.** L'invariant d'`ARCHITECTURE.md` §1 (« rien de daté ne traverse un
  `.await` ») devient : **rien de daté ne survit d'un tour de boucle à l'autre**. Un job
  conserve un curseur re-résolvable (clé), jamais un idx de slab ni un pointeur (sauf la
  save, qui a sa barrière COW, cf. `SAVE_DESIGN.md`).
- **Machines à états plates.** Chaque machine a peu d'états. Ses transitions ne viennent
  que d'un CQE ou d'un `step` de job. Une seule imbrication : la connexion porte l'état
  de sa frame en cours (§5.3).

| Machine | États |
|---|---|
| Connexion | `Normal` · `Parked` · `Draining` |
| Réception | armée · affamée (`ENOBUFS`) · terminée |
| Envoi | libre · en vol |
| Parseur | frame entière · remplissage de valeur |
| Job | curseur + `step(&mut Server, budget) -> Done \| More` |

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
   - `refs` d'une connexion = ce qui la référence vraiment (recv armé, send en vol,
     inscriptions en file d'attente, jobs) ;
   - une connexion parquée est dans exactement les files qu'elle déclare, avec son
     `park_seq` ;
   - au plus un send en vol par connexion ;
   - aucun tampon de l'anneau retenu après le tour.
   Une incohérence explose tout de suite, au bon endroit.
3. **Tests déterministes sur un faux anneau** (§16) : séquences de CQE fabriquées (envoi
   partiel, connexion qui meurt en plein BLPOP, LPUSH et timeout dans le même tour,
   `ENOBUFS`), puis vérification de l'état et des invariants ; aussi tirées au hasard, comme
   les tests de `krange` contre une `BTreeMap`. Les bugs io_uring deviennent reproductibles.
4. **Une boucle mince** : récolte → `dispatch(kind)` → un handler par type d'événement.
   Aucune logique métier dans la boucle ; les commandes restent dans `execute_sync`, inchangé.
5. **Pas d'optimisation prématurée — le mot important est « prématurée ».** On optimise
   tard, pas jamais. Le premier prototype vise un squelette **correct et mesuré** : pas de
   lot sur tout le tour, pas de délégation des envois, pas de modes extrêmes (§17). Mais le
   design leur garde leur place dès le départ (idx de slot stables, `out` par connexion,
   dispatch par lots possible), pour que les optimiser plus tard ne demande pas de tout
   réécrire. Chaque optimisation arrive ensuite **avec sa mesure avant/après**, comme les
   chantiers de l'arbre (SWAR, prefetch des frères, multi-clés : chaque gain chiffré avant
   commit).

---

## 2. La boucle

```rust
loop {
    let now = clock();                    // une lecture par tour ; remplace la tâche ticker
    srv.art.set_now(now);

    for cqe in srv.ring.completion() {     // 1. récolte
        dispatch(&mut srv, cqe);
    }
    srv.timers.fire(now, &mut srv);        // 2. échéances (BLPOP, autosave, fsync AOF)
    srv.flush_dirty();                     // 3. un send par connexion sale (§6)
    if let Some(job) = srv.jobs.front() {  // 4. une tranche de travail de fond
        srv.run_slice(job, BUDGET);
    }
    // 5. soumission + attente
    if srv.jobs.is_empty() && cq_vide {
        enter(to_submit, 1, GETEVENTS | EXT_ARG, timeout = srv.timers.next())
    } else {
        enter(to_submit, 0, GETEVENTS)     // on continue de tourner
    }
}
```

**Setup de l'anneau (mode A, défaut).** `SINGLE_ISSUER` + `DEFER_TASKRUN` +
`TASKRUN_FLAG` (§6.4), `register_ring_fd`, CQ dimensionnée large (`setup_cqsize`).

**Mode B (option de démarrage, à mesurer).** SQ_POLL épinglé sur un cœur réservé, à la
place de `DEFER_TASKRUN` (les deux sont incompatibles). Un seul thread noyau exécute
toutes les SQE, sends compris : il sort le travail TCP de notre cœur sans passage de
relais, mais tourne en continu et n'a aucun parallélisme. Nos meilleurs YCSB de mai
venaient de SQ_POLL **sur un autre cœur** ; sur notre propre cœur il dégradait. À
comparer au mode A + io-wq à budget égal (2 cœurs). Attendu, non vérifié : SQ_POLL
meilleur en latence à charge moyenne, io-wq meilleur quand il faut plus d'un cœur de TCP.

**⚠️ `GETEVENTS` obligatoire.** Avec `DEFER_TASKRUN`, les complétions (y compris la
copie des données reçues) ne sont exécutées que pendant un `io_uring_enter` avec
`GETEVENTS`. `Submitter::submit()` ne passe pas ce flag quand `want == 0` : on appelle
`enter` nous-mêmes. Sans ça, une save qui tourne affame les connexions (même effet que
le bug de yield monoio).

**Réglages à mesurer :** `SubmitArgs::min_wait_usec` (regrouper les complétions à faible
charge), `BUDGET` d'une tranche de job.

---

## 3. `user_data`

```
u64 = [ kind : 32 ][ slot : 32 ]
```

| kind | slot | Remarque |
|---|---|---|
| `Accept` | — | multishot, un seul SQE pour toute la vie du serveur |
| `Recv` | conn | multishot |
| `Send` | conn | au plus un en vol par conn |
| `Cancel` | conn | annulation du recv au passage en `Draining` |
| `Close` | conn | ne touche pas la table (le slot peut déjà être redonné) |
| `File` | job | écritures et fsync du dump / de l'AOF |
| `Signal` | — | lecture de la `signalfd` |
| `Timeout` | — | si un timer passe par io_uring plutôt que par le timeout d'`enter` |

**Pas de génération.** Une CQE ne peut jamais viser une connexion qui a été remplacée
(§4.2), donc aucun test d'obsolescence dans le dispatch.

---

## 4. Connexions

### 4.1 L'index = le slot de fixed file

- `register_files_sparse(MAX_CONN)` à l'init.
- `AcceptMulti` avec `allocate_file_index(true)` (ACCEPT_DIRECT) : le CQE renvoie l'index
  choisi par le noyau, qui sert directement d'index dans `conns: Vec<Conn>` (préallouée).
- Le noyau ne redonne pas un index tant qu'on n'a pas fait `CLOSE_DIRECT` sur ce slot.
- Il n'alloue pas forcément le plus bas index libre : il avance avec un curseur et repart
  au début en bout de table. Toute la table finit par servir, donc dimensionner `MAX_CONN`
  au besoin réel (64 K conns × ~128 o = 8 Mo).
- Pas de `fget`/`fput` par op.
- Revers : les connexions sont invisibles pour `lsof`, `ss` et `/proc/pid/fd` → les
  exposer dans `INFO clients`.

**`TCP_NODELAY`.** Un socket direct n'a pas de fd, donc pas de `setsockopt` classique.
Deux pistes à tester dans le prototype : l'opcode `SetSockOpt` sur le fixed file, ou
l'héritage du flag posé sur le socket d'écoute (probable sous Linux, non vérifié). Il est
indispensable : il avait corrigé les p99.99 de 43 ms en pipeline.

### 4.2 Fin de connexion : le compteur `refs`

**Règle unique : `refs` compte tout ce qui peut encore désigner le slot. `CLOSE_DIRECT`
seulement à `refs == 0`.**

| Source | +1 | −1 |
|---|---|---|
| recv multishot armé | armement | CQE sans `F_MORE` |
| send en vol | soumission | CQE du send |
| job qui vise la conn (KEYS, UNLINK…) | création | fin du job |

Passage en `Draining` (EOF, erreur, QUIT, dépassement de limite) :
1. On ne soumet plus rien pour la connexion.
2. On annule le recv (`AsyncCancel` sur son `user_data`). Le send en vol se termine ou
   échoue tout seul.
3. On la retire **tout de suite** des registres pub/sub et blockers (elle connaît ses
   canaux et ses clés). Ces registres ne comptent pas dans `refs` : aucune entrée ne doit
   pointer vers le slot quand le noyau le redonnera.
4. Chaque CQE restant fait `refs -= 1`. À 0 : `CLOSE_DIRECT`.

### 4.3 Structure

```rust
struct Conn {
    mode: Mode,                       // Normal | Parked(Reason) | Draining
    refs: u8,
    rx: Vec<u8>,                      // reliquat de frame coupée (§5.3), vide en général
    partial: Option<Box<Partial>>,    // frame en cours de remplissage de valeur (§5.3)
    out: Vec<u8>,                     // réponses en cours d'accumulation
    sending: Option<Vec<u8>>,         // tampon en vol (None = pas de send en vol)
    sent: u32,                        // reprise sur envoi partiel
    channels / blocked_keys …         // pour le retrait immédiat des registres
}
```

Le prototype garde des `Vec` pour `out`/`sending`. Le pool enregistré arrive avec
`SEND_ZC` (§6.3).

---

## 5. Réception

### 5.1 Recv multishot + anneau incrémental

- `RecvMulti` sur `Fixed(slot)`, groupe de tampons partagé par toutes les connexions.
- Anneau enregistré avec `register_buf_ring_with_flags(..., IOU_PBUF_RING_INC)` (6.12+) :
  le noyau consomme un tampon **par morceaux**. Le CQE porte l'id du tampon
  (`cqueue::buffer_select`), et `cqueue::buffer_more` dit si le noyau continuera d'y
  écrire.
- **Quelques gros tampons** plutôt que beaucoup de petits : **4 × 2 Mo** (une huge page
  chacun, cohérent avec le THP de HiSlab), réglables. Soit **8 Mo pour tout le serveur**,
  contre 3 × 512 Ko réservés par connexion avec monoio.
- Les morceaux de **connexions différentes s'entrelacent** dans un même tampon : c'est
  toujours le tampon en tête du groupe qui sert, quel que soit le socket. Interprétation
  du mécanisme, à confirmer par un test dans le prototype. On ne compte donc pas sur la
  contiguïté entre deux morceaux d'une même connexion.

Pourquoi si peu de tampons suffisent : avec `DEFER_TASKRUN`, le noyau n'écrit dans
l'anneau que pendant notre `enter`. Pour tomber à court, il faudrait recevoir plus que
l'espace libre pendant un seul appel. Vérifier dans le prototype qu'`ENOBUFS` n'arrive
jamais.

### 5.2 Handler

```rust
Kind::Recv => {
    let more = cqueue::more(flags);              // F_MORE absent → recv terminé
    if !more { conn.refs -= 1; }
    match res {
        n if n > 0 => {
            let bid = cqueue::buffer_select(flags).unwrap();
            on_data(conn, bufs.chunk(bid, n));   // parse + exécution (§5.3)
            bufs.release_if_done(bid, cqueue::buffer_more(flags));
            if !more { rearm(conn) }
        }
        0 => conn.drain(),                       // EOF
        e if e == -ENOBUFS => starved.push(slot),// réarmé quand des tampons reviennent
        _ => conn.drain(),
    }
}
```

**Invariant : aucune référence vers un tampon de l'anneau ne survit au handler de son
CQE.** Tout ce qui n'est pas consommé est copié avant de sortir, y compris les commandes
mises en attente par une connexion parquée. Un tampon est donc rendable dès que le noyau
l'a fini, sans compteur par tampon.

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
- Écarté : le parseur reprenable à n'importe quel octet (un état par élément du protocole)
  et la détection d'adjacence entre morceaux (compteurs par tampon).

---

## 6. Émission

### 6.1 Règles

- **Au plus un send en vol par connexion.** C'est l'ordre des octets, et c'est requis par
  la délégation io-wq, qui ne sérialise pas les envois sur un socket.
- **Flush en fin de récolte.** Les connexions qui ont produit des réponses pendant le tour
  vont dans une liste « sales ». En fin de récolte : un send par connexion sale. Toutes les
  réponses d'un tour sont regroupées, et `read_and_flush` disparaît (le recv reste armé).
- **Envoi partiel** : on resoumet le reste (`sent`).
- Au CQE du send : si `out` n'est pas vide, on échange les tampons et on renvoie.

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
  arrête d'être parsé. Ça règle le TODO « buffers bornés » pour toutes les connexions.
- L'autocorking TCP du noyau fusionne de petits sends rapprochés : un seuil un peu bas
  coûte moins cher que prévu.

### 6.3 Ce qui est zero-copy, et ce qui ne l'est pas

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

**Règle de bascule.** Signal de saturation gratuit : le drapeau `IORING_SQ_TASKRUN`
(option `TASKRUN_FLAG`), levé par le noyau quand des complétions attendent qu'on entre.
Avec `DEFER_TASKRUN`, regarder la CQ ne sert à rien (rien n'y est publié entre deux
`enter`).

```
fin de tour :
  si sq.taskrun()   → tours_charges += 1, tours_libres = 0 ; enter(n, 0, GETEVENTS)
  sinon             → tours_libres += 1, tours_charges = 0 ; enter(n, 1, GETEVENTS, timeout)
sends en ASYNC  si tours_charges ≥ 4  ou travail de fond en attente (save, GC, compression)
sends en direct si tours_libres  ≥ 16 et pas de travail de fond
plafond io-wq   : 1 par défaut ; +1 si on reste saturé alors que les sends sont délégués
                  et que leur délai soumission → CQE monte ; −1 quand ça retombe
```

- Saturé, un send en direct retarde la récolte des CQE suivants (il tourne pendant
  `enter`). Délégué, il libère notre cœur ; le passage de relais (quelques µs) est payé
  ailleurs.
- Seuils asymétriques (4 / 16, à régler) : on bascule vite, on revient lentement.
- Config de départ : 1 worker unbounded épinglé sur un cœur physique réservé (pas le
  frère SMT du nôtre).
- Alternative écartée : notre propre thread d'envoi alimenté par `IORING_OP_MSG_RING`
  (contrôle total, mais un deuxième thread userland, contraire au mono-thread).

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
| ~100 % | ≥ 60 % | ~1 | clients sans pipeline, limite = TCP | délégation io-wq, ou mode B ; conseiller l'auto-pipelining côté client |
| ~100 % | faible | élevé | pipeline, limite = ART + mémoire | préchargement par lots (§8bis) ; déléguer ne sert presque à rien |
| ~100 % | élevé | élevé | les deux | délégation + préchargement |

Le serveur peut écrire ce diagnostic dans ses logs (« le noyau prend 80 % du cœur, clients
sans pipeline : activer l'auto-pipelining côté client ou le mode SQ_POLL »).

---

## 7. Connexions parquées

Un mécanisme unique pour KEYS, UNLINK, FLUSHDB pendant une save, et BLPOP / BRPOP plus
tard.

- La connexion passe en `Mode::Parked(reason)`.
- Le recv reste armé : on détecte le FIN, et les commandes suivantes sont **copiées** dans
  `rx` sans être exécutées (même sémantique que Redis, et invariant §5.2).
- Celui qui termine écrit la réponse dans `out`, repasse la connexion en `Normal` et
  rejoue `rx` : le job à la fin de sa dernière tranche, LPUSH pour BLPOP, le timer pour un
  timeout.
- Si la connexion meurt pendant le parquage : `Draining`. Un job qui la vise garde sa
  référence (`refs`) jusqu'à sa fin et n'écrit rien.

Remplace dans `CONN_DESIGN.md` le « transfert complet + respawn de task » pour BLPOP, et
les commandes async de `delayed.rs`. Même esprit (proactif, sans waker, sans oneshot),
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
2. **Slot réutilisé** : une inscription dans une file d'attente BLPOP prend une référence
   (`refs += 1`, §4.2) ; le slot ne peut pas être redonné tant qu'elle existe. Le LPUSH ou
   le timer qui retire l'inscription rend la référence. Connexion en `Draining` trouvée dans
   une file : sautée.
3. **Événement d'un parquage précédent** : `park_seq` par connexion, incrémenté à chaque
   parquage ; timers et inscriptions portent `(slot, park_seq)`, un événement dont le numéro
   ne correspond plus est ignoré. Règle aussi BLPOP multi-clés : inscrit dans plusieurs
   files, le premier réveil gagne, les autres inscriptions deviennent obsolètes et sont
   retirées paresseusement.

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

- `step(&mut Server, budget) -> Done | More`. Une tranche par tour, round-robin.
- Budget minimal par tour même sous charge (sinon un serveur toujours occupé ne nettoie
  jamais), budget large quand il n'y a ni CQE ni send en attente.
- À réécrire en curseurs sync : `deln_async` / `getn_async` / `SaveDone` d'oxidart
  (`async_command.rs`). `save_step` a déjà la bonne forme.
- I/O fichier de la save : ops `Write` à offset sur un fd classique (kind `File`, slot =
  job), `Fsync` à la fin. Le double tampon de `bgsave.rs` (`fill` / `spare` / `in_flight`)
  se garde tel quel.
- Une chose à la fois : pas de GC pendant une save (règle existante).

---

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
- Les `Cmd<'a>` du lot empruntent l'anneau jusqu'à l'exécution : l'invariant §5.2 devient
  « aucune référence vers l'anneau ne survit au tour ». Les tampons sont rendus après le lot,
  avant le prochain `enter`.
- Équité : plafonner le nombre de commandes d'une même connexion par tour, pour qu'un client
  à pipeline énorme ne monopolise pas le lot.

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

- **Horloge** : une lecture par tour → `art.set_now`. Plus de tâche ticker.
- **Timers** : petit tas binaire (deadlines BLPOP, autosave, fsync AOF chaque seconde). Le
  timeout de l'`enter` bloquant = la prochaine échéance, sinon l'horloge gèlerait au repos.
- **Évicteur TTL** : job à curseur sur le bitmap tagué (remplace le tirage statistique).
- **Signaux** : `signalfd` lue par io_uring (kind `Signal`). Remplace le sondage toutes les
  20 ms de `shutdown.rs`. Un second signal sort tout de suite, comme aujourd'hui.

---

## 10. Pub/Sub

- PUBLISH encode le message une fois, l'ajoute à l'`out` de chaque abonné et le marque
  sale. Pas de writer task, de channel ni de `CancelationFutur`.
- **Limite d'`out` par abonné** → `Draining` (équivalent de `client-output-buffer-limit
  pubsub`). Règle le TODO OOM sur abonné lent.
- Plus tard : un bloc partagé par refcount + `sendmsg` multi-iovec, au lieu de N memcpy.

---

## 11. Versions noyau

| Fonction | Noyau min. |
|---|---|
| recv multishot, anneau de tampons | 6.0 / 5.19 |
| ACCEPT_DIRECT multishot, alloc d'index | 5.19 |
| `DEFER_TASKRUN` | 6.1 |
| bundles | 6.10 |
| anneau incrémental (`IOU_PBUF_RING_INC`) | 6.12 |
| zcrx (`RecvZc`) | 6.15 |
| Machine de dev | 7.2-zen ✓ |

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
  à refaire. Les scripts du dépôt ne passent pas encore l'option.
- **memtier 2.4.2 : `--key-pattern=Z` est inutilisable** (quasi uniforme quel que soit
  l'exposant). Zipf réaliste : YCSB (scrambled zipfian) ou générateur maison.
- **Base de 10 M clés réutilisable** : `user:10000000..19999999`, valeurs 100–300 o,
  `target/bench_dump/dump.rxd` (2 Go), reload 0,78 s avec `RADIXOX_DIR=target/bench_dump`.
- Préciser comment l'arbre a été construit : un reload ou un load séquentiel donne des
  nœuds contigus (ordre DFS), des SET aléatoires des nœuds dispersés.

### 12.2 Comparaison Valkey 9.1.2 (base 10 M, uniforme, 1:4, 1 cœur chacun)

| | p1 (80 conns) | p32 | p256 | RSS | rechargement |
|---|---|---|---|---|---|
| RadixOx avant préchargement | — | ~935 K | 1,21 M | 3,33 Go | 0,78 s |
| RadixOx préchargement 8 | 196 K (82 % noyau) | 1,09–1,21 M | **1,81 M** | 3,33 Go | 0,78 s |
| Valkey 1 thread | — | **1,32 M** | 1,69 M | **2,58 Go** | 6,5 s (SAVE 18 s) |
| Valkey io-threads 2 (2 cœurs) | — | 2,06 M | 2,81 M | — | — |

Points faibles : mémoire (~333 contre ~258 o/clé), pipeline court. Points forts : pipeline
profond, persistance. Écart sans pipeline / pipeline 64 : ×7,4.

---

## 13. Migration

1. **Binaire parallèle `radixox-uring`** (io-uring 0.7.15) : boucle, accept, recv sur
   l'anneau incrémental, send, toutes les commandes sync (`execute_sync` ne dépend pas du
   runtime). Bench A/B contre le binaire monoio, en pipeline 1 et 256 : aucune régression.
2. Horloge, timers, `signalfd`, évicteur à curseur.
3. Connexions parquées, jobs KEYS / UNLINK / FLUSHDB, port de BGSAVE.
4. Pub/Sub.
5. Suppression de monoio : `oxidart/monoio.rs`, `async_command.rs` (→ curseurs),
   `#[monoio::test]`, `resp.rs` / `utils/mod.rs`.
6. Ensuite : lots de recherches sur tout le tour (§8bis), `IOSQE_ASYNC` + règle `TASKRUN`, mode B SQ_POLL à comparer, flush anticipé réglé, cas 3 du parseur, saves auto + AOF,
   pool enregistré + `SEND_ZC`, allocateur bump, GC.

---

## 14. Écarté

| Option | Raison |
|---|---|
| Runtime à futures maison (`RawTask`, `fn_poll`, `syscall_nb`, `CURRENT_TASK`) | garde la machinerie des futures sans en avoir besoin ; le multishot y est pénible à modéliser |
| SQ_POLL **par défaut** | incompatible avec `DEFER_TASKRUN` ; gardé comme mode B à mesurer (§2) |
| Notre propre thread d'envoi (`MSG_RING`) | contrôle total d'io-wq, mais un deuxième thread userland |
| Dépasser 8–10 clés par lot de préchargement | mesuré : plus lent (§8bis), dans la fenêtre du pipeline. **À re-mesurer** : hors réseau, MGET via `descend_batch` (idx réutilisé, sans seconde descente) va de 155 ns/clé à 8 clés à 105 ns/clé à 32–64 (§17.1) |
| Slotmap / clé générationnelle | l'index de fixed file est stable tant qu'on ne ferme pas (§4.2) |
| `READ_FIXED` | un tampon réservé par connexion, pas de multishot, copie noyau de toute façon |
| Beaucoup de petits tampons dans l'anneau | frontières fréquentes ; le mode incrémental permet quelques gros tampons |
| Contiguïté par connexion dans l'anneau (un groupe par conn) | ids sur 16 bits, mémoire réservée par connexion |
| Parseur reprenable à n'importe quel octet | trop d'états ; RESP annonce les tailles (§5.3) |
| Découper un pipeline par nombre de commandes | un send coûte ~30 commandes de CPU ; découpage par octets à la place (§6.2) |
| NAPI busy poll par défaut | brûle du CPU pour quelques µs ; coûteux en cloud sur les instances à crédits ou facturées au CPU. Option possible, pas dans le prototype |
| Client de bench sur le desktop, serveur dans le cloud | RTT internet, Wi-Fi, trafic sortant facturé (~130 $/h à 400 Mo/s) |

---

## 15. À valider

- Prototype en binaire parallèle plutôt qu'une réécriture de `resp.rs` sur place.
- Connexions parquées comme mécanisme unique (rend obsolètes des parties de
  `CONN_DESIGN.md` et de la mémoire « blocking ops »).
- Comportement réel de l'anneau incrémental avec plusieurs sockets (entrelacement, §5.1).
- `TCP_NODELAY` sur socket direct : `SetSockOpt` ou héritage du socket d'écoute (§4.1).
- Mode A (`DEFER_TASKRUN` + io-wq) contre mode B (SQ_POLL) à budget égal (§2).

---

## 16. Points pas encore traités (à ne pas oublier)

**Anneau et boucle**
- **CQ pleine** : une rafale de multishot peut déborder la CQ. Surveiller
  `IORING_SQ_CQ_OVERFLOW` et entrer pour vider ; dimensionner la CQ large.
- **SQ pleine en plein tour** (beaucoup de connexions sales) : soumettre tout de suite
  puis continuer, au lieu de perdre des sends.
- **Fin de multishot** : un recv ou un accept multishot peut s'arrêter pour d'autres
  raisons qu'`ENOBUFS` (CQ pleine, table de fixed files pleine → `ENFILE`). Règle : tout CQE
  sans `F_MORE` réarme, ou met la connexion (ou l'accept) dans une liste « à réarmer ».
- **Mode incrémental** : ne jamais rendre un tampon tant que son dernier morceau porte
  `F_BUF_MORE`.
- **Détection des fonctions noyau au démarrage** (`io_uring_disabled`, version, opcodes
  supportés) avec un message d'erreur clair, plutôt qu'un échec obscur.
- **Arrêt propre** : annuler les multishots, finir les sends en vol, `CLOSE_DIRECT` de
  tous les slots, puis sortir (ou save d'arrêt).

**Connexions et protection**
- **Clients lents à envoyer** (« slowloris ») : une frame partielle qui n'avance plus garde
  un reliquat → limite de taille de `rx` et timeout de frame partielle.
- **Timeout d'inactivité** (équivalent du `timeout` de Redis), via le tas de timers.
- **Limite de connexions** : table de fixed files pleine → refuser proprement, et le dire
  dans `INFO`.
- **Grosses réponses** (KEYS, HGETALL énorme, MGET de 10 000 clés) : production par
  tranches avec la backpressure, au lieu d'un `out` géant.
- AUTH absent, bind `0.0.0.0` (déjà noté côté prod).

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
- **Horloge du TTL** : la boucle lit une horloge monotone par tour, mais un dump rechargé
  après redémarrage demande une horloge murale pour les échéances. Vérifier quelle horloge
  la save et le reload utilisent.

**Tests**
- **Tests déterministes de la boucle** : comme tout passe par des CQE, on peut alimenter
  les machines à états avec des séquences de CQE fabriquées (faux anneau) : envoi partiel,
  `ENOBUFS`, `ECONNRESET` en plein send, annulation qui croise une complétion, CQ pleine.
  Impossible proprement sous monoio, naturel sans runtime.

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
    parse      : toutes les connexions → un lot global de Cmd (ordre gardé par connexion)
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
| Mode B : SQ_POLL épinglé | tous les syscalls de soumission | §2 |
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
4. Délégation réseau mesurée : mode A + io-wq contre mode B SQ_POLL, à budget égal (§2, §6.4).
5. Fusion des lectures identiques (§17.2.2), avec une charge zipfienne pour la justifier.
6. Modes extrêmes sur VM dédiée : NAPI busy poll, `SEND_ZC`, zcrx (§17.3) ; bench cloud.
