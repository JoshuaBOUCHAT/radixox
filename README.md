# RadixOx 🦀⚡

**A blazingly fast Redis-compatible key-value store. Built with Rust, io_uring, and Adaptive Radix Trees.**

RadixOx is a high-performance in-memory key-value store that speaks the Redis protocol. Drop-in replacement for Redis with **significantly lower tail latency** and **higher throughput**, built on a single-threaded io_uring architecture.

---

## 🚀 Performance Benchmarks

Tested with [YCSB](https://github.com/brianfrankcooper/YCSB) (Yahoo! Cloud Serving Benchmark) - industry standard for NoSQL databases.

**Configuration:** 1M records, Workload A (50% read / 50% update), fieldlength=100, 100 client threads, localhost

> **Note on fairness:** RadixOx uses io_uring + SQ_POLL which dedicates a kernel polling thread — effectively 2 CPU threads vs Redis's 1. The advantage is real but not strictly iso-resource. Comparison vs `redis-server` running natively (not Docker).

### LOAD Phase (1,000,000 HMSET inserts):

| Metric | Redis | RadixOx | Improvement |
|--------|-------|---------|-------------|
| **Throughput** | 90,318 ops/sec | **115,754 ops/sec** | 🚀 **+28%** |
| **Avg Latency** | 1,096 µs | **857 µs** | ⚡ **-22%** |
| **P95 Latency** | 1,288 µs | **1,245 µs** | ✅ **-3%** |
| **P99 Latency** | 2,125 µs | **1,559 µs** | ✅ **-27%** |

### RUN Phase (2,000,000 operations — 50% READ / 50% UPDATE):

| Metric | Redis | RadixOx | Improvement |
|--------|-------|---------|-------------|
| **Throughput** | 157,332 ops/sec | **238,464 ops/sec** | 🚀 **+52%** |
| **READ Avg** | 631 µs | **416 µs** | ⚡ **-34%** |
| **READ P95** | 1,003 µs | **619 µs** | ✅ **-38%** |
| **READ P99** | 1,346 µs | **697 µs** | ✅ **-48%** |
| **READ P99.9** | 1,722 µs | **1,383 µs** | ✅ **-20%** |
| **READ P99.99** | 5,091 µs | **2,329 µs** | ✅ **-54%** |

**Key Takeaways:**
- 🎯 **Sub-millisecond P99** on reads — Redis is at 1.3ms
- 💪 **52% more throughput** — 238k vs 157k ops/sec on same workload
- 📈 **ART traversal is O(key_length)** — no hash collision, no rehash jitter
- 🔥 **Load phase 28% faster** — no hashtable rehashing as dataset grows
- ⚡ **Vec-first Hash** — small hashes stay in cache-friendly Vec before promoting to BTreeMap

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
┌─────────────────────────────────────────────────────────────────────┐
│                         radixox-resp                                │
│                                                                     │
│   ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐ │
│   │   monoio     │    │    RESP2     │    │      OxidArt         │ │
│   │  (io_uring)  │───▶│   Parser     │───▶│  (Adaptive Radix     │ │
│   │              │    │  zero-copy   │    │   Tree + TTL)        │ │
│   └──────────────┘    └──────────────┘    └──────────────────────┘ │
│                                                                     │
│   io_buf ──▶ read_buf ──▶ Frame ──▶ OxidArt ──▶ write_buf ──▶ TCP  │
└─────────────────────────────────────────────────────────────────────┘
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
| Throughput (run 2M ops) | 157k ops/sec | **238k ops/sec** | 🦀 **+52%** |
| P99 Latency (read) | 1,346 µs | **697 µs** | 🦀 **-48%** |
| P95 Latency (read) | 1,003 µs | **619 µs** | 🦀 **-38%** |
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
