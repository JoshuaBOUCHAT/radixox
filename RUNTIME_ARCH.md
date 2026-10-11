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

**Setup de l'anneau.** `SINGLE_ISSUER` + `DEFER_TASKRUN`, `register_ring_fd`, CQ
dimensionnée large (`setup_cqsize`). Pas de SQ_POLL : il est incompatible avec
`DEFER_TASKRUN`, et il a dégradé les perfs en mai (thread noyau sur notre cœur).

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

Règle décidée le 2026-10-10. Par défaut, le noyau exécute le send sur notre thread
pendant `enter`. `IOSQE_ASYNC` l'envoie aux workers io-wq. On marque `ASYNC` **quand du
travail de fond attend** (save, GC, compression) **ou quand un submit dépasse K SQE**,
avec hystérésis (au-dessus de K, retour sous K/2). Garde-fous : un send en vol par conn
(§6.1), `register_iowq_max_workers` à 1–2, `register_iowq_aff` sur un cœur réservé.
Seulement les envois.

Justification chiffrée (§12) : en pipeline 1, le chemin du send représente ~75 % du CPU
de notre thread. Mesure loopback, donc gonflée : à revalider sur un vrai réseau.

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
6. Ensuite : `IOSQE_ASYNC`, flush anticipé réglé, cas 3 du parseur, saves auto + AOF,
   pool enregistré + `SEND_ZC`, allocateur bump, GC.

---

## 14. Écarté

| Option | Raison |
|---|---|
| Runtime à futures maison (`RawTask`, `fn_poll`, `syscall_nb`, `CURRENT_TASK`) | garde la machinerie des futures sans en avoir besoin ; le multishot y est pénible à modéliser |
| SQ_POLL | incompatible avec `DEFER_TASKRUN` ; a dégradé les perfs en mai |
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
