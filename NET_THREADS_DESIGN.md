# Threads IO + bus : dépasser 1 M ops/s sans pipeline (design, 2026-10-24)

> **Statut : noté pour plus tard, rien d'implémenté.** Discuté avec l'utilisateur le
> 2026-10-24 après les mesures SQPOLL / `RECV_ASYNC` (RUNTIME_ARCH §6.4, §6.4 bis).
> Développe la piste 4 de RUNTIME_ARCH §17.6. Revient en partie sur la décision du
> 2026-07-29 (mono-thread pur, modèle « 2 threads » de mai abandonné) : le **métier reste
> mono-thread**, seul le réseau sort de son thread, et sans `OwnedByte` (§3).

## 1. Pourquoi : un seul thread de réception plafonne

Mesures loopback, Ryzen 7600X, Zipf 0,99, 1:4, 100 o, 80 conns, sans pipeline :

| travail par op | coût | où |
|---|---|---|
| recv multishot (notification, task_work, CQE, copie) | ~1,3 µs | le thread qui fait `enter`, ou le thread SQPOLL |
| send (loopback : réception du pair + netfilter inclus) | ~3,8 µs | workers io-wq, se délègue bien |
| métier (exécution, encodage, parse) | ~0,4 µs ; ~0,25 µs en pipeline 64 | la boucle |

- Sur ~1,3 µs de recv, la copie fait ~8 % ; ~60 % est la mécanique io_uring (task_work,
  poll, choix du tampon, CQE). Le recv est un « tu auras » : le coût est la notification.
- Un send délégué coûte ~0,2 µs au propriétaire pour ~3,8 µs déplacés ; un recv délégué
  (`RECV_ASYNC`) coûte **plus** au propriétaire qu'il ne lui retire (relais
  `EAGAIN` → poll → task_work du propriétaire → io-wq). Dans io_uring, une requête
  appartient au thread qui l'a soumise : on ne répartit la réception qu'en répartissant
  la propriété, donc des anneaux à des threads différents.
- Plafond d'un thread de réception : ~700–900 K ops/s (estimé). Le métier seul tiendrait
  2,5 M sans pipeline et ~4 M en pipeline (mesuré : 4 M à 100 % utilisateur sous SQPOLL).

## 2. Options écartées

| option | pourquoi non |
|---|---|
| K anneaux SQPOLL | un cœur brûlé par anneau sous trafic : jugé invendable |
| K anneaux servis par le même thread | le travail retombe sur ce thread (`DEFER_TASKRUN`) |
| recv simples sur io-wq (`RECV_ASYNC`) | mesuré en perte, y compris sous SQPOLL (§6.4) |
| un anneau recv SQPOLL + un anneau send | possible (fd dans deux anneaux via `MSG_RING`), ~+15 % seulement |
| `IOSQE_CQE_SKIP_SUCCESS` sur les sends | la CQE du send porte : tampon `sending` réutilisable, send suivant (un seul en vol, ordre), contre-pression (`drawable`), `refs` de fermeture ; sans pipeline rien à regrouper |
| multiplexeur eBPF (sockmap, `sk_skb` + `sk_msg`) | découpage RESP sous le vérificateur, gain non démontré, `CAP_BPF` refusé en conteneur : piste de recherche seulement |
| kernel bypass (DPDK + TCP utilisateur, AF_XDP) | carte réservée, pile TCP à maintenir, limite PPS des petites VM inchangée ; option lointaine pour machines dédiées |

## 3. Architecture

```
threads IO (N)                                  thread métier (1)
──────────────                                  ─────────────────
anneau propre, DEFER_TASKRUN, multishot
connexions réparties à l'accept
recv → assemble → parse (cmd.rs)
  → bus requêtes : Request { conn, cmd } ──────► lit les N bus (SPSC), exécute
    (pointeurs dans SES tampons)                  copie seulement ce qu'il stocke
                                                  écrit les réponses dans la zone
  ◄────── bus réponses : (conn, début, len) ──────  de réponses partagée
send sur la tranche (zéro copie)
  → file de libération : zones envoyées ───────►  réutilise la zone
réponse n = ack des tampons de la commande n
  → rend les tampons de réception à l'anneau
```

- **Requêtes parsées par le thread IO.** Le métier reçoit `Cmd<'a>` dont les clés et
  valeurs pointent dans les tampons du thread IO (même modèle d'emprunt qu'aujourd'hui
  avec `read_buf` : clé empruntée, seule la valeur stockée est copiée/allouée). Pas
  d'`OwnedByte`, ce qui levait l'objection de juillet. Erreurs de protocole : répondues
  par le thread IO lui-même.
- **Ack = réponse.** Les réponses suivent l'ordre des commandes d'une connexion : la
  réponse n libère les tampons de la commande n. Le métier ne garde aucun pointeur après
  avoir répondu, sauf déclaration explicite (« je garde » / « je relâche ») pour :
  BLPOP parqué (§7), file MULTI, AOF et réplication s'ils réutilisent les octets bruts,
  jobs (UNLINK, KEYS…). Par défaut ces cas copient.
- **Commandes coupées entre deux recv** : le thread IO les rassemble dans un tampon
  contigu (ce que fait déjà le cœur : `rx` + morceaux) ; les pointeurs visent ce tampon.
- **Bus à taille variable** : en-tête fixe + zone variable (MSET, HSET à N champs).
- **Réponses** : le métier écrit « comme un bourrin » dans la zone de réponses du thread
  IO de la connexion, groupées par connexion et par tour (comme `out`) ; le thread IO
  envoie la tranche puis la rend par la file de libération.
- **Grosses valeurs** : copie dans la zone de réponses par défaut. Envoyer depuis le pool
  sans copie exigerait d'épingler le bloc (GC, compactage ; en-têtes sans compteur de
  références) : plus tard.
- **Contre-pression** : bus plein → le thread IO cesse de lire la connexion (TCP freine le
  client). Côté réponses, le métier compte les octets non acquittés par connexion et
  cesse d'exécuter ses commandes au-delà d'un seuil (équivalent de `out_backpressure`).
  Grosses réponses (KEYS, LRANGE énorme) par morceaux.
- **Commandes qui touchent d'autres connexions** (Pub/Sub, réveils BLPOP, réplicas) : le
  métier route chaque réponse vers le bus du thread IO propriétaire de la connexion.
- **Fichiers** (AOF, save) : restent au métier sur son propre anneau, ou passent à un
  thread dédié ; le métier ne fait plus d'I/O réseau.

## 4. Réveils : personne ne tourne à vide

- **Métier** : drapeau « je dors », revérifier les N bus, `futex_wait`. Un thread IO ne
  fait `futex_wake` que si le drapeau est levé : aucun appel système sous charge.
- **Thread IO** : dort dans `enter`. Son anneau garde un `IORING_OP_FUTEX_WAIT` (noyau
  ≥ 6.7 ; on exige 6.12) sur le compteur du bus réponses : réseau et métier le réveillent
  par le même `enter`. Même règle drapeau + `futex_wake` côté métier.
- Attente active courte avant de dormir, des deux côtés (quelques µs, à régler).

## 5. Répartition des connexions

- `SO_REUSEPORT` : un socket d'écoute par thread IO.
- Option : programme eBPF `SK_REUSEPORT` (ou `SO_INCOMING_CPU`) pour donner la connexion
  au thread du CPU qui reçoit ses IRQ (softirq et recv sur le même cœur). Une fois par
  connexion, quelques dizaines de lignes.
- Répartition par connexion, pas par requête : verrou de socket et cache non partagés,
  ordre gratuit. Déplacer une connexion très chargée : plus tard si besoin.

## 6. Repli epoll et compatibilité

Le métier ne voit que le bus : un thread IO peut être io_uring (défaut) ou epoll
(`epoll_wait` + `read()` dans ses tampons + `send()` sur les tranches, même modèle de
pointeurs et d'ack). Un peu plus de CPU par message, à mesurer. Intérêt : io_uring est
souvent bloqué (seccomp de conteneur, environnements gérés — à vérifier au cas par cas).
Pour un vrai « marche partout », prévoir aussi un repli fichiers (`pwrite` / `fsync` dans
un thread). Mode actuel conservé : `RADIXOX_NET_THREADS=0` (défaut), boucle seule, le plus
efficace en charge moyenne.

## 7. Dimensionnement (estimé, à mesurer à deux machines)

Coût réseau par op sans pipeline, vraie carte : recv ~1,3 µs + softirq de réception
~1–1,5 µs (payé par memtier en loopback, par nos CPU d'IRQ sur une carte) + send ~1–2 µs +
bus ~0,1–0,2 µs ≈ **3,5–5 µs** (loopback : ~5,3 µs).

| sans pipeline | cœurs réseau | métier | machine |
|---|---|---|---|
| 1 M | ~4–5 | 1 | 8 cœurs |
| 2 M | ~7–10 | 1 | 12–16 cœurs |
| 4 M | ~14–20 | 1, à sa limite | 24–32 cœurs |

4 M ops ≈ 8 M paquets/s : carte ≥ 25 Gbit/s multi-files, IRQ réglées ; en cloud, grosse
instance réseau optimisé. Busy poll NAPI (`IORING_REGISTER_NAPI`) : −20 à −30 % plausible
côté réseau. Un pipeline client même faible (4 commandes) divise le coût réseau par ~4.

**Argument** : une seule base, atomicité totale (transactions sans coordination, save
cohérente) jusqu'à plusieurs millions d'ops/s — Redis Cluster découpe la base, Dragonfly
coordonne ses threads pour les commandes multi-clés. Le plafond ~4 M vient de GET/SET
simples ; les commandes lourdes l'abaissent.

## 8. À vérifier avant d'écrire du code

1. La boucle peut consommer la CQ / produire dans la SQ d'un anneau `DEFER_TASKRUN` dont
   un autre thread fait `enter` (ou on garde des files à nous, plus simple à raisonner).
2. Socket enregistré dans deux anneaux (`MSG_RING` fixed file) : recv d'un côté, send de
   l'autre.
3. Part du parse dans la boucle actuelle (build `profiling`, `perf`) : chiffre le gain de
   parser côté thread IO.
4. `SharedByte` non `Send`, allocateur v2 mono-thread : vérifier qu'aucune valeur du pool
   ne traverse la frontière (les pointeurs vont dans les tampons IO, pas l'inverse ; les
   réponses sont copiées dans la zone).

## 9. Ordre

1. Points 1–3 du §8 (tests courts).
2. Prototype N = 1 contre le mode actuel à débit égal : coût du bus.
3. N = 2–3, bench à deux machines (client sur l'autre PC) : vrai plafond.
4. Repli epoll ; `SK_REUSEPORT` ; NAPI busy poll.
