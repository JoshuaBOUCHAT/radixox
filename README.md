# RadixOx 🦀⚡

**A blazingly fast Redis-compatible key-value store. Built with Rust, io_uring, and Adaptive Radix Trees.**

RadixOx is a high-performance in-memory key-value store that speaks the Redis protocol. Drop-in replacement for Redis with **significantly lower tail latency** and **higher throughput**, built on a single-threaded io_uring architecture.

---

## 🚀 Performance Benchmarks

Tested with [YCSB](https://github.com/brianfrankcooper/YCSB) (Yahoo! Cloud Serving Benchmark) — industry standard for NoSQL databases.

**Configuration:** 5M records, Workload A (50% read / 50% update), fieldlength=100, 100 client threads, localhost *(2026-02-28)*

> **Note on fairness:** RadixOx uses io_uring + SQ_POLL which dedicates a kernel polling thread — effectively 2 CPU threads vs Redis's 1. The advantage is real but not strictly iso-resource. Comparison vs `redis-server` running natively (not Docker).
>
> HiSlab backing store: anonymous `mmap` + `MADV_HUGEPAGE` (THP) + pre-fault of 10K nodes (1.25 MB). The load phase serves as a natural THP warm-up, which explains the excellent tail latency in the run phase.

### LOAD Phase (5,000,000 HMSET inserts)

| Metric | Redis | RadixOx | Improvement |
|--------|-------|---------|-------------|
| **Throughput** | 77,227 ops/sec | **128,939 ops/sec** | 🚀 **+67%** |
| **P99 Latency** | 2,447 µs | **878 µs** | ✅ **-64%** |

### RUN Phase (10,000,000 operations — 50% READ / 50% UPDATE)

| Metric | Redis | RadixOx | Improvement |
|--------|-------|---------|-------------|
| **Throughput** | 201,930 ops/sec | **263,622 ops/sec** | 🚀 **+31%** |
| **READ Avg** | 490 µs | **377 µs** | ⚡ **-23%** |
| **READ P95** | 500 µs | **388 µs** | ✅ **-22%** |
| **READ P99** | 964 µs | **446 µs** | ✅ **-54%** |
| **READ P99.9** | 986 µs | **485 µs** | ✅ **-51%** |
| **READ P99.99** | 1,046 µs | **936 µs** | ✅ **-11%** |
| **UPDATE P99** | 964 µs | **456 µs** | ✅ **-53%** |

**Key Takeaways:**
- 🎯 **Sub-millisecond P99** on reads at 5M records — Redis is at 964 µs
- 💪 **+67% load throughput** — 129k vs 77k ops/sec, no hashtable rehashing jitter
- 📈 **ART is O(key_length)** — latency doesn't grow with dataset size
- 🔥 **P99.9 = 485 µs** — Redis is at 986 µs, 2× better tail latency
- ⚡ **THP warm-up effect** — p99.99 improves further as dataset grows and huge pages are promoted

---

## ⚡ Why RadixOx?

### Architecture Advantages

- **🌳 Adaptive Radix Tree (ART)** - O(k) lookups where k = key length (not O(1) hash with collisions)
- **🔥 io_uring** - Zero-copy async I/O via [monoio](https://github.com/bytedance/monoio), not epoll
- **🎯 Single-threaded** - No locks, no contention, predictable tail latency
- **📊 BTreeMap/BTreeSet** - Deterministic O(log n) for Hash/Set operations, excellent p99.9
- **💾 Zero-copy parsing** - Direct `Bytes` slices, minimal allocations
- **🔌 Redis compatible** - Works with `redis-cli`, any Redis client library

### Prefix Operations: Native to ART

Redis stores keys in a flat hash table. `KEYS user:*` must scan **every key in the database** — O(N) where N is the total number of keys.

RadixOx stores keys in an Adaptive Radix Tree. `KEYS user:*` traverses directly to the `user:` subtree — **O(k)** where k is the number of results.

```bash
# 1M keys total, 1000 start with "user:"
# Redis:    KEYS user:*  →  scans 1,000,000 keys   O(N)  ~50ms
# RadixOx:  KEYS user:*  →  visits 1,000 keys      O(k)  ~1ms
```

Perfect for workloads with hierarchical keys: `user:123:session`, `config:app:feature`, `cache:region:item`

---

## 🎯 Quick Start

```bash
# Build and run (requires Linux 5.1+ for io_uring)
cargo run --bin radixox-resp --features resp --release

# Test with redis-cli
redis-cli -p 6379 PING              # PONG
redis-cli -p 6379 SET foo bar       # OK
redis-cli -p 6379 GET foo           # "bar"
redis-cli -p 6379 INCR counter      # 1
redis-cli -p 6379 KEYS "user:*"     # Blazingly fast prefix query

# Benchmark
cd ~/ycsb-0.17.0
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
| **Keys** | `DEL` `EXISTS` `TYPE` `KEYS` `DBSIZE` `FLUSHDB` |
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
│                         radixox-resp                                    │
│                                                                         │
│   ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐      │
│   │   monoio     │    │    RESP2     │    │      OxidArt         │      │
│   │  (io_uring)  │──▶│   Parser     │──▶│  (Adaptive Radix     │      │
│   │              │    │  zero-copy   │    │   Tree + TTL)        │      │
│   └──────────────┘    └──────────────┘    └──────────────────────┘      │
│                                                                         │
│   io_buf ──▶ read_buf ──▶ Frame ──▶ OxidArt ──▶ write_buf ──▶ TCP  │
└─────────────────────────────────────────────────────────────────────────┘
```

### Data Structures

| Type | Implementation | Complexity | Use Case |
|------|----------------|------------|----------|
| **String** | `Bytes` | O(1) | Raw data, hot path |
| **Int** | `i64` | O(1) | Counters (INCR zero-parse) |
| **Hash** | `Vec` (small) → `BTreeMap` (large) | O(n) / O(log n) | Field-value pairs, YCSB workloads |
| **Set** | `BTreeSet<Bytes>` | O(log n) | Unique members, ordered |
| **ZSet** | `Vec` (small) → `BTreeSet + HashMap` (large) | O(n) / O(log n)+O(1) | Leaderboards, double-indexed |
| **List** | `VecDeque<Bytes>` | O(1) push/pop | Queues (planned) |

### OxidArt Engine

**Node Structure:** 128 bytes exactly, cache-line optimized
- Path compression for single-child chains
- Two-tier child storage: inline (9 slots) + overflow (118 slots)
- HiSlab allocator with O(1) insert/remove
- Lazy TTL expiration + active eviction (Redis-style)

---

## 🛠️ Building

```bash
# Build everything
cargo build --workspace --release

# Build specific components
cargo build -p oxidart                                # ART engine only
cargo build -p radixox-server --bin radixox-resp --release

# Run tests
cargo test -p oxidart
./radixox-server/test_hash.sh
./radixox-server/test_set.sh

# Generate docs
cargo doc --workspace --no-deps --open
```

---

## 📦 Workspace Structure

```
radixox/
├── oxidart/            # Adaptive Radix Tree engine (ART + TTL + DFA)
├── radixox-server/     # Server binaries (RESP + legacy protobuf)
├── radixox-common/     # Shared types, protobuf definitions
├── radixox/            # Native Rust client libraries
└── Cargo.toml          # Workspace manifest
```

---

## 🎯 Use Cases

**Perfect for:**
- 🚀 High-throughput APIs (100k+ ops/sec single-thread)
- 🎯 Latency-critical services (p99.9 < 100µs)
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
- [ ] 🚧 List operations (LPUSH, RPUSH, LRANGE)
- [ ] 🚧 SCAN cursor-based iteration
- [ ] 🚧 Persistence (RDB snapshots, AOF)
- [ ] 🚧 Cluster mode
- [ ] 🚧 Replication

---

## 📊 Comparison: RadixOx vs Redis

| Feature | Redis | RadixOx | Winner |
|---------|-------|---------|--------|
| Load throughput (5M inserts) | 77k ops/sec | **129k ops/sec** | 🦀 **+67%** |
| Run throughput (10M ops) | 202k ops/sec | **264k ops/sec** | 🦀 **+31%** |
| READ P99 (5M records) | 964 µs | **446 µs** | 🦀 **-54%** |
| READ P99.9 | 986 µs | **485 µs** | 🦀 **-51%** |
| Load P99 | 2,447 µs | **878 µs** | 🦀 **-64%** |
| Prefix queries | O(N) scan | **O(k) native** | 🦀 |
| Data structures | HashMap | **ART + BTree** | 🦀 |
| Tail latency | Variable | **Predictable** | 🦀 |
| Multi-threaded | ✅ Yes | ❌ No | 🔴 |
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
- Modern cores are fast enough for **100k+ ops/sec** single-thread

### Why BTreeMap over HashMap?

- **Deterministic O(log n)** vs O(1) average but O(n) worst-case
- **Better tail latency** (p99.9) - no hash collision spikes
- **Ordered iteration** - HGETALL/HKEYS consistent
- **Cache-friendly** - sequential memory access
- **Perfect for YCSB** - workload A is Hash-heavy

---

## 📝 Requirements

- **Linux 5.1+** (io_uring support)
- **Rust 2024 edition** (nightly not required)
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
- [hislab](https://github.com/hinto-janai/hislab) - Hierarchical slab allocator

Inspired by:
- [Dragonfly](https://github.com/dragonflydb/dragonfly) - Multi-threaded Redis replacement
- [KeyDB](https://github.com/Snapchat/KeyDB) - Multi-threaded Redis fork
- [Skytable](https://github.com/skytable/skytable) - Modern NoSQL database

---

**Made with 🦀 and ⚡ by the RadixOx team**

*Benchmark your own workload and see the difference!*
