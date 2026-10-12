# RadixOx

**A Redis-compatible in-memory key-value store built on an Adaptive Radix Tree, in Rust, on io_uring.**

RadixOx speaks RESP2, so `redis-cli` and existing Redis client libraries work with it. Its
storage engine, **OxidArt**, keeps keys in an ordered radix tree instead of a hash table. That
gives it lower tail latency than Valkey on a single core, and it makes ordered and prefix
operations native: sorted pagination (`KRANGE`), prefix scans that only touch matching keys,
and binary-safe keys.

OxidArt is also usable directly as an embedded Rust library.

---

## Highlights

Measured on 2026-10-12, one core each, 1M keys (see [Benchmarks](#benchmarks) for the setup
and caveats):

| | RadixOx | Valkey 9.1.2 | |
|---|---|---|---|
| GET p99, no pipeline (80 conns) | **0.44 ms** | 0.75 ms | −42 % |
| GET p99.9, no pipeline | **0.52 ms** | 0.84 ms | −38 % |
| Throughput, no pipeline | **224 k ops/s** | 202 k ops/s | +11 % |
| Throughput, pipeline 32 | **2.22 M ops/s** | 1.96 M ops/s | +13 % |
| `KEYS user:*` (1,000 matches in 1M keys) | **0.15 ms** | 21.4 ms | ×140 |
| `SAVE` of 1M keys | **71 ms** (raw) / 112 ms (zstd) | 1,989 ms | ×18–28 |
| Restart with 1M keys | **~0.2 s** | ~0.45 s | ×2 |
| Memory per key (100 B values) | 251 B | **152 B** | Valkey −40 % |

Plus what Redis doesn't have:

- **`KRANGE` / `KREVRANGE`**: the next N keys under a prefix, in sorted order, after a cursor
  key. Pagination for message threads, logs or feeds, without maintaining a sorted set.
- **Prefix and glob queries that prune**: `KEYS user:*` walks only the `user:` subtree, and
  complex globs are compiled to a DFA that skips whole subtrees.
- **Binary-safe keys**: any byte 0x00–0xFF at any position, ordered like `memcmp`.
- **Snapshots without `fork`**: an application-level copy-on-write barrier gives a consistent
  point-in-time dump while the server keeps serving.

---

## Quick start

```bash
# hislab (the slab allocator) is currently a path dependency: clone it next to radixox
git clone https://github.com/JoshuaBOUCHAT/hislab
git clone https://github.com/JoshuaBOUCHAT/radixox
cd radixox
cargo build --release --bin radixox
./target/release/radixox                 # listens on 6379

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
  `performance`, loopback.
- **One core each.** The server is pinned to CPU 0 with its SMT sibling idle. RadixOx runs
  without SQ_POLL; Valkey 9.1.2 runs with `io-threads 1`, persistence off. memtier runs on
  physical cores 2–5.
- 1,000,000 keys (`key:1` … `key:1000000`), 100-byte values, uniform random access,
  `memtier_benchmark --threads 4 --clients 20 --distinct-client-seed --randomize`,
  10-second runs, **single run per configuration**.

### Throughput and latency (1:4 SET:GET)

| | Throughput | p50 | p99 | p99.9 |
|---|---|---|---|---|
| **No pipeline**, RadixOx | **224,212 ops/s** | **0.351 ms** | **0.439 ms** | **0.527 ms** |
| No pipeline, Valkey | 202,053 ops/s | 0.375 ms | 0.751 ms | 0.839 ms |
| **Pipeline 32**, RadixOx | **2,216,319 ops/s** | **0.991 ms** | **1.727 ms** | **1.975 ms** |
| Pipeline 32, Valkey | 1,964,902 ops/s | 1.103 ms | 2.079 ms | 2.431 ms |

Without pipelining, both servers are bounded by the TCP stack and the client's round trips:
the kernel takes most of the core. The tail is where RadixOx stands out: p99 and p99.9 are
about 40 % lower.

### Memory

| | RSS after loading 1M keys | Per key |
|---|---|---|
| RadixOx | 253 MB | 251 B |
| Valkey | 168 MB | **152 B** |

This is RadixOx's weak spot today. Each key costs a 64-byte tree node plus a separately
allocated value (mimalloc), and random short keys get little benefit from path compression.
Reducing the footprint (value compaction, leaf/value fusion) is on the roadmap.

### Prefix queries

`KEYS user:*` with 1,000 matching keys among 1,001,000, average of 50 calls, measured
server-side (round trip subtracted):

| RadixOx | Valkey |
|---|---|
| **0.15 ms** | 21.4 ms |

Valkey has to scan the whole keyspace; RadixOx walks only the `user:` subtree. The gap grows
with the size of the database.

### Persistence (1M keys, 100-byte values)

| | `SAVE` | Dump size | Restart until all keys are back |
|---|---|---|---|
| RadixOx, raw dump | **71 ms** | 105 MB | ~0.2 s |
| RadixOx, zstd level 3 | 112 ms | (see note) | ~0.2 s |
| Valkey (RDB, LZF) | 1,989 ms | 113 MB | ~0.45 s |

memtier fills every value with the same bytes, so whole-stream compression (zstd) is
unrealistically effective on this data; only the raw size is meaningful here. `BGSAVE` runs
the same snapshot in slices while the server keeps serving, without `fork`.

### Engine microbenchmarks (in-process, no network)

1M random keys, one core:

| Operation | Cost |
|---|---|
| `GET` of a random cold key | ~410 ns |
| `MGET` of 64 random keys | **~105 ns per key** (was ~375 before interleaved lookups) |
| `KRANGE` page of 64 entries | **5.2 µs** (~82 ns per entry; 64 separate GETs: 15.4 µs) |
| `KRANGE` page of 1 entry | ~620 ns |

### Caveats

- Loopback only: the client's TCP receive path and netfilter are billed to the same machine.
  Real two-machine numbers are planned.
- Single runs, short durations: treat the numbers as orders of magnitude, not as precise
  ratios.
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
| Connection / server | `PING` `ECHO` `QUIT` `SELECT` `INFO` `DBSIZE` `FLUSHDB` `SAVE` `BGSAVE` |
| Strings | `GET` `SET` (`EX` `PX` `NX` `XX`) `SETNX` `SETEX` `MGET` `MSET` |
| Counters | `INCR` `DECR` `INCRBY` `DECRBY` |
| Keys | `DEL` `UNLINK` `EXISTS` `TYPE` `KEYS` |
| Ordered keys | `KRANGE` `KREVRANGE` |
| Expiration | `EXPIRE` `PEXPIRE` `PERSIST` `TTL` `PTTL` |
| Hash | `HSET` `HMSET` `HGET` `HMGET` `HGETALL` `HDEL` `HEXISTS` `HLEN` `HKEYS` `HVALS` `HINCRBY` |
| Set | `SADD` `SREM` `SISMEMBER` `SCARD` `SMEMBERS` `SPOP` |
| Sorted set | `ZADD` `ZREM` `ZSCORE` `ZCARD` `ZRANGE` (`WITHSCORES`) `ZINCRBY` |
| Pub/Sub | `SUBSCRIBE` `UNSUBSCRIBE` `PUBLISH` |

Not yet: list commands, `SCAN`, `MULTI`/`EXEC`, `CLIENT` / `CONFIG` / `HELLO`, Lua,
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
| `RADIXOX_DIR` | `.` | directory of `dump.rxd` |
| `RADIXOX_CPU_PIN` | unset | pin the server thread to this CPU |
| `RADIXOX_PREFETCH` | `8` | pipelined commands whose key lookups are interleaved (`0` = off) |
| `RADIXOX_SAVE_ZSTD` | `3` | `BGSAVE` zstd level (`0` = raw) |
| `RADIXOX_SAVE_SYNC_ZSTD` | raw | zstd level of `SAVE` and of the exit save |
| `RADIXOX_SAVE_BACKLOG_MB` | `64` | compression backlog of `BGSAVE` |
| `RADIXOX_SAVE_ON_EXIT` | off | save on SIGINT / SIGTERM |
| `SQ_POLL` | off | io_uring SQ polling (idle time in ms) |
| `RADIXOX_SQ_POLL_PIN` | unset | CPU of the SQ polling kernel thread (**required** with `SQ_POLL` today) |

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
- **64-byte nodes** in a slab (HiSlab) backed by anonymous `mmap` with transparent huge pages.
  Stable `u32` indices instead of pointers.
- **Path compression** (`CompactPath`: up to 7 bytes inline, longer segments on the heap) and
  automatic re-compression after deletions: the tree always has the same canonical shape for
  the same content.
- **Child lookup without loops**: 6 children inline, 10 more in an overflow slot, searched with
  a single 8-byte load and a bit trick (SWAR). Wider nodes use 8-child groups (up to 112)
  then a direct 256-entry table.
- **Interleaved descents**: when several keys are known at once (pipeline window, `MGET`,
  `EXISTS`), their lookups advance level by level so cache misses overlap.
- **TTL**: expired keys are invisible to reads and removed lazily or by an active sampling
  evictor.

**Values**
- Strings: `SharedByte`, an 8-byte single-threaded reference-counted buffer (no atomics),
  allocated with mimalloc. Integers are stored as `i64` and formatted on read.
- Hash and Set: **valart**, a compact radix tree per container.
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
- Lower memory per key.
- Client compatibility commands (`HELLO`, `CLIENT`, `CONFIG`, `MULTI`/`EXEC`), `SCAN`, lists.

---

## Repository layout

```
radixox/
├── oxidart/        # radix tree engine: TTL, prefix / glob / range queries, snapshots
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
