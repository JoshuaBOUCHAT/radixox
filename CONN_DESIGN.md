# Refactor Architecture — Conn State Machine & Pub/Sub

## Contexte

Remplacement de l'architecture actuelle (actor model : `pubsub_writer` task persistante + mpsc channel) par un modèle **proactor** : buffers partagés + write tasks éphémères.

Transformation : **Actor (Active Object) → Proactor (completion handlers éphémères)**

---

## Dépendances

### Objectif : minimiser les dépendances externes

- `slotmap` → **supprimer**, remplacer par `GenArena` dans `radixox-lib`
- `bytes` → **supprimer plus tard**, `SharedByte` est déjà là, buffers I/O remplaçables
- `local_sync::mpsc::unbounded` → **supprimer** (plus de writer task persistante)

---

## Structures de données

### `GenArena<T>` — à implémenter dans `radixox-lib`

Arène générationelle minimale (~80 lignes, zéro dep) :

```rust
pub struct GenArena<T> {
    slots: Vec<(u32, Option<T>)>,  // (generation, value)
    free: Vec<u32>,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Key { idx: u32, gen: u32 }

impl<T> GenArena<T> {
    pub fn insert(&mut self, v: T) -> Key  // O(1)
    pub fn remove(&mut self, k: Key) -> Option<T>  // O(1)
    pub fn get(&self, k: Key) -> Option<&T>  // O(1)
    pub fn get_mut(&mut self, k: Key) -> Option<&mut T>
}
```

Le `Key` générationnel permet la détection automatique des connexions mortes : si une conn est retirée de l'arène, tout lookup avec son ancien Key retourne `None` sans signal explicite.

```rust
type ConnId = Key;
type SharedConns = Rc<RefCell<GenArena<Conn>>>;
type SharedRegistry = Rc<RefCell<HashMap<SharedByte, HashSet<ConnId>>>>;
```

---

### `Conn`

```rust
struct Conn {
    // Buffers — toujours présents, jamais dans l'enum
    io_buffer: BytesMut,            // on écrit TOUJOURS ici
    swap_buffer: Option<BytesMut>,  // None = kernel l'a, Some = libre

    // Write half — None quand une write_task l'a
    write: Option<OwnedWriteHalf<TcpStream>>,

    // État
    state: ConnState,

    // Signaux (mode Pub uniquement)
    write_fail_tx: Option<oneshot::Sender<()>>,  // write_task → main task : écriture morte
    shutdown_write: bool,                         // main task → write_task : stop proprement
    write_done_tx: Option<oneshot::Sender<()>>,  // write_task → main task : write_half rendu
}
```

### `ConnState`

```rust
enum ConnState {
    Normal,           // write est Some, accès direct, pas de partage
    Pub {
        channels: HashSet<SharedByte>,  // channels abonnés
        conn_id: ConnId,                // clé dans SharedConns pour PUBLISH
    },
    Blocking {
        keys: SmallVec<[SharedByte; 2]>,
        deadline: Option<Instant>,   // None = bloque indéfiniment
    },
}
```

---

## Mode Blocking : transfert complet + reconstruction (Proactor pur)

### Principe

Quand BLPOP arrive sur une liste vide, la task Normal ne se suspend pas —
elle **transfère** l'ownership de toutes les ressources dans un `BlockingConn`
stocké dans la GenArena, puis **sort**. Zéro future en vol, zéro select!.

```rust
struct BlockingConn {
    read:     OwnedReadHalf<TcpStream>,
    write:    OwnedWriteHalf<TcpStream>,
    io_buf:   BytesMut,
    keys:     SmallVec<[SharedByte; 2]>,
    deadline: Option<Instant>,
}

type SharedBlockers = Rc<RefCell<HashMap<SharedByte, VecDeque<(ConnId, BlockingConn)>>>>;
```

### Transition Normal → Blocking

```rust
// Dans handle_normal, BLPOP sur liste vide :
let conn = BlockingConn { read, write, io_buf, keys, deadline };
blockers.borrow_mut().entry(key).or_default().push_back((conn_id, conn));
return; // task terminée
```

### Wakeup (LPUSH/RPUSH)

```rust
fn notify_blocker(blockers: &SharedBlockers, key: &SharedByte, value: Bytes, art: SharedART, registry: SharedRegistry) {
    let Some((_, b)) = blockers.borrow_mut().get_mut(key).and_then(|q| q.pop_front()) else { return };
    monoio::spawn(async move {
        let (res, write) = b.write.write_all(encode_blpop_response(key, value)).await;
        if res.is_err() { return; } // conn morte → drop = TCP FIN
        // Reconstruction from scratch du Normal mode
        handle_connection_normal(b.read, write, b.io_buf, art, registry).await;
    });
}
```

### Timeout

```rust
// Task timer lancée au moment de la transition Normal → Blocking :
monoio::spawn(async move {
    monoio::time::sleep_until(deadline).await;
    let removed = blockers.borrow_mut()
        .get_mut(&key)
        .and_then(|q| { /* retirer par conn_id */ });
    let Some(b) = removed else { return }; // déjà réveillé par un push
    let (res, write) = b.write.write_all(encode_nil_array()).await;
    if res.is_err() { return; }
    handle_connection_normal(b.read, write, b.io_buf, art, registry).await;
});
```

### Détection de déconnexion pendant le blocage

**Option A (défaut)** : personne ne lit pendant le blocage. La mort silencieuse
est détectée au moment du wakeup si le write échoue → cleanup implicite par drop.
Simple, acceptable (même comportement que Redis).

**Option B** : lightweight reader task qui surveille QUIT / FIN :
```rust
monoio::spawn(async move {
    let (res, _) = b.read.read(BytesMut::with_capacity(8)).await;
    // Ok(0) = FIN, Err = RST → retirer du registry
    if matches!(res, Ok(0) | Err(_)) {
        remove_blocker(&blockers, conn_id, &keys);
    }
});
```
Si reader task et LPUSH wakeup arrivent simultanément :
le premier `remove` par `conn_id` gagne, l'autre obtient `None` → exit silencieux.

### Garanties

| Scénario | Détection | Cleanup |
|---|---|---|
| LPUSH/RPUSH | immédiat | wakeup task → Normal |
| Timeout | à l'expiry | timer task → Normal (NIL) |
| Client FIN/RST | au wakeup (write fail) | drop implicite |
| Client FIN/RST (option B) | immédiat | reader task → remove |

---

## Modèle de buffers (double buffering)

```
io_buffer   : on écrit toujours ici (commandes, PUBLISH entrant)
swap_buffer : tampon vide qui "stationne" pendant qu'on accumule

Flush :
  1. swap(io_buffer, swap_buffer.take())  →  io_buffer = vide, buf_to_write = données
  2. write_all(buf_to_write).await        →  kernel écrit
  3. returned (vide) → swap_buffer = Some(returned)
  4. si io_buffer non vide : recommencer (loop ou spawn)

Pendant le write :
  swap_buffer = None   →  "write en cours"
  io_buffer   = libre  →  accumulation possible
```

---

## Dispatch selon ConnState

Chaque mode a sa propre fonction de dispatch — pas de `if sub_tx.is_none()` éparpillés :

```rust
impl Conn {
    async fn handle_cmd(&mut self, cmd, args, art, shared_conns, registry) {
        match self.state {
            ConnState::Normal       => self.handle_normal(cmd, args, art, registry).await,
            ConnState::Pub { .. }   => self.handle_pub(cmd, args, shared_conns, registry),
            ConnState::Blocking     => { /* TODO */ }
        }
    }
}

fn handle_pub(cmd) {
    match cmd {
        SUBSCRIBE   => ajouter channels
        UNSUBSCRIBE => retirer channels, si vide → transition Normal
        PING        => PONG dans io_buffer
        QUIT        => OK dans io_buffer, signal fermeture
        PUBLISH     => cmd_publish(...)
        _           => erreur "only SUBSCRIBE/UNSUBSCRIBE/PING/QUIT/PUBLISH"
    }
}

fn handle_normal(cmd) {
    // dispatch table COMMANDS[] + ASYNC_COMMANDS[]
    // PUBLISH autorisé ici aussi
}
```

---

## Mode Pub : accès partagé via SharedConns

En mode **Pub** : `Conn` vit dans `SharedConns` (GenArena). PUBLISH accède à l'`io_buffer` du subscriber via `conn_id` lookup.

### Règle fondamentale
> **Jamais de borrow `SharedConns` à travers un `.await`.**
> Borrow → opération synchrone → release → await.

### Transition Normal → Pub

```rust
// 1. Mettre Conn dans SharedConns
let conn_id = shared_conns.borrow_mut().insert(conn);

// 2. Créer les signaux
let (fail_tx, fail_rx) = oneshot::channel();
let (done_tx, done_rx) = oneshot::channel();
shared_conns.borrow_mut()[conn_id].write_fail_tx = Some(fail_tx);
shared_conns.borrow_mut()[conn_id].write_done_tx = Some(done_tx);

// 3. ConnState::Pub
// main task garde: conn_id, fail_rx, done_rx comme variables locales
```

### Transition Pub → Normal (UNSUBSCRIBE, tous channels vides)

```rust
// Signaler à write_task de s'arrêter après le write en cours
shared_conns.borrow_mut()[conn_id].shutdown_write = true;

// Attendre retour du write_half (ou fail)
monoio::select! {
    _ = done_rx => {
        let wh = shared_conns.borrow_mut()[conn_id].write.take();
        match wh {
            Some(wh) => { /* transition Normal(wh) */ }
            None     => { /* write avait fail, on ferme */ break }
        }
    }
    _ = fail_rx => break,  // connexion morte
}

// Retirer de SharedConns
shared_conns.borrow_mut().remove(conn_id);
```

---

## Write task éphémère (Proactor)

```rust
async fn write_task(
    shared_conns: SharedConns,
    conn_id: ConnId,
    mut write_half: OwnedWriteHalf<TcpStream>,
    buf: BytesMut,
) {
    let mut buf = buf;
    loop {
        let (res, returned) = write_half.write_all(buf).await;
        buf = returned;
        buf.clear();

        // Conn encore vivante ?
        let Some(slot) = shared_conns.borrow_mut().get_mut(conn_id) else {
            return; // conn morte → drop write_half → TCP FIN
        };

        if res.is_err() {
            // Signaler la main task
            let _ = slot.write_fail_tx.take().map(|tx| tx.send(()));
            // write_done aussi pour débloquer une éventuelle transition Pub→Normal
            let _ = slot.write_done_tx.take().map(|tx| tx.send(()));
            return; // drop write_half
        }

        if slot.shutdown_write {
            // Main task demande à reprendre write_half (UNSUBSCRIBE)
            slot.write = Some(write_half);
            let _ = slot.write_done_tx.take().map(|tx| tx.send(()));
            return;
        }

        if slot.io_buffer.is_empty() {
            // Rien de plus à écrire, rendre write_half
            slot.write = Some(write_half);
            slot.swap_buffer = Some(buf);
            return;
        }

        // Plus de données accumulées : swap et recommencer
        std::mem::swap(&mut slot.io_buffer, &mut buf);
    }
}
```

### Déclenchement (trigger_write)

```rust
fn trigger_write(shared_conns: &SharedConns, conn_id: ConnId) {
    let mut s = shared_conns.borrow_mut();
    let Some(slot) = s.get_mut(conn_id) else { return };

    // Swap uniquement si write_half dispo ET données présentes
    if slot.write.is_none() || slot.io_buffer.is_empty() { return }
    let Some(empty) = slot.swap_buffer.take() else { return };

    let mut buf = std::mem::replace(&mut slot.io_buffer, empty);
    let write_half = slot.write.take().unwrap();
    drop(s);

    monoio::spawn(write_task(shared_conns.clone(), conn_id, write_half, buf));
}
```

---

## PUBLISH

```rust
fn cmd_publish(args, registry, shared_conns) -> Frame {
    let channel = &args[0];
    let message = &args[1];

    // Encoder une seule fois
    let mut encoded = BytesMut::new();
    extend_encode(&mut encoded, &Frame::Array(vec![
        Frame::BulkString("message".into()),
        Frame::BulkString(channel.clone()),
        Frame::BulkString(message.clone()),
    ]));

    let reg = registry.borrow();
    let Some(subs) = reg.get(channel) else { return Frame::Integer(0) };

    let count = subs.len() as i64;
    let dead: Vec<ConnId> = subs.iter().filter_map(|&id| {
        let mut s = shared_conns.borrow_mut();
        let slot = s.get_mut(id)?;              // None = mort → nettoyage auto
        slot.io_buffer.extend_from_slice(&encoded);
        drop(s);
        trigger_write(&shared_conns, id);
        None  // vivant, garder
    }).collect();

    // Nettoyer les morts
    // ...

    Frame::Integer(count)
}
```

---

## Shutdown / Cleanup

### Séquence de fermeture (toujours le même chemin)

```rust
async fn handle_connection(...) {
    // ... loop principale ...

    // Cleanup — UN SEUL point de sortie
    cleanup(conn_id, &registry, &shared_conns);
}

fn cleanup(conn_id, registry, shared_conns) {
    // 1. Retirer des channels pub/sub
    let mut reg = registry.borrow_mut();
    reg.retain(|_, subs| { subs.remove(&conn_id); !subs.is_empty() });

    // 2. Retirer du GenArena → Conn droppé → write_half droppé (si Some) → TCP FIN
    //    Key générationnel invalidé → write_task verra None au prochain lookup → exit silencieux
    shared_conns.borrow_mut().remove(conn_id);
}
```

### Garanties par scénario

| Scénario | write_half droppé | SharedConns nettoyé | write_task stoppée |
|---|---|---|---|
| Read retourne 0 / err | Par Conn drop ou write_task | Oui (cleanup) | Gen invalide → None → exit |
| Write fail (write_task) | write_task drop | Oui (après Signal 1) | Auto (elle sort) |
| QUIT | Par Conn drop | Oui | Gen invalide → exit |
| UNSUBSCRIBE (Pub→Normal) | Repris par main task | Partiel (reste jusqu'à close) | Via shutdown_write flag |
| Panic write_task | Drop implicite | Oui | Auto |

---

## Optimisation future : broadcast writev

Quand PUBLISH cible N subscribers, au lieu de N `trigger_write` indépendants (N soumissions io_uring séparées) :

```
PUBLISH channel msg
  ├─ écrire msg dans io_buffer de chaque subscriber
  ├─ collecter les "libres" (write_half dispo) → batch
  ├─ spawn broadcast_task(batch)
  └─ les "occupés" : leur write_task s'en chargera au retour

broadcast_task(batch):
  ├─ swap io↔swap pour chaque conn du batch
  ├─ join_all write_all × N    ← N SQEs en un seul batch io_uring
  ├─ pour chaque résultat :
  │    ok  + io_buffer non vide → next_batch
  │    ok  + io_buffer vide     → rend write_half dans Conn
  │    err                      → signal write_fail, drop write_half
  └─ si next_batch → loop
```

Gain : N SQEs soumis en un syscall. Particulièrement utile pour high-fanout (1 publisher, beaucoup de subscribers).

---

## Ordre d'implémentation

1. **`GenArena<T>`** dans `radixox-lib` — prerequis, ~80 lignes
2. **`ConnState` + double buffer** dans `resp.rs` — refactor complet
3. **`bytes` removal** — chantier séparé, plus tard
4. **`broadcast_task` (writev)** — optimisation future, après que le modèle de base fonctionne
