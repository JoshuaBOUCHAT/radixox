#[cfg(not(target_os = "linux"))]
compile_error!("RadixOx requires Linux to run (io_uring and mmap support).");


mod resp_cmd;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use local_sync::mpsc::unbounded;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt, Splitable};
use monoio::net::{TcpListener, TcpStream};
use monoio::time::TimeDriver;
use monoio::{IoUringDriver, Runtime, RuntimeBuilder};
use smallvec::SmallVec;

use oxidart::counter::CounterError;
use oxidart::value::Value;
use oxidart::{OxidArt, TtlResult};
use redis_protocol::resp2::decode::decode_bytes_mut;
use redis_protocol::resp2::encode::extend_encode;
use redis_protocol::resp2::types::BytesFrame as Frame;
use resp_cmd::{
    cmd_hdel, cmd_hexists, cmd_hget, cmd_hgetall, cmd_hincrby, cmd_hkeys, cmd_hlen, cmd_hmget,
    cmd_hmset, cmd_hset, cmd_hvals, cmd_sadd, cmd_scard, cmd_sismember, cmd_smembers, cmd_spop,
    cmd_srem, cmd_zadd, cmd_zcard, cmd_zincrby, cmd_zrange, cmd_zrem, cmd_zscore,
};

type IOResult<T> = std::io::Result<T>;
type SharedART = Rc<RefCell<OxidArt>>;
type ConnId = u64;
type SharedRegistry = Rc<RefCell<HashMap<Bytes, HashMap<ConnId, unbounded::Tx<Bytes>>>>>;

const BUFFER_SIZE: usize = 64 * 1024;

// Static responses (avoid allocation)
static PONG: Bytes = Bytes::from_static(b"PONG");
static OK: Bytes = Bytes::from_static(b"OK");
static ERR_EMPTY_CMD: &str = "ERR empty command";


fn main() -> std::io::Result<()> {
    let mut runtime = get_runtime()?;

    runtime.block_on(async {
        let listener = TcpListener::bind("0.0.0.0:6379")?;
        println!("RadixOx RESP Server listening on 0.0.0.0:6379");

        let shared_art =
            OxidArt::shared_with_evictor(Duration::from_millis(100), Duration::from_secs(1));

        let registry: SharedRegistry = Rc::new(RefCell::new(HashMap::new()));
        let conn_counter: Rc<Cell<ConnId>> = Rc::new(Cell::new(0));

        loop {
            let (stream, _addr) = listener.accept().await?;
            //println!("New connection from {}", addr);
            let conn_id = conn_counter.get();
            conn_counter.set(conn_id.wrapping_add(1));
            monoio::spawn(handle_connection(
                stream,
                shared_art.clone(),
                registry.clone(),
                conn_id,
            ));
        }
    })
}
fn get_runtime() -> std::io::Result<Runtime<TimeDriver<IoUringDriver>>> {
    let mut uring_builder = io_uring::IoUring::builder();

    // 2. Configurer SQPOLL (le kernel poll la queue de soumission)
    // Paramètre : temps d'idle en millisecondes avant que le thread kernel s'endorme
    uring_builder.setup_sqpoll(2);

    // Optionnel : on peut aussi binder le thread SQPOLL sur un cœur spécifique
    //uring_builder.setup_sqpoll_cpu(8);

    RuntimeBuilder::<monoio::IoUringDriver>::new()
        .with_entries(1024)
        .uring_builder(uring_builder) // C'est ici qu'on injecte notre config
        .enable_timer()
        .build()
}

struct Conn {
    write_buf: BytesMut,
    sub_tx: Option<unbounded::Tx<Bytes>>,
    sub_channels: HashSet<Bytes>,
    conn_id: ConnId,
}

impl Conn {
    fn new(conn_id: ConnId) -> Self {
        Self {
            write_buf: BytesMut::with_capacity(BUFFER_SIZE),
            sub_tx: None,
            sub_channels: HashSet::new(),
            conn_id,
        }
    }

    /// Route a frame to the client — direct write (normal) or channel (subscriber).
    #[inline]
    fn send(&mut self, frame: Frame) {
        if let Some(tx) = &self.sub_tx {
            send_via_tx(tx, frame);
        } else {
            extend_encode(&mut self.write_buf, &frame, false).expect("encode should not fail");
        }
    }

    /// Dispatch a non-SUBSCRIBE command.
    fn handle_cmd(
        &mut self,
        cmd: &[u8],
        args: &[Bytes],
        art: &SharedART,
        registry: &SharedRegistry,
    ) {
        if cmd.eq_ignore_ascii_case(b"UNSUBSCRIBE") {
            if let Some(tx) = &self.sub_tx {
                handle_unsubscribe(args, registry, self.conn_id, &mut self.sub_channels, tx);
            }
        } else if cmd.eq_ignore_ascii_case(b"PUBLISH") {
            self.send(cmd_publish(args, registry));
        } else if !self.sub_channels.is_empty() {
            // Subscriber mode: only PING/QUIT allowed
            if cmd.eq_ignore_ascii_case(b"PING") {
                self.send(resp_pong());
            } else if cmd.eq_ignore_ascii_case(b"QUIT") {
                self.send(resp_ok());
            } else {
                self.send(Frame::Error(
                    "ERR only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT / PUBLISH are allowed in this context".into(),
                ));
            }
        } else {
            // Normal mode (or unsubscribed back to normal)
            self.send(dispatch_command(cmd, args, &mut art.borrow_mut()));
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    art: SharedART,
    registry: SharedRegistry,
    conn_id: ConnId,
) -> IOResult<()> {
    let (mut read, write) = stream.into_split();
    let mut read_buf = BytesMut::with_capacity(BUFFER_SIZE);
    let mut io_buf = BytesMut::with_capacity(BUFFER_SIZE);
    let mut write_half = Some(write);
    let mut conn = Conn::new(conn_id);

    loop {
        let (res, returned_buf) = read.read(io_buf).await;
        io_buf = returned_buf;

        let n = match res {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                cleanup_subscriptions(&registry, conn_id, &conn.sub_channels);
                return Err(e);
            }
        };

        read_buf.extend_from_slice(&io_buf[..n]);
        io_buf.clear();

        loop {
            let frame = match decode_bytes_mut(&mut read_buf) {
                Ok(Some((frame, _, _))) => frame,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("Parse error: {:?}", e);
                    conn.send(Frame::Error(format!("ERR parse error: {:?}", e).into()));
                    break;
                }
            };

            let args = match frame_to_args(frame) {
                Some(args) if !args.is_empty() => args,
                _ => {
                    conn.send(Frame::Error(ERR_EMPTY_CMD.into()));
                    continue;
                }
            };

            let cmd = &args[0];

            if cmd.eq_ignore_ascii_case(b"SUBSCRIBE") {
                if conn.sub_tx.is_none() {
                    let (tx, rx) = unbounded::channel::<Bytes>();
                    let mut w = write_half.take().unwrap();
                    if !conn.write_buf.is_empty() {
                        let buf = std::mem::replace(&mut conn.write_buf, BytesMut::new());
                        let (res, ret) = w.write_all(buf).await;
                        conn.write_buf = ret;
                        res?;
                        conn.write_buf.clear();
                    }
                    monoio::spawn(pubsub_writer(rx, w));
                    conn.sub_tx = Some(tx);
                }
                handle_subscribe(
                    &args[1..],
                    &registry,
                    conn_id,
                    &mut conn.sub_channels,
                    conn.sub_tx.as_ref().unwrap(),
                );
            } else {
                conn.handle_cmd(cmd, &args[1..], &art, &registry);
            }
        }

        // Flush (normal mode only — subscriber mode writes go through channel)
        if let Some(w) = &mut write_half
            && !conn.write_buf.is_empty()
        {
            let buf = std::mem::replace(&mut conn.write_buf, BytesMut::new());
            let (res, ret) = w.write_all(buf).await;
            conn.write_buf = ret;
            res?;
            conn.write_buf.clear();
        }
    }

    cleanup_subscriptions(&registry, conn_id, &conn.sub_channels);
    Ok(())
}

/// Writer task spawned per subscriber. Owns the write half exclusively.
/// Batch-drains the channel to minimize syscalls. Dies on write error.
async fn pubsub_writer(mut rx: unbounded::Rx<Bytes>, mut write: impl AsyncWriteRentExt) {
    let mut buf = BytesMut::with_capacity(BUFFER_SIZE);
    while let Some(msg) = rx.recv().await {
        buf.extend_from_slice(&msg);
        while let Ok(m) = rx.try_recv() {
            buf.extend_from_slice(&m);
        }
        let (res, returned) = write.write_all(buf).await;
        buf = returned;
        if res.is_err() {
            return;
        }
        buf.clear();
    }
}

enum Handler {
    Static(fn() -> Frame),
    Args(fn(&[Bytes]) -> Frame),
    Data(fn(&[Bytes], &mut OxidArt) -> Frame),
    DataOnly(fn(&mut OxidArt) -> Frame),
}

fn resp_pong() -> Frame {
    Frame::SimpleString(PONG.clone())
}
fn resp_ok() -> Frame {
    Frame::SimpleString(OK.clone())
}

static COMMANDS: &[(&[u8], Handler)] = &[
    // Meta commands - no data access
    (b"GET", Handler::Data(cmd_get)),
    (b"SET", Handler::Data(cmd_set)),
    (b"INCR", Handler::Data(cmd_incr)),
    (b"DECR", Handler::Data(cmd_decr)),
    (b"INCRBY", Handler::Data(cmd_incrby)),
    (b"DECRBY", Handler::Data(cmd_decrby)),
    (b"PING", Handler::Static(resp_pong)),
    (b"QUIT", Handler::Static(resp_ok)),
    (b"SELECT", Handler::Static(resp_ok)),
    (b"ECHO", Handler::Args(cmd_echo)),
    // Data commands - need OxidArt
    (b"DEL", Handler::Data(cmd_del)),
    (b"KEYS", Handler::Data(cmd_keys)),
    (b"TTL", Handler::Data(cmd_ttl)),
    (b"PTTL", Handler::Data(cmd_pttl)),
    (b"EXPIRE", Handler::Data(cmd_expire)),
    (b"PEXPIRE", Handler::Data(cmd_pexpire)),
    (b"PERSIST", Handler::Data(cmd_persist)),
    (b"EXISTS", Handler::Data(cmd_exists)),
    (b"MGET", Handler::Data(cmd_mget)),
    (b"MSET", Handler::Data(cmd_mset)),
    (b"SETNX", Handler::Data(cmd_setnx)),
    (b"SETEX", Handler::Data(cmd_setex)),
    (b"TYPE", Handler::Data(cmd_type)),
    (b"DBSIZE", Handler::DataOnly(cmd_dbsize)),
    (b"FLUSHDB", Handler::DataOnly(cmd_flushdb)),
    // Set commands
    (b"SADD", Handler::Data(cmd_sadd)),
    (b"SREM", Handler::Data(cmd_srem)),
    (b"SISMEMBER", Handler::Data(cmd_sismember)),
    (b"SCARD", Handler::Data(cmd_scard)),
    (b"SMEMBERS", Handler::Data(cmd_smembers)),
    (b"SPOP", Handler::Data(cmd_spop)),
    // Hash commands
    (b"HSET", Handler::Data(cmd_hset)),
    (b"HMSET", Handler::Data(cmd_hmset)), // Legacy compatibility (YCSB)
    (b"HGET", Handler::Data(cmd_hget)),
    (b"HGETALL", Handler::Data(cmd_hgetall)),
    (b"HDEL", Handler::Data(cmd_hdel)),
    (b"HEXISTS", Handler::Data(cmd_hexists)),
    (b"HLEN", Handler::Data(cmd_hlen)),
    (b"HKEYS", Handler::Data(cmd_hkeys)),
    (b"HVALS", Handler::Data(cmd_hvals)),
    (b"HMGET", Handler::Data(cmd_hmget)),
    (b"HINCRBY", Handler::Data(cmd_hincrby)),
    // ZSet commands
    (b"ZADD", Handler::Data(cmd_zadd)),
    (b"ZCARD", Handler::Data(cmd_zcard)),
    (b"ZRANGE", Handler::Data(cmd_zrange)),
    (b"ZSCORE", Handler::Data(cmd_zscore)),
    (b"ZREM", Handler::Data(cmd_zrem)),
    (b"ZINCRBY", Handler::Data(cmd_zincrby)),
];

fn dispatch_command(cmd: &[u8], args: &[Bytes], art: &mut OxidArt) -> Frame {
    for (name, handler) in COMMANDS {
        if cmd.eq_ignore_ascii_case(name) {
            return match handler {
                Handler::Static(f) => f(),
                Handler::Args(f) => f(args),
                Handler::Data(f) => f(args, art),
                Handler::DataOnly(f) => f(art),
            };
        }
    }

    Frame::Error(format!("ERR unknown command '{}'", String::from_utf8_lossy(cmd)).into())
}

fn frame_to_args(frame: Frame) -> Option<SmallVec<[Bytes; 3]>> {
    match frame {
        Frame::Array(arr) => {
            let mut args = SmallVec::with_capacity(arr.len());
            for f in arr {
                match f {
                    Frame::BulkString(b) => args.push(b),
                    Frame::SimpleString(s) => args.push(s),
                    _ => return None,
                }
            }
            Some(args)
        }
        Frame::BulkString(b) => Some(smallvec::smallvec![b]),
        Frame::SimpleString(s) => Some(smallvec::smallvec![s]),
        _ => None,
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Condition for SET command - makes invalid states unrepresentable
#[derive(Default)]
enum SetCondition {
    #[default]
    Always,
    IfNotExists, // Redis NX flag
    IfExists,    // Redis XX flag
}

/// Parsed SET options
struct SetOptions {
    ttl: Option<Duration>,
    condition: SetCondition,
}

impl Default for SetOptions {
    fn default() -> Self {
        Self {
            ttl: None,
            condition: SetCondition::Always,
        }
    }
}

/// Parse SET command options (EX, PX, NX, XX)
fn parse_set_options(args: &[Bytes]) -> Result<SetOptions, Frame> {
    let mut opts = SetOptions::default();
    let mut i = 0;

    while i < args.len() {
        if args[i].eq_ignore_ascii_case(b"EX") {
            i += 1;
            if i >= args.len() {
                return Err(Frame::Error("ERR syntax error".into()));
            }
            let secs: u64 = parse_int(&args[i]).ok_or_else(|| {
                Frame::Error("ERR value is not an integer or out of range".into())
            })?;
            opts.ttl = Some(Duration::from_secs(secs));
        } else if args[i].eq_ignore_ascii_case(b"PX") {
            i += 1;
            if i >= args.len() {
                return Err(Frame::Error("ERR syntax error".into()));
            }
            let ms: u64 = parse_int(&args[i]).ok_or_else(|| {
                Frame::Error("ERR value is not an integer or out of range".into())
            })?;
            opts.ttl = Some(Duration::from_millis(ms));
        } else if args[i].eq_ignore_ascii_case(b"NX") {
            opts.condition = SetCondition::IfNotExists;
        } else if args[i].eq_ignore_ascii_case(b"XX") {
            opts.condition = SetCondition::IfExists;
        }
        // Ignore unknown options (KEEPTTL, GET, etc.)
        i += 1;
    }

    Ok(opts)
}

/// Parse an integer argument from bytes
fn parse_int<T: std::str::FromStr>(arg: &Bytes) -> Option<T> {
    std::str::from_utf8(arg).ok().and_then(|s| s.parse().ok())
}

// =============================================================================
// Commands
// =============================================================================

fn cmd_get(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'GET' command".into());
    }
    match art.get(&args[0]) {
        Some(val) => match val.as_bytes() {
            Some(b) => Frame::BulkString(b),
            None => Frame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".into(),
            ),
        },
        None => Frame::Null,
    }
}

fn cmd_set(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SET' command".into());
    }

    let key = args[0].clone();
    let val = Value::String(args[1].clone());
    let opts = match parse_set_options(&args[2..]) {
        Ok(o) => o,
        Err(e) => return e,
    };

    // Check condition before setting (skip lookup when Always)
    if !matches!(opts.condition, SetCondition::Always) {
        let key_exists = art.get(&key).is_some();
        match opts.condition {
            SetCondition::IfNotExists if key_exists => return Frame::Null,
            SetCondition::IfExists if !key_exists => return Frame::Null,
            _ => {}
        }
    }

    match opts.ttl {
        Some(duration) => art.set_ttl(key, duration, val),
        None => art.set(key, val),
    }

    Frame::SimpleString(OK.clone())
}

fn counter_err(e: CounterError) -> Frame {
    match e {
        CounterError::NotAnInteger => {
            Frame::Error("ERR value is not an integer or out of range".into())
        }
        CounterError::Overflow => Frame::Error("ERR increment or decrement would overflow".into()),
    }
}

fn cmd_incr(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'INCR' command".into());
    }
    match art.incr(args[0].clone()) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

fn cmd_decr(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'DECR' command".into());
    }
    match art.decr(args[0].clone()) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

fn cmd_incrby(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'INCRBY' command".into());
    }
    let delta: i64 = match parse_int(&args[1]) {
        Some(d) => d,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    match art.incrby(args[0].clone(), delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

fn cmd_decrby(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'DECRBY' command".into());
    }
    let delta: i64 = match parse_int(&args[1]) {
        Some(d) => d,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    match art.decrby(args[0].clone(), delta) {
        Ok(val) => Frame::Integer(val),
        Err(e) => counter_err(e),
    }
}

fn cmd_del(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'DEL' command".into());
    }

    let mut count = 0i64;
    for key in args {
        if art.del(key).is_some() {
            count += 1;
        }
    }
    Frame::Integer(count)
}

fn cmd_keys(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        // No pattern = all keys
        let keys: Vec<Frame> = art
            .getn(Bytes::new())
            .into_iter()
            .map(|(k, _)| Frame::BulkString(k))
            .collect();
        return Frame::Array(keys);
    }

    let pattern = &args[0];

    // Fast path: simple "prefix*" or bare prefix with no glob chars → getn
    if is_simple_prefix(pattern) {
        let prefix = if pattern.ends_with(b"*") {
            pattern.slice(..pattern.len() - 1)
        } else {
            pattern.clone()
        };
        let keys: Vec<Frame> = art
            .getn(prefix)
            .into_iter()
            .map(|(k, _)| Frame::BulkString(k))
            .collect();
        return Frame::Array(keys);
    }

    // Slow path: complex glob → convert to regex → DFA traversal
    let regex = glob_to_regex(pattern);
    match art.getn_regex(&regex) {
        Ok(pairs) => {
            let keys: Vec<Frame> = pairs
                .into_iter()
                .map(|(k, _)| Frame::BulkString(k))
                .collect();
            Frame::Array(keys)
        }
        Err(_) => Frame::Error("ERR invalid pattern".into()),
    }
}

/// Returns true if the pattern is a simple prefix (no glob chars except a trailing *)
fn is_simple_prefix(pattern: &[u8]) -> bool {
    let end = if pattern.ends_with(b"*") {
        pattern.len() - 1
    } else {
        pattern.len()
    };
    !pattern[..end]
        .iter()
        .any(|&b| b == b'*' || b == b'?' || b == b'[' || b == b']')
}

/// Converts a Redis glob pattern to an anchored regex.
///
/// Redis glob rules:
///   *      → .*       (match any sequence)
///   ?      → .        (match one char)
///   [abc]  → [abc]    (character class, passed through)
///   \x     → \x       (escape, passed through)
///   other  → escaped literal
fn glob_to_regex(pattern: &[u8]) -> String {
    let mut regex = String::with_capacity(pattern.len() * 2);
    regex.push('^');

    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'*' => regex.push_str(".*"),
            b'?' => regex.push('.'),
            b'[' => {
                regex.push('[');
                i += 1;
                while i < pattern.len() && pattern[i] != b']' {
                    regex.push(pattern[i] as char);
                    i += 1;
                }
                if i < pattern.len() {
                    regex.push(']');
                }
            }
            b'\\' if i + 1 < pattern.len() => {
                regex.push('\\');
                i += 1;
                regex.push(pattern[i] as char);
            }
            // Escape regex metacharacters
            b'.' | b'+' | b'^' | b'$' | b'{' | b'}' | b'(' | b')' | b'|' | b'#' | b'&' | b'~' => {
                regex.push('\\');
                regex.push(pattern[i] as char);
            }
            b => regex.push(b as char),
        }
        i += 1;
    }

    regex.push('$');
    regex
}

fn cmd_ttl(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'TTL' command".into());
    }

    match art.get_ttl(args[0].clone()) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer(secs as i64),
    }
}

fn cmd_expire(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'EXPIRE' command".into());
    }

    let secs: u64 = match std::str::from_utf8(&args[1])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) => s,
        None => return Frame::Error("ERR value is not an integer".into()),
    };

    if art.expire(args[0].clone(), Duration::from_secs(secs)) {
        Frame::Integer(1)
    } else {
        Frame::Integer(0)
    }
}

fn cmd_persist(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'PERSIST' command".into());
    }

    if art.persist(args[0].clone()) {
        Frame::Integer(1)
    } else {
        Frame::Integer(0)
    }
}

fn cmd_exists(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'EXISTS' command".into());
    }

    let mut count = 0i64;
    for key in args {
        if art.get(key).is_some() {
            count += 1;
        }
    }
    Frame::Integer(count)
}

fn cmd_mget(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'MGET' command".into());
    }

    let results: Vec<Frame> = args
        .iter()
        .map(|key| match art.get(key) {
            Some(val) => match val.as_bytes() {
                Some(b) => Frame::BulkString(b),
                None => Frame::Null,
            },
            None => Frame::Null,
        })
        .collect();

    Frame::Array(results)
}

fn cmd_mset(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() || !args.len().is_multiple_of(2) {
        return Frame::Error("ERR wrong number of arguments for 'MSET' command".into());
    }

    for pair in args.chunks_exact(2) {
        art.set(pair[0].clone(), Value::String(pair[1].clone()));
    }

    Frame::SimpleString(OK.clone())
}

fn cmd_setnx(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SETNX' command".into());
    }

    let key = args[0].clone();
    if art.get(&key).is_some() {
        return Frame::Integer(0);
    }

    art.set(key, Value::String(args[1].clone()));
    Frame::Integer(1)
}

fn cmd_setex(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 3 {
        return Frame::Error("ERR wrong number of arguments for 'SETEX' command".into());
    }

    let key = args[0].clone();
    let secs: u64 = match parse_int(&args[1]) {
        Some(s) => s,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    let val = Value::String(args[2].clone());

    art.set_ttl(key, Duration::from_secs(secs), val);
    Frame::SimpleString(OK.clone())
}

fn cmd_pttl(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'PTTL' command".into());
    }

    match art.get_ttl(args[0].clone()) {
        TtlResult::KeyNotExist => Frame::Integer(-2),
        TtlResult::KeyWithoutTtl => Frame::Integer(-1),
        TtlResult::KeyWithTtl(secs) => Frame::Integer((secs * 1000) as i64),
    }
}

fn cmd_pexpire(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'PEXPIRE' command".into());
    }

    let ms: u64 = match parse_int(&args[1]) {
        Some(m) => m,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };

    if art.expire(args[0].clone(), Duration::from_millis(ms)) {
        Frame::Integer(1)
    } else {
        Frame::Integer(0)
    }
}

fn cmd_echo(args: &[Bytes]) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'ECHO' command".into());
    }
    Frame::BulkString(args[0].clone())
}

fn cmd_dbsize(art: &mut OxidArt) -> Frame {
    let count = art.getn(Bytes::new()).len() as i64;
    Frame::Integer(count)
}

fn cmd_flushdb(art: &mut OxidArt) -> Frame {
    art.deln(Bytes::new());
    Frame::SimpleString(OK.clone())
}

fn cmd_type(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'TYPE' command".into());
    }

    match art.get(&args[0]) {
        Some(val) => Frame::SimpleString(Bytes::from_static(val.redis_type().as_str().as_bytes())),
        None => Frame::SimpleString(Bytes::from_static(b"none")),
    }
}

// =============================================================================
// Pub/Sub
// =============================================================================

fn encode_pubsub_push(channel: &Bytes, message: &Bytes) -> Bytes {
    let frame = Frame::Array(vec![
        Frame::BulkString(Bytes::from_static(b"message")),
        Frame::BulkString(channel.clone()),
        Frame::BulkString(message.clone()),
    ]);
    let mut buf = BytesMut::new();
    extend_encode(&mut buf, &frame, false).expect("encode should not fail");
    buf.freeze()
}

fn handle_subscribe(
    args: &[Bytes],
    registry: &SharedRegistry,
    conn_id: ConnId,
    sub_channels: &mut HashSet<Bytes>,
    tx: &unbounded::Tx<Bytes>,
) {
    if args.is_empty() {
        send_via_tx(
            tx,
            Frame::Error("ERR wrong number of arguments for 'SUBSCRIBE' command".into()),
        );
        return;
    }

    let mut reg = registry.borrow_mut();
    for ch in args {
        sub_channels.insert(ch.clone());
        reg.entry(ch.clone())
            .or_default()
            .insert(conn_id, tx.clone());

        send_via_tx(
            tx,
            Frame::Array(vec![
                Frame::BulkString(Bytes::from_static(b"subscribe")),
                Frame::BulkString(ch.clone()),
                Frame::Integer(sub_channels.len() as i64),
            ]),
        );
    }
}

fn handle_unsubscribe(
    args: &[Bytes],
    registry: &SharedRegistry,
    conn_id: ConnId,
    sub_channels: &mut HashSet<Bytes>,
    tx: &unbounded::Tx<Bytes>,
) {
    let channels_to_remove: Vec<Bytes> = if args.is_empty() {
        sub_channels.iter().cloned().collect()
    } else {
        args.to_vec()
    };

    let mut reg = registry.borrow_mut();
    for ch in &channels_to_remove {
        sub_channels.remove(ch);
        if let Some(subs) = reg.get_mut(ch) {
            subs.remove(&conn_id);
            if subs.is_empty() {
                reg.remove(ch);
            }
        }

        send_via_tx(
            tx,
            Frame::Array(vec![
                Frame::BulkString(Bytes::from_static(b"unsubscribe")),
                Frame::BulkString(ch.clone()),
                Frame::Integer(sub_channels.len() as i64),
            ]),
        );
    }
}

/// Encode a frame and send it through the pub/sub writer channel.
#[inline]
fn send_via_tx(tx: &unbounded::Tx<Bytes>, frame: Frame) {
    let mut buf = BytesMut::new();
    extend_encode(&mut buf, &frame, false).expect("encode should not fail");
    let _ = tx.send(buf.freeze());
}

fn cmd_publish(args: &[Bytes], registry: &SharedRegistry) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'PUBLISH' command".into());
    }

    let channel = &args[0];
    let message = &args[1];
    let encoded = encode_pubsub_push(channel, message);

    let mut reg = registry.borrow_mut();
    let Some(subs) = reg.get_mut(channel) else {
        return Frame::Integer(0);
    };

    // Send to all subscribers, remove dead ones
    subs.retain(|_, tx| tx.send(encoded.clone()).is_ok());
    let count = subs.len() as i64;

    if subs.is_empty() {
        reg.remove(channel);
    }

    Frame::Integer(count)
}

fn cleanup_subscriptions(registry: &SharedRegistry, conn_id: ConnId, channels: &HashSet<Bytes>) {
    if channels.is_empty() {
        return;
    }
    let mut reg = registry.borrow_mut();
    for ch in channels {
        if let Some(subs) = reg.get_mut(ch) {
            subs.remove(&conn_id);
            if subs.is_empty() {
                reg.remove(ch);
            }
        }
    }
}
