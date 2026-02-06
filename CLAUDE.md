# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

RadixOx is a high-performance in-memory key-value store built on OxidArt (Adaptive Radix Tree). It speaks the Redis RESP2 protocol (drop-in replacement) and also has a native protobuf protocol with a Rust client library. Requires **Linux 5.1+** (io_uring) and **Rust 2024 edition**.

## Workspace Structure

```
radixox/
├── radixox-server/     # Server binaries (RESP + legacy protobuf)
├── radixox-common/     # Shared types, protobuf definitions, build.rs codegen
├── radixox/            # Native client library (monoio; tokio planned)
└── Cargo.toml          # Workspace config
```

## Build and Run Commands

```bash
# Build everything
cargo build

# Build specific server
cargo build -p radixox-server --features resp     # Redis RESP server only
cargo build -p radixox-server --features legacy   # Legacy protobuf server only

# Run servers
cargo run --bin radixox-resp --features resp      # Port 6379
cargo run --bin radixox-legacy --features legacy  # Port 8379

# Unit tests
cargo test -p radixox-server

# Integration tests (requires RESP server running on :6379)
./radixox-server/test_resp.sh

# Benchmark
redis-benchmark -p 6379 -t SET,GET -n 100000 -q
```

## Feature Flags (radixox-server)

- `legacy`: Protobuf server binary (pulls in prost, radixox-common)
- `resp`: Redis RESP server binary (pulls in redis-protocol)
- Default: both enabled

## Architecture

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

**Supported RESP commands**: PING, QUIT, ECHO, SELECT, GET, SET (with EX/PX/NX/XX), SETNX, SETEX, MGET, MSET, DEL, EXISTS, TYPE, KEYS (prefix), TTL, PTTL, EXPIRE, PEXPIRE, PERSIST, DBSIZE, FLUSHDB.

### Legacy Protobuf Server (`radixox-server/src/bin/legacy.rs`)

Port 8379. Length-prefixed protobuf messages (4-byte big-endian size + payload). Supports SET, GET, DEL, GETN, DELN (prefix operations with `*` wildcard). Uses batch message parsing via `read_message_batch()`.

### Common Library (`radixox-common/`)

- `build.rs` compiles `src/proto/messages.proto` with prost-build (auto-generates Rust types)
- `NetValidate<T>` trait: validates network messages into typed command structs
- `NetEncode<T>` trait: encodes responses with 4-byte length prefix into `BytesMut`
- Protocol types: `NetCommand` (request) and `NetResponse` (response) with oneof actions

### Client Library (`radixox/`)

`ArtClient` trait defines async operations: `get`, `set`, `del`, `getn`, `deln`, `get_json<T>`, `set_json<T>`.

`SharedMonoIOClient` implementation:
- Split read/write loop architecture on monoio runtime
- **Write batching**: accumulates commands in a `BytesMut` buffer, flushes every 1ms
- **Request tracking**: `SlotMap<DefaultKey, Sender<Response>>` maps request IDs to oneshot channels
- Wrapped in `Rc<>` for cloning across async tasks (single-threaded)
- Tokio bridge is planned but not yet implemented (`src/tokio_client/mod.rs`)

## Key Dependencies

- **oxidart** (crates.io): Adaptive Radix Tree with TTL support
- **monoio**: Async runtime with io_uring (Linux-only)
- **redis-protocol**: RESP2/RESP3 parser with `Bytes` integration
- **prost / prost-build**: Protobuf codegen (legacy protocol)
- **slotmap**: Request tracking in client (key → response channel)
- **local-sync**: Thread-local oneshot channels for client request/response

## TODO / Future Work

### RESP Commands
- [ ] `SCAN cursor [MATCH pattern] [COUNT count]` - Cursor-based iteration
- [ ] `INCR key` / `DECR key` - Increment/decrement integers
- [ ] `INCRBY key n` / `DECRBY key n` - Increment/decrement by n
- [ ] `APPEND key value` - Append to string
- [ ] `STRLEN key` - Get string length
- [ ] `GETRANGE key start end` - Get substring
- [ ] `RENAME key newkey` - Rename key

### Future Features
- [ ] Pub/Sub support
- [ ] Cluster mode
- [ ] Persistence (RDB/AOF)
