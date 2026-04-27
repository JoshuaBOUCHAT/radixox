#[cfg(not(target_os = "linux"))]
compile_error!("RadixOx requires Linux to run (io_uring and mmap support).");

mod resp_cmd;
mod utils;

use std::cell::RefCell;

use std::rc::Rc;
use std::time::Duration;

use bytes::BytesMut;
use monoio::io::{AsyncReadRent, Splitable};
use monoio::net::tcp::TcpOwnedReadHalf;
use monoio::net::{TcpListener, TcpStream};
use monoio::time::TimeDriver;
use monoio::{IoUringDriver, Runtime, RuntimeBuilder, select};

use redis_protocol::resp2::decode::decode_bytes_mut;
use redis_protocol::resp2::types::BytesFrame;
use smallvec::SmallVec;

use oxidart::monoio::spawn_stats_logger;

use oxidart::OxidArt;
use radixox_lib::shared_byte::SharedByte;
pub(crate) use radixox_lib::shared_frame::SharedFrame as Frame;

use resp_cmd::delayed::{AsyncFrame, cmd_keys, cmd_unlink};
use resp_cmd::pub_sub::{cmd_publish, cmd_subscribe, cmd_unsubscribe};
use resp_cmd::string::*;
use resp_cmd::{
    cmd_hdel, cmd_hexists, cmd_hget, cmd_hgetall, cmd_hincrby, cmd_hkeys, cmd_hlen, cmd_hmget,
    cmd_hmset, cmd_hset, cmd_hvals, cmd_sadd, cmd_scard, cmd_sismember, cmd_smembers, cmd_spop,
    cmd_srem, cmd_zadd, cmd_zcard, cmd_zincrby, cmd_zrange, cmd_zrem, cmd_zscore,
};

use crate::utils::{ConnState, SubRegistry};

pub(crate) type IOResult<T> = std::io::Result<T>;
type SharedART = Rc<RefCell<OxidArt>>;
pub(crate) type SharedRegistry = Rc<RefCell<SubRegistry>>;
pub(crate) type CmdArgs = SmallVec<[SharedByte; 3]>;

const BUFFER_SIZE: usize = 64 * 1024;
static ERR_EMPTY_CMD: &str = "ERR empty command";
const NB_ACCEPTOR: usize = 16;

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> std::io::Result<()> {
    let mut runtime = get_runtime()?;

    runtime.block_on(async {
        let port: u16 = std::env::var("RADIXOX_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(6379);
        let addr = format!("0.0.0.0:{port}");
        let listener = Rc::new(TcpListener::bind(&addr)?);
        println!("RadixOx RESP Server listening on {addr}");

        let shared_art =
            OxidArt::shared_with_evictor(Duration::from_millis(100), Duration::from_secs(1));
        spawn_stats_logger(shared_art.clone(), Duration::from_secs(5));

        let registry: SharedRegistry = Rc::new(RefCell::new(SubRegistry::default()));

        let mut handles = Vec::with_capacity(NB_ACCEPTOR);
        for _ in 0..NB_ACCEPTOR {
            handles.push(spawn_acceptor(
                shared_art.clone(),
                listener.clone(),
                registry.clone(),
            ));
        }
        for h in handles {
            h.await;
        }

        Ok(())
    })
}

fn get_runtime() -> std::io::Result<Runtime<TimeDriver<IoUringDriver>>> {
    RuntimeBuilder::<monoio::IoUringDriver>::new()
        .with_entries(4096)
        .uring_builder(io_uring::IoUring::builder())
        .enable_timer()
        .build()
}

fn spawn_acceptor(
    shared_art: SharedART,
    listener: Rc<TcpListener>,
    registry: SharedRegistry,
) -> monoio::task::JoinHandle<()> {
    monoio::spawn(async move {
        use std::io::ErrorKind;
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock
                    | ErrorKind::Interrupted
                    | ErrorKind::ConnectionAborted => continue,
                    ErrorKind::OutOfMemory => {
                        monoio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    _ => panic!("accept fatal: {e}"),
                },
            };
            monoio::spawn(handle_connection(
                stream,
                shared_art.clone(),
                registry.clone(),
            ));
        }
    })
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_connection(
    stream: TcpStream,
    art: SharedART,
    registry: SharedRegistry,
) -> IOResult<()> {
    let (mut read, write) = stream.into_split();
    let mut conn_state = ConnState::Normal(write, Vec::with_capacity(BUFFER_SIZE));
    let result = handle_loop(&mut read, &mut conn_state, &registry, &art).await;

    // Cleanup
    match conn_state {
        ConnState::PubSub(sub_id) => {
            registry.borrow_mut().cleanup(sub_id);
        }

        ConnState::Blocking => {
            todo!()
        }
        ConnState::Normal(_, _) => {} //Nothing to clean
        ConnState::None => {}         //Nothing too
    }

    result
}
async fn handle_loop(
    read: &mut TcpOwnedReadHalf,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<()> {
    let mut read_buf = BytesMut::with_capacity(BUFFER_SIZE);
    let mut io_buf = BytesMut::with_capacity(BUFFER_SIZE);

    loop {
        let (n, returned) = read_with_conn_state(io_buf, conn_state, registry, read).await?;
        io_buf = returned;
        if n == 0 {
            return Ok(());
        }
        read_buf.extend_from_slice(&io_buf[..n]);
        io_buf.clear();
        handle_buffer(&mut read_buf, conn_state, registry, art).await?;
    }
}

async fn read_with_conn_state(
    io_buf: BytesMut,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    read: &mut TcpOwnedReadHalf,
) -> IOResult<(usize, BytesMut)> {
    let (res, returned) = match conn_state {
        ConnState::Normal(_, _) => read.read(io_buf).await,
        ConnState::PubSub(sub_id) => {
            let cancelation = registry
                .borrow_mut()
                .get(*sub_id)
                .expect("can't get cancelation")
                .cancelation
                .clone();

            select! {
                err_msg =cancelation=>{
                    let err=std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        String::from_utf8_lossy(&err_msg)
                    );
                    return Err(err);
                }
                res_tuple=read.read(io_buf)=>{
                    res_tuple
                }
            }
        }
        ConnState::Blocking => todo!(),
        ConnState::None => panic!("No read should occurs while conn state is None"),
    };

    Ok((res?, returned))
}

// ── Buffer parsing & dispatch ─────────────────────────────────────────────────

async fn handle_buffer(
    read_buf: &mut BytesMut,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<()> {
    loop {
        let frame = match decode_bytes_mut(read_buf) {
            Ok(Some((frame, _, _))) => frame,
            Ok(None) => return Ok(()),
            Err(e) => {
                let _ = conn_state
                    .send(Frame::Error(format!("ERR parse error: {e:?}")), registry)
                    .await;
                return Ok(());
            }
        };

        let Some((mut cmd, args)) = frame_to_args(frame) else {
            conn_state
                .send(Frame::Error(ERR_EMPTY_CMD.into()), registry)
                .await?;
            continue;
        };
        cmd.to_uppercase();
        dispatch(&cmd, &args, conn_state, registry, art).await?;
    }
}

async fn dispatch(
    cmd: &SharedByte,
    args: &[SharedByte],
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<()> {
    let handler = get_handler(cmd.as_slice());
    match conn_state {
        ConnState::PubSub(_) => match handler {
            Some(Handler::Subscribe) => cmd_subscribe(args, conn_state, registry).await?,
            Some(Handler::Unsubscribe) => cmd_unsubscribe(args, conn_state, registry).await?,
            Some(Handler::Ping) => conn_state.send(resp_pong(), registry).await?,
            Some(Handler::Quit) => {
                conn_state.send(resp_ok(), registry).await?;
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            }
            _ => {
                let frame = Frame::Error(String::from(
                    "ERR only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT allow",
                ));

                conn_state.send(frame, registry).await?;
            }
        },
        ConnState::Normal(_, _) => match handler {
            Some(Handler::Subscribe) => cmd_subscribe(args, conn_state, registry).await?,
            Some(Handler::Publish) => cmd_publish(args, conn_state, registry).await?,
            Some(Handler::Ping) => conn_state.send(resp_pong(), registry).await?,
            Some(Handler::Quit) => {
                conn_state.send(resp_ok(), registry).await?;
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            }
            Some(h) => {
                conn_state
                    .send(run_handler(h, args, art).await, registry)
                    .await?
            }
            None => {
                let frame = Frame::Error(format!(
                    "ERR unknown command '{}'",
                    String::from_utf8_lossy(cmd)
                ));

                conn_state.send(frame, registry).await?;
            }
        },
        _ => {}
    }
    Ok(())
}

// ── Command dispatch ──────────────────────────────────────────────────────────

fn resp_pong() -> Frame {
    Frame::SimpleString(SharedByte::from_slice(b"PONG"))
}
fn resp_ok() -> Frame {
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}

enum Handler {
    // ── Data commands (state-free) ────────────────────────────────────────────
    Static(fn() -> Frame),
    Args(fn(&[SharedByte]) -> Frame),
    Data(fn(&[SharedByte], &mut OxidArt) -> Frame),
    DataOnly(fn(&mut OxidArt) -> Frame),
    Async(fn(&[SharedByte], SharedART) -> AsyncFrame),
    // ── State-sensitive commands ──────────────────────────────────────────────
    Ping,
    Quit,
    Subscribe,
    Unsubscribe,
    Publish,
}

fn get_handler(cmd: &[u8]) -> Option<Handler> {
    Some(match cmd {
        // ── Connection ────────────────────────────────────────────────────────
        b"PING" => Handler::Ping,
        b"QUIT" => Handler::Quit,
        b"SELECT" => Handler::Static(resp_ok),
        b"ECHO" => Handler::Args(cmd_echo),
        // ── Pub/Sub ───────────────────────────────────────────────────────────
        b"SUBSCRIBE" => Handler::Subscribe,
        b"UNSUBSCRIBE" => Handler::Unsubscribe,
        b"PUBLISH" => Handler::Publish,
        // ── Strings / Keys ────────────────────────────────────────────────────
        b"GET" => Handler::Data(cmd_get),
        b"SET" => Handler::Data(cmd_set),
        b"SETNX" => Handler::Data(cmd_setnx),
        b"SETEX" => Handler::Data(cmd_setex),
        b"MGET" => Handler::Data(cmd_mget),
        b"MSET" => Handler::Data(cmd_mset),
        b"DEL" => Handler::Data(cmd_del),
        b"EXISTS" => Handler::Data(cmd_exists),
        b"TYPE" => Handler::Data(cmd_type),
        b"KEYS" => Handler::Async(cmd_keys),
        b"UNLINK" => Handler::Async(cmd_unlink),
        // ── Counters ──────────────────────────────────────────────────────────
        b"INCR" => Handler::Data(cmd_incr),
        b"DECR" => Handler::Data(cmd_decr),
        b"INCRBY" => Handler::Data(cmd_incrby),
        b"DECRBY" => Handler::Data(cmd_decrby),
        // ── TTL ───────────────────────────────────────────────────────────────
        b"TTL" => Handler::Data(cmd_ttl),
        b"PTTL" => Handler::Data(cmd_pttl),
        b"EXPIRE" => Handler::Data(cmd_expire),
        b"PEXPIRE" => Handler::Data(cmd_pexpire),
        b"PERSIST" => Handler::Data(cmd_persist),
        // ── Server ────────────────────────────────────────────────────────────
        b"DBSIZE" => Handler::DataOnly(cmd_dbsize),
        b"FLUSHDB" => Handler::DataOnly(cmd_flushdb),
        // ── Hash ──────────────────────────────────────────────────────────────
        b"HSET" => Handler::Data(cmd_hset),
        b"HMSET" => Handler::Data(cmd_hmset),
        b"HGET" => Handler::Data(cmd_hget),
        b"HGETALL" => Handler::Data(cmd_hgetall),
        b"HDEL" => Handler::Data(cmd_hdel),
        b"HEXISTS" => Handler::Data(cmd_hexists),
        b"HLEN" => Handler::Data(cmd_hlen),
        b"HKEYS" => Handler::Data(cmd_hkeys),
        b"HVALS" => Handler::Data(cmd_hvals),
        b"HMGET" => Handler::Data(cmd_hmget),
        b"HINCRBY" => Handler::Data(cmd_hincrby),
        // ── Set ───────────────────────────────────────────────────────────────
        b"SADD" => Handler::Data(cmd_sadd),
        b"SREM" => Handler::Data(cmd_srem),
        b"SISMEMBER" => Handler::Data(cmd_sismember),
        b"SCARD" => Handler::Data(cmd_scard),
        b"SMEMBERS" => Handler::Data(cmd_smembers),
        b"SPOP" => Handler::Data(cmd_spop),
        // ── ZSet ──────────────────────────────────────────────────────────────
        b"ZADD" => Handler::Data(cmd_zadd),
        b"ZCARD" => Handler::Data(cmd_zcard),
        b"ZRANGE" => Handler::Data(cmd_zrange),
        b"ZSCORE" => Handler::Data(cmd_zscore),
        b"ZREM" => Handler::Data(cmd_zrem),
        b"ZINCRBY" => Handler::Data(cmd_zincrby),
        _ => return None,
    })
}

/// Executes a state-free handler and returns the response frame.
/// State-sensitive variants (Ping, Quit, Subscribe, Unsubscribe, Publish)
/// are handled in `dispatch` before this is ever called.
async fn run_handler(handler: Handler, args: &[SharedByte], art: &SharedART) -> Frame {
    match handler {
        Handler::Static(f) => f(),
        Handler::Args(f) => f(args),
        Handler::Data(f) => f(args, &mut art.borrow_mut()),
        Handler::DataOnly(f) => f(&mut art.borrow_mut()),
        Handler::Async(f) => f(args, art.clone()).await,
        _ => unreachable!("state-sensitive handler reached run_handler"),
    }
}

fn frame_to_args(frame: BytesFrame) -> Option<(SharedByte, CmdArgs)> {
    match frame {
        BytesFrame::Array(arr) if !arr.is_empty() => {
            let mut iter = arr.into_iter();
            let cmd = match iter.next().unwrap() {
                BytesFrame::BulkString(b) => SharedByte::from_slice(&b),
                BytesFrame::SimpleString(s) => SharedByte::from_slice(&s),
                _ => return None,
            };
            let mut args = CmdArgs::with_capacity(iter.len());
            for f in iter {
                match f {
                    BytesFrame::BulkString(b) => args.push(SharedByte::from_slice(&b)),
                    BytesFrame::SimpleString(s) => args.push(SharedByte::from_slice(&s)),
                    _ => return None,
                }
            }
            Some((cmd, args))
        }
        BytesFrame::BulkString(b) => Some((SharedByte::from_slice(&b), CmdArgs::new())),
        BytesFrame::SimpleString(s) => Some((SharedByte::from_slice(&s), CmdArgs::new())),
        _ => None,
    }
}

// ── SET options ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub(crate) enum SetCondition {
    #[default]
    Always,
    IfNotExists,
    IfExists,
}

pub(crate) struct SetOptions {
    pub(crate) ttl: Option<Duration>,
    pub(crate) condition: SetCondition,
}

impl Default for SetOptions {
    fn default() -> Self {
        Self {
            ttl: None,
            condition: SetCondition::Always,
        }
    }
}

pub(crate) fn parse_set_options(args: &[SharedByte]) -> Result<SetOptions, Frame> {
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
        i += 1;
    }
    Ok(opts)
}

pub(crate) fn parse_int<T: std::str::FromStr>(arg: &[u8]) -> Option<T> {
    std::str::from_utf8(arg).ok().and_then(|s| s.parse().ok())
}
