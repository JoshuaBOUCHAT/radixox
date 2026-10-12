# OxidArt

An in-memory **ordered key-value store engine**: an adaptive radix tree with TTL, prefix /
range / glob queries, Redis-style value types and fork-free snapshots. It is the engine of
[RadixOx](https://github.com/JoshuaBOUCHAT/radixox), a Redis-compatible server, and can be
embedded directly in a Rust application.

[![Crates.io](https://img.shields.io/crates/v/oxidart.svg)](https://crates.io/crates/oxidart)
[![Documentation](https://docs.rs/oxidart/badge.svg)](https://docs.rs/oxidart)

## Features

- **Ordered, binary-safe keys**: any byte 0x00–0xFF, sorted like `memcmp`. Lookups cost
  O(key length), independent of the number of entries.
- **Range and prefix queries**: `krange` (sorted page after a cursor key, ascending or
  descending), `getn` / `deln` (whole prefix), `getn_glob` / `getn_regex` (byte-wise DFA that
  prunes subtrees; feature `regex`).
- **Fast reads**: 64-byte nodes, branch-free child lookup (SWAR), interleaved descents for
  batched lookups (`mget_ref`), values returned by reference (no clone, no refcount traffic).
- **TTL**: lazy expiration on read plus an active sampling evictor.
- **Value types**: strings, integers, hashes, sets, sorted sets.
- **Snapshots without `fork`**: an application-level copy-on-write barrier gives a consistent
  point-in-time dump while the tree keeps changing; bulk reload.

## Installation

```toml
[dependencies]
oxidart = "0.6"
radixox-lib = "0.6"   # SharedByte, the value buffer type
```

Linux only (the optional runtime helpers use monoio / io_uring).

## Quick start

```rust
use oxidart::krange::RangeVal;
use oxidart::{OxidArt, Value};
use radixox_lib::shared_byte::SharedByte;
use std::time::Duration;

let mut art = OxidArt::new();
let s = |v: &str| Value::String(SharedByte::from_str(v));

art.set(SharedByte::from_str("user:10:message:1"), s("hello"));
art.set(SharedByte::from_str("user:10:message:2"), s("world"));
art.set_ttl(SharedByte::from_str("session:abc"), Duration::from_secs(60), s("token"));

// Exact lookup
assert!(art.get(b"user:10:message:1").is_some());

// Sorted page: up to 30 entries under the prefix, after the cursor ("" = from the start)
for (key, val) in art.krange(b"user:10:message:", b"", 30, false).iter() {
    if let RangeVal::Bytes(v) = val {
        println!("{} = {}", key.escape_ascii(), v.escape_ascii());
    }
}

// Batched lookups (descents interleaved), values owned
let vals = art.mget_owned(&[b"user:10:message:1", b"missing"]);
assert!(vals[0].is_some() && vals[1].is_none());

// Every key under a prefix
assert_eq!(art.getn(SharedByte::from_str("user:10:")).len(), 2);
```

## Time and TTL

The tree has its own clock, in **seconds**: call `set_now(unix_seconds)` from your event loop,
and `evict_expired()` periodically to reclaim expired keys that are never read. On monoio,
`OxidArt::shared_with_ticker(interval)` and `shared_with_evictor(tick, evict)` return an
`Rc<RefCell<OxidArt>>` with background tasks doing both.

## Notes

- **Single-threaded by design**: values use a non-atomic reference count (`!Send`). Use one
  tree per thread, or wrap it in your own synchronization.
- References returned by `krange` / `mget_ref` borrow the tree; use `krange_owned` /
  `mget_owned` (converted in place, no extra allocation) to keep results past the borrow.

## License

MIT
