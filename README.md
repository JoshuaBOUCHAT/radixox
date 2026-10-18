# RadixOx

**An in-memory key-value store that speaks the Redis protocol (RESP2, a subset of the commands), built on an Adaptive Radix Tree, in Rust, on io_uring.**

RadixOx speaks RESP2, so `redis-cli` and existing Redis client libraries work with it for the
[supported commands](#supported-commands). Its
storage engine, **OxidArt**, keeps keys in an ordered radix tree instead of a hash table. On a
single core it beats Valkey on throughput, tail latency and, with structured keys, memory, and
it makes ordered and prefix operations native: sorted pagination (`KRANGE`), prefix scans that
only touch matching keys, and binary-safe keys.

OxidArt is also usable directly as an embedded Rust library.

---

## Highlights

One core each, 1M keys, measured on 2026-10-18 (memory on 2026-10-17); see
[Benchmarks](#benchmarks) for the setup and caveats:

| | RadixOx | Valkey 9.1.2 | |
|---|---|---|---|
| p99, no pipeline (80 conns) | **0.38 ms** | 0.74 ms | −48 % |
| p99.9, no pipeline | **0.44 ms** | 0.76 ms | −42 % |
| Throughput, no pipeline | **239 k ops/s** | 209 k ops/s | +14 % |
| Throughput, pipeline 32 | **2.44 M ops/s** | 2.00 M ops/s | +22 % |
| Operations per second of server CPU, no pipeline | **389 k** | 325 k | +20 % |
| YCSB A, READ p99 (1M records) | **544 µs** | 1,040 µs | −48 % |
| Memory, 1M keys × 100 B values (structured keys) | **131 MB** | 163 MB | −20 % |
| Memory, 1M random UUID keys × 100 B | 209 MB | 205 MB | ≈ |
| Memory, 1M one-field hashes (YCSB-like) | **233 MB** | 344 MB | −32 % |
| Memory, 1M two-field hashes (short values) | 161 MB | **129 MB** | Valkey −20 % |
| `SAVE` of 1M keys | **84 ms** (raw) / 70 ms (zstd) | 1,810 ms | ×22–26 |
| Restart with 1M keys | **~0.17 s** | ~0.53 s | ×3 |
| `KEYS user:*` (1,000 matches in 1M keys, 2026-10-12) | **0.15 ms** | 21.4 ms | ×140 |
| Full `SCAN MATCH user:12*` (11K matches in 1M keys, server CPU, 2026-10-18) | **< 10 ms**, 12 calls | 110 ms, 999 calls | ×10+ |

Plus what Redis doesn't have:

- **`KRANGE` / `KREVRANGE`**: the next N keys under a prefix, in sorted order, after a cursor
  key. Pagination for message threads, logs or feeds, without maintaining a sorted set.
- **Prefix and glob queries that prune**: `KEYS user:*` and `SCAN MATCH user:*` walk only the
  `user:` subtree, and complex `KEYS` globs are compiled to a DFA that skips whole subtrees.
- **A stricter `SCAN`**: no duplicates, `COUNT` is exact, and every key present for the whole
  scan is returned exactly once, even while the data changes.
- **Binary-safe keys**: any byte 0x00–0xFF at any position, ordered like `memcmp`.
- **Snapshots without `fork`**: an application-level copy-on-write barrier gives a consistent
  point-in-time dump while the server keeps serving.

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
- **One core each.** Both servers run at the same time, pinned to CPU 0 (its SMT sibling stays
  idle); only one is loaded at a time, and the order alternates every round so that drift on
  the machine hits both. RadixOx runs without SQ_POLL; Valkey 9.1.2 runs with `io-threads 1`,
  persistence off. Clients run on physical cores 2–5.
- memtier: 1,000,000 keys (`key:1` … `key:1000000`), 100-byte values, uniform random access,
  `memtier_benchmark --threads 4 --clients 20 --distinct-client-seed --randomize`,
  **3 rounds of 10 s per configuration, median reported** (min–max for throughput).
- Server CPU is the user + kernel time of the server process (`/proc/<pid>/stat`) during the
  run. On loopback, part of the network work is billed to the client side, so a server never
  shows a full core.
- Reproduce with [`scripts/bench_vs_valkey.sh`](scripts/bench_vs_valkey.sh) (about 3 minutes).

### Throughput and latency (1:4 SET:GET)

| | Throughput (min–max) | p50 | p99 | p99.9 | p99.99 | Server CPU | Ops / CPU-second |
|---|---|---|---|---|---|---|---|
| **No pipeline**, RadixOx | **238,673** (236,636–263,541) | **0.343 ms** | **0.383 ms** | **0.439 ms** | **0.671 ms** | 0.62 core | **388,719** |
| No pipeline, Valkey | 209,106 (208,654–209,180) | 0.375 ms | 0.743 ms | 0.759 ms | 0.807 ms | 0.64 core | 324,814 |
| **Pipeline 32**, RadixOx | **2,436,822** (2,431,731–2,439,907) | **0.911 ms** | **1.527 ms** | **1.639 ms** | 2.367 ms | 0.86 core | **2,826,940** |
| Pipeline 32, Valkey | 2,002,807 (1,999,635–2,020,254) | 1.063 ms | 1.943 ms | 2.079 ms | **2.191 ms** | 0.88 core | 2,268,185 |

Without pipelining, both servers are bounded by the TCP stack and the clients' round trips.
The tail is where RadixOx stands out: p99 and p99.9 are about half of Valkey's. RadixOx also
does about 20–25 % more work per second of CPU. With pipeline 32, Valkey's p99.99 is slightly
lower.

### YCSB workload A

1M records (one hash field of 100 B per record), 2M operations (50 % read / 50 % update),
100 client threads, one server thread each, **single run** (2026-10-18):

| | Load | Run | READ avg | READ p50 | READ p99 | READ p99.9 | READ p99.99 | UPDATE p99 | Peak RSS |
|---|---|---|---|---|---|---|---|---|---|
| RadixOx | **108 k ops/s** | **219 k ops/s** | **454 µs** | **450 µs** | **544 µs** | **739 µs** | 4,579 µs | **545 µs** | **331 MB** |
| Valkey | 86 k ops/s | 187 k ops/s | 532 µs | 520 µs | 1,040 µs | 1,117 µs | **3,799 µs** | 1,040 µs | 430 MB |

The p99.99 of a single run of 2M operations rests on about 100 samples per operation type:
treat it as noise-level. On the previous run (5M records, 10M operations, 2026-10-17, busier
machine), RadixOx was ahead on p99.99 too (1,112 µs against 1,826 µs).

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
  the tie on UUIDs.
- **Values**: a string is a separate allocation (100 B → 112 B in mimalloc); with large
  values both servers converge.
- **Small hashes and sets** (up to 8 entries) are one compact block of field/value pointers
  instead of a tree. One-field hashes are much smaller than in Valkey; hashes with several
  short fields are still larger, because each field and value stays a separately allocated
  `SharedByte` (a deliberate trade-off: no copies on `HGET`/`HSET`).
- **Connections**: each connection keeps three 64 KiB buffers while it is active (they grow
  for large commands). With many concurrent clients this adds a few MB.
- Before 2026-10-17, `DBSIZE` built the whole key list to count it, which permanently
  inflated RSS (+16 to +46 MB on 1M keys); it is now an O(1) counter. The figure published
  earlier (253 MB, then 151 MB on 2026-10-17 with 80 connections) included this effect.

What to expect on other key shapes, tree only (in-process, 1M keys, integer values):

| Keys | Before | Now |
|---|---|---|
| sequential ids (`key:N`, `memtier-N`) | 72 B | **16 B** |
| `user:{id}:{email,uuid,name,age}` | 83 B | **37 B** |
| random UUIDs, binary (16 B) | 102 B | **64 B** |
| YCSB (`user<hash>`) | 112 B | **77 B** |
| random UUIDs as text, session tokens | 135 B | **100 B** |
| `user:{n}` + `user:{n}:email` (two keys per id) | 64 B | 68 B |

Dense or structured keys gain the most; random keys fan out only two or three ways at the
deepest levels, so each node holds fewer keys.

### Prefix queries

`KEYS user:*` with 1,000 matching keys among 1,001,000, average of 50 calls, measured
server-side (round trip subtracted):

| RadixOx | Valkey |
|---|---|
| **0.15 ms** | 21.4 ms |

Valkey has to scan the whole keyspace; RadixOx walks only the `user:` subtree. The gap grows
with the size of the database.

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

With a small `COUNT`, Valkey wins: each RadixOx page descends again from the root to the
resume key. Use a large `COUNT`.

### Persistence (1M keys, 100-byte values)

Dump written to tmpfs, same session (2026-10-17):

| | `SAVE` | Dump size | Restart until all keys are back |
|---|---|---|---|
| RadixOx, raw dump | **84 ms** | 105 MB | **~0.17 s** |
| RadixOx, zstd level 3 | **70 ms** | (see note) | **~0.16 s** |
| Valkey (RDB, LZF) | 1,810 ms | 25 MB | ~0.53 s |

memtier fills every value with the same bytes, so compression (zstd for the whole stream,
LZF per value in Valkey) is unrealistically effective on this data; only RadixOx's raw size
is meaningful here. `BGSAVE` runs the same snapshot in slices while the server keeps serving,
without `fork`.

### Engine microbenchmarks (in-process, no network)

1M random keys, one core (2026-10-12, previous 64-byte nodes):

| Operation | Cost |
|---|---|
| `GET` of a random cold key | ~410 ns |
| `MGET` of 64 random keys | **~105 ns per key** (was ~375 before interleaved lookups) |
| `KRANGE` page of 64 entries | **5.2 µs** (~82 ns per entry; 64 separate GETs: 15.4 µs) |
| `KRANGE` page of 1 entry | ~620 ns |

### Caveats

- Loopback only: the client's TCP receive path and netfilter are billed to the same machine.
  Real two-machine numbers are planned.
- Short runs: memtier results are the median of 3 rounds of 10 s, YCSB is a single run.
  Treat the numbers as orders of magnitude, not as precise ratios.
- Valkey can use more cores (`io-threads`); RadixOx is single-threaded by design. The
  comparison is per core.

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

Not yet: list commands, `MULTI`/`EXEC`, `CONFIG`, RESP3, Lua,
replication, cluster.

---

## Persistence

- **`SAVE`** (blocking) and **`BGSAVE`** (incremental, in slices between client requests)
  write `RADIXOX_DIR/dump.rxd`. The dump is reloaded at startup.
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
| `RADIXOX_SCAN_IDLE_SECS` | `60` | idle time after which a `SCAN` cursor can be reclaimed |
| `RADIXOX_SCAN_MAX_CURSORS` | `65536` | active `SCAN` cursors at most; beyond, a new `SCAN` gets an error |
| `RADIXOX_SCAN_MAX_CURSORS_PER_CLIENT` | `128` | active `SCAN` cursors at most per connection (its own idle cursors are reclaimed first) |
| `SQ_POLL` | off | io_uring SQ polling (idle time in ms) |
| `RADIXOX_SQ_POLL_PIN` | unset | CPU of the SQ polling kernel thread (**required** with `SQ_POLL` today) |

### Security

- RadixOx listens on `127.0.0.1` unless `RADIXOX_BIND` says otherwise, and prints a warning if
  it listens elsewhere without a password.
- `RADIXOX_PASSWORD` works like Redis's `requirepass`: before `AUTH` (or `HELLO 2 AUTH …`),
  every command except `AUTH`, `HELLO` and `QUIT` gets `-NOAUTH`. The comparison is in constant
  time. Use a long random password (e.g. `openssl rand -hex 32`).
- No ACLs (one `default` user) and no TLS yet: put a TLS proxy in front for remote clients.

---

## Architecture

```
TCP ──io_uring (monoio)──▶ RESP parser ──▶ batch of commands ──▶ interleaved key lookups
                              (borrows the                        (prefetch, misses overlap)
                               read buffer)                                │
                                                                           ▼
      client ◀── one write per batch ◀── RESP encoder ◀── OxidArt (values by reference)
```

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

- **Custom io_uring event loop** replacing monoio: no futures, no wakers, batches across all
  ready connections, network work delegated to the kernel —
  [`RUNTIME_ARCH.md`](RUNTIME_ARCH.md).
- **Atomic conditional transactions**: `MULTI ATOMIC` where an empty read aborts the whole
  block, `DELPREFIX` (delete a whole namespace in O(prefix length)), durable commits —
  [`TX_DESIGN.md`](TX_DESIGN.md).
- AOF and replication (sharing one stream of effects).
- Lower memory per key on random keys: long suffixes stored inline on several cells (no
  separate allocation), a smaller node class for nodes with two or three entries.
- Client compatibility: `MULTI`/`EXEC`, `CONFIG`, RESP3, lists.

---

## Repository layout

```
radixox/
├── oxidart/        # radix tree engine: TTL, prefix / glob / range queries, snapshots
├── oxidart-legacy/ # previous engine (64-byte nodes), test oracle only, not published
├── valart/         # compact radix trees for Hash and Set values
├── radixox-lib/    # SharedByte, RESP parser and encoder, save barrier
├── radixox/        # server binary (RESP2 over io_uring)
├── RUNTIME_ARCH.md # design of the custom io_uring loop
├── SAVE_DESIGN.md  # design of copy-on-write snapshots
└── TX_DESIGN.md    # design of atomic conditional transactions
```

Tests: `cargo test -p oxidart --features regex`, `cargo test -p radixox-lib`,
`cargo test -p radixox`.

---

## License

MIT

Built with [monoio](https://github.com/bytedance/monoio),
[io-uring](https://github.com/tokio-rs/io-uring),
[mimalloc](https://github.com/microsoft/mimalloc),
[regex-automata](https://github.com/rust-lang/regex) and
[hislab](https://github.com/JoshuaBOUCHAT/hislab). Benchmarked against
[Valkey](https://github.com/valkey-io/valkey).
