# Runtime Async Custom — Architecture

Remplacement de monoio par un runtime minimal io_uring single-thread,
taillé exactement pour RadixOx. Objectif : zéro syscall, zéro copy,
zéro allocation sur le chemin chaud.

---

## 1. Event Loop

```
loop {
    io_uring_enter(submit_pending, wait=1)   // 1 syscall, batch SQEs

    for cqe in cq.drain() {
        match cqe.user_data() {
            TIMEOUT_UDATA => wake_timers(),
            CANCEL_UDATA  => { /* ignore */ },
            packed        => {
                let (op_idx, gen) = unpack(packed);
                if op_slab[op_idx].generation == gen {
                    op_slab[op_idx].result = Some(cqe.result());
                    task_queue.push(op_slab[op_idx].task_idx);
                }
                // sinon : stale CQE (zombie) → ignore
            }
        }
    }

    while let Some(task_idx) = task_queue.pop() {
        poll_task(task_idx);
        // les SQEs produits pendant poll() s'accumulent dans le SQ
        // partent en batch au prochain io_uring_enter
    }
}
```

Avec SQ_POLL (déjà utilisé dans RadixOx) : le kernel thread consomme
les SQEs en continu → `io_uring_enter` sert uniquement à attendre les CQEs.
Le batching CQ reste utile : drainer toutes les CQEs avant de re-poll
évite N tours de boucle pour N CQEs.

---

## 2. Task

Une seule alloc : header + future inline.

```rust
struct Task {
    ptr_poll: Option<NonNull<fn(*mut (), *mut Context) -> Poll<()>>>,
    //        └── None = poll déjà consommé
    ptr_drop: fn(*mut ()),
    rc:       u16,    // nb de SQEs en vol → 0 = libérer le slot
    data:     *mut (), // pointe sur F inline après le header
}
```

États encodés sans discriminant :

| `Option<NonNull<Task>>` | `ptr_poll` | `rc` | Signification              |
|-------------------------|------------|------|----------------------------|
| None                    | —          | —    | slot libre                 |
| Some                    | Some       | > 0  | task vivante               |
| Some                    | None       | > 0  | poll consommé, SQEs en vol |
| Some                    | None       | 0    | → libérer                  |

### Construction (type erasure à la création)

```rust
fn Task::new<F: Future<Output=()>>(f: F) -> *mut Task {
    // Layout::extend → Task header + F inline, une seule alloc
    // instancie poll_fn::<F> et drop_fn::<F> pendant que F est connu
    // après : type complètement effacé, stocké comme fn ptrs bruts
    // cache locality : fn ptrs + future data dans la même alloc
}

unsafe fn poll_fn<F: Future<Output=()>>(data: *mut (), cx: *mut Context) -> Poll<()>;
unsafe fn drop_fn<F>(data: *mut ());
```

### Slab de tasks

```rust
// Option<NonNull<Task>> : niche NonNull → None = 0x0 = slot libre
// pas de discriminant supplémentaire
type TaskSlab = Slab<Option<NonNull<Task>>>;
```

---

## 3. Op Slots

Un slot par SQE en vol. `user_data` du SQE = valeur packée sur 64 bits.

```
u64 user_data :
[ 32 bits : op_idx ][ 16 bits : generation ][ 16 bits : task_idx ]
```

```rust
struct OpSlot {
    result:     Option<i32>,
    task_idx:   u16,
    generation: u16,  // incrémenté à chaque réutilisation du slot
                      // → détecte les CQEs stale sans zombie flag
}
```

### select! avec deux branches

```
select! { read | timeout }

SQE read    → op_slab[42], user_data=pack(42, gen=1, task=7)
SQE timeout → op_slab[43], user_data=pack(43, gen=1, task=7)

CQE timeout arrive :
  → op_slab[43].result = Some(0)
  → task_queue.push(7)
  → task 7 re-pollée
  → select re-poll read    → op_slab[42].result == None → Pending
  → select re-poll timeout → op_slab[43].result == Some → Ready ✓
  → cancel SQE 42 : IORING_OP_ASYNC_CANCEL, user_data=pack(42,gen=1,task=7)
  → op_slab[42].generation++ (slot réutilisable)
  → CQE annulation arrive → generation mismatch → ignore
```

---

## 4. Connexions — Kernel-owned fds

### ACCEPT_DIRECT + MULTISHOT (Linux 5.19+)

```rust
sqe.accept(listener_fd)
   .flags(IORING_ACCEPT_MULTISHOT | IORING_ACCEPT_DIRECT);

// CQE result = fixed_file_index (pas un fd userspace)
// le fd n'existe PAS dans la fdtable userspace
// le kernel le détient directement dans la fixed files table
```

- Un seul SQE soumis → CQEs indéfinies, une par connexion entrante
- Zéro `fget`/`fput` sur toutes les ops suivantes (read, write, close)
- Les connexions sont **invisibles aux outils standard** (`lsof`, `ss`, `/proc/pid/fd`)
  → implémenter une commande `INFO connections` pour l'observabilité

### Fixed files table — init sparse

```rust
// réserve N slots à l'init (mémoire virtuelle, pas physique)
io_uring_register(IORING_REGISTER_FILES_SPARSE, null, MAX_CONNECTIONS)

// à chaque accept → slot rempli automatiquement par ACCEPT_DIRECT
// à chaque close  → IORING_OP_CLOSE_DIRECT libère le slot
```

**Limite kernel** : `IORING_MAX_FIXED_FILES` = 1 048 576.
**100K connexions pub/sub** : faisable — ~800 KB pour la table, ~800 MB RAM
pour les socket buffers kernel (avec `SO_RCVBUF/SO_SNDBUF` réduits à 4 KB
sur les connexions subscriber idle).

**Tradeoffs kernel-owned fds** :

| Avantage | Inconvénient |
|---|---|
| zéro fget/fput par op | invisible à lsof/ss/proc |
| zéro fdtable lookup | sendmsg SCM_RIGHTS impossible |
| fixed_file_idx = O(1) | MAX_CONNECTIONS fixé à l'init |

---

## 5. Buffers — SlabBuffer registered dual read/write

### Structure

```rust
struct SlabBuffer {
    data:      MmapMut,    // [block0: 4096][block1: 4096]...
    //         └── page-aligned → compatible IORING_REGISTER_BUFFERS
    refcounts: Vec<u16>,   // tableau parallèle — pas inline dans le bloc
    lens:      Vec<u16>,   // (alignement : le buffer doit commencer à 0)
    free_list: Vec<u32>,
}
```

Refcounts séparés de data : les deux accès sont naturellement disjoint
dans le temps (kernel écrit data pendant op, userspace lit refcount au
Clone/Drop) → cache miss non critique.

### BufGuard — O(1) clone

```rust
struct BufGuard {
    idx:  u32,
    slab: Rc<SlabBuffer>,
}

impl Clone for BufGuard {
    fn clone(&self) -> Self {
        self.slab.refcounts[self.idx] += 1;  // O(1)
        BufGuard { idx: self.idx, slab: Rc::clone(&self.slab) }
    }
}

impl Drop for BufGuard {
    fn drop(&mut self) {
        self.slab.refcounts[self.idx] -= 1;
        if self.slab.refcounts[self.idx] == 0 {
            self.slab.free(self.idx);
        }
    }
}
```

### Enregistrement

```rust
// syscall bloquant — one-shot à l'init du runtime
slab.register(ring);   // IORING_REGISTER_BUFFERS

// les blocs sont maintenant utilisables en WRITE_FIXED et BUFFER_SELECT
// buf_index = idx dans la slab (même espace d'indexation)
```

### BUFFER_SELECT — reads sans allocation

```rust
// read sur Fixed(conn_idx) sans spécifier de buffer
sqe.read(Fixed(conn_idx), buf_group=0)
   .flags(IOSQE_BUFFER_SELECT);

// CQE retourne :
//   result  = nb bytes lus
//   buf_id  = quel bloc de la slab a été utilisé
//   → BufGuard(buf_id) créé immédiatement
//   → bloc retourné au pool après parsing (Drop)
```

Si message > 4096 (rare sur RESP) : fallback `Vec<u8>` classique.

### Flow complet chemin chaud

```
ACCEPT_DIRECT     → fixed_file_idx (pas de fd userspace)
BUFFER_SELECT     → kernel remplit SlabBlock directement
                  → CQE { buf_id, bytes }
                  → BufGuard(buf_id), parse RESP in-place (zero copy)
                  → BufGuard::drop() → bloc retourne au pool

execute_command() → OxidArt

encode réponse    → BufGuard depuis pool
WRITE_FIXED       → Fixed(conn_idx), buf_id
                  → BufGuard::drop() à la CQE
```

### Flow PUBLISH (fan-out)

```
encode message    → BufGuard rc=1
N × clone()       → rc=N, O(1) par subscriber
N × WRITE_FIXED   → tous dans le SQ en un seul batch
io_uring_enter    → 1 syscall (ou 0 avec SQ_POLL)
CQEs arrivent     → chaque Drop décrémente rc
dernier Drop      → rc=0 → bloc retourne au pool
```

Comparaison :

| | Valkey (epoll) | RadixOx |
|---|---|---|
| Syscalls N subscribers | N writev() | 0 (SQ_POLL) |
| Copies mémoire | N memcpy output buffers | 0 (WRITE_FIXED) |
| Parallélisme kernel | séquentiel | N writes simultanés |

---

## 6. spawn / thread-local

```rust
thread_local! {
    static RT: RefCell<Runtime> = RefCell::new(Runtime::new());
}

pub fn spawn<F: Future<Output=()> + 'static>(f: F) {
    RT.with(|rt| rt.borrow_mut().task_queue.push(Task::new(f)));
}
```

Tout le runtime en thread-local → `Rc<RefCell<>>` partout, zéro `Arc`/`Mutex`.
`spawn` panique si appelé hors d'un contexte runtime (thread-local non initialisé).

---

## 7. Sentinels user_data réservés

```rust
const TIMEOUT_UDATA:  u64 = u64::MAX;
const CANCEL_UDATA:   u64 = u64::MAX - 1;
const WAKER_UDATA:    u64 = u64::MAX - 2;
// MIN_RESERVED       u64 = u64::MAX - 2
// tous les op_idx packés seront << u32::MAX << MIN_RESERVED
```

---

## 8. Fonctions à implémenter

### Runtime core

| Fonction | Description |
|---|---|
| `Runtime::new(max_conn)` | init ring, fixed files sparse, slabs, thread-locals |
| `Runtime::run()` | event loop principal |
| `Runtime::poll_task(idx)` | poll une task, gère Ready/Pending/drop |
| `spawn<F>(f)` | enqueue une nouvelle task |
| `pack/unpack(op_idx, gen, task_idx)` | encode/decode user_data u64 |

### Ops io_uring

| Fonction | Opcode | Description |
|---|---|---|
| `op_accept_multishot(fd)` | `IORING_OP_ACCEPT` | MULTISHOT + DIRECT — un seul SQE pour toujours |
| `op_read_select(fixed_idx, group)` | `IORING_OP_READ` | BUFFER_SELECT, kernel choisit le bloc |
| `op_write(fixed_idx, buf)` | `IORING_OP_WRITE` | write normal |
| `op_write_fixed(fixed_idx, buf_id)` | `IORING_OP_WRITE_FIXED` | write depuis registered buffer |
| `op_close_direct(fixed_idx)` | `IORING_OP_CLOSE` | libère le slot fixed files |
| `op_cancel(user_data)` | `IORING_OP_ASYNC_CANCEL` | annulation SQE en vol |
| `op_timeout(duration)` | `IORING_OP_TIMEOUT` | timer |

### TCP

| Fonction | Description |
|---|---|
| `TcpListener::bind(addr)` | crée socket + bind + listen |
| `TcpListener::accept_loop(cb)` | lance le multishot accept, cb appelé par CQE |
| `ConnHandle::write(buf)` | `op_write` sur Fixed(idx) |
| `ConnHandle::write_fixed(guard)` | `op_write_fixed` sur Fixed(idx) |
| `ConnHandle::close()` | `op_close_direct` → libère fixed file slot |

### Buffers

| Fonction | Description |
|---|---|
| `SlabBuffer::new(n_blocks)` | mmap anonyme, n × 4096 bytes page-aligned |
| `SlabBuffer::alloc()` | retourne `BufGuard` depuis free list |
| `SlabBuffer::register(ring)` | `IORING_REGISTER_BUFFERS` — one-shot init |
| `BufGuard::clone()` | bump refcount O(1) |
| `BufGuard::drop()` | décrémente rc, libère si 0 |

### Timers

| Fonction | Description |
|---|---|
| `sleep(duration)` | future sur `IORING_OP_TIMEOUT` |
| `ticker(interval)` | spawn task qui loop sur sleep |

---

## 9. Chemin chaud — zéro overhead

```
Connexion entrante  : 0 syscall (multishot, SQ_POLL)
                      0 fget/fput (ACCEPT_DIRECT)
Read données        : 0 copy (BUFFER_SELECT → SlabBuffer registered)
                      0 allocation (pool pré-alloué)
Parse RESP          : in-place depuis SlabBlock
Execute command     : OxidArt lookup
Encode réponse      : BufGuard depuis pool
Write réponse       : 0 copy (WRITE_FIXED)
                      0 syscall (SQ_POLL)
PUBLISH N subs      : 0 copy (BufGuard::clone() × N)
                      0 syscall (batch SQEs + SQ_POLL)
```

---

## 10. Ce qui disparaît vs monoio

| monoio | Runtime custom |
|---|---|
| `IoUringDriver` opaque, `register_buffers` inaccessible | ring directement accessible |
| `RuntimeBuilder` avec feature flags | `Runtime::new()` direct |
| `AsyncReadRent` / `AsyncWriteRentExt` | `ConnHandle` simple |
| fd userspace par connexion | `fixed_file_idx` uniquement |
| buffer ownership ad-hoc | `BufGuard` unifié read + write |
| deps transitives (~15 crates) | `io-uring` + `slab` + rien |

---

## 11. Versions kernel requises

| Feature | Kernel minimum |
|---|---|
| io_uring base | 5.1 (déjà requis) |
| `BUFFER_SELECT` | 5.7 |
| `ACCEPT_DIRECT` + `MULTISHOT` | 5.19 |
| `RECV_ZC` (zero-copy receive, optionnel) | 6.1 |
| Machine actuelle | **6.19** ✓ tout disponible |
