# RadixOx 🦀⚡

**A blazingly fast Redis-compatible key-value store. Built with Rust, io_uring, and Adaptive Radix Trees.**

RadixOx is a high-performance in-memory key-value store that speaks the Redis protocol. Drop-in replacement for Redis/Valkey with **lower tail latency** and **higher throughput**, built on a single-threaded io_uring architecture.

---

## 🚀 Performance Benchmarks

Tested with [YCSB](https://github.com/brianfrankcooper/YCSB) (Yahoo! Cloud Serving Benchmark) — industry standard for NoSQL databases.

**Configuration:** 5M records, Workload A (50% read / 50% update), fieldlength=100, 100 client threads, localhost *(2026-04-09)*

> **Opponent:** Valkey 9.0.3 — the actively maintained Redis fork, single-threaded event loop, same architecture class as RadixOx.
>
> **Fair comparison:** Both servers run single-threaded on the same machine. SQ_POLL is **disabled** — RadixOx uses standard io_uring without a dedicated kernel polling thread.
>
> **Memory model:** HiSlab backing store uses anonymous `mmap` + `MADV_HUGEPAGE` (THP) + pre-fault of 10K nodes (1.25 MB). The load phase serves as natural THP warm-up.

### LOAD Phase (5,000,000 HMSET inserts)

| Metric | Valkey 9.0.3 | RadixOx | Improvement |
|--------|-------------|---------|-------------|
| **Throughput** | 69,722 ops/sec | **94,480 ops/sec** | 🚀 **+35%** |
| **P99 Latency** | 2,723 µs | **1,313 µs** | ✅ **-52%** |

### RUN Phase (10,000,000 operations — 50% READ / 50% UPDATE)

| Metric | Valkey 9.0.3 | RadixOx | Improvement |
|--------|-------------|---------|-------------|
| **Throughput** | 170,978 ops/sec | **201,191 ops/sec** | 🚀 **+18%** |
| **READ Avg** | 582 µs | **496 µs** | ⚡ **-15%** |
| **READ P95** | 606 µs | **552 µs** | ✅ **-9%** |
| **READ P99** | 1,137 µs | **711 µs** | ✅ **-37%** |
| **READ P99.9** | 1,161 µs | **786 µs** | ✅ **-32%** |
| **READ P99.99** | 1,230 µs | 1,251 µs | — |
| **UPDATE P99** | 1,138 µs | **712 µs** | ✅ **-37%** |
| **Peak RSS** | 2,049 MB | 2,362 MB | — |

**Key Takeaways:**
- 🚀 **+35% load throughput** — 94k vs 70k ops/sec, no hashtable rehashing jitter
- 🎯 **-37% P99 on reads and updates** — Valkey at 1,137 µs, RadixOx at 711 µs
- 💾 **RAM within 15% of Valkey** — thanks to `SharedByte` (custom `!Send` ref-counted buffer, replaces `Arc<Bytes>`)
- 📈 **ART is O(key_length)** — latency doesn't grow with dataset size
- ⚡ **THP warm-up effect** — p99.99 improves further as dataset grows and huge pages are promoted

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
# Build and run (requires Linux 5.1+ for io_uring)
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
┌─────────────────────────────────────────────────────────────────────────┐
│                              radixox                                    │
│                                                                         │
│   ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐      │
│   │   monoio     │    │    RESP2     │    │      OxidArt         │      │
│   │  (io_uring)  │──▶│   Parser     │──▶│  (Adaptive Radix     │      │
│   │              │    │  zero-copy   │    │   Tree + TTL)        │      │
│   └──────────────┘    └──────────────┘    └──────────────────────┘      │
│                                                                         │
│   io_buf ──▶ read_buf ──▶ BytesFrame ──▶ SharedByte args ──▶ OxidArt  │
│                                                    │                    │
│                             write_buf ◀─ SharedFrame ◀─────────────────┘
└─────────────────────────────────────────────────────────────────────────┘
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

**Node Structure:** 128 bytes exactly, cache-line optimized
- Path compression for single-child chains
- Two-tier child storage: inline (9 slots) + overflow (118 slots)
- HiSlab allocator with O(1) insert/remove, `mmap` + THP backing
- Lazy TTL expiration + active eviction (Redis-style)

### SharedByte — Custom Reference-Counted Buffer

`SharedByte` replaces `bytes::Bytes` (`Arc<[u8]>`) for all stored values:

```
Heap layout: [ len: u32 | rc: u16 | data: u8... ]
```

- **8-byte pointer** (niche-optimized: `Option<SharedByte>` = `SharedByte`)
- **`!Send`** — single-threaded by design, no atomic ops
- **u16 refcount** — 65k max clones, no `Arc` overhead
- **Borrow<[u8]>** — zero-cost use in BTreeMap/BTreeSet without conversion

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
- 🚀 High-throughput APIs (200k+ ops/sec single-thread)
- 🎯 Latency-critical services (p99 < 750 µs at 5M records)
- 🌳 Hierarchical key spaces (`user:*`, `cache:*`, prefix operations)
- 📊 Real-time leaderboards (ZSet with O(1) ZSCORE)
- 💾 Session stores, caching layers
- 🔥 YCSB-style workloads (Hash-heavy)

**Not recommended for:**
- ❌ Multi-threaded workloads (single-threaded by design)
- ❌ Persistence-critical (in-memory only, no RDB/AOF yet)
- ❌ Complex transactions (no Lua scripting)

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

## 📊 Comparison: RadixOx vs Valkey 9.0.3

| Feature | Valkey 9.0.3 | RadixOx | Winner |
|---------|-------------|---------|--------|
| Load throughput (5M inserts) | 70k ops/sec | **94k ops/sec** | 🦀 **+35%** |
| Run throughput (10M ops) | 171k ops/sec | **201k ops/sec** | 🦀 **+18%** |
| READ P99 (5M records) | 1,137 µs | **711 µs** | 🦀 **-37%** |
| READ P99.9 | 1,161 µs | **786 µs** | 🦀 **-32%** |
| Load P99 | 2,723 µs | **1,313 µs** | 🦀 **-52%** |
| Prefix queries | O(N) scan | **O(k) native** | 🦀 |
| Peak RSS (5M records) | 2,049 MB | 2,362 MB | 🔴 |
| Data structures | HashMap flat | **ART + BTree** | 🦀 |
| Tail latency | Variable | **Predictable** | 🦀 |
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
- **io_uring batching** → syscall amortization
- Modern cores are fast enough for **200k+ ops/sec** single-thread

### Why BTreeMap over HashMap?

- **Deterministic O(log n)** vs O(1) average but O(n) worst-case
- **Better tail latency** (p99.9) — no hash collision spikes
- **Ordered iteration** — HGETALL/HKEYS consistent
- **Cache-friendly** — sequential memory access
- **Perfect for YCSB** — Workload A is Hash-heavy

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
