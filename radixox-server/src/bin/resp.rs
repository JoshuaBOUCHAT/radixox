use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use monoio::io::{AsyncReadRent, AsyncWriteRentExt, Splitable};
use monoio::net::{TcpListener, TcpStream};
use smallvec::SmallVec;

use oxidart::monoio::{spawn_evictor, spawn_ticker};
use oxidart::{OxidArt, TtlResult};
use redis_protocol::resp2::decode::decode_bytes_mut;
use redis_protocol::resp2::encode::extend_encode;
use redis_protocol::resp2::types::BytesFrame as Frame;

type IOResult<T> = std::io::Result<T>;
type SharedART = Rc<RefCell<OxidArt>>;

const BUFFER_SIZE: usize = 64 * 1024;

// Static responses (avoid allocation)
static PONG: Bytes = Bytes::from_static(b"PONG");
static OK: Bytes = Bytes::from_static(b"OK");
static ERR_EMPTY_CMD: &str = "ERR empty command";

#[monoio::main(enable_timer = true)]
async fn main() -> IOResult<()> {
    let listener = TcpListener::bind("0.0.0.0:6379")?;
    println!("RadixOx RESP Server listening on 0.0.0.0:6379");

    let shared_art = SharedART::new(RefCell::new(OxidArt::new()));
    spawn_ticker(shared_art.clone(), Duration::from_millis(100));
    spawn_evictor(shared_art.clone(), Duration::from_secs(1));
    shared_art.borrow_mut().tick();

    loop {
        let (stream, addr) = listener.accept().await?;
        println!("New connection from {}", addr);
        monoio::spawn(handle_connection(stream, shared_art.clone()));
    }
}

async fn handle_connection(stream: TcpStream, art: SharedART) -> IOResult<()> {
    //stream.set_nodelay(true)?;
    let (mut read, mut write) = stream.into_split();
    let mut read_buf = BytesMut::with_capacity(BUFFER_SIZE);
    let mut write_buf = BytesMut::with_capacity(BUFFER_SIZE);
    // Separate buffer for io_uring reads - gets ownership transferred to kernel
    let mut io_buf = BytesMut::with_capacity(BUFFER_SIZE);

    loop {
        // Read data from socket (monoio takes ownership, returns it back)
        let (res, returned_buf) = read.read(io_buf).await;
        io_buf = returned_buf;

        let n = match res {
            Ok(0) => return Ok(()), // Connection closed
            Ok(n) => n,
            Err(e) => return Err(e),
        };

        // Append read data to parse buffer
        read_buf.extend_from_slice(&io_buf[..n]);
        io_buf.clear();

        // Parse and execute commands (can be multiple in pipeline)
        loop {
            match decode_bytes_mut(&mut read_buf) {
                Ok(Some((frame, _consumed, _buf))) => {
                    let response = execute_command(frame, &mut art.borrow_mut());
                    extend_encode(&mut write_buf, &response).expect("encode should not fail");
                }
                Ok(None) => break, // Need more data
                Err(e) => {
                    eprintln!("Parse error: {:?}", e);
                    let err_response = Frame::Error(format!("ERR parse error: {:?}", e).into());
                    extend_encode(&mut write_buf, &err_response).expect("encode should not fail");
                    break;
                }
            }
        }

        // Write all responses at once
        if !write_buf.is_empty() {
            let (res, buf) = write.write_all(write_buf).await;
            write_buf = buf;
            res?;
            write_buf.clear();
        }
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
];

fn execute_command(frame: Frame, art: &mut OxidArt) -> Frame {
    let args = match frame_to_args(frame) {
        Some(args) if !args.is_empty() => args,
        _ => return Frame::Error(ERR_EMPTY_CMD.into()),
    };

    let cmd = &args[0];
    for (name, handler) in COMMANDS {
        if cmd.eq_ignore_ascii_case(name) {
            return match handler {
                Handler::Static(f) => f(),
                Handler::Args(f) => f(&args[1..]),
                Handler::Data(f) => f(&args[1..], art),
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
    match art.get(args[0].clone()) {
        Some(val) => Frame::BulkString(val),
        None => Frame::Null,
    }
}

fn cmd_set(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SET' command".into());
    }

    let key = args[0].clone();
    let val = args[1].clone();
    let opts = match parse_set_options(&args[2..]) {
        Ok(o) => o,
        Err(e) => return e,
    };

    // Check condition before setting (skip lookup when Always)
    if !matches!(opts.condition, SetCondition::Always) {
        let key_exists = art.get(key.clone()).is_some();
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

fn cmd_del(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'DEL' command".into());
    }

    let mut count = 0i64;
    for key in args {
        if art.del(key.clone()).is_some() {
            count += 1;
        }
    }
    Frame::Integer(count)
}

fn cmd_keys(args: &[Bytes], art: &mut OxidArt) -> Frame {
    let pattern = if args.is_empty() {
        Bytes::new()
    } else {
        // Simple prefix matching: "user:*" -> prefix "user:"
        let mut p = args[0].clone();
        if p.ends_with(b"*") {
            p = p.slice(..p.len() - 1);
        }
        p
    };

    let pairs = art.getn(pattern);
    let keys: Vec<Frame> = pairs
        .into_iter()
        .map(|(k, _)| Frame::BulkString(k))
        .collect();

    Frame::Array(keys)
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
        if art.get(key.clone()).is_some() {
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
        .map(|key| match art.get(key.clone()) {
            Some(val) => Frame::BulkString(val),
            None => Frame::Null,
        })
        .collect();

    Frame::Array(results)
}

fn cmd_mset(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() || args.len() % 2 != 0 {
        return Frame::Error("ERR wrong number of arguments for 'MSET' command".into());
    }

    for pair in args.chunks_exact(2) {
        art.set(pair[0].clone(), pair[1].clone());
    }

    Frame::SimpleString(OK.clone())
}

fn cmd_setnx(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SETNX' command".into());
    }

    let key = args[0].clone();
    if art.get(key.clone()).is_some() {
        return Frame::Integer(0);
    }

    art.set(key, args[1].clone());
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
    let val = args[2].clone();

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

    match art.get(args[0].clone()) {
        Some(_) => Frame::SimpleString(Bytes::from_static(b"string")),
        None => Frame::SimpleString(Bytes::from_static(b"none")),
    }
}
