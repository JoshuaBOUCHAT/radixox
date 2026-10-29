# RadixOx

**An in-memory key-value store that speaks the Redis protocol, with its keys kept in
order.** Rust, an adaptive radix tree instead of a hash table, a hand-written io_uring
loop, one thread per dataset.

Because the keys are sorted, prefix and range queries are cheap: `SCAN MATCH user:*` only
walks the `user:` subtree, `KRANGE` returns the next N keys of a prefix, and
`UNLINK user:*` drops a whole namespace in one command. `redis-cli` and the usual Redis
client libraries work for the [supported commands](#commands).

## At a glance

Against Valkey 9.1.2 on the same cores, 1M keys of 100 bytes, loopback, 2026-10-29
([setup](#benchmarks)):

| | RadixOx | Valkey 9.1.2 |
|---|---|---|
| 1 core, no pipeline | 312 k ops/s, p99 0.39 ms | 191 k ops/s, p99 0.54 ms |
| 1 core, pipeline 32 | 3.08 M ops/s, p99 0.90 ms | 2.15 M ops/s, p99 1.66 ms |
| 3 cores, no pipeline | 607 k ops/s, p99 0.18 ms | 421 k ops/s, p99 0.27 ms |
| RSS with 1M keys | 132 MB | 165 MB |
| `SCAN MATCH key:1234*` over 1M keys | 4 ms | 121 ms |
| ZSet of 1M members: mixed ops, pipeline 32 / memory | 1.37 M ops/s / 66 MB | 0.67 M ops/s / 69 MB |
| `BRPOP`: items handed to blocked consumers | 133 k/s, `LPUSH` p99 0.26 ms | 76 k/s, `LPUSH` p99 0.44 ms |
| `BGSAVE` under saturating load: duration / p99.99 | 1.42 s / 1.21 ms (same core) | 2.01 s / 0.88 ms (child on a 2nd core) |
| Restart with 1M keys | 0.17 s | 0.54 s |
| AOF `everysec`, `SET` pipeline 16 | 1.67 M ops/s | 0.66 M ops/s |
| AOF `always`, `SET` without pipeline | 31 k ops/s, p99.9 52 ms | 51 k ops/s, p99.9 6.0 ms |
| `maxmemory 200mb`, once full: keys kept / process RSS | 1.26 M / 160 MB | 1.47 M / 224 MB |
| LRU hit rate, Zipf, same RSS (42 MB) | 71.2 % | 71.9 % |
| Full sync of a replica, 1M keys | 0.25 s | 2.5 s |

Each line has its details and caveats in [Benchmarks](#benchmarks). The weak points
measured here are listed as clearly as the strong ones and are being worked on, starting
with `appendfsync always`.

## Status

Ready for caches, session stores and prefix or pagination indexes. **Not yet for primary
data that must survive anything**:

- no `MULTI`/`EXEC`, no Lua, no cluster, no TLS, no ACLs (one password), RESP2 only;
- replication is asynchronous, the failover is manual;
- Linux ≥ 6.12 only, with io_uring allowed (see [Deployment](#deployment)).

## Quick start

```bash
git clone https://github.com/JoshuaBOUCHAT/radixox && cd radixox
cargo build --release -p radixox                 # target/release/radixox
./target/release/radixox                         # 127.0.0.1:6379

redis-cli SET user:10:msg:1 hello
redis-cli SET user:10:msg:2 world
redis-cli KRANGE user:10:msg: "" 10              # both, in order
redis-cli UNLINK 'user:10:*'                     # the whole namespace
```

- `--features eviction` adds the LRU and LFU policies (one extra write per read, so the
  default build leaves them out).
- `--target x86_64-unknown-linux-musl` gives a ~2.6 MB static binary that runs on any
  Linux ≥ 6.12.

## What Redis doesn't have

- **`KRANGE prefix from count` / `KREVRANGE`**: the next `count` keys under `prefix` after
  the key `from` (exclusive, `""` = from the start), with their values. Pagination of feeds,
  logs or threads with no sorted set to maintain and no server-side state.
- **Prefix-pruned `SCAN` and `KEYS`**: only the subtree of the pattern's literal prefix is
  walked (`user:*:email` → `user:`).
- **A strict `SCAN`**: no duplicates, an exact `COUNT` (1,000 by default), and every key
  present during the whole scan is returned exactly once, even while the data changes.
- **`UNLINK prefix*`**: the keys disappear at once, and the memory is freed in the
  background.
- **Snapshots without `fork`**: a copy-on-write barrier inside the process, with no second
  process and no doubled pages.
- **A strict `maxmemory`**: the limit covers the whole process (network buffers, AOF,
  replication backlog, allocator), and an `-OOM` comes before the memory is used, not after.
- **Binary-safe ordered keys**, compared like `memcmp`.

## Benchmarks

Ryzen 5 7600X (6 cores, 12 threads), 30 GB, Linux 7.2.11, power profile `performance`,
loopback, RadixOx at `e1fff74`, Valkey 9.1.2. **One server at a time**, its main thread on
CPU 0. Valkey's background threads and `BGSAVE` child run on CPU 1, a core of their own.
memtier 2.4.2 on 6 other CPUs, 60 connections, `--distinct-client-seed --randomize`, 100-byte
values, 8 s per point, **one run per point**. These are orders of magnitude: repeated runs
move by a few percent. Dumps and AOF go to a SATA SSD (PNY CS900, ext4) with fsync.
Reproduce with `DIR=<directory on disk> scripts/readme_bench.sh`, which takes about 10 minutes.

### Throughput and latency

1M keys loaded beforehand, uniform random access, 1:4 SET:GET.

| | ops/s | p50 | p99 | p99.9 | p99.99 |
|---|---|---|---|---|---|
| RadixOx, 1 core, no pipeline | **311,910** | **0.22 ms** | **0.39 ms** | **0.46 ms** | **0.54 ms** |
| Valkey, 1 core, no pipeline | 190,864 | 0.27 ms | 0.54 ms | 0.57 ms | 0.69 ms |
| RadixOx, 1 core, pipeline 32 | **3,078,471** | **0.70 ms** | **0.90 ms** | **1.05 ms** | **1.23 ms** |
| Valkey, 1 core, pipeline 32 | 2,145,401 | 0.85 ms | 1.66 ms | 2.00 ms | 2.69 ms |
| RadixOx, 3 cores, no pipeline | **606,656** | **0.11 ms** | **0.18 ms** | **0.25 ms** | **0.94 ms** |
| Valkey, 3 cores (`io-threads 3`), no pipeline | 421,040 | 0.14 ms | 0.27 ms | 0.38 ms | 1.85 ms |

On 3 cores, the data stays on one thread. Its sends are handed to two kernel io_uring
workers pinned to the other two cores (`RADIXOX_SEND_ASYNC=1 RADIXOX_IOWQ_WORKERS=2
RADIXOX_IOWQ_CPUS=1,2`). That is how RadixOx uses more cores without pipelining, with no
locks and no shared state.

The pipelined gain comes from interleaved lookups. Inside a window of 64 commands, the
descents of all keys advance level by level, so their cache misses overlap.

### Data types

Same setup, one core, 60 connections. All four types are loaded first, one after the
other, and "Memory" is the RSS growth during each load. The throughput runs come after
all the loads, since they grow the Set and the ZSet (random members).

| Workload | Memory | ops/s, no pipeline | p99 | ops/s, pipeline 32 | p99 |
|---|---|---|---|---|---|
| **Hash**: 1M keys × 1 field of 100 B, `HSET`:`HGET` 1:4 | | | | | |
| RadixOx | **151 MB** | **238,326** | **0.33 ms** | **2,365,092** | **1.34 ms** |
| Valkey | 312 MB | 190,594 | 0.59 ms | 1,112,789 | 2.48 ms |
| **Set**: one Set of 1M members, `SADD`:`SISMEMBER` 1:4 (half misses) | | | | | |
| RadixOx | **28 MB** | **240,743** | **0.33 ms** | **2,335,964** | **1.42 ms** |
| Valkey | 30 MB | 214,244 | 0.54 ms | 1,479,781 | 1.93 ms |
| **Sorted set**: one ZSet of 1M members, `ZADD` / `ZSCORE` / `ZRANK` / `ZRANGEBYSCORE … LIMIT 0 10`, 1/2/1/1 | | | | | |
| RadixOx | **66 MB** | **226,894** | **0.35 ms** | **1,373,224** | **2.27 ms** |
| Valkey | 69 MB | 163,496 | 0.62 ms | 667,246 | 4.64 ms |
| **List**: 1,000 lists × 1,000 items of 100 B, `LPUSH` / `RPOP` / `LRANGE 0 9`, 2/2/1 | | | | | |
| RadixOx | 112 MB | **237,477** | **0.34 ms** | **1,711,844** | **1.68 ms** |
| Valkey | **107 MB** | 177,937 | 0.57 ms | 1,274,327 | 2.90 ms |

- One-field hashes take half of Valkey's memory.
- A large ZSet costs about the same as in Valkey (66 B against 69 B per member with these
  short numeric members). Ranks and ranges run on a counted B+tree, twice Valkey's
  throughput under pipelining.
- Lists take 5 % more memory than Valkey's quicklists.

**Blocking commands.** 60 connections wait in `BRPOP` on one shared queue, and 20 producer
connections `LPUSH` 100-byte items into it as fast as they can (no pipeline). Every item
pushed was consumed by the end (queue empty for both servers).

| | Items handed over per second | `LPUSH` p50 | p99 | p99.9 |
|---|---|---|---|---|
| RadixOx | **133,397** | **0.14 ms** | **0.26 ms** | 0.93 ms |
| Valkey | 75,549 | 0.23 ms | 0.44 ms | **0.47 ms** |

When a client is waiting, RadixOx's `LPUSH` hands it the item directly, and the item is
never stored in the list.

### Prefix scan

`redis-cli --scan --pattern 'key:1234*' --count 1000` over the same 1M keys, 111 matches,
wall time including the client: **RadixOx 4 ms, Valkey 121 ms**. Valkey scans the whole
keyspace and filters it. RadixOx walks the `key:1234` subtree only, so the gap grows with
the size of the database.

### Snapshots and restart

`BGSAVE` 3 s into a saturating run (1 core, no pipeline, 1:4 SET:GET), raw dump on the SSD.
RadixOx writes its files from an io_uring worker on CPU 1 (`RADIXOX_FILE_CPUS=1`), the same
core Valkey's child uses.

| | ops/s, no save | ops/s, during the save | p99.99, during the save | `BGSAVE` | Restart with 1M keys |
|---|---|---|---|---|---|
| RadixOx | **320,906** | **274,249** | 1.21 ms | **1.42 s** | **170 ms** (100 MB dump) |
| Valkey | 191,771 | 211,883 | **0.88 ms** | 2.01 s | 543 ms (23 MB dump) |

RadixOx saves from its single serving thread, in slices. Under saturation it gets at least
10 % of the thread (`RADIXOX_SAVE_CPU_SHARE`) and every idle moment, so the save costs
throughput (−15 %) and some tail latency. Valkey's forked child works on its own core, and
the CPU and copy-on-write memory it uses are not in the table. During the save, RadixOx's
RSS rose from 132 to 161 MB. Its dump is larger here because this run turned compression
off (`RADIXOX_SAVE_ZSTD=0`; zstd level 3 is the default).

### Append-only file

`SET` only, 100 bytes, AOF on the SSD. The log's writes and fsyncs run on CPU 1 for both
servers.

| | pipeline 1 | p99.9 | pipeline 16 | p99.99 |
|---|---|---|---|---|
| RadixOx, no AOF | 242,209 | 0.40 ms | 1,973,813 | 0.86 ms |
| RadixOx, `everysec` | **247,193** | **0.57 ms** | **1,667,585** | **80 ms** |
| Valkey, `everysec` | 172,685 | 6.72 ms | 663,070 | 193 ms |
| RadixOx, `always` | 31,237 | 52 ms | **355,074** | 72 ms |
| Valkey, `always` | **51,242** | **6.0 ms** | 227,872 | **64 ms** |

- With `everysec`, RadixOx loses nothing without pipelining and keeps 84 % of its pipelined
  throughput, 2.5× Valkey's.
- With `always`, every reply waits for the fsync that covers it (group commit). Without
  pipelining, **Valkey is ahead (+64 %) and has a much lower tail**. RadixOx is ahead from
  pipeline 16 on.
- Both have tail spikes of tens of milliseconds under a pipelined write flood. That is the
  SSD's fsync, not the event loop.

### `maxmemory`

3M keys of 100 bytes written with pipeline 64 into a server limited to `maxmemory 200mb`:

| | Keys kept | Process RSS once full | Next `SET` |
|---|---|---|---|
| RadixOx, `noeviction` | 1,261,227 | **160 MB** | `-OOM` |
| Valkey, `noeviction` | **1,472,451** | 224 MB | `-OOM` |
| RadixOx, `allkeys-lru` | 1,174,302 | **162 MB** | `OK` |
| Valkey, `allkeys-lru` | **1,468,766** | 225 MB | `OK` |

**The two limits do not mean the same thing.** RadixOx's covers the whole process: buffers,
AOF, backlog, allocator and its reserves. It keeps 10 % for input buffers and 10 % for output
buffers, and checks the cost of a write before running it. With 200 MB set, the process
stays at 160 MB. Valkey counts its dataset, and the process ends 12 % above the figure set.
Valkey keeps more keys, and RadixOx stores more keys per MB of RSS (7,900 against 6,600).

### Eviction (LRU / LFU)

A cache under pressure: Zipf 0.99 accesses over 3M keys, 1:4 SET:GET, no pipeline, 6 s of
warm-up and then 8 s of measurement. The limits were chosen so that **both processes end at
the same RSS, about 42 MB** (RadixOx `maxmemory 48mb`, Valkey `24mb`).

| | GET hit rate | ops/s | p99 | Keys held |
|---|---|---|---|---|
| RadixOx `allkeys-random` | 67.1 % | 276,765 | 0.33 ms | 114 k |
| RadixOx `allkeys-lru` (CLOCK) | 71.2 % | **269,528** | **0.34 ms** | 110 k |
| RadixOx `allkeys-lfu` (Morris counter) | 70.3 % | 241,825 | 0.34 ms | 92 k |
| Valkey `allkeys-lru` | **71.9 %** | 246,259 | 0.53 ms | **169 k** |

- LRU and LFU beat random eviction by 3–4 points.
- At the same footprint, the hit rate is within a point of Valkey's, with a lower p99.
- With so small a dataset, RadixOx's fixed reserves (I/O budgets, compaction reserve) make
  it hold fewer keys than Valkey. With 1M keys it is the other way round (see
  [Throughput and latency](#throughput-and-latency)).
- `volatile-*` policies and `volatile-ttl` exist as well. The random and TTL policies are in
  every build, LRU and LFU need `--features eviction`.

### Replication

Primary on CPU 0, replica on CPU 1, 1M keys, then a load on the primary:

| | Full sync of 1M keys | pipeline 1 | pipeline 32 |
|---|---|---|---|
| RadixOx | **248 ms** | **299,813 ops/s** (−4 %) | **2,558,777 ops/s** (−17 %) |
| Valkey | 2,535 ms | 182,713 ops/s (−5 %) | 1,743,568 ops/s (−19 %) |

The percentages compare with the same server without a replica. After the load, the
content digests (`DEBUG DIGEST`) of the primary and the replica were identical for both
servers. Valkey ran with `repl-diskless-sync-delay 0`, so its 2.5 s does not include its
default 5 s wait.

### Caveats

- Loopback only: the clients' TCP stack runs on the same machine. Two-machine figures are
  still to be done.
- One run per point. The numbers are measured, but a few percent between two lines is noise.
- Values from memtier repeat from key to key, which favours compression (Valkey's 23 MB
  dump).

## Commands

| Category | Commands |
|---|---|
| Strings, counters | `GET` `SET` (`EX` `PX` `NX` `XX`) `SETNX` `SETEX` `MGET` `MSET` `INCR` `DECR` `INCRBY` `DECRBY` |
| Keys, TTL | `DEL` `UNLINK` (`prefix*`) `EXISTS` `TYPE` `KEYS` `SCAN` (`MATCH` `COUNT` `TYPE`) `EXPIRE` `PEXPIRE` `PERSIST` `TTL` `PTTL` |
| Ordered keys | `KRANGE` `KREVRANGE` |
| Hash | `HSET` `HMSET` `HGET` `HMGET` `HGETALL` `HDEL` `HEXISTS` `HLEN` `HKEYS` `HVALS` `HINCRBY` |
| Set | `SADD` `SREM` `SISMEMBER` `SCARD` `SMEMBERS` `SPOP` |
| Sorted set | `ZADD` (`NX` `XX` `GT` `LT` `CH` `INCR`) `ZINCRBY` `ZREM` `ZSCORE` `ZMSCORE` `ZCARD` `ZCOUNT` `ZLEXCOUNT` `ZRANGE` (`BYSCORE` `BYLEX` `REV` `LIMIT` `WITHSCORES`) `ZREVRANGE` `ZRANGEBYSCORE` `ZREVRANGEBYSCORE` `ZRANGEBYLEX` `ZREVRANGEBYLEX` `ZRANK` `ZREVRANK` `ZPOPMIN` `ZPOPMAX` `ZREMRANGEBYRANK` `ZREMRANGEBYSCORE` `ZREMRANGEBYLEX` `ZRANDMEMBER` `ZSCAN` `ZUNION` `ZINTER` `ZDIFF` `ZUNIONSTORE` `ZINTERSTORE` `ZDIFFSTORE` `ZINTERCARD` `BZPOPMIN` `BZPOPMAX` |
| List | `LPUSH` `RPUSH` `LPUSHX` `RPUSHX` `LPOP` `RPOP` `LLEN` `LRANGE` `LINDEX` `LSET` `LINSERT` `LREM` `LTRIM` `LPOS` `LMOVE` `RPOPLPUSH` `BLPOP` `BRPOP` `BLMOVE` `BRPOPLPUSH` |
| Pub/Sub | `SUBSCRIBE` `UNSUBSCRIBE` `PUBLISH` |
| Persistence | `SAVE` `BGSAVE` (`RESERVE`) `BGREWRITEAOF` `LASTSAVE` |
| Replication | `REPLICATE` `REPLICAOF NO ONE` `WAIT` `DEBUG DIGEST` |
| Server | `PING` `ECHO` `AUTH` `HELLO` `CLIENT` `COMMAND` `CONFIG` (`maxmemory`, policy, save reserve) `INFO` `DBSIZE` `FLUSHDB` `SELECT 0` `SHUTDOWN` `QUIT` |

`KEYS`, `UNLINK prefix*`, `FLUSHDB` and large deletions run in slices between other
commands, so no command blocks the server. Inline commands (telnet, `nc`) work.

## Persistence

- **Snapshots**: `BGSAVE` writes `RADIXOX_DIR/dump.rxd`, compressed with zstd. The file
  ends with a CRC and a content digest, and a dump that does not reload into exactly the
  saved state is refused at startup. Automatic rules use Valkey's syntax and defaults
  (`RADIXOX_SAVE="3600 1 300 100 60 10000"`), with a save on SIGTERM and `-MISCONF` after
  a failed save.
- **AOF** (`RADIXOX_APPENDONLY=yes`): commands are logged in blocks, one per loop turn,
  each with the clock of its turn, so TTLs replay identically. `RADIXOX_APPENDFSYNC` takes
  `everysec` (default), `always` (replies wait for their fsync, group commit), `no`, or
  `continuous` (back-to-back write and fsync cycles, replies don't wait: a few ms lost on a
  power cut instead of a second). The log is rewritten in the background, and a block cut by
  a crash is truncated at load.

## Replication

The primary pushes: `REPLICATE <host> <port> [AUTH <pass>]` on the primary (or
`RADIXOX_REPLICATE=host:port,…`). The replica is an ordinary RadixOx server that becomes
read-only (`-READONLY`) and replays the AOF's blocks byte for byte. After a short
disconnection it resumes from the backlog (`RADIXOX_REPL_BACKLOG_MB`, 64 MB). Otherwise it
gets a full sync from a snapshot cut at an exact point of the stream. A replica can have
its own AOF and can feed further replicas. `WAIT n timeout` works as in Valkey. Failover is
manual: `REPLICAOF NO ONE` on the chosen replica, then `REPLICATE` from it.

## Configuration

Environment variables (`CONFIG SET` for `maxmemory`, `maxmemory-policy` and
`save-reserve`):

| Variable | Default | |
|---|---|---|
| `RADIXOX_BIND` / `RADIXOX_PORT` | `127.0.0.1` / `6379` | listening address |
| `RADIXOX_PASSWORD` | unset | `requirepass` |
| `RADIXOX_DIR` | `.` | dumps, `appendonlydir/`, received full syncs |
| `RADIXOX_CPU_PIN` | unset | CPU of the serving thread |
| `RADIXOX_MAXMEMORY` / `_POLICY` | `0` / `noeviction` | limit of the whole process; `allkeys-` / `volatile-` `random` `lru` `lfu`, `volatile-ttl` |
| `RADIXOX_SAVE` | `3600 1 300 100 60 10000` | automatic `BGSAVE` rules, `""` = off |
| `RADIXOX_SAVE_ZSTD` | `3` | dump compression level, `0` = raw |
| `RADIXOX_SAVE_RESERVE` | `0` | memory kept free under `maxmemory` for a save's copies |
| `RADIXOX_APPENDONLY` / `RADIXOX_APPENDFSYNC` | `no` / `everysec` | AOF |
| `RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE` / `_MIN_SIZE` | `100` / `64mb` | log rewrite trigger |
| `RADIXOX_REPLICATE` / `RADIXOX_REPLICATE_AUTH` | unset | replicas to push to |
| `RADIXOX_REPL_BACKLOG_MB` | `64` | backlog for partial resyncs |
| `RADIXOX_SEND_ASYNC` | off | sends on kernel io-wq workers: more throughput without pipelining when cores are free |
| `RADIXOX_IOWQ_WORKERS` / `RADIXOX_IOWQ_CPUS` | `1` / unset | how many io-wq workers, and on which CPUs |
| `RADIXOX_FILE_CPUS` | unset | dump and AOF writes on io-wq workers pinned there |
| `RADIXOX_SQPOLL_CPU` | unset | kernel SQPOLL thread on that CPU (burns it; helps pipelined loads) |
| `RADIXOX_TIMEOUT` | `0` | close idle clients after N seconds |
| `RADIXOX_CLIENT_OUTPUT_LIMIT_MB` | `1024` | disconnect a client with more pending output |
| `RADIXOX_INPUT_BUDGET_MB` / `RADIXOX_OUTPUT_BUDGET_MB` | `1024` (10 % of `maxmemory` each when set) | global buffer budgets, enforced with backpressure |
| `RADIXOX_GC` | `1` | compaction of the memory pool and node slab |

The remaining tuning knobs (`RADIXOX_AMAC`, `RADIXOX_PREFETCH`, `RADIXOX_BATCH_CMDS`,
`RADIXOX_RING_BUFS`, `RADIXOX_SCAN_*`, `RADIXOX_AOF_*`, `RADIXOX_REPL_*_LAG_MB`, …) are
described in [`RUNTIME_ARCH.md`](RUNTIME_ARCH.md), [`MEMORY_ARCH.md`](MEMORY_ARCH.md) and
[`DURABILITY_DESIGN.md`](DURABILITY_DESIGN.md).

## Deployment

- **Linux 6.12+** with io_uring. There is no epoll fallback: the server says so at startup
  if io_uring is unavailable. `kernel.io_uring_disabled` must be `0`.
- **Containers**: the default seccomp profiles of Docker and containerd block io_uring.
  Allow `io_uring_setup`, `io_uring_enter` and `io_uring_register`.
- Listens on `127.0.0.1` by default, and warns if it is exposed without a password. No TLS:
  put a TLS proxy in front.
- `panic = "abort"` in release: client input is bounds-checked so that it can never cause
  a panic.

## Inside

- **OxidArt** (`oxidart/`): the radix tree of keys. 128-byte nodes with inline leaves (a
  key without descendants costs a few bytes in its parent, not a node), rank addressing with
  bitmaps, path compression, a canonical shape after deletions. It is also usable as an
  embedded Rust library (`OxidArt::set`, `krange`, `mget_owned`, snapshots).
- **Values**: strings in reference-counted buffers. Small hashes and sets are one compact
  block, large ones a compact radix tree (`valart/`). Lists are chained blocks. Sorted sets
  are a columnar block, or a hashed trie plus a counted B+tree when large.
- **Memory**: RadixOx's own allocator is the process's global allocator. Its pools are
  compacted while the server is idle, and freed memory goes back to the system. `INFO`
  accounts for every resident byte.
- **Event loop**: io_uring with multishot receives into a shared buffer ring, no async
  runtime, and a core free of I/O, tested against a simulated kernel.
- **Tests**: differential fuzzing against the previous engine, snapshots under mutation,
  model tests against `BTreeMap`, and server integration tests over TCP.

Design documents: [`ARCHITECTURE.md`](ARCHITECTURE.md) (overview and invariants, in French),
[`RUNTIME_ARCH.md`](RUNTIME_ARCH.md), [`SAVE_DESIGN.md`](SAVE_DESIGN.md),
[`DURABILITY_DESIGN.md`](DURABILITY_DESIGN.md), [`MEMORY_ARCH.md`](MEMORY_ARCH.md),
[`SCAN_DESIGN.md`](SCAN_DESIGN.md).

## License

MIT
