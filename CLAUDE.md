# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

RadixOx is a high-performance in-memory key-value store built on OxidArt (Adaptive Radix Tree). It speaks the Redis RESP2 protocol (drop-in replacement) and also has a native protobuf protocol with a Rust client library. Requires **Linux 5.1+** (io_uring) and **Rust 2024 edition**.

## Workspace Structure

```
radixox/
├── oxidart/            # Adaptive Radix Tree engine (ART with TTL, DFA, counters)
├── radixox-server/     # Server binaries (RESP + legacy protobuf)
├── radixox-common/     # Shared types, protobuf definitions, build.rs codegen
├── radixox/            # Native client library (monoio; tokio planned)
└── Cargo.toml          # Workspace config
```

## Build and Run Commands

```bash
# Build everything
cargo build

# Build specific crate
cargo build -p oxidart                                # ART engine only
cargo build -p radixox-server --features resp         # Redis RESP server only
cargo build -p radixox-server --features legacy       # Legacy protobuf server only

# Run servers
cargo run --bin radixox-resp --features resp          # Port 6379
cargo run --bin radixox-legacy --features legacy      # Port 8379

# Tests
cargo test -p oxidart                                 # ART engine tests
cargo test -p oxidart -- regex                        # Regex tests only
cargo test -p oxidart -- counter                      # Counter tests only
cargo test -p radixox-server                          # Server tests

# Integration tests (requires RESP server running on :6379)
./radixox-server/test_resp.sh

# Benchmark
redis-benchmark -p 6379 -t SET,GET -n 100000 -q
```

## Feature Flags

### oxidart
- `ttl` (default): Time-to-live support (activates `hislab/tagged`, `hislab/rand`, `rand`)
- `monoio`: Async integration for monoio (single-thread, io_uring) — implies `ttl`
- `tokio`: Async integration for tokio (multi-thread) — implies `ttl`
- `regex`: DFA-based pattern matching via `regex-automata` — implies `ttl`
- `monoio` and `tokio` are mutually exclusive (compile_error!)

### radixox-server
- `legacy`: Protobuf server binary (pulls in prost, radixox-common)
- `resp`: Redis RESP server binary (pulls in redis-protocol)
- Default: both enabled

## Architecture

### OxidArt — Adaptive Radix Tree (`oxidart/src/`)

O(k) key-value operations where k is key length.

**Core structure (`lib.rs`)**:
- `OxidArt` struct with `HiSlab<Node>` (hierarchical bitmap slab, O(1) insert/remove)
- Separate `HiSlab<HugeChilds>` for overflow child pointers
- With TTL: `now: u64` timestamp for expiration checks

**Node structure** (`repr(C)`, **128 bytes** with TTL — fits allocator bucket exactly):
- With TTL: `childs: Childs`, `compression: CompactStr` (16 bytes, inline ≤14), `val: Option<(Value, u64)>`, `huge_childs_idx: u32`, `parent_idx: u32`, `parent_radix: u8`
- Without TTL: `compression: CompactStr`, `val: Option<Value>`, `childs: Childs`

**Two-tier child storage (`node_childs.rs`)**:
- `Childs`: Inline, up to 9 children (64-byte aligned)
- `HugeChilds`: Overflow for remaining 118 radix values

**Key algorithms**:
- **Path compression**: Single-child paths collapse into compression vector
- **Auto-recompression**: After deletions, absorbs single-child nodes
- **Prefix operations**: `getn`/`deln` traverse to prefix then collect/delete descendants
- **DFA pattern matching** (`regex.rs`): `getn_regex` compiles regex → DFA, walks tree iteratively. Dead DFA state → prune subtree. O(m) where m = visited subtree.
- **Counter operations** (`counter.rs`): `incr`/`decr`/`incrby`/`decrby` via `traverse_to_key` + `node_value_mut` — single traversal, TTL preserved, expired = non-existent
- **Lazy TTL**: Expired entries filtered on access
- **Active eviction**: Redis-style probabilistic sampling via `evict_expired()` — sample 20 tagged nodes, delete expired, repeat if ≥25% expired

**Internal helpers (`pub(crate)`)**:
- `traverse_to_key(&self, key) -> Option<u32>`: Navigate to key, returns node index
- `node_value_mut(&mut self, idx) -> Option<&mut Bytes>`: Mutable ref to value, checks TTL internally. `NO_EXPIRY` sentinel stays private.

**Public API**:

| Method | Description |
|--------|-------------|
| `new()` | Create empty tree |
| `get(key)` / `set(key, val)` / `del(key)` | Basic CRUD |
| `set_ttl(key, duration, val)` | Insert with TTL |
| `getn(prefix)` / `deln(prefix)` | Prefix operations |
| `get_ttl(key)` / `expire(key, dur)` / `persist(key)` | TTL management |
| `incr(key)` / `decr(key)` / `incrby(key, n)` / `decrby(key, n)` | Atomic counters |
| `getn_regex(pattern)` | DFA-pruned regex scan |
| `set_now(ts)` / `tick()` / `evict_expired()` | Clock & eviction |
| `shared_with_ticker(interval)` | Shared tree + auto-ticker (monoio/tokio) |
| `shared_with_evictor(tick, evict)` | Shared tree + ticker + evictor |

### RESP Server (`radixox-server/src/bin/resp.rs`)

Single-threaded, async, connection-per-task model on monoio (io_uring):

```
Client ──TCP──> io_buf ──extend──> read_buf ──decode_bytes_mut──> Frame
                                                                    │
                                                        execute_command
                                                                    ▼
                                                                OxidArt
                                                                    │
                                                          Response Frame
                                                                    │
                                                         extend_encode
                                                                    ▼
                                                              write_buf ──TCP──> Client
```

**Buffer ownership model** (monoio-specific): `io_buf` ownership is transferred to the kernel for io_uring reads, then returned. Data is copied into `read_buf` for parsing. `write_buf` is reused per connection after each TCP write.

**Zero-copy optimizations**: `decode_bytes_mut()` returns `BytesFrame` with `Bytes` slices directly into `read_buf` (no allocation for command arguments). Static responses (`PONG`, `OK`) use `Bytes::from_static()`. Command matching uses `eq_ignore_ascii_case()` to avoid uppercase allocation.

**Shared state**: OxidArt tree is wrapped in `Rc<RefCell<>>` (single-threaded, no locks needed).

**Supported RESP commands**: PING, QUIT, ECHO, SELECT, GET, SET (with EX/PX/NX/XX), SETNX, SETEX, MGET, MSET, DEL, EXISTS, TYPE, KEYS, TTL, PTTL, EXPIRE, PEXPIRE, PERSIST, DBSIZE, FLUSHDB, INCR, DECR, INCRBY, DECRBY, SUBSCRIBE, UNSUBSCRIBE, PUBLISH.

**KEYS glob pattern matching**: Two code paths:
- **Fast path**: Simple `prefix*` patterns → `art.getn(prefix)` (pure tree traversal, no DFA)
- **Slow path**: Complex globs → `glob_to_regex()` → `art.getn_regex()` (DFA-pruned traversal)

**Pub/Sub architecture**:
- Writer task pattern: first SUBSCRIBE spawns a monoio writer task owning the write half
- Channel: `local_sync::mpsc::unbounded` — sync `send()`, async `recv()`, `Tx` is Clone
- Registry: `HashMap<Bytes, HashMap<ConnId, Tx<Bytes>>>` in `Rc<RefCell<>>`
- PUBLISH encodes RESP once → N `Bytes::clone()` (refcount bump) → N `send()`
- `Conn` struct groups connection state + `send()` / `handle_cmd()` methods

**Performance optimizations**: `SmallVec<[Bytes; 3]>` for `frame_to_args`, GET/SET first in dispatch table, SET condition check skipped when no NX/XX flag.

### Legacy Protobuf Server (`radixox-server/src/bin/legacy.rs`)

Port 8379. Length-prefixed protobuf messages (4-byte big-endian size + payload). Supports SET, GET, DEL, GETN, DELN (prefix operations with `*` wildcard). Uses batch message parsing via `read_message_batch()`.

### Common Library (`radixox-common/`)

- `build.rs` compiles `src/proto/messages.proto` with prost-build (auto-generates Rust types)
- `NetValidate<T>` / `NetEncode<T>` traits for network message handling
- Protocol types: `NetCommand` (request) and `NetResponse` (response) with oneof actions

### Client Library (`radixox/`)

`ArtClient` trait defines async operations: `get`, `set`, `del`, `getn`, `deln`, `get_json<T>`, `set_json<T>`.

`SharedMonoIOClient` implementation:
- Split read/write loop architecture on monoio runtime
- **Write batching**: accumulates commands in a `BytesMut` buffer, flushes every 1ms
- **Request tracking**: `SlotMap<DefaultKey, Sender<Response>>` maps request IDs to oneshot channels

## Key Dependencies

- **oxidart** (workspace member): Adaptive Radix Tree with TTL, DFA pattern matching, counters
- **monoio**: Async runtime with io_uring (Linux-only)
- **redis-protocol**: RESP2/RESP3 parser with `Bytes` integration
- **prost / prost-build**: Protobuf codegen (legacy protocol)
- **slotmap**: Request tracking in client
- **local-sync**: Thread-local channels for Pub/Sub and client request/response
- **hislab**: Hierarchical bitmap slab allocator with tagged random sampling

## TODO / Future Work

### Data Structures (next milestone)
- [ ] Replace `Bytes` with `Value` enum in oxidart (not generic — specific to radixox)
- [ ] `Value::None` / `String(Bytes)` / `Int(i64)` / `Hash(Box<Vec<(Bytes, Bytes)>>)` / `List(Box<VecDeque<Bytes>>)` / `Set(Box<HashSet<Bytes>>)` / `ZSet(Box<ZSetInner>)`
- [ ] String ↔ Int transparent conversion (INCR parses once → Int, GET formats on the fly)
- [ ] Box all variants except String/Int/None to keep enum at 32 bytes
- [ ] WRONGTYPE error on type mismatch (except String/Int same family)
- [ ] Hash commands: HSET, HGET, HGETALL, HDEL, HEXISTS, HLEN, HKEYS, HVALS, HMGET, HINCRBY
- [ ] List commands: LPUSH, RPUSH, LPOP, RPOP, LRANGE, LLEN
- [ ] Set commands: SADD, SREM, SMEMBERS, SISMEMBER, SINTER, SUNION, SDIFF, SCARD
- [ ] ZSet commands: ZADD, ZRANGE, ZRANGEBYSCORE, ZRANK, ZSCORE, ZREM, ZCARD, ZINCRBY

### RESP Commands (not yet implemented)
- [ ] `SCAN cursor [MATCH pattern] [COUNT count]` - Cursor-based iteration
- [ ] `APPEND key value` - Append to string
- [ ] `STRLEN key` - Get string length
- [ ] `GETRANGE key start end` - Get substring
- [ ] `RENAME key newkey` - Rename key

### Future Features
- [ ] Cluster mode
- [ ] Persistence (RDB/AOF)
