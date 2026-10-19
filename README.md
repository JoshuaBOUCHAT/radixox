# RadixOx

**An ordered in-memory key-value store that speaks the Redis protocol (RESP2, a subset of the commands). Rust, Adaptive Radix Tree, io_uring.**

Keys live in a radix tree instead of a hash table, so they are always sorted. That makes
things Redis cannot do natively cheap: sorted pagination under a prefix (`KRANGE`), `SCAN`
and `KEYS` that only walk the matching subtree, a `SCAN` without duplicates. `redis-cli` and
existing Redis client libraries work for the [supported commands](#supported-commands), and
the engine, **OxidArt**, is also usable as an embedded Rust library.

It is single-threaded by design. On one core it has about half of Valkey's tail latency and
13–20 % more throughput; a multi-threaded Valkey has more total throughput (see
[Benchmarks](#benchmarks)).

---

## Status

RadixOx is **not a drop-in Redis replacement for production data yet**.

Good fit today:
- a cache or a pagination / prefix index whose data can be rebuilt;
- OxidArt embedded in a Rust program (ordered map with TTL, prefix and range queries).

Missing for primary data:
- **no memory limit or eviction** (`maxmemory`, LRU/LFU): a growing dataset ends in OOM;
- **durability is manual**: `SAVE` / `BGSAVE` and a save on SIGTERM, no AOF, no periodic
  saves; a crash loses everything since the last save;
- no replication, no cluster, no TLS, no ACLs (one password);
- no lists, `MULTI`/`EXEC` or Lua, which queue and job libraries (Sidekiq, Celery, BullMQ)
  rely on;
- Linux only, and io_uring must be allowed (see [Deployment](#deployment)).

The I/O layer will be rewritten (custom io_uring loop, see [Roadmap](#roadmap)).

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

Measured on 2026-10-18 (memory on 2026-10-17), 1M keys, loopback; see
[Benchmarks](#benchmarks) for the setup and caveats.

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

Requirements: **Linux** with io_uring (developed on 6.12 LTS and 7.2), **Rust 2024 edition**
(stable).

---

## Benchmarks

### Setup

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
- **Values**: a string is a separate allocation (100 B → 112 B in mimalloc); with large
  values both servers converge.
- **Small hashes and sets** (up to 8 entries) are one compact block of field/value pointers
  instead of a tree. One-field hashes are much smaller than in Valkey; hashes with several
  short fields are larger, because each field and value stays a separately allocated
  `SharedByte` (a deliberate trade-off: no copies on `HGET`/`HSET`).
- **Connections**: each connection keeps three 64 KiB buffers while it is active (they grow
  for large commands). With many concurrent clients this adds a few MB.

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

RadixOx snapshots in the server thread, in slices. While clients are active, the save takes
at most 10 % of the thread (`RADIXOX_SAVE_CPU_SHARE`), so client latency stays at its
reference level; on an idle server it runs at full speed (about 50 ms for these 1M keys).
Without that cap, the save took about 60 % of the thread for 100 ms and doubled the p99.9.
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
| Connection / server | `PING` `AUTH` `HELLO` (RESP2; `HELLO 3` → `-NOPROTO`) `CLIENT` (`SETNAME` `GETNAME` `ID` `SETINFO`) `COMMAND` `ECHO` `QUIT` `SELECT` `INFO` `DBSIZE` `FLUSHDB` `SAVE` `BGSAVE` |
| Strings | `GET` `SET` (`EX` `PX` `NX` `XX`) `SETNX` `SETEX` `MGET` `MSET` |
| Counters | `INCR` `DECR` `INCRBY` `DECRBY` |
| Keys | `DEL` `UNLINK` `EXISTS` `TYPE` `KEYS` `SCAN` (`MATCH` `COUNT` `TYPE`) |
| Ordered keys | `KRANGE` `KREVRANGE` |
| Expiration | `EXPIRE` `PEXPIRE` `PERSIST` `TTL` `PTTL` |
| Hash | `HSET` `HMSET` `HGET` `HMGET` `HGETALL` `HDEL` `HEXISTS` `HLEN` `HKEYS` `HVALS` `HINCRBY` |
| Set | `SADD` `SREM` `SISMEMBER` `SCARD` `SMEMBERS` `SPOP` |
| Sorted set | `ZADD` `ZREM` `ZSCORE` `ZCARD` `ZRANGE` (`WITHSCORES`) `ZINCRBY` |
| Pub/Sub | `SUBSCRIBE` `UNSUBSCRIBE` `PUBLISH` |

- `UNLINK key` deletes that key only; `UNLINK prefix*` deletes every key under `prefix`.
- `KEYS`, `UNLINK prefix*` and `FLUSHDB` never block other clients: the work is done in
  slices between other commands. `FLUSHDB` replies at once (`DBSIZE` is 0 right after) and
  frees the memory in the background.
- Commands can also be sent inline, as with Redis (`SET k "a b"` from telnet or `nc`).

Not yet: list commands, `MULTI`/`EXEC`, `CONFIG`, RESP3, Lua,
replication, cluster.

---

## Persistence

- **`SAVE`** (blocking) and **`BGSAVE`** (incremental, in slices between client requests)
  write `RADIXOX_DIR/dump.rxd`. The dump is reloaded at startup. While clients are active,
  `BGSAVE` uses at most 10 % of the server thread (`RADIXOX_SAVE_CPU_SHARE`), so it does not
  raise their tail latency.
- Snapshots use an **application-level copy-on-write barrier** instead of `fork`: a node is
  copied only if it is modified before the snapshot reaches it. No memory doubling, no fork
  latency on a large heap.
- `BGSAVE` compresses with zstd (level 3 by default); the file ends with a CRC footer, and a
  truncated or corrupted dump prevents startup instead of loading partial data.
- `RADIXOX_SAVE_ON_EXIT=1`: save on SIGINT / SIGTERM.
- Not yet: AOF, automatic periodic saves.

Design: [`SAVE_DESIGN.md`](SAVE_DESIGN.md).

---

## Configuration

Environment variables:

| Variable | Default | Effect |
|---|---|---|
| `RADIXOX_PORT` | `6379` | listening port |
| `RADIXOX_BIND` | `127.0.0.1` | listening address (`0.0.0.0` or `::` to accept remote clients; a warning is printed if no password is set) |
| `RADIXOX_PASSWORD` | unset | require `AUTH <password>` (or `AUTH default <password>`) before any other command; use a long random value |
| `RADIXOX_DIR` | `.` | directory of `dump.rxd` |
| `RADIXOX_CPU_PIN` | unset | pin the server thread to this CPU |
| `RADIXOX_PREFETCH` | `8` | pipelined commands whose key lookups are interleaved (`0` = off) |
| `RADIXOX_SAVE_ZSTD` | `3` | `BGSAVE` zstd level (`0` = raw) |
| `RADIXOX_SAVE_SYNC_ZSTD` | raw | zstd level of `SAVE` and of the exit save |
| `RADIXOX_SAVE_BACKLOG_MB` | `64` | compression backlog of `BGSAVE` |
| `RADIXOX_SAVE_ON_EXIT` | off | save on SIGINT / SIGTERM |
| `RADIXOX_SAVE_CPU_SHARE` | `0.1` | share of the server thread a `BGSAVE` may use while clients are active (no cap after 1 ms without network reads) |
| `RADIXOX_SCAN_IDLE_SECS` | `60` | idle time after which a `SCAN` cursor can be reclaimed |
| `RADIXOX_SCAN_MAX_CURSORS` | `65536` | active `SCAN` cursors at most; beyond, a new `SCAN` gets an error |
| `RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT` | `128` | active `SCAN` cursors at most per connection (its own idle cursors are reclaimed first) |
| `RADIXOX_TIMEOUT` | `0` | close a client idle for this many seconds (`0` = never), like Redis's `timeout` |
| `RADIXOX_BATCH_CMDS` | `2048` | commands per slice of a loop turn; replies ready between slices are sent |
| `RADIXOX_RING_BUFS` / `RADIXOX_RING_BUF_KB` | `64` / `32` | receive buffer ring shared by all connections |

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

- **Linux only**, with io_uring (tested on 6.12 LTS and 7.2; older kernels are untested).
  There is no epoll fallback: if io_uring is unavailable, the server fails at startup.
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
  are sized to mimalloc's size classes and shrink with hysteresis.
- **Path compression** (`CompactPath`: up to 7 bytes inline, longer segments on the heap) and
  automatic re-compression after deletions: the tree always has the same canonical shape for
  the same content.
- **Interleaved descents**: when several keys are known at once (pipeline window, `MGET`,
  `EXISTS`), their lookups advance level by level so cache misses overlap.
- **TTL**: expired keys are invisible to reads and removed lazily or by an active sampling
  evictor.

**Values**
- Strings: `SharedByte`, an 8-byte single-threaded reference-counted buffer (no atomics),
  allocated with mimalloc. Integers are stored as `i64` and formatted on read.
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
- AOF and replication (sharing one stream of effects).
- Lower memory per key on random keys: long suffixes stored inline on several cells (no
  separate allocation), a smaller node class for nodes with two or three entries.
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
[regex-automata](https://github.com/rust-lang/regex) and
[hislab](https://github.com/JoshuaBOUCHAT/hislab). Benchmarked against
[Valkey](https://github.com/valkey-io/valkey).
