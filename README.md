# RadixOx

**High-performance in-memory key-value store built with Rust and io_uring.**

RadixOx is a Redis-like data store optimized for modern Linux systems, leveraging io_uring for minimal syscall overhead and maximum throughput.

## Features

- **Blazing fast** - io_uring based networking with monoio runtime
- **Adaptive Radix Tree** - Memory-efficient storage via [OxidART](https://github.com/your-repo/oxidart)
- **Prefix operations** - Native support for `getn`/`deln` pattern queries
- **Simple protocol** - Protobuf-based wire format
- **Ergonomic client** - Flexible API accepting `&str`, `String`, `Bytes`, etc.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      radixox (client)                       │
│  ┌─────────────────┐  ┌─────────────────┐                   │
│  │  monoio_client  │  │  tokio_client   │  (future)         │
│  │  (io_uring)     │  │  (epoll bridge) │                   │
│  └────────┬────────┘  └─────────────────┘                   │
└───────────┼─────────────────────────────────────────────────┘
            │ TCP + Protobuf
            ▼
┌─────────────────────────────────────────────────────────────┐
│                   radixox-server                            │
│  ┌─────────────────────────────────────────────────────┐    │
│  │                    OxidART                          │    │
│  │              (Adaptive Radix Tree)                  │    │
│  └─────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
            │
            ▼
┌─────────────────────────────────────────────────────────────┐
│                   radixox-common                            │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐       │
│  │   Protocol   │  │   Network    │  │   Protobuf   │       │
│  │  (Commands)  │  │  (Encoding)  │  │  (Messages)  │       │
│  └──────────────┘  └──────────────┘  └──────────────┘       │
└─────────────────────────────────────────────────────────────┘
```

## Quick Start

### Server

```bash
cargo run -p radixox-server --release
# Listening on 0.0.0.0:8379
```

### Client

```rust
use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
use radixox::{ArtClient, monoio_client::monoio_art::SharedMonoIOClient};

#[monoio::main]
async fn main() {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8379));
    let client = SharedMonoIOClient::new(addr).await.unwrap();

    // SET / GET / DEL
    client.set("user:1", "Alice").await.unwrap();
    let val = client.get("user:1").await.unwrap();
    println!("Got: {:?}", val); // Some(b"Alice")

    // Prefix operations
    client.set("session:abc", "data1").await.unwrap();
    client.set("session:xyz", "data2").await.unwrap();

    let sessions = client.getn("session").await.unwrap();
    println!("Found {} sessions", sessions.len()); // 2

    client.deln("session").await.unwrap(); // Delete all sessions
}
```

### JSON Serialization

```rust
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
struct User {
    id: u64,
    name: String,
}

let user = User { id: 1, name: "Alice".into() };
client.set_json("user:1", &user).await?;

let retrieved: User = client.get_json("user:1").await?.unwrap();
```

## Commands

| Command | Description | Example |
|---------|-------------|---------|
| `SET` | Store a value | `set("key", "value")` |
| `GET` | Retrieve a value | `get("key")` |
| `DEL` | Delete a key | `del("key")` |
| `GETN` | Get all values with prefix | `getn("user")` → matches `user:*` |
| `DELN` | Delete all keys with prefix | `deln("session")` → deletes `session:*` |

## Workspace Structure

```
radixox/
├── radixox/           # Client library
│   └── src/
│       ├── lib.rs           # ArtClient trait
│       ├── monoio_client/   # io_uring implementation
│       └── tokio_client/    # Tokio bridge (planned)
│
├── radixox-server/    # Server binary
│   └── src/
│       └── main.rs          # TCP server with OxidART backend
│
├── radixox-common/    # Shared protocol
│   └── src/
│       ├── lib.rs           # Network encoding/validation
│       ├── protocol.rs      # Command types
│       └── proto/
│           └── messages.proto
│
└── Cargo.toml         # Workspace manifest
```

## Performance

RadixOx is designed for high throughput:

- **Request batching** - Client buffers requests and flushes every 1ms
- **Response batching** - Server processes multiple commands per read
- **Zero-copy parsing** - Protobuf with `Bytes` for minimal allocations
- **io_uring** - Kernel-level async I/O on Linux 5.1+

## Requirements

- **Rust** 2024 edition (nightly)
- **Linux** 5.1+ (for io_uring)
- **Dependencies**: monoio, prost, bytes, serde

## Building

```bash
# Build all
cargo build --workspace --release

# Run tests (requires server running)
cargo run -p radixox-server --release &
cargo test -p radixox --release

# Generate docs
cargo doc --workspace --no-deps --open
```

## License

MIT
