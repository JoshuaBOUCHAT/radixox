# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

RadixOx is a high-performance key-value store built on OxidArt (Adaptive Radix Tree). It provides two server implementations and client libraries.

## Workspace Structure

```
radixox/
├── radixox-server/     # Server binaries (legacy + RESP)
├── radixox-common/     # Shared types, protobuf definitions
├── radixox/            # Client libraries (monoio + tokio)
└── Cargo.toml          # Workspace config
```

## Build and Run Commands

```bash
# Build everything
cargo build

# Build specific binaries
cargo build -p radixox-server --features legacy   # Legacy protobuf server
cargo build -p radixox-server --features resp     # Redis RESP server

# Run servers
cargo run --bin radixox-legacy --features legacy  # Port 8379
cargo run --bin radixox-resp --features resp      # Port 6379

# Test with redis-cli (RESP server)
redis-cli -p 6379 PING
redis-cli -p 6379 SET foo bar
redis-cli -p 6379 GET foo
```

## Server Binaries

### radixox-resp (Redis Protocol)

**Port:** 6379 (standard Redis port)

**Supported Commands:**
| Command | Description |
|---------|-------------|
| `PING` | Returns PONG |
| `QUIT` | Close connection |
| `ECHO message` | Echo message back |
| `SELECT db` | Accept & ignore (single-db) |
| `GET key` | Get value |
| `SET key value [EX s] [PX ms] [NX\|XX]` | Set with optional TTL and condition |
| `SETNX key value` | SET if not exists (returns 0/1) |
| `SETEX key seconds value` | SET with TTL in seconds |
| `MGET key [key ...]` | Get multiple keys |
| `MSET key value [key value ...]` | Set multiple key-value pairs |
| `DEL key [key ...]` | Delete keys |
| `EXISTS key [key ...]` | Check existence (returns count) |
| `TYPE key` | Returns "string" or "none" |
| `KEYS pattern*` | Get keys by prefix |
| `TTL key` | Get remaining TTL in seconds (-1 = no TTL, -2 = not found) |
| `PTTL key` | Get remaining TTL in milliseconds |
| `EXPIRE key seconds` | Set TTL on existing key |
| `PEXPIRE key ms` | Set TTL in milliseconds |
| `PERSIST key` | Remove TTL |
| `DBSIZE` | Return number of keys |
| `FLUSHDB` | Delete all keys |

**Architecture (src/bin/resp.rs):**
- monoio async runtime (io_uring on Linux)
- Zero-copy parsing with `redis-protocol` crate (BytesFrame)
- Buffer reuse for reads/writes
- Single-threaded, connection-per-task model

### radixox-legacy (Protobuf Protocol)

**Port:** 8379

**Protocol:** Length-prefixed protobuf messages (see radixox-common)

## Performance Optimizations (RESP Server)

1. **Buffer Management:**
   - `io_buf`: Dedicated buffer for io_uring reads (ownership transferred to kernel)
   - `read_buf`: Parsing buffer, accumulates data
   - `write_buf`: Response buffer, reused after each write

2. **Zero-Copy Parsing:**
   - `decode_bytes_mut()` returns `BytesFrame` with `Bytes` slices into read_buf
   - No allocation for command arguments

3. **Static Responses:**
   - `PONG`, `OK` are `Bytes::from_static()` - no allocation

4. **Case-Insensitive Matching:**
   - `eq_ignore_ascii_case()` instead of `to_ascii_uppercase()` - no allocation

## Data Flow (RESP)

```
Client ──TCP──> io_buf ──extend──> read_buf
                                      │
                     decode_bytes_mut │ (zero-copy)
                                      ▼
                                   Frame (Bytes refs)
                                      │
                          execute_command │
                                      ▼
                                  OxidArt
                                      │
                                      ▼
                               Response Frame
                                      │
                           extend_encode │
                                      ▼
                                 write_buf ──TCP──> Client
```

## Dependencies

- **oxidart**: Adaptive Radix Tree with TTL support (local path: `../oxidart`)
- **monoio**: Async runtime with io_uring
- **redis-protocol**: RESP2/RESP3 parser (features: std, resp2, resp3, bytes)
- **bytes**: Zero-copy byte buffers
- **prost**: Protobuf (legacy protocol only)

## Feature Flags (radixox-server)

- `legacy`: Enable legacy protobuf server (requires prost, radixox-common)
- `resp`: Enable Redis RESP server (requires redis-protocol)
- Default: both enabled

## Testing

```bash
# Unit tests
cargo test -p radixox-server

# Integration test script (requires server running on :6379)
./radixox-server/test_resp.sh

# Manual test with redis-cli
redis-cli -p 6379 SET mykey myvalue EX 60
redis-cli -p 6379 TTL mykey
redis-cli -p 6379 KEYS "my*"

# Benchmark with redis-benchmark
redis-benchmark -p 6379 -t SET,GET -n 100000 -q
```

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
