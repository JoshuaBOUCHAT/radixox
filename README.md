# RadixOx

**An ordered in-memory key-value store that speaks the Redis protocol (RESP2, a subset of the commands). Rust, Adaptive Radix Tree, io_uring.**

Keys live in a radix tree instead of a hash table, so they are always sorted. That makes
things Redis cannot do natively cheap: sorted pagination under a prefix (`KRANGE`), `SCAN`
and `KEYS` that only walk the matching subtree, a `SCAN` without duplicates. `redis-cli` and
existing Redis client libraries work for the [supported commands](#supported-commands), and
the engine, **OxidArt**, is also usable as an embedded Rust library.

The data path is **single-threaded by design**: one thread owns the data, with no locks and
no shared state. The network work runs in the kernel through a hand-written io_uring loop
(no async runtime), and can be spread over more cores by the kernel's io_uring workers.
Against Valkey 9.1.2 on the same cores, without pipelining (loopback, 1M keys, see
[Benchmarks](#benchmarks)): **+29 % capacity on one core and +54 % on three**; at the same
60–80 % load, **p99 and p99.9 about half of Valkey's**; a `BGSAVE` **5–6× shorter** with no
`fork` and no copy-on-write memory, even with Valkey's child given a core of its own. The
p99.99 during a save is a tie.

---

## Status

RadixOx is **not a drop-in Redis replacement for production data yet**.

Good fit today:
- a cache or a pagination / prefix index whose data can be rebuilt;
- OxidArt embedded in a Rust program (ordered map with TTL, prefix and range queries).

Missing for primary data:
- **no memory limit or eviction** (`maxmemory`, LRU/LFU): a growing dataset ends in OOM;
- **AOF and basic replication**: `RADIXOX_APPENDONLY=yes` logs every write (`everysec`, `no`,
  or `always` with group commit), rewrites the log in the background and replays it at startup;
  without it, automatic `BGSAVE` rules as in Valkey and a save on SIGTERM, and a crash loses
  what was written since the last snapshot. Replication is asynchronous and pushed by the
  primary (`REPLICATE host port`), with `WAIT`, replicas with their own AOF and chained
  replicas; no synchronous replication (see [Replication](#replication));
- no cluster, no TLS, no ACLs (one password);
- no lists, `MULTI`/`EXEC` or Lua, which queue and job libraries (Sidekiq, Celery, BullMQ)
  rely on;
- Linux only, and io_uring must be allowed (see [Deployment](#deployment)).


---

## What Redis doesn't have

- **`KRANGE` / `KREVRANGE`**: the next N keys under a prefix, in sorted order, after a cursor
  key. Pagination for message threads, logs or feeds, without maintaining a sorted set. A
  page of 64 entries costs about a third of 64 separate lookups (in-process: 5.2 µs vs
  15.4 µs).
- **Prefix queries that prune**: `SCAN MATCH user:*` and `KEYS user:*` walk only the `user:`
  subtree; other globs walk only the subtree of their literal prefix (`user:*:email` →
  `user:`).
- **`UNLINK user:*`**: deletes a whole namespace; its keys disappear at once, the memory is
  freed in the background.
- **A stricter `SCAN`**: no duplicates, `COUNT` is exact, and every key present for the whole
  scan is returned exactly once, even while the data changes.
- **Binary-safe ordered keys**: any byte 0x00–0xFF at any position, ordered like `memcmp`.
- **Snapshots without `fork`**: an application-level copy-on-write barrier gives a consistent
  point-in-time dump while the server keeps serving, without a second process or memory
  doubling.

## Highlights

Against Valkey 9.1.2, 2026-10-20 (loopback, no pipeline, 1M keys of 100 random bytes, 1:4
SET:GET, 78 connections, saves on a SATA SSD with fsync; two runs per point; see
[Against Valkey: partial load and BGSAVE](#against-valkey-partial-load-and-bgsave-2026-10-20)):

| | RadixOx | Valkey 9.1.2 | |
|---|---|---|---|
| Capacity, **1 core** each | **266 k ops/s** | 207 k | **+29 %** |
| Capacity, **3 physical cores** each | **622 k ops/s** | 404 k | **+54 %** |
| Same traffic, 60–80 % of Valkey's capacity, 1 core: p99 | **0.41–0.44 ms** | 0.73–0.77 ms | ÷1.8 |
| same, p99.9 | **0.47–0.52 ms** | 0.84–0.94 ms | ÷1.8 |
| same, p99.99 during a `BGSAVE` | 0.88–1.31 ms | 1.00–1.16 ms | tie |
| same, dump writes on a second core (`RADIXOX_FILE_CPUS`, like Valkey's child) | **0.82–1.02 ms** | 1.00–1.41 ms | ahead |
| `BGSAVE` of 105–113 MB under that load (Valkey's child on a **second, free core**) | **0.35–0.43 s**, same core | 2.1–2.2 s + ~2 core-seconds | **÷5–6** |
| Extra memory during the save | **~0** | +160–170 MB (copy-on-write) | |
| Server RSS | **141 MB** | 190 MB | −26 % |

Older series, with the custom io_uring loop (2026-10-19, loopback, **single runs**; see
[Custom io_uring loop](#custom-io_uring-loop-2026-10-19)). **Caveat, to be re-run**: the
YCSB *run* phase of these two series only touched 1,000 of the 5M keys (a script bug, fixed
since); both servers had the same bias, but the absolute figures describe a hot-key
workload:

| | RadixOx | Valkey 9.1.2 | |
|---|---|---|---|
| YCSB A, 5M records, **1 core** each: throughput | **258 k ops/s** | 199 k | **+30 %** |
| same, READ avg / p99 | **386 µs / 570 µs** | 500 µs / 971 µs | −23 % / −41 % |
| YCSB A, 5M records, **3 physical cores** each: throughput | **411 k ops/s** | 366 k | **+12 %** |
| same, READ avg / p99 | **234 µs** / 484 µs | 271 µs / **440 µs** | −14 % / +10 % |
| same, peak RSS | **1,556 MB** | 2,056 MB | −24 % |

During a `BGSAVE` under load (pipeline 32, 1M keys), RadixOx's p99.99 stays at **3.25 ms**,
its level without a save: the snapshot does not show in the tail.

Earlier measurements (2026-10-18, memory on 2026-10-17, **previous I/O layer**, 1M keys,
loopback; to be re-run with the new loop):

| | RadixOx (1 core) | Valkey 9.1.2 | |
|---|---|---|---|
| Full `SCAN MATCH user:12*` (11K matches in 1M keys), server CPU | **< 10 ms**, 12 calls | 110 ms, 999 calls | ×10+ |
| p99, no pipeline (80 connections) | **0.39 ms** | 0.74 ms (1 core) | −47 % |
| p99.9, no pipeline | **0.46 ms** | 0.77 ms (1 core) | −40 % |
| Throughput, no pipeline | 236 k ops/s | 208 k (1 core) · **454 k** (`io-threads 4`, 2 cores) | |
| p99 of that multi-threaded Valkey | **0.42 ms** | 2.26 ms | |
| Throughput, pipeline 32 | **2.39 M ops/s** | 1.98 M (1 core) · 2.37 M (`io-threads 4`) | |
| Operations per second of server CPU, no pipeline | **381 k** | 323 k (1 core) · 142 k (`io-threads 4`) | |
| YCSB A, READ p99 (1M records) | **544 µs** | 1,040 µs (1 core) | −48 % |
| `BGSAVE` of 1M keys under load (to tmpfs) | **~670 ms**, one core, tail near its reference | ~1,750 ms, on a second core | ×2.6 |
| Restart with 1M keys | **~0.17 s** | ~0.53 s | ×3 |
| Memory, 1M structured keys (`key:N`) × 100 B | **131 MB** | 163 MB | −20 % |
| Memory, 1M random UUID keys × 100 B | 209 MB | 205 MB | ≈ |
| Memory, 1M one-field hashes (YCSB-like) | **233 MB** | 344 MB | −32 % |
| Memory, 1M two-field hashes (short values) | 161 MB | **129 MB** | Valkey −20 % |

---

## Quick start

```bash
cargo install radixox                    # or: git clone … && cargo build --release --bin radixox
radixox                                  # listens on 127.0.0.1:6379 (RADIXOX_BIND to change)

redis-cli SET user:10:message:1 hello
redis-cli SET user:10:message:2 world
redis-cli KRANGE user:10:message: "" 10  # 1) user:10:message:1  2) hello  3) …:2  4) world
redis-cli KEYS "user:10:*"
redis-cli BGSAVE                         # snapshot to ./dump.rxd, reloaded at next start
```

Requirements: **Linux 6.12 or newer** with io_uring (developed on 6.12 LTS and 7.2), **Rust 2024 edition**
(stable).

---

## Benchmarks

### Against Valkey: partial load and BGSAVE (2026-10-20)

A cache rarely runs at 100 %: this series compares the two servers at the **same offered
load** (60, 70 and 80 % of Valkey's one-core capacity), with a `BGSAVE` in every run.

**Setup.** Ryzen 5 7600X, Linux 7.2-zen, power profile `performance`, loopback. Server on
CPU 0 (one core each); memtier on cores 3–5 (and their SMT siblings), 6 threads × 13
connections, no pipeline, `--distinct-client-seed --randomize`, 1:4 SET:GET over 1,000,001
keys of 100 random bytes (`--random-data`), 12 s per run, `BGSAVE` at t = 3 s. **Valkey gets
an advantage**: its `BGSAVE` child runs on CPU 1, a core of its own (`bgsave-cpulist`);
RadixOx saves on its only core. Dumps go to a SATA SSD (PNY CS900), fsync included. Two
runs per point, server order alternated; ranges cover both runs. Server CPU is the busy time
of CPU 0 from `/proc/stat`, softirq included.

RadixOx writes a **raw dump** here (`RADIXOX_SAVE_ZSTD=0`): memtier's "random" values repeat
from key to key, which RadixOx's streaming zstd compresses away (an 11–24 MB dump) while
Valkey's per-value LZF cannot (113 MB). Raw, both write about the same volume.

| Load | | p50 | p99 | p99.9 | p99.99 | `BGSAVE` | Server CPU |
|---|---|---|---|---|---|---|---|
| ~124 k ops/s (60 %) | RadixOx | **0.29 ms** | **0.42 ms** | **0.50–0.51 ms** | 1.03–1.31 ms | **348–360 ms** | **0.4 core** |
| | Valkey | 0.37 ms | 0.76–0.77 ms | 0.93–0.94 ms | 1.10–1.16 ms | 2,061–2,153 ms | 0.6 core + child |
| ~145 k ops/s (70 %) | RadixOx | **0.29 ms** | **0.42 ms** | **0.50 ms** | 1.08–1.21 ms | **376–383 ms** | **0.5 core** |
| | Valkey | 0.37 ms | 0.75 ms | 0.89–0.90 ms | 1.04–1.16 ms | 2,150–2,245 ms | 0.6–0.7 core + child |
| ~165 k ops/s (80 %) | RadixOx | **0.29 ms** | **0.41–0.44 ms** | **0.48–0.54 ms** | 0.88–1.27 ms | **408–427 ms** | **0.5–0.6 core** |
| | Valkey | 0.36–0.38 ms | 0.73–0.77 ms | 0.84–0.91 ms | 1.00–1.07 ms | 2,149–2,163 ms | 0.7–0.8 core + child |
| 213 k ops/s (80 % of RadixOx's capacity) | RadixOx | 0.29 ms | 0.38–0.42 ms | 0.47–0.50 ms | 1.04–1.26 ms | 613–671 ms | 0.7 core |
| ~165 k ops/s, **no save** | RadixOx | **0.29 ms** | **0.42–0.45 ms** | **0.47–0.52 ms** | **0.57–0.73 ms** | — | **0.6 core** |
| | Valkey | 0.37 ms | 0.74–0.75 ms | 0.86 ms | 0.96–1.02 ms | — | 0.7–0.8 core |

- p99 and p99.9 are about **half of Valkey's** at every load, save or not. At 80 % of its own
  capacity, RadixOx still has a lower p99 than Valkey at 60 % of its own.
- **The p99.99 during a save is a tie**, the only figure where Valkey's extra core pays off:
  RadixOx's save adds 0.3–0.6 ms to its p99.99, as the dump's writes share its only core.
  With `RADIXOX_FILE_CPUS=1`, the writes (copy into the page cache, fsync) run on an io_uring
  worker pinned to CPU 1, the same resources as Valkey's child: the longest save slice drops
  from 0.5–0.8 ms to 0.1 ms and the p99.99 to **0.82–1.02 ms** against Valkey's 1.35–1.41 ms
  in that session (two runs each), for ~0.3 core-second of CPU 1 per save against ~2.5.
- Valkey's child keeps CPU 1 busy for the whole save (~2 core-seconds per save, on top of
  the server's CPU); RadixOx's slices take ~60 ms of CPU on its own core, plus the kernel's
  writes. Valkey also needs 160–170 MB of copy-on-write memory during the save (20 % of
  writes over uniformly random keys touch nearly every page: the worst case for `fork`,
  but a common one for a cache). RadixOx's RSS: 141–145 MB; Valkey's: 190 MB.

**Saturated, 3 physical cores each** (CPUs 0–2; RadixOx: event loop + 2 io_uring send
workers; Valkey: `io-threads 3`, its child on the same CPUs; single runs):

| | Throughput | p99 | p99.9 | p99.99 | `BGSAVE` |
|---|---|---|---|---|---|
| RadixOx | **622 k ops/s** | **0.21 ms** | **0.26 ms** | **0.76 ms** | — |
| Valkey | 404 k ops/s | 0.24 ms | 0.33 ms | 1.19 ms | — |
| RadixOx, `BGSAVE` at t = 3 s | **536 k ops/s** | **0.27 ms** | **0.39 ms** | **2.08 ms** | **355 ms** |
| Valkey, `BGSAVE` at t = 3 s | 379 k ops/s | 0.49 ms | 2.27 ms | 3.01 ms | 3,032 ms |

Caveats: loopback (the client's TCP stack runs on the same machine); memtier's rate limiter
delivers ±10 % around the target, but both servers of a pair received the same load; the
3-core save figures predate a scheduling fix that lowered RadixOx's save tail on one core
(p99.99 1.7–2.4 → 0.9–1.3 ms). Details and the method: [`RUNTIME_ARCH.md`](RUNTIME_ARCH.md)
§12.6; background work while idle: §8.2.

### Custom io_uring loop (2026-10-19)

The server now runs on a hand-written io_uring loop (no async runtime): see
[`RUNTIME_ARCH.md`](RUNTIME_ARCH.md). Same machine as below, power profile `performance`,
loopback, **single runs**.

**YCSB workload A** ([`bench_compare.sh`](bench_compare.sh)): 5M records (one hash field of
100 B), 10M operations (50 % read / 50 % update), 100 client threads. `SERVER_CORES=K` gives
each server K physical cores, **one thread per core, the same CPUs for both**, and leaves
the SMT siblings of those cores idle; YCSB runs on the other cores. RadixOx: event loop on
the first core, sends delegated to io_uring kernel workers (`IOSQE_ASYNC`) pinned on the
others. Valkey: `io-threads K` on the same CPUs.

| | Load | Run | READ avg | READ p95 | READ p99 | READ p99.9 | READ p99.99 | Peak RSS |
|---|---|---|---|---|---|---|---|---|
| RadixOx, 1 core | **115 k** | **258 k** | **386 µs** | **504 µs** | **570 µs** | **660 µs** | **1,007 µs** | **1,557 MB** |
| Valkey, 1 core | 76 k | 199 k | 500 µs | 602 µs | 971 µs | 892 µs | 1,065 µs | 2,053 MB |
| RadixOx, 3 cores | **223 k** | **411 k** | **234 µs** | 329 µs | 484 µs | 727 µs | **1,983 µs** | **1,556 MB** |
| Valkey, 3 cores (`io-threads 3`) | 182 k | 366 k | 271 µs | **319 µs** | **440 µs** | **702 µs** | 2,283 µs | 2,056 MB |

⚠ The *run* phase of these series only touched 1,000 of the 5M keys (YCSB's `workloada`
sets `recordcount=1000` and the script did not override it for the run; fixed since): a
hot-key workload, the same for both servers. To be re-run on the whole key space.

On three cores, Valkey has a slightly better p95–p99.9 (3–10 %): handing sends to the
kernel's workers adds some variance, which a planned adaptive mode should reduce.

**memtier**, 1M keys, 100 B values, 1:4 SET:GET, 80 connections (8 threads × 10), RadixOx
alone (no Valkey run in this series):

| | Throughput | avg | p99 |
|---|---|---|---|
| No pipeline, 1 core | 310 k ops/s | 0.295 ms | 0.447 ms |
| No pipeline, 1 core + 2 io_uring send workers on a second physical core | **526 k ops/s** | **0.174 ms** | **0.279 ms** |
| Pipeline 32, 1 core | 2.08–2.41 M ops/s | 1.06–1.09 ms | 1.46–1.55 ms |
| Pipeline 32, 1 core, `BGSAVE` at t = 3 s | 1.91 M ops/s | — | 2.22 ms (p99.99 **3.25 ms**) |

The previous I/O layer had a p99.99 of ~10 ms during the same `BGSAVE`; freeing the
snapshot's copies in slices removed that spike.

### Setup (sections below: previous I/O layer, 2026-10-18)

- AMD Ryzen 5 7600X (6 cores / 12 threads), 30 GB RAM, Linux 7.2-zen, power profile
  `performance`, loopback, idle machine. Measured on 2026-10-18.
- **One core each** unless stated: both servers run at the same time with their main thread
  pinned to CPU 0 (its SMT sibling stays idle); only one is loaded at a time, and the order
  alternates every round so that drift on the machine hits both. Valkey 9.1.2 runs with
  `io-threads 1`, persistence off, and its background threads and `BGSAVE` child on CPU 1
  (`bio-cpulist`, `bgsave-cpulist`), as in production where the fork does not take the
  server's core. RadixOx runs without SQ_POLL. Clients run on physical cores 2–5.
- memtier: 1,000,000 keys (`key:1` … `key:1000000`), 100-byte values, uniform random access,
  `memtier_benchmark --threads 4 --clients 20 --distinct-client-seed --randomize`,
  **3 rounds of 10 s per configuration, median reported** (min–max for throughput).
- Server CPU is the user + kernel time of the server process (`/proc/<pid>/stat`) during the
  run. On loopback, part of the network work is billed to the client side, so a single-threaded
  server never shows a full core.
- Reproduce with [`scripts/bench_vs_valkey.sh`](scripts/bench_vs_valkey.sh) (about 3 minutes;
  `VK_IO_THREADS=4 VK_CPUS=0,1,6,7` for the multi-threaded Valkey).

### Throughput and latency (1:4 SET:GET)

| | Throughput (min–max) | p50 | p99 | p99.9 | p99.99 | Server CPU | Ops / CPU-second |
|---|---|---|---|---|---|---|---|
| **No pipeline**, RadixOx | **236,046** (235,886–236,234) | **0.343 ms** | **0.391 ms** | **0.463 ms** | **0.679 ms** | 0.62 core | **381,022** |
| No pipeline, Valkey | 208,318 (207,672–210,111) | 0.375 ms | 0.743 ms | 0.767 ms | 0.871 ms | 0.65 core | 322,973 |
| **Pipeline 32**, RadixOx | **2,385,531** (2,355,385–2,402,540) | **0.927 ms** | **1.575 ms** | **1.647 ms** | **1.799 ms** | 0.86 core | **2,757,840** |
| Pipeline 32, Valkey | 1,981,735 (1,969,750–1,985,917) | 1.071 ms | 1.975 ms | 2.111 ms | 2.223 ms | 0.89 core | 2,239,249 |

Without pipelining, both servers are bounded by the TCP stack and the clients' round trips.
The tail is where RadixOx stands out: p99 and p99.9 are about half of Valkey's, and it does
about 20 % more work per second of CPU.

### Against a multi-threaded Valkey

Valkey with `io-threads 4` on two physical cores (CPUs 0, 1 and their SMT siblings 6, 7),
RadixOx still on one core, same protocol (separate session, same day):

| | Throughput | p50 | p99 | p99.9 | Server CPU | Ops / CPU-second |
|---|---|---|---|---|---|---|
| No pipeline, RadixOx | 227,242 | 0.351 ms | **0.415 ms** | **0.471 ms** | 0.61 core | **370,102** |
| No pipeline, Valkey `io-threads 4` | **454,109** | **0.119 ms** | 2.255 ms | 2.911 ms | 3.25 cores | 141,632 |
| Pipeline 32, RadixOx | 2,266,504 | **0.983 ms** | 1.983 ms | 2.319 ms | 0.86 core | **2,632,408** |
| Pipeline 32, Valkey `io-threads 4` | **2,371,822** | 1.079 ms | **1.191 ms** | **1.455 ms** | 3.97 cores | 595,876 |

With more cores, Valkey doubles RadixOx's throughput without pipelining and matches it with
pipelining, at the cost of 4–5× more CPU and, without pipelining, a p99 five times higher.
RadixOx's answer for more throughput is sharding by namespace, which does not exist yet. The
clients share the machine, so the multi-threaded figures are a lower bound for Valkey.

### YCSB workload A

1M records (one hash field of 100 B per record), 2M operations (50 % read / 50 % update),
100 client threads, one server thread each, **single run** (2026-10-18):

| | Load | Run | READ avg | READ p50 | READ p99 | READ p99.9 | READ p99.99 | UPDATE p99 | Peak RSS |
|---|---|---|---|---|---|---|---|---|---|
| RadixOx | **108 k ops/s** | **219 k ops/s** | **454 µs** | **450 µs** | **544 µs** | **739 µs** | 4,579 µs | **545 µs** | **331 MB** |
| Valkey | 86 k ops/s | 187 k ops/s | 532 µs | 520 µs | 1,040 µs | 1,117 µs | **3,799 µs** | 1,040 µs | 430 MB |

The p99.99 of a single run of 2M operations rests on about 100 samples per operation type:
too few to rank the two servers on it.

### Memory

RSS after loading 1M keys through a single connection (`redis-cli --pipe`), measured before
any other command (2026-10-17):

| Data | Valkey | RadixOx | |
|---|---|---|---|
| `key:N` + 100 B string | 163 MB | **131 MB** | −20 % |
| `key:N` + 1 KB string | 1,301 MB | **1,249 MB** | −4 % |
| random UUID (text) + 100 B string | **205 MB** | 209 MB | ≈ |
| hash, 1 field of 100 B, random keys (YCSB-like) | 344 MB | **233 MB** | −32 % |
| hash, 2 short fields (`name`, `email`) | **129 MB** | 161 MB | +25 % |

What drives these numbers:

- **The tree**: a key without descendants is an **inline leaf** in its parent node, not a node
  of its own. On structured keys (`key:1` … `key:1000000`, ten keys per node) the whole tree
  costs **16 B per key**; on random keys each node holds only two or three keys and the tree
  costs 60–100 B per key, which is about what Valkey's hash table and key strings cost: hence
  the tie on UUIDs. The −20 % above is the best case.
- **Values**: a string is a separate allocation in RadixOx's pool (100 B → 104 B: a 4-byte
  header, no size classes); with large values both servers converge.
- **Small hashes and sets** (up to 8 entries) are one compact block of field/value pointers
  instead of a tree. One-field hashes are much smaller than in Valkey; hashes with several
  short fields are larger, because each field and value stays a separately allocated
  `SharedByte` (a deliberate trade-off: no copies on `HGET`/`HSET`).
- **Connections** (measured with the previous I/O layer, which kept three 64 KiB buffers per
  connection): the new loop receives into one 2 MB ring shared by all connections, and keeps
  per connection only its pending replies and the fragment of a split command.

#### After deletes and rewrites (2026-10-23)

What production does to memory: values of mixed sizes, deletes spread over the keyspace,
values rewritten with other sizes. 1M keys per scenario, the same data and the same deleted
keys for every server (RadixOx: `UNLINK prefix*`; Valkey: `SCAN MATCH` + `UNLINK`), one
core each, no persistence. RSS in MB, measured once the server has settled (15 s; 30 s for
Valkey with active defragmentation, which works gradually). Valkey 9.1.2 built from source
(its patched jemalloc: the distribution package cannot enable `activedefrag`), with
`active-defrag-ignore-bytes 1mb` so that defragmentation runs at this size.

| Scenario | RadixOx loaded → after | Valkey loaded → after | Valkey `activedefrag` loaded → after |
|---|---|---|---|
| strings `key:N`, sizes 0–200 B (bell), delete 56 % | **142 → 86** | 170 → 138 | 168 → 102 |
| random UUID → UUID (22 B), delete 50 % | 108 → 70 | 93 → 93 | **93 → 66** |
| one-field hashes (YCSB-like), 0–200 B, delete 56 % | **246 → 118** | 330 → 330 | 329 → 168 |
| strings 0–200 B, then all rewritten with 100–300 B | **142 → 258** | 168 → 342 | 168 → 287 |

- Deleted memory goes back to the system: values, key suffixes, node pages and small
  hashes live in one pool that RadixOx compacts in its idle moments, and the node slab is
  compacted and shrunk too. Without `activedefrag`, Valkey keeps everything after scattered
  deletes; with it, it gets back most of it.
- Short random keys (UUIDs) are RadixOx's weak case: few shared prefixes, so the tree costs
  more than Valkey's hash table (+15 MB loaded).
- Not measured here: the CPU spent by RadixOx's compaction and by Valkey's defragmentation.
  One run per cell.

Tree cost per key on other key shapes (in-process, 1M keys, integer values):

| Keys | Tree bytes per key |
|---|---|
| sequential ids (`key:N`, `memtier-N`) | **16 B** |
| `user:{id}:{email,uuid,name,age}` | 37 B |
| random UUIDs, binary (16 B) | 64 B |
| YCSB (`user<hash>`) | 77 B |
| random UUIDs as text, session tokens | 100 B |
| `user:{n}` + `user:{n}:email` (two keys per id) | 68 B |

### Prefix queries

Full `SCAN` loops (cursor until 0) over 1M keys (500K `user:N`, 300K `session:<hex>`, 200K
hashes), server CPU time read from `/proc` (2026-10-18, single run):

| | RadixOx | Valkey 9.1.2 |
|---|---|---|
| `COUNT 1000` | **50 ms** (1,001 calls) | 120 ms (999 calls) |
| `COUNT 1000 MATCH user:12*` (11,111 keys) | **< 10 ms** (12 calls) | 110 ms (999 calls) |
| `COUNT 1000 MATCH session:*` (300K keys) | **30 ms** (301 calls) | 110 ms (999 calls) |
| `COUNT 1000 MATCH *:99*` (3,426 keys) | **50 ms** (1,001 calls) | 180 ms (999 calls) |
| one `SCAN 0 MATCH *:99* COUNT 1000` call, latency (avg / p99) | **45 µs** / 71 µs | 78 µs / 135 µs |
| `COUNT 10` | 750 ms (100K calls) | **620 ms** (84K calls) |

Valkey scans the whole keyspace and filters; RadixOx walks only the subtree of the pattern's
literal prefix. The gap grows with the size of the database. With an explicit small `COUNT`,
Valkey wins: each RadixOx page descends again from the root to the resume key (without
`COUNT`, RadixOx uses pages of 1,000).

`KEYS user:*` (1,000 matches among 1,001,000 keys, server-side, 2026-10-12): RadixOx
0.15 ms, Valkey 21.4 ms. `KEYS` is rarely acceptable in production on Redis; the `SCAN`
figures above are the fair comparison.

### Persistence (1M keys, 100-byte values)

`BGSAVE` triggered 3 s into a 10-second memtier run without pipeline, dump on tmpfs (disk
cost excluded), two separate runs (2026-10-18):

| | `BGSAVE` duration | Throughput | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|
| RadixOx, no save (reference) | — | 236,398 ops/s | 0.407 ms | 0.511 ms | 0.703 ms | — |
| RadixOx, during `BGSAVE` (runs 1 / 2) | **662 / 686 ms** | 234,630 / 233,568 | **0.415 / 0.415 ms** | **0.735 / 0.511 ms** | **0.927 / 0.719 ms** | **2.42 / 2.10 ms** |
| Valkey, no save (reference) | — | 207,954 ops/s | 0.743 ms | 0.767 ms | 1.007 ms | — |
| Valkey, during `BGSAVE` (runs 1 / 2) | 1,728 / 1,781 ms (fork 2.1 / 2.4 ms) | 185,512 / 206,334 | 0.751 / 0.743 ms | 0.855 / 0.791 ms | 1.175 / 0.927 ms | 3.74 / 4.06 ms |

RadixOx snapshots in the server thread, in slices. *(Since 2026-10-20 — see the
[series above](#against-valkey-partial-load-and-bgsave-2026-10-20) — the save also runs
whenever neither a client nor the kernel has anything waiting, uncapped; the share below is
only a floor when the server is saturated.)* At the time of this table, while clients were
active, the save took at most 10 % of the thread (`RADIXOX_SAVE_CPU_SHARE`), so client
latency stayed at its reference level; on an idle server it runs at full speed (about 50 ms
for these 1M keys). Without that cap, the save took about 60 % of the thread for 100 ms and
doubled the p99.9.
Valkey forks: its child works on a second core (`bgsave-cpulist`), and the fork itself costs
about 2 ms here, growing with the heap (tens to hundreds of ms on tens of GB).

Restart with 1M keys (dump on tmpfs, 2026-10-17): RadixOx ~0.17 s (105 MB raw dump), Valkey
~0.53 s.

### Engine microbenchmarks (in-process, no network)

1M random keys, one core (2026-10-12, previous 64-byte nodes):

| Operation | Cost |
|---|---|
| `GET` of a random cold key | ~410 ns |
| `MGET` of 64 random keys | **~105 ns per key** |
| `KRANGE` page of 64 entries | **5.2 µs** (~82 ns per entry; 64 separate GETs: 15.4 µs) |
| `KRANGE` page of 1 entry | ~620 ns |

### Caveats

- Loopback only: the client's TCP receive path and netfilter are billed to the same machine.
  Two-machine numbers are planned.
- Check where a benchmark writes its dumps: on tmpfs (`/tmp` on many distributions) the
  fsync is free for both servers and saves look several times faster than on a disk.
- Short runs: memtier results are the median of 3 rounds of 10 s; YCSB is a single run and
  the `BGSAVE` measurement two runs. Treat the numbers as orders of magnitude, not as precise ratios.
- These numbers move as the project changes (the tree was redesigned on 2026-10-17); the
  script above regenerates them.

---

## Ordered keyspace

Keys live in a radix tree, so they are always sorted (unsigned byte order, like Redis's
`ZRANGEBYLEX`).

```
KRANGE    <prefix> <from> <count>   # up to <count> entries under <prefix>, keys > <from>, ascending
KREVRANGE <prefix> <from> <count>   # keys < <from>, descending
                                    # <from> = "" → from the start (or the end)
                                    # reply: [key1, value1, key2, value2, …]
```

- `from` is **exclusive**: pass the last key of a page to get the next one. No server-side
  cursor state, no duplicates, stable while the data changes.
- Only string values are returned (and counted); hashes, sets and sorted sets under the
  prefix are skipped.
- Order is lexicographic: `msg:12` < `msg:120` < `msg:13`. Zero-pad numbers if you need
  numeric order.

```bash
redis-cli KRANGE user:10:message: user:10:message:0042 30     # the 30 messages after #42
redis-cli KREVRANGE user:10:message: "" 30                      # the 30 most recent
```

Other prefix-aware operations: `KEYS prefix*` (subtree walk, no DFA), `KEYS` with any glob
(compiled to a byte-wise DFA that prunes subtrees; pathological patterns are refused instead of
freezing the server).

`SCAN` uses the same ordered walk, with the position kept server-side (the client gets a small
integer cursor):

- no duplicates, and every key present for the whole scan is returned once, even if the data
  changes between calls;
- without `COUNT`, pages hold 1,000 keys (Redis uses 10, as a hint);
- `COUNT` is exact: each call returns `COUNT` keys unless its examination budget
  (`max(COUNT, 1000)` keys) runs out first. A large `COUNT` saves round trips;
- `MATCH user:*` only walks the `user:` subtree; any other glob walks the subtree of its literal
  prefix;
- unlike Redis, an unknown, finished or reclaimed cursor gets `ERR invalid or expired cursor`.

Design: [`SCAN_DESIGN.md`](SCAN_DESIGN.md).

---

## Embedded use (OxidArt)

The engine is a regular Rust crate (`oxidart`) and can be used without the server:

```rust
use oxidart::{OxidArt, Value};
use oxidart::krange::RangeVal;
use radixox_lib::shared_byte::SharedByte;

let mut art = OxidArt::new();
art.set(SharedByte::from_str("user:10:message:1"), Value::String(SharedByte::from_str("hello")));

// Sorted page, values borrowed from the tree (no clone, no refcount traffic)
for (key, val) in art.krange(b"user:10:message:", b"", 30, false).iter() {
    if let RangeVal::Bytes(v) = val { /* … */ }
}

// Same page, owning its values (converted in place, no extra allocation)
let page = art.krange_owned(b"user:10:message:", b"", 30, false);

// Batched point lookups: the descents of all keys are interleaved
let vals = art.mget_owned(&[b"user:10:name", b"user:10:email"]);
```

Also available: `get`, `set`, `set_ttl`, `del`, `incr`/`incrby`, hash / set / sorted-set
commands, `getn` (prefix), `getn_glob` / `getn_regex` (feature `regex`), `mget_ref` /
`exists_count`, snapshots (`save_start` / `save_step`, `load_dump`).

The tree is single-threaded (`!Send` values): use one tree per thread, or put it behind your
own synchronization.

---

## Supported commands

| Category | Commands |
|---|---|
| Connection / server | `PING` `AUTH` `HELLO` (RESP2; `HELLO 3` → `-NOPROTO`) `CLIENT` (`SETNAME` `GETNAME` `ID` `SETINFO`) `COMMAND` `ECHO` `QUIT` `SELECT` `INFO` `DBSIZE` `FLUSHDB` `SAVE` `BGSAVE` `BGREWRITEAOF` `LASTSAVE` |
| Strings | `GET` `SET` (`EX` `PX` `NX` `XX`) `SETNX` `SETEX` `MGET` `MSET` |
| Counters | `INCR` `DECR` `INCRBY` `DECRBY` |
| Keys | `DEL` `UNLINK` `EXISTS` `TYPE` `KEYS` `SCAN` (`MATCH` `COUNT` `TYPE`) |
| Ordered keys | `KRANGE` `KREVRANGE` |
| Expiration | `EXPIRE` `PEXPIRE` `PERSIST` `TTL` `PTTL` |
| Hash | `HSET` `HMSET` `HGET` `HMGET` `HGETALL` `HDEL` `HEXISTS` `HLEN` `HKEYS` `HVALS` `HINCRBY` |
| Set | `SADD` `SREM` `SISMEMBER` `SCARD` `SMEMBERS` `SPOP` |
| Sorted set | `ZADD` `ZREM` `ZSCORE` `ZCARD` `ZRANGE` (`WITHSCORES`) `ZINCRBY` |
| Pub/Sub | `SUBSCRIBE` `UNSUBSCRIBE` `PUBLISH` |
| Replication | `REPLICATE` (`host port [AUTH pass]`, `STOP host port`) `REPLICAOF NO ONE` `WAIT` `DEBUG DIGEST` (internal: `RXHELLO` `RXLOAD` `RXBLOCK` `RXTICK`) |

- `UNLINK key` deletes that key only; `UNLINK prefix*` deletes every key under `prefix`.
- `KEYS`, `UNLINK prefix*` and `FLUSHDB` never block other clients: the work is done in
  slices between other commands. `FLUSHDB` replies at once (`DBSIZE` is 0 right after) and
  frees the memory in the background.
- Commands can also be sent inline, as with Redis (`SET k "a b"` from telnet or `nc`).

Not yet: list commands, `MULTI`/`EXEC`, `CONFIG`, RESP3, Lua, cluster.

---

## Persistence

- **`SAVE`** (blocking) and **`BGSAVE`** (incremental, in slices between client requests)
  write `RADIXOX_DIR/dump.rxd`. The dump is reloaded at startup. `BGSAVE` works whenever no
  client and no kernel completion is waiting, and is guaranteed 10 % of the server thread
  under load (`RADIXOX_SAVE_CPU_SHARE`).
- **Automatic saves, as in Valkey**: `RADIXOX_SAVE` takes Valkey's `save` syntax and
  defaults — `"3600 1 300 100 60 10000"`: a `BGSAVE` after 3600 s if at least 1 key changed,
  after 300 s if 100 changed, after 60 s if 10,000 changed (counted per element, like Redis's
  `dirty`). `RADIXOX_SAVE=""` disables them. After a failed `BGSAVE`, the next automatic try
  waits 5 s, and writes are refused with `-MISCONF` until a save succeeds
  (`RADIXOX_STOP_WRITES_ON_BGSAVE_ERROR`, on by default). `LASTSAVE` and the
  `# Persistence` section of `INFO` (`rdb_changes_since_last_save`, `rdb_bgsave_in_progress`,
  `rdb_last_save_time`, `rdb_last_bgsave_status`, `rdb_last_bgsave_time_sec`, …) report
  them.
- Snapshots use an **application-level copy-on-write barrier** instead of `fork`: a node is
  copied only if it is modified before the snapshot reaches it. No memory doubling, no fork
  latency on a large heap.
- `BGSAVE` compresses with zstd (level 3 by default); the file ends with a CRC footer, and a
  truncated or corrupted dump prevents startup instead of loading partial data.
- **Save on SIGINT / SIGTERM** when automatic saves are on, as Valkey does
  (`RADIXOX_SAVE_ON_EXIT=0` / `1` to force).
- **Append-only file** (`RADIXOX_APPENDONLY=yes`): the commands received are
  logged in blocks, one per loop turn, each carrying the clock of its turn, so that TTLs and
  lazy expiry replay identically; only non-deterministic commands are rewritten (`SPOP` →
  `SREM` of the popped members). Files live in `RADIXOX_DIR/appendonlydir/` (a base dump, log
  segments preallocated by 64 MiB, a manifest); at startup the base is loaded, its content
  digest checked, and the segments replayed; a block cut by a crash at the end of the last
  segment is truncated (`RADIXOX_AOF_LOAD_TRUNCATED`), anything else inconsistent prevents
  startup. `appendfsync everysec` (default): replies go out at once, even before the log
  write, and an `fdatasync` runs every second or as soon as `RADIXOX_AOF_SYNC_MB` of written
  data is not yet synced, so a crash loses at most about that much. `continuous`: replies go
  out at once too, and the log is written in back-to-back cycles of "write everything pending,
  then a linked `fdatasync`" (`IOSQE_IO_LINK`), one cycle in flight and nothing written during
  an `fdatasync`; a power loss loses about two cycles (a few ms on an idle SSD) instead of a
  second, at about `everysec`'s throughput. `no`: the kernel decides.
  `always`: replies — to writes, and to reads that may have seen a write not yet on disk —
  are held until the `fdatasync` that covers them, one `fdatasync` in flight at a time, everything that arrives meanwhile goes into
  the next one (group commit; the loop never blocks). Writes are refused with `-MISCONF` while
  the log cannot be written, and writers wait when the disk falls behind by more than
  `RADIXOX_AOF_MAX_PENDING_MB`. `INFO` reports `aof_*` fields.
- **Log rewrite**: with the AOF on, every `BGSAVE` (command, automatic rule, `BGREWRITEAOF`,
  or the log growing past `RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE` % of the base and
  `RADIXOX_AUTO_AOF_REWRITE_MIN_SIZE`) writes a new base at an exact cut point in the stream
  of commands, in slices as any `BGSAVE`, while the log moves on to a new segment; the old
  files are deleted once the new manifest is on disk. `SAVE` still writes `dump.rxd`.
- Every dump ends with a **content digest** checked at load time, computed by two different
  traversals (the save's and one of the reloaded tree): a dump that reloads into anything but
  the saved state is refused.
- Not yet: `CONFIG GET` / `CONFIG SET` (settings are environment variables).

Design: [`SAVE_DESIGN.md`](SAVE_DESIGN.md), [`DURABILITY_DESIGN.md`](DURABILITY_DESIGN.md).

---

## Replication

The primary pushes to its replicas: `REPLICATE <host> <port> [AUTH <password>]` on the
primary (or `RADIXOX_REPLICATE="host1:port1,host2:port2"` at startup) adds a target, which it
connects to as a client and keeps reconnecting every second. A replica is an ordinary RadixOx
server: it follows the first source that connects, refuses writes from other clients
(`-READONLY`) and serves reads.

- The replica receives **the same blocks as the AOF**, byte for byte (commands received, with
  the clock of their turn): it replays them with the normal executor, so TTLs expire on the
  replica as on the primary whatever its own clock. Each block is acknowledged.
- **Partial resync**: after a short cut, the replica resumes from the last block it applied if
  the primary still has the following ones in memory (`RADIXOX_REPL_BACKLOG_MB`). Otherwise
  **full sync**: a `BGSAVE` cut at an exact point in the stream (an AOF rewrite when the AOF is
  on), sent in 1 MiB chunks; the replica loads it (CRC and content digest checked) and the
  blocks written meanwhile follow.
- A replica that falls behind by more than `RADIXOX_REPL_MAX_LAG_MB` is disconnected (and
  resyncs).
- A replica can have its own AOF (`RADIXOX_APPENDONLY=yes`): it writes the blocks it receives
  as is; restarted on its files, it comes back as a replica (read-only at once) and resumes
  from the primary's backlog without a new full sync. With `appendfsync always`, its
  acknowledgements wait for its fsync.
- **Chained replicas**: a replica can itself `REPLICATE` to others; it forwards the same blocks
  under the primary's history id.
- **Failover** (by the operator): `REPLICAOF NO ONE` on the chosen replica, then `REPLICATE` on
  it towards the others. The promoted server starts a new history and refuses to become a
  replica again until restarted, so that the old primary, if still alive and configured,
  cannot overwrite it.
- A primary restart always leads to a full resync (new history id at each start: a primary that
  lost the end of its log in a crash cannot be mistaken for the same history).
- `WAIT <replicas> <timeout ms>` blocks the connection until that many replicas have
  acknowledged its last write (or the timeout; `0` = no timeout), and returns how many have,
  as in Valkey. Other connections are served meanwhile.
- `INFO` has a `# Replication` section (`role`, `connected_replicas`, one `replicaN:` line per
  target with its state and lag, `sync_full`, `sync_partial_ok`); `DEBUG DIGEST [at]` returns
  the content digest, to compare two servers.
- Not yet: `min-replicas-to-write`, zero-copy sends, TLS, partial resync after a primary
  restart or a failover. The replica blocks while loading a full sync.

Design and deviations: [`DURABILITY_DESIGN.md`](DURABILITY_DESIGN.md) §10.

---

## Configuration

Environment variables:

| Variable | Default | Effect |
|---|---|---|
| `RADIXOX_PORT` | `6379` | listening port |
| `RADIXOX_BIND` | `127.0.0.1` | listening address (`0.0.0.0` or `::` to accept remote clients; a warning is printed if no password is set) |
| `RADIXOX_PASSWORD` | unset | require `AUTH <password>` (or `AUTH default <password>`) before any other command; use a long random value |
| `RADIXOX_DIR` | `.` | directory of `dump.rxd`, of `appendonlydir/`, and of a full sync being received by a replica |
| `RADIXOX_CPU_PIN` | unset | pin the server thread to this CPU |
| `RADIXOX_PREFETCH` | `64` | execution window: commands whose key lookups are interleaved, across connections and pipelines (`1` = one by one) |
| `RADIXOX_SAVE_ZSTD` | `3` | `BGSAVE` zstd level (`0` = raw) |
| `RADIXOX_SAVE_SYNC_ZSTD` | raw | zstd level of `SAVE` and of the exit save |
| `RADIXOX_SAVE_BACKLOG_MB` | `64` | compression backlog of `BGSAVE` |
| `RADIXOX_SAVE` | `3600 1 300 100 60 10000` | automatic `BGSAVE` rules, Valkey's `save` syntax: pairs `<seconds> <changes>`; `""` disables |
| `RADIXOX_STOP_WRITES_ON_BGSAVE_ERROR` | on | refuse writes (`-MISCONF`) after a failed `BGSAVE` while automatic saves are on |
| `RADIXOX_SAVE_ON_EXIT` | on with automatic saves | save on SIGINT / SIGTERM |
| `RADIXOX_APPENDONLY` | `no` | append-only file in `RADIXOX_DIR/appendonlydir/` (created from `dump.rxd` the first time) |
| `RADIXOX_APPENDFSYNC` | `everysec` | `everysec` (one `fdatasync` per second, off the loop), `no` (left to the kernel), `always` (replies wait for the `fdatasync` covering them, group commit), or `continuous` (back-to-back write + `fdatasync` cycles, replies do not wait) |
| `RADIXOX_AOF_LOAD_TRUNCATED` | `yes` | at startup, cut a block left incomplete by a crash at the end of the log instead of refusing to start |
| `RADIXOX_AOF_MAX_PENDING_MB` | `64` | log bytes not yet written beyond which writers wait (reads go on) |
| `RADIXOX_AOF_SYNC_MB` | `32` | `everysec`: written but unsynced log bytes that trigger an `fdatasync` before the one-second timer |
| `RADIXOX_AOF_CYCLE_MIN_KB` / `RADIXOX_AOF_CYCLE_MS` | `4` / `10` | `continuous`: a cycle starts as soon as this much is pending, or after this delay for less (limits SSD wear under light traffic) |
| `RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE` / `_MIN_SIZE` | `100` / `64mb` | rewrite the log when it has grown by this percentage of the base and at least this size (`0` = never) |
| `RADIXOX_REPLICATE` | unset | replicas to push to, `host:port,host:port` (IPv6 in brackets) |
| `RADIXOX_REPLICATE_AUTH` | unset | password sent with `AUTH` to the replicas of `RADIXOX_REPLICATE` |
| `RADIXOX_REPL_BACKLOG_MB` | `64` | recent blocks kept in memory for partial resyncs |
| `RADIXOX_REPL_MAX_LAG_MB` | `256` | bytes waiting for a replica beyond which it is disconnected |
| `RADIXOX_SAVE_CPU_SHARE` | `0.1` | share of the server thread a `BGSAVE` is guaranteed while clients are active; whenever no client and no kernel completion is waiting, background work (save, `FLUSHDB` freeing, long `KEYS`) runs uncapped |
| `RADIXOX_SCAN_IDLE_SECS` | `60` | idle time after which a `SCAN` cursor can be reclaimed |
| `RADIXOX_SCAN_MAX_CURSORS` | `65536` | active `SCAN` cursors at most; beyond, a new `SCAN` gets an error |
| `RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT` | `128` | active `SCAN` cursors at most per connection (its own idle cursors are reclaimed first) |
| `RADIXOX_TIMEOUT` | `0` | close a client idle for this many seconds (`0` = never), like Redis's `timeout` |
| `RADIXOX_BATCH_CMDS` | `2048` | commands per slice of a loop turn; replies ready between slices are sent |
| `RADIXOX_RING_BUFS` / `RADIXOX_RING_BUF_KB` | `64` / `32` | receive buffer ring shared by all connections |
| `RADIXOX_SEND_ASYNC` | off | run sends on kernel io-wq workers (`IOSQE_ASYNC`): helps clients without pipelining when a second core is available |
| `RADIXOX_SQPOLL_CPU` / `RADIXOX_SQPOLL_IDLE_MS` / `RADIXOX_SQPOLL_SPIN_US` | unset / `1000` / `1000` | dedicated machines: a kernel SQPOLL thread pinned on this CPU submits and receives, so the loop makes no syscalls while busy (pair with `RADIXOX_SEND_ASYNC`). Burns that core; +10 % pipelined throughput measured, no gain without pipelining |
| `RADIXOX_AMAC` / `RADIXOX_AMAC_PF` | `8` / `2` | in each window, every run of consecutive supported commands (string commands, MGET / MSET / DEL / EXISTS, Hash / Set / ZSet on one key; AOF and replication included) first runs all its key lookups as `N` interleaved state machines, then executes in order from the found positions; other commands split the runs (`0` = off: plain prefetch, then normal lookups). For fields of large Hashes and Sets, a second interleaved pass prefetches the field lookup inside the container. `AMAC_PF` prefetches ahead of execution (`2` = node and value). Pipelined: +50 % GET/SET, +15 % Hash, +19 % with AOF, +10–23 % on fields of large Hashes / Sets |
| `RADIXOX_RECV_ASYNC` | off | experimental: single-shot receives on io-wq workers instead of multishot on the loop; measured slower on loopback (more CPU per command), kept for testing on real NICs |
| `RADIXOX_IOWQ_WORKERS` / `RADIXOX_IOWQ_CPUS` | `1` / unset | io-wq network workers at most, and the CPUs they may run on (e.g. `1,7`) |
| `RADIXOX_FILE_CPUS` | unset | run the dump's writes and fsync on an io-wq worker pinned to these CPUs (e.g. `1`), off the server's core; io-wq has one CPU mask, the union of this and `RADIXOX_IOWQ_CPUS` |
| `RADIXOX_MIN_WAIT_US` / `RADIXOX_WAIT_NR` | off / `32` | experimental: when idle, let the kernel wait up to this many µs for `WAIT_NR` completions before waking the loop for one (lower CPU per command, same throughput unless the loop is the bottleneck) |
| `RADIXOX_GC` | `1` | `0`: no compaction of the pool nor of the node slab (pages that empty by themselves are still returned) |
| `RADIXOX_GC_LOG` | `0` | `1`: one stderr line per compaction pass (pool and node slab) |
| `RADIXOX_GC_RESERVE_DIV` | `16` | blocks kept in reserve for compaction: `max(4 MiB, live / div)` |
| `RADIXOX_MEMORY_DECAY` | `10` | seconds an empty 2 MiB page beyond the reserve is kept before being returned to the system |
| `RADIXOX_POOL_VIRT_GB` | `256` | virtual range reserved for the value pool (nothing is committed until used) |

### Memory

The whole tree lives in RadixOx's own pool: values up to 16 KiB, key suffixes, node pages,
small hashes and sets, valart overflow blocks. 64 KiB bump blocks on 2 MiB huge pages, a live
counter per block, compacted in the loop's idle moments (and in bounded slices under load);
the node slab is compacted in the background too and its tail returned to the system. Memory
freed by deletions goes back to the system, which mimalloc alone never did after scattered
deletions. Larger values go to jemalloc. `INFO` shows `mem_pool_*`, `gc_*` and `compact_*`.
Build with `RUSTFLAGS="--cfg radixox_nopool"` to use mimalloc instead.

### Security

- RadixOx listens on `127.0.0.1` unless `RADIXOX_BIND` says otherwise, and prints a warning if
  it listens elsewhere without a password.
- `RADIXOX_PASSWORD` works like Redis's `requirepass`: before `AUTH` (or `HELLO 2 AUTH …`),
  every command except `AUTH`, `HELLO` and `QUIT` gets `-NOAUTH`. The comparison is in constant
  time. Use a long random password (e.g. `openssl rand -hex 32`).
- No ACLs (one `default` user) and no TLS yet: put a TLS proxy in front for remote clients.

---

## Correctness

A data store has to be right before it is fast. What is tested today:

- **Differential fuzzing of the tree**: random operations (SET, DEL, TTL, prefix deletes,
  hashes, sets) applied to the current tree and to the previous engine (`oxidart-legacy`,
  kept as an oracle), with identical answers required and the whole structure checked after
  every operation (canonical shape, counters, back-pointers).
- **Snapshots under mutation**: a save is started, the tree is mutated between its slices,
  and the dump must equal the state at the start of the save; dumps are reloaded across both
  engines. A truncated or corrupted dump refuses to load (CRC footer).
- **Model tests** against `BTreeMap` for ordered queries (`KRANGE`, `SCAN` pages with writes
  between pages), and glob matching checked against a port of Redis's `stringmatchlen`.
- **Unsafe code** (the slab, node layout, reference-counted buffers: a few hundred `unsafe`
  blocks): the test suites have been run under AddressSanitizer, with the allocator switched
  to malloc so that every block is visible. The procedure, including negative controls for
  the copy-on-write tests, is in [`SAVE_DESIGN.md`](SAVE_DESIGN.md); it is not automated yet.
- **Server integration tests** over TCP with the Rust `redis` client and raw RESP: every
  command family, `AUTH` (refused commands have no effect, pipelines, near-miss passwords),
  `HELLO`/`CLIENT`, `SCAN` cursors, `BGSAVE` and restart.

About 300 engine and library tests plus about 130 server tests
(`cargo test -p oxidart --features regex`, `-p valart`, `-p radixox-lib`, `-p radixox`).
Not done yet: fuzzing of the RESP parser, Redis's own test suite, crash (kill -9) tests.

---

## Deployment

- **Linux 6.12 or newer**, with io_uring (incremental buffer rings; tested on 6.12 LTS and
  7.2). There is no epoll fallback: if io_uring is unavailable, the server fails at startup
  with an explanation.
- **Open files**: the server raises its soft `RLIMIT_NOFILE` to the hard limit at startup;
  if that stays below 10,000 it accepts fewer connections and prints a warning.
- **Containers**: the default seccomp profiles of recent Docker and containerd versions
  block the io_uring system calls. Allow `io_uring_setup`, `io_uring_enter` and
  `io_uring_register` in a custom profile (or run with `--security-opt seccomp=unconfined`
  if that is acceptable). Some managed Kubernetes offerings disable io_uring entirely.
- **Kernel setting** (Linux 6.6+): `/proc/sys/kernel/io_uring_disabled` must be `0` (or `1`
  with the process in the allowed group).
- Listens on `127.0.0.1` by default; see [Security](#security) before exposing it.

---

## Architecture

```
TCP ──io_uring──▶ shared buffer ring ──▶ windows of K commands ──▶ interleaved key lookups
     (multishot recv,   (parsed in place,      (across connections)      (prefetch, misses overlap)
      no runtime)        right before use)                                       │
                                                                                 ▼
      client ◀── one send per connection ◀── RESP encoder ◀── OxidArt (values by reference)
```

**The event loop** ([`RUNTIME_ARCH.md`](RUNTIME_ARCH.md)): a hand-written io_uring loop, no
async runtime, no futures, no wakers. Each turn harvests completions (multishot accept and
receive into a shared ring of small buffers), runs the ready connections' commands in
windows (parse, prefetch, execute), sends one reply buffer per connection, then advances
background jobs (`KEYS`, `UNLINK prefix*`, `FLUSHDB`, `BGSAVE`) by one slice. A long command
parks only its own connection. The connection logic is a pure state machine tested against
a simulated kernel.

**OxidArt, the radix tree**
- **128-byte nodes with inline leaves** in a slab (HiSlab) backed by anonymous `mmap` with
  transparent huge pages. Stable `u32` indices instead of pointers. A key without
  descendants is not a node: it is a leaf stored in its parent (radix, suffix, value, TTL),
  so the last level of the tree costs a few bytes per key instead of a whole node.
- **Rank addressing**: entries are sorted; bitmaps say which ones are children, carry a TTL or
  a suffix, and a popcount gives the offset of any entry. Children grow from one end of the
  node's cells, leaves from the other.
- **Four shapes**: 8 entries inline (radixes searched with one 8-byte load and a bit trick,
  SWAR), 8 more in an overflow page sharing the same bitmaps, then up to 32 with the metadata
  in the node and the cells in the page, then up to 256 with a 256-bit radix bitmap. Pages
  grow by size classes, shrink with hysteresis, and live in the pool (moved by compaction).
- **Path compression** (`CompactPath`: up to 7 bytes inline, longer segments on the heap) and
  automatic re-compression after deletions: the tree always has the same canonical shape for
  the same content.
- **Interleaved descents**: when several keys are known at once (pipeline window, `MGET`,
  `EXISTS`), their lookups advance level by level so cache misses overlap.
- **TTL**: expired keys are invisible to reads and removed lazily or by an active sampling
  evictor.

**Values**
- Strings: `SharedByte`, an 8-byte single-threaded reference-counted buffer (no atomics,
  4-byte header), allocated in the pool up to 16 KiB and in jemalloc beyond. Integers are
  stored as `i64` and formatted on read.
- Hash and Set: up to 8 entries, one compact block of `SharedByte` pointers; beyond that,
  **valart**, a compact radix tree per container.
- Sorted set: `BTreeSet<(score, member)>` + hash map for O(1) `ZSCORE`.

**Single-threaded on purpose**: no locks, no contention, predictable tail latency. Scaling
across cores is meant to come from sharding by namespace, not from shared-state threads.

---

## Roadmap

- **Production basics first**: periodic automatic saves, `maxmemory` with eviction,
  `MULTI`/`EXEC`, lists, fuzzing of the RESP parser.
- **Event loop next steps** ([`RUNTIME_ARCH.md`](RUNTIME_ARCH.md)): prefetch ahead (next
  window's data while the current one runs), sends delegated to kernel workers
  (`IOSQE_ASYNC`), `SEND_ZC` for large replies.
- **Atomic conditional transactions**: `MULTI ATOMIC` where an empty read aborts the whole
  block, `DELPREFIX` (delete a whole namespace in O(prefix length)), durable commits —
  [`TX_DESIGN.md`](TX_DESIGN.md).
- Replication, next steps ([`DURABILITY_DESIGN.md`](DURABILITY_DESIGN.md)): zero-copy sends
  of the shared blocks (`SEND_ZC`, `splice` of the dump) if a two-machine measurement shows
  the copies matter (they are ~0.4 % of a profile on one machine), `min-replicas-to-write`,
  partial resync after a failover.
- Lower memory per key on random keys: long suffixes stored inline on several cells (no
  separate allocation), a smaller node class for nodes with two or three entries.
- Memory, next steps ([`MEMORY_ARCH.md`](MEMORY_ARCH.md)): a short-lived pool for command
  temporaries (no holes left by a bulk load, less compaction work under load), compaction of
  valart's slab, a capped reserve.
- Client compatibility: `CONFIG`, RESP3.
- Sharding by namespace for multi-core throughput.

---

## Repository layout

```
radixox/
├── oxidart/        # radix tree engine: TTL, prefix / glob / range queries, snapshots
├── oxidart-legacy/ # previous engine (64-byte nodes), test oracle only, not published
├── valart/         # compact radix trees for Hash and Set values
├── radixox-lib/    # SharedByte, RESP parser and encoder, save barrier
├── radixox/        # server binary (RESP2 over io_uring)
├── ARCHITECTURE.md # architecture overview, invariants, map of the design docs (French)
├── RUNTIME_ARCH.md # design of the custom io_uring loop
├── SAVE_DESIGN.md  # design of copy-on-write snapshots
└── TX_DESIGN.md    # design of atomic conditional transactions
```

Tests: `cargo test -p oxidart --features regex`, `cargo test -p radixox-lib`,
`cargo test -p radixox`.

---

## License

MIT

Built with [io-uring](https://github.com/tokio-rs/io-uring),
[mimalloc](https://github.com/microsoft/mimalloc),
[jemalloc](https://github.com/tikv/jemallocator),
[regex-automata](https://github.com/rust-lang/regex) and
[hislab](https://github.com/JoshuaBOUCHAT/hislab). Benchmarked against
[Valkey](https://github.com/valkey-io/valkey).
