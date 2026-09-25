# RadixOx — Analyse de performance & architecture I/O

> Synthèse complète issue d'une session de benchmarking (2026-06-08).
> Référence pour les futures sessions LLM sur ce projet.

---

## 1. Setup de benchmark

### bench-mark (JS, `~/rust/bench-mark/`)
- `bench.js` avec ioredis `enableAutoPipelining: true`
- Workers async partagent N connexions TCP en round-robin
- ioredis groupe tous les `await client.get/set()` émis dans le même tick Node.js → un seul write TCP ("auto-pipeline")
- Valeurs générées : 60% small (~150B), 30% medium (~1.5KB), 10% large (~10KB) → **moyenne ~1.6KB par SET**
- Commandes par batch ≈ nb workers / nb connexions

### Valkey de référence
- Valkey 9.0.3 (compatible Redis)
- Single-thread ou io-threads (configurable via `--io-threads N --io-threads-do-reads yes`)

### CPU isolation (important pour des mesures propres)
- Machine : Ryzen 5 7600X · 6 cores physiques · 12 threads (SMT)
- Topology : core physique 0 = CPUs 0+6, core 1 = CPUs 1+7, etc.
- RadixOx : `RADIXOX_CPU_PIN=0` (env var déjà dans le binaire)
- Client Node.js : `taskset -c 1-5` (cores physiques 1-5, **éviter CPU 6** qui partage le core physique 0 avec RadixOx)
- Si Valkey io-threads=4 : `taskset -c 0-3`, client sur `taskset -c 4-11`
- **Ne pas utiliser SQ_POLL sauf test spécifique** — overhead kernel thread dédié, bruit dans les mesures

---

## 2. Bugs / régressions identifiés et corrigés

### 2.1 Flush intermédiaire à `round & 127` — CORRIGÉ

**Fichier** : `radixox/src/bin/resp.rs`, fonction `handle_buffer`

**Problème** : un flush était déclenché tous les 128 commandes dispatchées :
```rust
// Ancien code — NE PAS REMETTRE
if round & 127 == 0 {
    conn_state.flush().await?;
}
```

**Impact mesuré** (1000 workers / 1 connexion) :
- Phase 2 r/w : **11K ops/s** avec flush → **382K ops/s** sans flush (**×34**)
- Phase 3 read : 176K → 346K (**×2**)

**Cause** : le flush à round=128 consomme le `write_buf` avant que la boucle principale puisse faire son `join!(write, read)`. Pour les batches de taille exactement multiple de 128, le `write_buf` est vide au retour dans `handle_loop` → `read_and_flush` fait un read seul au lieu d'un join! concurrent. Avec 200 workers auto-pipelinés, le steady-state converge naturellement vers des batches de 128 (taille du flush) → pathologie auto-entretenue.

**Fix** : supprimer le flush intermédiaire. La boucle principale gère déjà tout via `join!(write_all(write_buf), read(io_buf))`.

**Code actuel correct** :
```rust
async fn handle_buffer(...) -> IOResult<usize> {
    let mut offset = 0;
    while let Some((cmd, n)) = Cmd::parse(&read_buf[offset..]) {
        offset += n;
        dispatch(cmd, conn_state, registry, art).await?;
    }
    Ok(offset)
}
```

### 2.2 BUFFER_SIZE trop petit pour des valeurs réalistes — CORRIGÉ

**Fichier** : `radixox/src/bin/resp.rs`, ligne ~43

**Problème** : `const BUFFER_SIZE: usize = 64 * 1024;`

Avec 1000 workers et des valeurs ~1.6KB en moyenne :
- Batch entrant Phase 2 : 700 GET×30B + 300 SET×1.6KB = **~492KB**
- Buffer 64KB → **~8 aller-retours io_uring** pour lire un batch complet
- Phase 3 (GET seuls) : 1000×30B = 30KB → **1 seul read**

Impact mesuré (1000w / 1 conn) :
- Phase 2 avec 64KB : **65K ops/s**, p50 23ms
- Phase 2 avec 512KB : **382K ops/s**, p50 1.87ms (**×5.8 throughput, ×12 latence**)
- Phase 3 inchangée (~350K dans les deux cas — les GET tiennent dans 64KB)

**Fix** : `const BUFFER_SIZE: usize = 512 * 1024;`

**Caveat mémoire** : 512KB × 2 buffers (`io_buf` + `read_buf`) = **1MB par connexion** alloué upfront via `Vec::with_capacity`. Sur 1000 connexions = 1GB de buffers. Pour production, envisager une croissance dynamique : démarrer à 64KB, étendre si une commande dépasse la capacité courante.

---

## 3. Résultats benchmark après corrections

### 1000 workers / 1 connexion / 5s (CPU isolé)

| Config | Phase 2 r/w | p50 | p99 | Phase 3 read | p50 |
|---|---|---|---|---|---|
| RadixOx 64KB (avant fix) | 65K | 23ms | 23ms | 351K | 2.18ms |
| RadixOx 512KB | **382K** | **1.87ms** | 9.1ms | 356K | 2.15ms |
| Valkey 1t | 378K | 2.04ms | 8.8ms | 347K | 2.26ms |

→ RadixOx 512KB ≈ Valkey 1t sur 1 connexion.

### 1000 workers / 10 connexions / 5s (CPU isolé, client taskset -c 1-5)

| Config | Phase 2 r/w | p50 | p99 | Phase 3 read | p50 |
|---|---|---|---|---|---|
| RadixOx 512KB · CPU 0 | 294K | 2.71ms | 11.4ms | 330K | 2.35ms |
| Valkey 1t · CPU 0 | **379K** | **2.13ms** | **9.9ms** | **393K** | **1.89ms** |
| Valkey 4 io-threads · CPUs 0-3 | 296K | 2.78ms | 11.9ms | 339K | 2.20ms |

→ Valkey 1t bat RadixOx sur 10 connexions (raison : voir §4).
→ Valkey io-threads=4 n'aide **pas** avec seulement 10 connexions.

### memtier (80 connexions / pipeline=50 / 100B / 10s)

| Config | SET/s |
|---|---|
| Valkey 1t · CPU 0 | 1.70M |
| Valkey 4 io-threads · CPUs 0-3 | 2.50M |
| **RadixOx 512KB · CPU 0** | **3.74M** |

→ RadixOx domine en mode haute-concurrence / petites valeurs / nombreuses connexions grâce au batching io_uring.

---

## 4. Bottleneck architecturel résiduel — cooperative scheduling monoio

### Symptôme
Sur 10 connexions avec des batches de ~160KB par connexion (1000w/10conn, valeurs 1.6KB) :
- RadixOx : 294K Phase 2
- Valkey 1t : 379K Phase 2 (+29%)

Mais sur memtier (80 connexions, 6.5KB/connexion) :
- RadixOx : 3.74M vs Valkey 1t : 1.70M (+120%)

### Cause : head-of-line blocking entre connexions

Modèle actuel (1 Future monoio par connexion) :
```
Task A: [read_A.await] → [handle_buffer_A]  ← CPU-bound, pas d'await
Task B: [read_B.await] → attend que A finisse handle_buffer
```

Quand `handle_buffer` traite 160KB (100 commandes × 1.6KB), il ne yield jamais. Le thread monoio est occupé à traiter la connexion A pendant ce temps. Les CQEs des connexions B, C, D... s'accumulent dans le ring sans être dépilés. Plus les batches sont gros (grosses valeurs) et plus il y a de connexions, plus l'effet est sévère.

Valkey utilise des appels `read()` non-bloquants en C dans une boucle epoll : drain de **toutes** les connexions disponibles avant de traiter les commandes, ce qui lui donne une vue cohérente de tout le travail disponible.

### Solution — event-loop centralisé (RUNTIME_ARCH.md)

Remplacer le modèle "1 Future par connexion" par une boucle unique qui traite tous les CQEs :

```
loop {
    // Drain TOUS les CQEs disponibles
    for cqe in ring.drain() {
        match cqe {
            ReadCQE(conn_id, buf_id) => {
                parse + execute toutes les cmds du buffer
                conn.write_buf.extend(responses)
                ring.push(SQE::recv(conn_id, BUFFER_SELECT))  // re-submit immédiat, sans await
            }
            WriteCQE(conn_id) => {
                if conn.write_buf.not_empty() {
                    ring.push(SQE::write(conn_id, ...))        // non-bloquant
                }
            }
        }
    }
    ring.submit_and_wait(1)  // 1 seul io_uring_enter pour tous les SQEs accumulés
}
```

**Gains de ce modèle** :
1. **Yield naturel** entre connexions : chaque CQE est un point de préemption coopérative
2. **Writes non-bloquants** : SQE write soumis sans `.await`, la boucle continue immédiatement
3. **Batching multi-connexions** : tous les SQEs (reads + writes de toutes les connexions) soumis en un seul `io_uring_enter`
4. **Pipelining correct** : commande N°0 ne bloque pas N°127 sur la même connexion

**Avec RECV_MULTISHOT + BUFFER_SELECT** (Linux 5.19+, kernel actuel 6.x ✓) :
- 1 seul SQE de recv par connexion soumis à l'ouverture
- CQEs arrivent à chaque paquet TCP sans re-soumission
- Buffer alloué par le kernel depuis une pool pré-enregistrée (zéro copy, zéro allocation hot path)
- `BufGuard` ref-counted : drop automatique → retour dans la pool

Le code de parsing RESP, OxidArt, et dispatch des commandes **ne change pas**. Seule la couche I/O change.

---

## 5. Valkey io-threads — quand ça aide et quand ça n'aide pas

Valkey io-threads utilise des spinlocks pour le handoff entre le thread I/O et le thread principal. Le coût de synchronisation est fixe.

| Nb connexions | io-threads aide ? | Pourquoi |
|---|---|---|
| < 50 | Non (parfois pire) | coût spinlock > gain parallélisme |
| 80-100 | Oui (~+47%) | ~20 connexions par io-thread, coût amorti |
| 1000+ | Oui fortement | maximum parallélisme I/O |

RadixOx io_uring bénéficie de plus de connexions sans surcoût de synchronisation (tout est single-thread, Rc<RefCell> sans locks).

---

## 6. GC Node.js — artefact benchmark à ne pas confondre avec un bug serveur

Sur des tests avec 1 connexion et beaucoup de workers faisant des SET (génération de nouvelles chaînes de valeurs), le GC V8 de Node.js déclenche des pauses de ~40ms périodiquement. Pendant la pause, tous les workers accumulent leurs commandes → à la reprise, un batch gigantesque arrive d'un coup → p99.9 élevé.

Ce n'est pas un problème RadixOx : Valkey présente le même p99.9 dans les mêmes conditions. Pour des mesures de latency précises, utiliser memtier_benchmark (C++, pas de GC) ou un client Rust.

---

## 7. Points d'attention futurs

### BUFFER_SIZE dynamique
Plutôt qu'une allocation fixe de 512KB par connexion, implémenter une croissance dynamique :
- Démarrer à 64KB
- Si `Cmd::parse` retourne `None` et qu'on est à la fin du buffer avec des bytes non-consommés, doubler la capacité
- Cap à 2MB pour éviter qu'un client malveillant ne force une allocation géante

### yield_now comme workaround monoio
En attendant le custom runtime, on peut réduire le head-of-line blocking en insérant des `monoio::task::yield_now().await` dans `handle_buffer` tous les N bytes traités (pas toutes les N commandes — les commandes ont des tailles très variables avec les grosses valeurs) :
```rust
if offset > last_yield + 32 * 1024 {
    last_yield = offset;
    monoio::task::yield_now().await;
}
```
Ceci permet aux autres connexions de progresser sur les gros batches, au coût d'un overhead de yield.

### SQ_POLL
Supporté via `SQ_POLL=<idle_ms> RADIXOX_SQ_POLL_PIN=<cpu>`. Élimine le coût syscall de soumission des SQEs (utile si > 500K req/s pour amortir). Ne pas utiliser pour des benchmarks de latence de base — le kernel thread SQ_POLL consomme un CPU entier en permanence.

### TCP_NODELAY
RadixOx ne set pas explicitement `TCP_NODELAY`. Valkey le set. Sur des workloads avec des petites réponses entrelacées, l'algorithme de Nagle peut introduire des délais. À investiguer si des latences anormales apparaissent sur des workloads mixtes avec petites valeurs.

---

## 8. Fichiers clés

| Fichier | Rôle |
|---|---|
| `radixox/src/bin/resp.rs` | Serveur RESP — entry point, handle_loop, handle_buffer, dispatch |
| `radixox/src/bin/utils/mod.rs` | ConnState, SubRegistry, write_task, read_and_flush |
| `RUNTIME_ARCH.md` | Spec du custom runtime io_uring (à implémenter) |
| `CONN_DESIGN.md` | Architecture connexions : ConnState machine, Pub/Sub, Blocking ops |
| `~/rust/bench-mark/bench.js` | Script benchmark JS (ioredis auto-pipeline) |
| `bench_memtier.sh` | Benchmark memtier (3 phases : load, caching, session) |
| `bench_compare.sh` | YCSB workload A vs Valkey |
