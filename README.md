# RadixOx 🦀⚡

**A blazingly fast Redis-compatible key-value store. Built with Rust, io_uring, and Adaptive Radix Trees.**

RadixOx is a high-performance in-memory key-value store that speaks the Redis protocol. Drop-in replacement for Redis/Valkey with **lower tail latency** and **higher throughput**, built on a single-threaded io_uring architecture.

---

## 🚀 Performance Benchmarks

### YCSB — Yahoo! Cloud Serving Benchmark

Tested with [YCSB](https://github.com/brianfrankcooper/YCSB) — industry standard for NoSQL databases.

**Configuration:** 5M records, Workload A (50% read / 50% update), fieldlength=100, 100 client threads, localhost *(2026-05-03)*

> **Opponent:** Valkey 9.0.3 running with **2 threads** (its typical production configuration).
>
> **RadixOx configuration:** 1 application thread + io_uring **SQ_POLL** (1 kernel polling thread). Total: 2 threads — same resource envelope as Valkey.
>
> **Memory model:** HiSlab backing store uses anonymous `mmap` + `MADV_HUGEPAGE` (THP) + pre-fault of 10K nodes (1.25 MB). The load phase serves as natural THP warm-up.

#### LOAD Phase (5,000,000 HMSET inserts)

| Metric | Valkey 9.0.3 (2t) | RadixOx (1t+poll) | Improvement |
|--------|-------------------|-------------------|-------------|
| **Throughput** | 86,973 ops/sec | **133,708 ops/sec** | 🚀 **+54%** |
| **P99 Latency** | 1,396 µs | **869 µs** | ✅ **-38%** |

#### RUN Phase (10,000,000 operations — 50% READ / 50% UPDATE)

| Metric | Valkey 9.0.3 (2t) | RadixOx (1t+poll) | Improvement |
|--------|-------------------|-------------------|-------------|
| **Throughput** | 172,491 ops/sec | **271,128 ops/sec** | 🚀 **+57%** |
| **READ Avg** | 578 µs | **368 µs** | ⚡ **-36%** |
| **READ P95** | 629 µs | **376 µs** | ✅ **-40%** |
| **READ P99** | 683 µs | **456 µs** | ✅ **-33%** |
| **READ P99.9** | 697 µs | **427 µs** | ✅ **-39%** |
| **READ P99.99** | 1,014 µs | **982 µs** | ✅ **-3%** |
| **UPDATE P99** | 684 µs | **456 µs** | ✅ **-33%** |
| **Peak RSS** | 2,080 MB | **1,969 MB** | ✅ **-5%** |

**Key Takeaways:**
- 🚀 **+57% run throughput** — 271k vs 172k ops/sec, single ART traversal path, no hashtable rehashing
- 🎯 **-39% P99.9 on reads** — 427 µs vs 697 µs; ART depth is bounded by key length, not dataset size
- 📈 **ART is O(key_length)** — latency stays flat as dataset grows
- 💾 **5% less RAM at 5M records** — `SharedByte` (no atomic ops) + mimalloc + THP coalesce live nodes

---

### memtier_benchmark — Real-World Workloads

**Configuration:** Same conditions — RadixOx (1t + SQ_POLL) vs Valkey (2t). Mixed GET/SET workloads, variable value sizes, Gaussian key distribution. *(2026-05-03)*

#### Load (pure SET, 20s sustained)

| Metric | Valkey (2t) | RadixOx (1t+poll) | Improvement |
|--------|-------------|-------------------|-------------|
| **Throughput** | 203,297 ops/sec | **255,830 ops/sec** | 🚀 **+26%** |

#### Caching Workload (1:10 GET:SET, Gaussian, 100–500 B values)

| Metric | Valkey (2t) | RadixOx (1t+poll) | Improvement |
|--------|-------------|-------------------|-------------|
| **Throughput** | 207,272 ops/sec | **243,804 ops/sec** | 🚀 **+18%** |
| **GET avg latency** | 0.39 ms | **0.33 ms** | ⚡ **-15%** |
| **GET P99** | 0.49 ms | **0.38 ms** | ✅ **-22%** |
| **GET P99.9** | 0.51 ms | **0.47 ms** | ✅ **-8%** |
| **GET P99.99** | **0.57 ms** | 1.67 ms | 🔴 Valkey **-66%** |
| **SET P99** | 0.49 ms | **0.38 ms** | ✅ **-22%** |

#### Session Store (1:1 GET:SET, TTL 10–300s, 50–200 B values)

| Metric | Valkey (2t) | RadixOx (1t+poll) | Improvement |
|--------|-------------|-------------------|-------------|
| **Throughput** | 201,848 ops/sec | **249,495 ops/sec** | 🚀 **+24%** |
| **GET avg latency** | 0.40 ms | **0.32 ms** | ⚡ **-20%** |
| **GET P99** | 0.50 ms | **0.38 ms** | ✅ **-24%** |
| **GET P99.9** | 0.54 ms | **0.39 ms** | ✅ **-28%** |
| **SET P99** | 0.50 ms | **0.38 ms** | ✅ **-24%** |

> **Note on P99.99 and memory:** RadixOx's P99.99 is higher than Valkey on the caching workload (1.67 ms vs 0.57 ms) — occasional GC-like pauses from the THP warm-up path on small datasets. At scale (5M records, YCSB), this effect disappears and P99.99 converges (982 µs vs 1,014 µs). For small hot-cache workloads, Valkey also uses ~12% less RAM (45 MB vs 51 MB) — the ART overhead isn't amortized until the key space grows.

---

## ⚡ Why RadixOx?

### Architecture Advantages

- **🌳 Adaptive Radix Tree (ART)** - O(k) lookups where k = key length (not O(1) hash with collisions)
- **🔥 io_uring** - Async I/O via [monoio](https://github.com/bytedance/monoio), not epoll
- **🎯 Single-threaded** - No locks, no contention, predictable tail latency
- **📊 BTreeMap/BTreeSet** - Deterministic O(log n) for Hash/Set operations, excellent p99.9
- **💾 SharedByte** - `!Send` ref-counted byte buffer: 8-byte pointer, no `Arc` overhead, custom inline header
- **🔌 Redis compatible** - Works with `redis-cli`, any Redis client library

### Prefix Operations: Native to ART

Redis/Valkey stores keys in a flat hash table. `KEYS user:*` must scan **every key in the database** — O(N) where N is the total number of keys.

RadixOx stores keys in an Adaptive Radix Tree. `KEYS user:*` traverses directly to the `user:` subtree — **O(k)** where k is the number of results.

```bash
# 1M keys total, 1000 start with "user:"
# Valkey:   KEYS user:*  →  scans 1,000,000 keys   O(N)  ~50ms
# RadixOx:  KEYS user:*  →  visits 1,000 keys      O(k)  ~1ms
```

Perfect for workloads with hierarchical keys: `user:123:session`, `config:app:feature`, `cache:region:item`

---

## 🎯 Quick Start

```bash
# Install from crates.io (requires Linux 5.1+ for io_uring)
cargo install radixox

# Or build from source
cargo build --bin radixox --release
./target/release/radixox

# Test with redis-cli
redis-cli -p 6379 PING              # PONG
redis-cli -p 6379 SET foo bar       # OK
redis-cli -p 6379 GET foo           # "bar"
redis-cli -p 6379 INCR counter      # 1
redis-cli -p 6379 KEYS "user:*"     # Blazingly fast prefix query

# Benchmark
cd ~/ycsb-redis-binding-0.18.0-SNAPSHOT
bin/ycsb.sh load redis -s -P workloads/workloada -p redis.port=6379
bin/ycsb.sh run redis -s -P workloads/workloada -p redis.port=6379
```

---

## 📚 Supported Commands

Full Redis RESP2 protocol support with all major data structures:

### 🔤 Strings & Keys
| Category | Commands |
|----------|----------|
| **Connection** | `PING` `QUIT` `ECHO` `SELECT` |
| **Strings** | `GET` `SET` `SETNX` `SETEX` `MGET` `MSET` |
| **Counters** | `INCR` `DECR` `INCRBY` `DECRBY` |
| **Keys** | `DEL` `EXISTS` `TYPE` `KEYS` `UNLINK` `DBSIZE` `FLUSHDB` |
| **Expiration** | `TTL` `PTTL` `EXPIRE` `PEXPIRE` `PERSIST` |

### 🗂️ Hash
`HSET` `HMSET` `HGET` `HGETALL` `HDEL` `HEXISTS` `HLEN` `HKEYS` `HVALS` `HMGET` `HINCRBY`

**Vec → BTreeMap adaptive:** small hashes stay in cache-friendly Vec (≤16 fields), promote to BTreeMap for larger sets

### 📦 Set
`SADD` `SREM` `SISMEMBER` `SCARD` `SMEMBERS` `SPOP`

**BTreeSet-based:** Ordered iteration, predictable performance

### 📊 Sorted Set (ZSet)
`ZADD` `ZCARD` `ZRANGE` `ZSCORE` `ZREM` `ZINCRBY`

**Vec → double-index adaptive:** small ZSets stay in sorted Vec (≤16 members), promote to BTreeSet+HashMap with pre-allocated capacity

### 📡 Pub/Sub
`SUBSCRIBE` `UNSUBSCRIBE` `PUBLISH`

**Monoio channels:** Lock-free, single-threaded message passing

---

## 🏗️ Architecture

```
┌───────────────────────────────────────────────────────────────────────────┐
│                              radixox                                      │
│                                                                           │
│   ┌──────────────┐     ┌──────────────┐     ┌──────────────────────┐      │
│   │   monoio     │     │    RESP2     │     │      OxidArt         │      │
│   │  (io_uring)  │──▶ │    Parser    │──▶ │  (Adaptive Radix     │      │
│   │   SQ_POLL    │     │  zero-copy   │     │   Tree + TTL)        │      │
│   └──────────────┘     └──────────────┘     └──────────────────────┘      │
│                                                                           │
│   io_buf ──▶ read_buf ──▶ BytesFrame ──▶ SharedByte args ──▶ OxidArt  │
│                                                                           │ 
│                             write_buf ◀─ SharedFrame ◀─────────────┘    │
└───────────────────────────────────────────────────────────────────────────┘
```

### Data Structures

| Type | Implementation | Complexity | Use Case |
|------|----------------|------------|----------|
| **String** | `SharedByte` | O(1) | Raw data, hot path |
| **Int** | `i64` | O(1) | Counters (INCR zero-parse) |
| **Hash** | `Vec` (small) → `BTreeMap` (large) | O(n) / O(log n) | Field-value pairs, YCSB workloads |
| **Set** | `BTreeSet<SharedByte>` | O(log n) | Unique members, ordered |
| **ZSet** | `Vec` (small) → `BTreeSet + HashMap` (large) | O(n) / O(log n)+O(1) | Leaderboards, double-indexed |
| **List** | `VecDeque<SharedByte>` | O(1) push/pop | Queues (planned) |

### OxidArt Engine

**Node Structure:** 64 bytes, cache-line optimized
- Path compression for single-child chains
- Two-tier child storage: inline (9 slots) + overflow — inline slots cover all ASCII digits, making keys like `user:1234` extremely cache-efficient
- HiSlab allocator with O(1) insert/remove, `mmap` + THP backing
- Lazy TTL expiration + active eviction (Redis-style)

### SharedByte — Custom Reference-Counted Buffer

`SharedByte` replaces `bytes::Bytes` — which relies on atomic reference counting for zero-copy cloning — with a `!Send` equivalent that eliminates all atomic operations:

```
Heap layout: [ len: u32 | rc: u16 | data: u8... ]
```

- **8-byte pointer** (niche-optimized: `Option<SharedByte>` = `SharedByte`)
- **`!Send`** — single-threaded by design, no atomic ops
- **u16 refcount** — 65k max clones, no `Arc` overhead
- **Borrow<[u8]>** — zero-cost use in BTreeMap/BTreeSet without conversion
- **mimalloc** — allocations backed by mimalloc instead of the system allocator, saving ~10% RSS

---

## 🛠️ Building

```bash
# Build everything
cargo build --workspace --release

# Build the server
cargo build --bin radixox --release

# Run tests
cargo test -p oxidart

# Generate docs
cargo doc --workspace --no-deps --open
```

---

## 📦 Workspace Structure

```
radixox/
├── oxidart/        # Adaptive Radix Tree engine (ART + TTL + DFA regex)
├── radixox/        # Server binary (RESP2, io_uring, monoio)
├── radixox-lib/    # Shared types: SharedByte, SharedFrame, RESP2 encoder
└── Cargo.toml      # Workspace manifest
```

---

## 🎯 Use Cases

**Perfect for:**
- 🚀 High-throughput APIs (270k+ ops/sec, 2-thread budget)
- 🎯 Latency-critical services (READ p99 < 500 µs at 5M records)
- 🌳 Hierarchical key spaces (`user:*`, `cache:*`, prefix operations)
- 📊 Real-time leaderboards (ZSet with O(1) ZSCORE)
- 💾 Session stores, caching layers (p99 0.38 ms, p99.9 0.39 ms)
- 🔥 YCSB-style workloads (Hash-heavy, large datasets)

**Not recommended for:**
- ❌ Multi-threaded workloads (single-threaded by design)
- ❌ Persistence-critical (in-memory only, no RDB/AOF yet)
- ❌ Complex transactions (no Lua scripting)
- ⚠️ Tiny hot-cache workloads where p99.99 outliers matter more than throughput

---

## 📊 Comparison: RadixOx vs Valkey 9.0.3

Both configurations: 2 threads total. RadixOx = 1 app thread + io_uring SQ_POLL. Valkey = 2 user-space threads.

| Feature | Valkey 9.0.3 (2t) | RadixOx (1t+poll) | Winner |
|---------|-------------------|-------------------|--------|
| Load throughput (5M inserts) | 87k ops/sec | **134k ops/sec** | 🦀 **+54%** |
| Run throughput (10M ops) | 172k ops/sec | **271k ops/sec** | 🦀 **+57%** |
| READ P99 (5M records) | 683 µs | **456 µs** | 🦀 **-33%** |
| READ P99.9 (5M records) | 697 µs | **427 µs** | 🦀 **-39%** |
| Load P99 | 1,396 µs | **869 µs** | 🦀 **-38%** |
| memtier GET P99 | 0.49 ms | **0.38 ms** | 🦀 **-22%** |
| memtier GET P99.99 | **0.57 ms** | 1.67 ms | 🔴 Valkey |
| Peak RSS (5M records) | 2,080 MB | **1,969 MB** | 🦀 **-5%** |
| Peak RSS (small dataset) | **45 MB** | 51 MB | 🔴 Valkey |
| Prefix queries | O(N) scan | **O(k) native** | 🦀 |
| Data structures | HashMap flat | **ART + BTree** | 🦀 |
| Tail latency (large dataset) | Variable | **Predictable** | 🦀 |
| Multi-threaded | ✅ Optional | ❌ No | 🔴 |
| Persistence | ✅ RDB/AOF | ❌ Not yet | 🔴 |
| Lua scripting | ✅ Yes | ❌ No | 🔴 |
| Ecosystem | 🔴 Massive | 🦀 Growing | 🔴 |

---

## 🔬 Technical Details

### Why Single-Threaded?

- **No lock contention** → predictable tail latency
- **Cache-friendly** → all data in L1/L2 cache
- **Simple to reason about** → no race conditions
- **io_uring batching** → syscall amortization via SQ_POLL
- Modern cores are fast enough for **270k+ ops/sec** with a single event loop

### Why BTreeMap over HashMap?

- **Deterministic O(log n)** vs O(1) average but O(n) worst-case
- **Better tail latency** (p99.9) — no hash collision spikes
- **Ordered iteration** — HGETALL/HKEYS consistent
- **Cache-friendly** — sequential memory access
- **Perfect for YCSB** — Workload A is Hash-heavy

---

## 🗺️ Roadmap

- [x] ✅ String, Int operations (GET, SET, INCR, DECR)
- [x] ✅ TTL support (EXPIRE, PERSIST, TTL, PTTL)
- [x] ✅ Hash (HSET, HGET, HGETALL, HINCRBY, etc.)
- [x] ✅ Set (SADD, SREM, SMEMBERS, SPOP)
- [x] ✅ Sorted Set (ZADD, ZRANGE, ZINCRBY with double-index)
- [x] ✅ Pub/Sub (SUBSCRIBE, PUBLISH)
- [x] ✅ Pattern matching (KEYS with glob/regex DFA)
- [x] ✅ SharedByte — Arc-free single-threaded ref-counting
- [ ] 🚧 List operations (LPUSH, RPUSH, LRANGE, BLPOP)
- [ ] 🚧 SCAN cursor-based iteration
- [ ] 🚧 Persistence (RDB snapshots, AOF)
- [ ] 🚧 Cluster mode
- [ ] 🚧 Replication

---

## 📝 Requirements

- **Linux 5.1+** (io_uring support)
- **Rust 2024 edition** (stable, no nightly required)
- **x86_64 or ARM64**

---

## 📜 License

MIT

---

## 🙏 Acknowledgments

Built with:
- [monoio](https://github.com/bytedance/monoio) - Async io_uring runtime
- [redis-protocol](https://github.com/aembke/redis-protocol.rs) - RESP parser
- [bytes](https://github.com/tokio-rs/bytes) - Zero-copy byte buffers
- [hislab](https://github.com/JoshuaBOUCHAT/hislab) - Hierarchical slab allocator with THP support

Inspired by:
- [Dragonfly](https://github.com/dragonflydb/dragonfly) - Multi-threaded Redis replacement
- [Valkey](https://github.com/valkey-io/valkey) - The Redis fork we benchmark against
- [Skytable](https://github.com/skytable/skytable) - Modern NoSQL database

---

**Made with 🦀 and ⚡**

*Benchmark your own workload and see the difference!*
