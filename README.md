# RadixOx

**A fast Redis-compatible key-value store. Built with Rust, io_uring, and Adaptive Radix Trees.**

RadixOx is a high-performance in-memory key-value store that speaks the Redis protocol. Drop-in replacement for Redis with lower tail latency, built on a single-threaded io_uring architecture.

## Benchmarks

Tested with [memtier_benchmark](https://github.com/RedisLabs/memtier_benchmark) on the same machine, both servers tuned (`1 thread, 500 clients, 50/50 GET/SET, 32B values, 100K key space`):

| Metric | Redis (optimized) | RadixOx | Delta |
|--------|-------------------|---------|-------|
| **Throughput** | 128,534 ops/s | 132,454 ops/s | **+3%** |
| **Avg Latency** | 3.89 ms | 3.80 ms | **-2%** |
| **p99 Latency** | 10.30 ms | 8.03 ms | **-22%** |

```
# RadixOx
Sets        66,227 ops/s    3.80ms avg    8.03ms p99
Gets        66,227 ops/s    3.80ms avg    8.06ms p99
Total      132,454 ops/s    3.80ms avg    8.03ms p99

# Redis (optimized config)
Sets        64,267 ops/s    3.89ms avg   10.24ms p99
Gets        64,267 ops/s    3.89ms avg   10.30ms p99
Total      128,534 ops/s    3.89ms avg   10.30ms p99
```

## Why RadixOx?

- **io_uring** - Zero-copy async I/O via [monoio](https://github.com/bytedance/monoio), not epoll
- **Adaptive Radix Tree** - Cache-friendly O(k) lookups with [OxidArt](https://crates.io/crates/oxidart)
- **Single-threaded** - No locks, no contention, predictable latency
- **Zero-copy parsing** - Direct `Bytes` slices, minimal allocations
- **Redis compatible** - Works with `redis-cli`, any Redis client library

### Prefix queries: where RadixOx really shines

Redis stores keys in a flat hash table. `KEYS user:*` must scan **every key in the database** — O(N) where N is the total number of keys, regardless of how many match.

RadixOx stores keys in an Adaptive Radix Tree, a trie-like structure where keys sharing a common prefix share the same path in the tree. `KEYS user:*` traverses directly to the `user:` subtree and only visits matching keys — **O(k)** where k is the number of results, not the total database size.

```
# 1M keys total, 1000 start with "user:"
# Redis:    KEYS user:*  →  scans 1,000,000 keys   O(N)
# RadixOx:  KEYS user:*  →  visits 1,000 keys       O(k)
```

This makes RadixOx a natural fit for workloads with hierarchical keys (`user:123:session`, `config:app:feature`, `cache:region:item`) where prefix operations are frequent.

## Quick Start

```bash
# Build and run
cargo run --bin radixox-resp --features resp --release

# Test with redis-cli
redis-cli -p 6379 PING        # PONG
redis-cli -p 6379 SET foo bar # OK
redis-cli -p 6379 GET foo     # "bar"

# Benchmark
redis-benchmark -p 6379 -t SET,GET -n 100000 -q
memtier_benchmark -p 6379 --protocol=redis -t 4 -c 50
```

## Supported Commands

Full Redis RESP2 protocol support:

| Category | Commands |
|----------|----------|
| **Connection** | `PING` `QUIT` `ECHO` `SELECT` |
| **Strings** | `GET` `SET` `SETNX` `SETEX` `MGET` `MSET` |
| **Keys** | `DEL` `EXISTS` `TYPE` `KEYS` `DBSIZE` `FLUSHDB` |
| **Expiration** | `TTL` `PTTL` `EXPIRE` `PEXPIRE` `PERSIST` |

### SET Options

```bash
SET key value EX 60      # Expire in 60 seconds
SET key value PX 5000    # Expire in 5000 milliseconds
SET key value NX         # Only set if not exists
SET key value XX         # Only set if exists
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         radixox-resp                                │
│                                                                     │
│   ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐  │
│   │   monoio     │    │    RESP2     │    │      OxidArt         │  │
│   │  (io_uring)  │───▶│   Parser     │───▶│  (Adaptive Radix     │  │
│   │              │    │  zero-copy   │    │   Tree + TTL)        │  │
│   └──────────────┘    └──────────────┘    └──────────────────────┘  │
│                                                                     │
│   io_buf ──▶ read_buf ──▶ Frame ──▶ OxidArt ──▶ write_buf ──▶ TCP   │
└─────────────────────────────────────────────────────────────────────┘
```

**Data Flow:**
1. `io_buf` - Kernel I/O buffer (io_uring ownership transfer)
2. `read_buf` - Accumulates data for parsing
3. `decode_bytes_mut()` - Zero-copy RESP parsing into `Frame`
4. `OxidArt` - Execute command on Adaptive Radix Tree
5. `write_buf` - Encode response, reused per connection

## Native Rust Client

For maximum performance, use the native monoio client (protobuf protocol on port 8379):

```rust
use radixox::{ArtClient, monoio_client::monoio_art::SharedMonoIOClient};
use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};

#[monoio::main]
async fn main() {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8379));
    let client = SharedMonoIOClient::new(addr).await.unwrap();

    // Basic operations
    client.set("user:1", "Alice").await.unwrap();
    let val = client.get("user:1").await.unwrap();

    // Prefix operations (native, not available in Redis)
    client.set("session:abc", "data1").await.unwrap();
    client.set("session:xyz", "data2").await.unwrap();

    let sessions = client.getn("session").await.unwrap(); // Get all session:*
    client.deln("session").await.unwrap();                // Delete all session:*

    // JSON serialization
    client.set_json("config", &my_struct).await.unwrap();
    let config: MyStruct = client.get_json("config").await.unwrap().unwrap();
}
```

## Server Binaries

| Binary           | Port | Protocol    | Use Case                       |
|------------------|------|-------------|--------------------------------|
| `radixox-resp`   | 6379 | Redis RESP2 | Drop-in Redis replacement      |
| `radixox-legacy` | 8379 | Protobuf    | Native Rust client, prefix ops |

```bash
# Redis-compatible server
cargo run --bin radixox-resp --features resp --release

# Native protobuf server
cargo run --bin radixox-legacy --features legacy --release
```

## Building

```bash
# Build everything
cargo build --workspace --release

# Build specific server
cargo build -p radixox-server --features resp --release

# Run tests
./radixox-server/test_resp.sh

# Generate docs
cargo doc --workspace --no-deps --open
```

## Requirements

- **Linux 5.1+** (io_uring support)
- **Rust 2024 edition**

## Workspace Structure

```
radixox/
├── radixox-server/     # Server binaries (RESP + legacy)
├── radixox-common/     # Shared types, protobuf definitions
├── radixox/            # Native Rust client libraries
└── Cargo.toml          # Workspace manifest
```

## Roadmap

- [ ] `SCAN` - Cursor-based iteration
- [ ] `INCR/DECR` - Atomic counters
- [ ] Pub/Sub
- [ ] Persistence (RDB/AOF)
- [ ] Cluster mode

## License

MIT
