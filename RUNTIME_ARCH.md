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
                let (task_idx, syscall_nb) = decode(packed);
                let task = &mut slab[task_idx];
                if syscall_nb == task.syscall_nb {
                    // CQE attendue : avancer le compteur puis poll
                    task.syscall_nb = task.syscall_nb.wrapping_add(1);
                    CURRENT_TASK.set(Some(CurrentTask {
                        task_idx,
                        result: cqe.result(),
                    }));
                    task.poll();
                    CURRENT_TASK.set(None);
                }
                // rc décrémenté dans tous les cas — CQE stale ou non
                slab[task_idx].rc -= 1;
                if slab[task_idx].rc == 0 {
                    slab.remove(task_idx);
                }
            }
        }
    }
    // les SQEs produits pendant les polls s'accumulent dans le SQ
    // partent en batch au prochain io_uring_enter
}
```

Avec SQ_POLL (déjà utilisé dans RadixOx) : le kernel thread consomme
les SQEs en continu → `io_uring_enter` sert uniquement à attendre les CQEs.
Le batching CQ reste utile : drainer toutes les CQEs avant de re-poll
évite N tours de boucle pour N CQEs.

---

## 2. Task

Une seule alloc : header + future inline. Pas de Waker / Context — le
runtime livre les résultats via `CURRENT_TASK`.

### Layout mémoire (RawTask)

```
[ fn_poll: *const () ][ fn_drop: *const () ][ alloc_size: u32 ][ rc: u16 ][ syscall_nb: u16 ][ future inline... ]
```

```rust
// header accédé via un pointeur brut — jamais instancié directement
struct RawTask {
    fn_poll:    unsafe fn(*mut ()) -> Poll<()>,
    fn_drop:    unsafe fn(*mut ()),
    alloc_size: u32,
    rc:         u16,  // SQEs en vol + token spawn → 0 = libérer
    syscall_nb: u16,  // prochaine CQE attendue ; wrapping_add sur match
}
```

`fn_poll` et `fn_drop` ne sont jamais nuls — la détection de stale passe
par `syscall_nb`, pas par un pointeur null.

États du slot dans la slab :

| `Option<NonNull<RawTask>>` | `rc` | Signification        |
|----------------------------|------|----------------------|
| None                       | —    | slot libre           |
| Some                       | > 0  | task vivante         |
| Some                       | 0    | → `slab.remove()`   |

### Construction (type erasure à la création)

```rust
fn RawTask::new<F: Future<Output=()>>(f: F) -> NonNull<RawTask> {
    // Layout::extend → RawTask header + F inline, une seule alloc
    // instancie poll_fn::<F> et drop_fn::<F> pendant que F est connu
    // après : type complètement effacé, stocké comme fn ptrs bruts
    // rc = 1 (token spawn), syscall_nb = 0
}

unsafe fn poll_fn<F: Future<Output=()>>(data: *mut ()) -> Poll<()>;
unsafe fn drop_fn<F: Future<Output=()>>(data: *mut ());
```

### CURRENT_TASK — livraison des résultats CQE

```rust
struct CurrentTask {
    task_idx: u32,
    result:   i32,   // résultat CQE (0 au spawn)
}

thread_local! {
    static CURRENT_TASK: Cell<Option<CurrentTask>> = Cell::new(None);
}
```

Chaque poll pose `CurrentTask` avant d'appeler `fn_poll` :
- au spawn : `result = 0`, la future lit `task_idx` pour stamper ses SQEs
- sur CQE : `result = cqe.result()`, la future lit le résultat de son op

La future lit `task.syscall_nb` (via `task_idx`) pour stamper ses SQEs avec
le `syscall_nb` courant. Le runtime avance `syscall_nb` avant le poll.

### Slab de tasks

```rust
// niche NonNull → Option<NonNull<RawTask>> sans discriminant
// task_idx : u32 → jusqu'à u32::MAX - 1 tasks simultanées
type TaskSlab = HiSlab<Option<NonNull<RawTask>>>;
```

---

## 3. Tag — interface runtime / future

`Tag` est le seul point de contact entre une future et le runtime. Ses
champs sont privés — la future ne manipule jamais `task_idx` ni `syscall_nb`
directement.

```rust
// champs privés — opaque pour les futures
pub struct Tag {
    task_idx:   u32,
    syscall_nb: u16,  // valeur brute (bit 15 = multi-send flag)
}

impl Tag {
    /// Submit unique : une CQE → advance syscall_nb → un poll.
    /// Si on sortait d'un multi-send (bit 15 set dans task.syscall_nb),
    /// strip le flag et incrémente : 1<<15|5 → 6.
    /// Les CQEs tardives du multi-send (nb=5) seront alors stales.
    pub fn submit_sqe(task: &mut RawTask, sqe: SqeRef<'_>) {
        if task.syscall_nb & 0x8000 != 0 {
            task.syscall_nb = (task.syscall_nb & 0x7FFF).wrapping_add(1);
        }
        sqe.set_user_data(encode(task.task_idx, task.syscall_nb));
    }

    /// Fan-out : N SQEs, chaque CQE déclenche un poll sans avancer syscall_nb.
    /// Pré-incrémente task.syscall_nb immédiatement (le handler ne sait pas
    /// quand le lot se termine — c'est la future qui compte ses CQEs).
    /// Contrainte : task.syscall_nb & 0x7FFF doit rester < 0x7FFF avant l'appel.
    pub fn multi_send(task: &mut RawTask, sqes: &[SqeRef<'_>]) {
        let nb = task.syscall_nb & 0x7FFF;
        debug_assert!(nb < 0x7FFF, "syscall_nb overflow multi-send");
        task.syscall_nb = 0x8000 | nb.wrapping_add(1);
        let ud = encode(task.task_idx, task.syscall_nb);
        for sqe in sqes {
            sqe.set_user_data(ud);
        }
    }
}
```

### user_data — packing 64 bits

```
u64 user_data :
[ 32 bits : task_idx ][ 1 bit : multi-send ][ 15 bits : syscall_nb ][ 16 bits : libre ]
```

```rust
fn encode(task_idx: u32, raw_nb: u16) -> u64 {
    (task_idx as u64) << 32 | (raw_nb as u64) << 16
}

fn decode(ud: u64) -> (u32, u16 /* raw_nb */) {
    ((ud >> 32) as u32, (ud >> 16) as u16)
}
```

`raw_nb & 0x8000` = flag multi-send. `raw_nb & 0x7FFF` = `syscall_nb` effectif.

### CURRENT_TASK

```rust
struct CurrentTask {
    tag:    Tag,
    result: i32,
}

thread_local! {
    static CURRENT_TASK: Cell<Option<CurrentTask>> = Cell::new(None);
}
```

### CQE → Tag + résultat

```rust
let (task_idx, raw_nb) = decode(cqe.user_data);
let task = &mut slab[task_idx];
let is_multi = raw_nb & 0x8000 != 0;
let nb       = raw_nb & 0x7FFF;

if nb == task.syscall_nb {
    if !is_multi {
        // normal : avance le compteur → CQEs du même round deviennent stales
        task.syscall_nb = task.syscall_nb.wrapping_add(1);
    }
    // multi-send : pas d'advance → toutes les CQEs du lot pollent
    CURRENT_TASK.set(Some(CurrentTask {
        tag: Tag { task_idx, syscall_nb: raw_nb },
        result: cqe.result(),
    }));
    task.poll();
    CURRENT_TASK.set(None);
}
// rc décrémenté dans tous les cas — stale ou non
task.rc -= 1;
if task.rc == 0 { slab.remove(task_idx); }
```

### Pattern future — submit unique

```rust
struct ReadFuture { tag: Option<Tag> }

impl Future for ReadFuture {
    fn poll(&mut self) -> Poll<i32> {
        match &self.tag {
            None => {
                let tag = CURRENT_TASK.with(|ct| ct.get().unwrap().tag);
                tag.submit_sqe(/* sqe read */);
                self.tag = Some(tag);
                Poll::Pending
            }
            Some(_) => {
                // pollé uniquement si syscall_nb a matché → result est le nôtre
                Poll::Ready(CURRENT_TASK.with(|ct| ct.get().unwrap().result))
            }
        }
    }
}
```

### Pattern future — fan-out (multi-send)

```rust
struct FanOutFuture { remaining: u32, errors: u32 }

impl Future for FanOutFuture {
    fn poll(&mut self) -> Poll<u32 /* nb errors */> {
        let result = CURRENT_TASK.with(|ct| ct.get().unwrap().result);
        if result < 0 { self.errors += 1; }
        self.remaining -= 1;
        if self.remaining == 0 { Poll::Ready(self.errors) } else { Poll::Pending }
    }
}
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
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        let task_idx = rt.slab.insert(RawTask::new(f));  // rc = 1
        CURRENT_TASK.set(Some(CurrentTask { task_idx, result: 0 }));
        rt.slab[task_idx].poll();
        CURRENT_TASK.set(None);
        rt.slab[task_idx].rc -= 1;  // consomme le token spawn
        if rt.slab[task_idx].rc == 0 {
            rt.slab.remove(task_idx);  // future terminée immédiatement
        }
    });
}
```

Tout le runtime en thread-local → `Rc<RefCell<>>` partout, zéro `Arc`/`Mutex`.
`spawn` panique si appelé hors d'un contexte runtime (thread-local non initialisé).

---

## 7. Sentinels user_data réservés

```rust
const TIMEOUT_UDATA:  u64 = u64::MAX;
const CANCEL_UDATA:   u64 = u64::MAX - 1;
const WAKER_UDATA:    u64 = u64::MAX - 2;  // réservé, pas encore utilisé
// MIN_RESERVED       u64 = u64::MAX - 2
```

Les valeurs packées légitimes ont `task_idx ≤ u32::MAX - 1`, donc
`packed ≤ (u32::MAX - 1) << 32 | u16::MAX << 16 | u16::MAX`
qui est bien inférieur à `u64::MAX - 2`.

---

## 8. Fonctions à implémenter

### Runtime core

| Fonction | Description |
|---|---|
| `Runtime::new(max_conn)` | init ring, fixed files sparse, slabs, thread-locals |
| `Runtime::run()` | event loop principal |
| `spawn<F>(f)` | insère la task dans la slab, premier poll via CURRENT_TASK |
| `encode(task_idx, gen, syscall_nb) -> u64` | packing user_data |
| `decode(u64) -> (u32, u16, u16)` | dépacking user_data |

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
