# OxidArt

A blazingly fast Adaptive Radix Tree (ART) implementation in Rust with path compression.

[![Crates.io](https://img.shields.io/crates/v/oxidart.svg)](https://crates.io/crates/oxidart)
[![Documentation](https://docs.rs/oxidart/badge.svg)](https://docs.rs/oxidart)
[![License: MPL 2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)

## Features

- **O(k) complexity** - All operations run in O(k) time where k is the key length, not the number of entries
- **Path compression** - Minimizes memory usage by collapsing single-child paths
- **Prefix queries** - `getn` and `deln` for efficient prefix-based operations
- **TTL support** - Built-in time-to-live with lazy expiration
- **Async runtime integration** - First-class support for monoio and tokio
- **Zero-copy values** - Uses `bytes::Bytes` for efficient value handling
- **Memory efficient** - Adaptive node sizing with `SmallVec` and `Slab` allocation

## Installation

```toml
[dependencies]
oxidart = "0.2"
bytes = "1"
```

## Feature Flags

| Feature | Description |
|---------|-------------|
| `ttl` (default) | Enables time-to-live support for entries |
| `monoio` | Async integration for monoio (single-thread, io_uring) |
| `tokio` | Async integration for tokio (multi-thread) |

> Note: `monoio` and `tokio` features are mutually exclusive.

## Quick Start

```rust
use oxidart::OxidArt;
use bytes::Bytes;

let mut tree = OxidArt::new();

// Insert key-value pairs
tree.set(Bytes::from_static(b"hello"), Bytes::from_static(b"world"));
tree.set(Bytes::from_static(b"hello:foo"), Bytes::from_static(b"bar"));

// Retrieve a value
assert_eq!(tree.get(Bytes::from_static(b"hello")), Some(Bytes::from_static(b"world")));

// Get all entries with a prefix
let entries = tree.getn(Bytes::from_static(b"hello"));
assert_eq!(entries.len(), 2);

// Delete a key
tree.del(Bytes::from_static(b"hello"));

// Delete all keys with a prefix
tree.deln(Bytes::from_static(b"hello"));
```

## TTL Support

With the `ttl` feature (enabled by default), you can set expiration times on entries:

```rust
use oxidart::OxidArt;
use bytes::Bytes;
use std::time::Duration;

let mut tree = OxidArt::new();

// Update internal clock (call periodically from your event loop)
tree.set_now(current_timestamp_secs);

// Insert with TTL - expires after 60 seconds
tree.set_ttl(
    Bytes::from_static(b"session:abc"),
    Duration::from_secs(60),
    Bytes::from_static(b"user_data")
);

// Insert without TTL - never expires
tree.set(Bytes::from_static(b"config:key"), Bytes::from_static(b"value"));

// Expired entries are automatically filtered on get/getn
```

## Async Runtime Integration

For TTL support, we recommend using the `shared_with_ticker` constructor which creates a shared tree with automatic timestamp updates.

### With monoio (single-threaded)

```toml
[dependencies]
oxidart = { version = "0.1", features = ["monoio"] }
```

```rust
use oxidart::OxidArt;
use bytes::Bytes;
use std::time::Duration;

#[monoio::main(enable_timer = true)]
async fn main() {
    // Recommended: creates Rc<RefCell<OxidArt>> with automatic ticker
    let tree = OxidArt::shared_with_ticker(Duration::from_millis(100));

    // Use TTL
    tree.borrow_mut().set_ttl(
        Bytes::from_static(b"session"),
        Duration::from_secs(3600),
        Bytes::from_static(b"data"),
    );

    // Your server loop...
}
```

### With tokio (multi-threaded)

```toml
[dependencies]
oxidart = { version = "0.1", features = ["tokio"] }
```

```rust
use oxidart::OxidArt;
use bytes::Bytes;
use std::time::Duration;

#[tokio::main]
async fn main() {
    // Recommended: creates Arc<Mutex<OxidArt>> with automatic ticker
    let tree = OxidArt::shared_with_ticker(Duration::from_millis(100)).await;

    // Use TTL
    tree.lock().await.set_ttl(
        Bytes::from_static(b"session"),
        Duration::from_secs(3600),
        Bytes::from_static(b"data"),
    );

    // Your server loop...
}
```

## API

| Method | Description |
|--------|-------------|
| `new()` | Create a new empty tree |
| `shared_with_ticker(interval)` | Create shared tree with auto-ticker (recommended for TTL) |
| `get(key)` | Get value by exact key |
| `set(key, value)` | Insert or update a key-value pair (no expiration) |
| `set_ttl(key, duration, value)` | Insert with TTL (requires `ttl` feature) |
| `del(key)` | Delete by exact key, returns the old value |
| `getn(prefix)` | Get all entries matching a prefix |
| `deln(prefix)` | Delete all entries matching a prefix |
| `set_now(timestamp)` | Update internal clock for TTL checks |
| `tick()` | Update clock to current time (requires `monoio` or `tokio`) |

## Why ART?

Adaptive Radix Trees combine the efficiency of radix trees with adaptive node sizes:

- Unlike hash maps, ART maintains key ordering and supports efficient range/prefix queries
- Unlike B-trees, ART has O(k) lookup independent of the number of entries
- Path compression eliminates redundant nodes, reducing memory overhead

## License

Licensed under the Mozilla Public License 2.0.
