#[cfg(not(target_os = "linux"))]
compile_error!("RadixOx requires Linux to run (io_uring and mmap support).");

mod resp_cmd;
mod utils;

use std::cell::RefCell;
use std::env;
use std::rc::Rc;
use std::time::Duration;

use monoio::io::{AsyncReadRent, Splitable};
use monoio::net::tcp::TcpOwnedReadHalf;
use monoio::net::{TcpListener, TcpStream};
use monoio::time::TimeDriver;
use monoio::{IoUringDriver, Runtime, RuntimeBuilder, select};

use oxidart::OxidArt;
use radixox_lib::cmd::Cmd;
use radixox_lib::shared_byte::OwnedByte;
use radixox_lib::shared_byte::SharedByte;
pub(crate) use radixox_lib::shared_frame::SharedFrame as Frame;

use resp_cmd::delayed::{cmd_keys, cmd_unlink};
use resp_cmd::pub_sub::{cmd_publish, cmd_subscribe, cmd_unsubscribe};
use resp_cmd::string::{
    cmd_dbsize, cmd_decr, cmd_decrby, cmd_del, cmd_exists, cmd_expire, cmd_flushdb, cmd_get,
    cmd_incr, cmd_incrby, cmd_mget, cmd_mset, cmd_persist, cmd_pttl, cmd_set, cmd_ttl, cmd_type,
};
use resp_cmd::{
    cmd_hdel, cmd_hexists, cmd_hget, cmd_hgetall, cmd_hincrby, cmd_hkeys, cmd_hlen, cmd_hmget,
    cmd_hset, cmd_hvals, cmd_sadd, cmd_scard, cmd_sismember, cmd_smembers, cmd_spop, cmd_srem,
    cmd_zadd, cmd_zcard, cmd_zincrby, cmd_zrange, cmd_zrem, cmd_zscore,
};

use crate::utils::{ConnState, SubRegistry};

pub(crate) type IOResult<T> = std::io::Result<T>;
type SharedART = Rc<RefCell<OxidArt>>;
pub(crate) type SharedRegistry = Rc<RefCell<SubRegistry>>;

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
    let mut builder = io_uring::IoUring::builder();
    if let Ok(sq_val) = env::var("SQ_POLL")
        && let Ok(idle) = sq_val.parse::<u32>()
    {
        builder.setup_sqpoll(idle);
        println!("Radixox lauched starting with SQ_POLL idle: {}ms", idle)
    }

    RuntimeBuilder::<monoio::IoUringDriver>::new()
        .with_entries(4096)
        .uring_builder(builder)
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

    match conn_state {
        ConnState::PubSub(sub_id) => {
            registry.borrow_mut().cleanup(sub_id);
        }
        ConnState::Blocking => todo!(),
        ConnState::Normal(_, _) => {}
        ConnState::None => {}
    }

    result
}

async fn handle_loop(
    read: &mut TcpOwnedReadHalf,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<()> {
    let mut read_buf: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
    let mut io_buf: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);

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
    io_buf: Vec<u8>,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    read: &mut TcpOwnedReadHalf,
) -> IOResult<(usize, Vec<u8>)> {
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
                err_msg = cancelation => {
                    let err = std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        String::from_utf8_lossy(&err_msg)
                    );
                    return Err(err);
                }
                res_tuple = read.read(io_buf) => {
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
    read_buf: &mut Vec<u8>,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<()> {
    let mut offset = 0;
    loop {
        match Cmd::parse(&read_buf[offset..]) {
            Some((cmd, n)) => {
                offset += n;
                dispatch(cmd, conn_state, registry, art).await?;
            }
            None => {
                read_buf.drain(..offset);
                return Ok(());
            }
        }
    }
}

async fn dispatch(
    cmd: Cmd,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<()> {
    match conn_state {
        ConnState::PubSub(_) => match cmd {
            Cmd::Subscribe(channels) => {
                cmd_subscribe(&*channels.into_shareds(), conn_state, registry).await?
            }
            Cmd::Unsubscribe(channels) => {
                cmd_unsubscribe(&*channels.into_shareds(), conn_state, registry).await?
            }
            Cmd::Ping(msg) => conn_state.send(resp_pong(msg), registry).await?,
            Cmd::Quit => {
                conn_state.send(resp_ok(), registry).await?;
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            }
            _ => {
                conn_state
                    .send(
                        Frame::Error(
                            "ERR only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT allowed".into(),
                        ),
                        registry,
                    )
                    .await?;
            }
        },

        ConnState::Normal(_, _) => match cmd {
            Cmd::Info => {
                let info = SharedByte::from_slice(
                    b"# Server\r\nredis_version:7.0.0\r\nradixox_version:0.5.0\r\n",
                );
                conn_state.send(Frame::BulkString(info), registry).await?;
            }
            Cmd::Ping(msg) => conn_state.send(resp_pong(msg), registry).await?,
            Cmd::Quit => {
                conn_state.send(resp_ok(), registry).await?;
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            }
            Cmd::Echo(msg) => {
                conn_state
                    .send(Frame::BulkString(msg.into_shared()), registry)
                    .await?
            }
            Cmd::Select(_) => conn_state.send(resp_ok(), registry).await?,
            Cmd::Subscribe(channels) => {
                cmd_subscribe(&*channels.into_shareds(), conn_state, registry).await?
            }
            Cmd::Publish { channel, message } => {
                let a = [channel.into_shared(), message.into_shared()];
                cmd_publish(&a, conn_state, registry).await?;
            }
            Cmd::HMSet { key, fields } => {
                // HMSET doit répondre +OK (jedis attend un status, pas un entier)
                let result = cmd_hset(
                    &mut art.borrow_mut(),
                    key.into_shared(),
                    &*fields.into_shareds(),
                );
                let frame = match result {
                    Frame::Error(_) => result,
                    _ => resp_ok(),
                };
                conn_state.send(frame, registry).await?;
            }
            cmd => {
                let frame = run_cmd(cmd, art).await;
                conn_state.send(frame, registry).await?;
            }
        },

        _ => {}
    }
    Ok(())
}

// ── Command execution ─────────────────────────────────────────────────────────

async fn run_cmd(cmd: Cmd, art: &SharedART) -> Frame {
    match cmd {
        // Async commands
        Cmd::Keys(pattern) => cmd_keys(pattern.into_shared(), art.clone()).await,
        Cmd::Unlink(keys) => {
            let v: Vec<SharedByte> = keys.into_shareds().into_iter().collect();
            cmd_unlink(v, art.clone()).await
        }
        // Sync commands
        cmd => execute_sync(cmd, &mut art.borrow_mut()),
    }
}

fn execute_sync(cmd: Cmd, art: &mut OxidArt) -> Frame {
    match cmd {
        // ── String / Keys ────────────────────────────────────────────────────
        Cmd::Get(key) => cmd_get(art, key.into_shared()),
        Cmd::Set { key, val, opts } => cmd_set(art, key.into_shared(), val.into_shared(), opts),
        Cmd::MGet(keys) => cmd_mget(art, &*keys.into_shareds()),
        Cmd::MSet(pairs) => cmd_mset(art, &*pairs.into_shareds()),
        Cmd::Del(keys) => cmd_del(art, &*keys.into_shareds()),
        Cmd::Exists(keys) => cmd_exists(art, &*keys.into_shareds()),
        Cmd::Type(key) => cmd_type(art, key.into_shared()),
        // ── TTL ──────────────────────────────────────────────────────────────
        Cmd::Ttl(key) => cmd_ttl(art, key.into_shared()),
        Cmd::Pttl(key) => cmd_pttl(art, key.into_shared()),
        Cmd::Expire { key, dur } => cmd_expire(art, key.into_shared(), dur),
        Cmd::Persist(key) => cmd_persist(art, key.into_shared()),
        // ── Counters ─────────────────────────────────────────────────────────
        Cmd::Incr(key) => cmd_incr(art, key.into_shared()),
        Cmd::Decr(key) => cmd_decr(art, key.into_shared()),
        Cmd::IncrBy { key, delta } => cmd_incrby(art, key.into_shared(), delta),
        Cmd::DecrBy { key, delta } => cmd_decrby(art, key.into_shared(), delta),
        // ── Server ───────────────────────────────────────────────────────────
        Cmd::DbSize => cmd_dbsize(art),
        Cmd::FlushDb => cmd_flushdb(art),
        // ── Hash ─────────────────────────────────────────────────────────────
        Cmd::HSet { key, fields } => cmd_hset(art, key.into_shared(), &*fields.into_shareds()),
        Cmd::HGet { key, field } => cmd_hget(art, key.into_shared(), field.into_shared()),
        Cmd::HGetAll(key) => cmd_hgetall(art, key.into_shared()),
        Cmd::HDel { key, fields } => cmd_hdel(art, key.into_shared(), &*fields.into_shareds()),
        Cmd::HExists { key, field } => cmd_hexists(art, key.into_shared(), field.into_shared()),
        Cmd::HLen(key) => cmd_hlen(art, key.into_shared()),
        Cmd::HKeys(key) => cmd_hkeys(art, key.into_shared()),
        Cmd::HVals(key) => cmd_hvals(art, key.into_shared()),
        Cmd::HMGet { key, fields } => cmd_hmget(art, key.into_shared(), &*fields.into_shareds()),
        Cmd::HIncrBy { key, field, delta } => {
            cmd_hincrby(art, key.into_shared(), field.into_shared(), delta)
        }
        // ── Set ──────────────────────────────────────────────────────────────
        Cmd::SAdd { key, members } => cmd_sadd(art, key.into_shared(), &*members.into_shareds()),
        Cmd::SRem { key, members } => cmd_srem(art, key.into_shared(), &*members.into_shareds()),
        Cmd::SIsMember { key, member } => {
            cmd_sismember(art, key.into_shared(), member.into_shared())
        }
        Cmd::SCard(key) => cmd_scard(art, key.into_shared()),
        Cmd::SMembers(key) => cmd_smembers(art, key.into_shared()),
        Cmd::SPop { key, count } => cmd_spop(art, key.into_shared(), count),
        // ── ZSet ─────────────────────────────────────────────────────────────
        Cmd::ZAdd { key, members } => {
            let m = members.into_shareds();
            cmd_zadd(art, key.into_shared(), &*m)
        }
        Cmd::ZCard(key) => cmd_zcard(art, key.into_shared()),
        Cmd::ZRange {
            key,
            start,
            stop,
            with_scores,
        } => cmd_zrange(art, key.into_shared(), start, stop, with_scores),
        Cmd::ZScore { key, member } => cmd_zscore(art, key.into_shared(), member.into_shared()),
        Cmd::ZRem { key, members } => cmd_zrem(art, key.into_shared(), &*members.into_shareds()),
        Cmd::ZIncrBy { key, delta, member } => {
            cmd_zincrby(art, key.into_shared(), delta, member.into_shared())
        }
        // ── Should not reach here (handled in dispatch) ───────────────────
        Cmd::Ping(_)
        | Cmd::Quit
        | Cmd::Echo(_)
        | Cmd::Select(_)
        | Cmd::Subscribe(_)
        | Cmd::Unsubscribe(_)
        | Cmd::Publish { .. }
        | Cmd::Keys(_)
        | Cmd::Unlink(_)
        | Cmd::Info
        | Cmd::HMSet { .. } => unreachable!(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn resp_pong(msg: Option<OwnedByte>) -> Frame {
    match msg {
        Some(m) => Frame::BulkString(m.into_shared()),
        None => Frame::SimpleString(SharedByte::from_slice(b"PONG")),
    }
}

fn resp_ok() -> Frame {
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}
