#[cfg(not(target_os = "linux"))]
compile_error!("RadixOx requires Linux to run (io_uring and mmap support).");

mod resp_cmd;
mod utils;

use std::cell::RefCell;
use std::env;
use std::rc::Rc;
use std::time::Duration;

use monoio::buf::SliceMut;
use monoio::io::Splitable;
use monoio::net::tcp::TcpOwnedReadHalf;
use monoio::net::{TcpListener, TcpStream};
use monoio::time::TimeDriver;
use monoio::{IoUringDriver, Runtime, RuntimeBuilder};

use oxidart::OxidArt;
use radixox_lib::cmd::Cmd;
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

const BUFFER_SIZE: usize = 512 * 1024;
static ERR_EMPTY_CMD: &str = "ERR empty command";
const NB_ACCEPTOR: usize = 16;

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> std::io::Result<()> {
    let mut runtime = get_runtime()?;
    handle_cpu_pin_env_var();

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
fn handle_cpu_pin_env_var() {
    let cpu_pin_id = env::var("RADIXOX_CPU_PIN")
        .map(|cpu_id_str| cpu_id_str.parse::<u64>())
        .ok();
    if let Some(cpu_pin_id) = cpu_pin_id {
        match cpu_pin_id {
            Ok(id) if id < 64 => {
                if pin_thread_to_core(id) {
                    println!("CPU pinned: {}", id);
                }
            }
            Ok(to_large_id) => {
                eprintln!("CPU PIN ID should be <64 but is is:{}", to_large_id)
            }
            Err(err) => {
                eprintln!("Error while parsing RADIXOX_CPU_PIN: {}", err);
            }
        }
    }
}

unsafe extern "C" {
    fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u8) -> i32;
}

fn pin_thread_to_core(core_id: u64) -> bool {
    // Sur Linux, un cpu_set_t est fondamentalement un tableau de bits.
    // Pour 64 cœurs, 8 octets suffisent (64 bits).
    let mask: u64 = 1 << core_id;

    unsafe {
        // pid 0 signifie "le thread actuel"
        let result = sched_setaffinity(
            0,
            std::mem::size_of::<u64>(),
            &mask as *const u64 as *const u8,
        );

        result == 0
    }
}
fn get_runtime() -> std::io::Result<Runtime<TimeDriver<IoUringDriver>>> {
    let mut builder = io_uring::IoUring::builder();
    if let Ok(sq_val) = env::var("SQ_POLL")
        && let Ok(idle) = sq_val.parse::<u32>()
    {
        builder.setup_sqpoll(idle);
        println!("Radixox launched with SQ_POLL idle: {}ms", idle);

        let sq_pin_id = env::var("RADIXOX_SQ_POLL_PIN")
            .map(|s| s.parse::<u32>())
            .ok();
        if let Some(sq_pin_id) = sq_pin_id {
            match sq_pin_id {
                Ok(id) if id < 64 => {
                    builder.setup_sqpoll_cpu(id);
                    println!("SQ_POLL pinned to CPU: {}", id);
                }
                Ok(id) => eprintln!("RADIXOX_SQ_POLL_PIN: {} >= 64, ignoring", id),
                Err(err) => eprintln!("RADIXOX_SQ_POLL_PIN parse error: {}", err),
            }
        } else {
            builder.setup_coop_taskrun();
        }
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
    let mut io_buf_offset = 0;

    loop {
        let io_buf_cap = io_buf.capacity();
        let slice = SliceMut::new(io_buf, io_buf_offset, io_buf_cap);
        let (n, returned) = conn_state.read_and_flush(slice, registry, read).await?;
        (io_buf, read_buf) = (read_buf, returned.into_inner());

        if n == 0 {
            return Ok(());
        }

        let consume_byte_count = handle_buffer(&mut read_buf, conn_state, registry, art).await?;
        io_buf_offset = read_buf.len() - consume_byte_count;
        io_buf.extend_from_slice(&read_buf[consume_byte_count..]);
        read_buf.clear();
    }
}

// ── Buffer parsing & dispatch ─────────────────────────────────────────────────

async fn handle_buffer(
    read_buf: &mut Vec<u8>,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<usize> {
    let mut offset = 0;
    while let Some((cmd, n)) = Cmd::parse(&read_buf[offset..]) {
        offset += n;
        dispatch(cmd, conn_state, registry, art).await?;
    }
    Ok(offset)
}

async fn dispatch(
    cmd: Cmd<'_>,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
) -> IOResult<()> {
    match conn_state {
        ConnState::PubSub(_) => match cmd {
            Cmd::Subscribe(channels) => cmd_subscribe(&channels, conn_state, registry).await?,
            Cmd::Unsubscribe(channels) => {
                cmd_unsubscribe(&channels, conn_state, registry).await?
            }
            Cmd::Ping(msg) => conn_state.encode(resp_pong(msg)),
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
                conn_state.encode(Frame::BulkString(info));
            }
            Cmd::Ping(msg) => conn_state.encode(resp_pong(msg)),
            Cmd::Quit => {
                conn_state.encode(resp_ok());
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            }
            Cmd::Echo(msg) => conn_state.encode(Frame::BulkString(SharedByte::from_slice(msg))),
            Cmd::Select(_) => conn_state.encode(resp_ok()),
            Cmd::Subscribe(channels) => cmd_subscribe(&channels, conn_state, registry).await?,
            Cmd::Publish { channel, message } => {
                let a = [channel, message];
                cmd_publish(&a, conn_state, registry).await?;
            }
            Cmd::HMSet { key, fields } => {
                // HMSET doit répondre +OK (jedis attend un status, pas un entier)
                let result = cmd_hset(&mut art.borrow_mut(), key, &fields);
                let frame = match result {
                    Frame::Error(_) => result,
                    _ => resp_ok(),
                };
                conn_state.encode(frame);
            }
            Cmd::Keys(pattern) => {
                let frame = cmd_keys(pattern, art.clone()).await;
                conn_state.encode(frame);
            }
            Cmd::Unlink(keys) => {
                let v: Vec<SharedByte> = keys.into_iter().collect();
                let frame = cmd_unlink(v, art.clone()).await;
                conn_state.encode(frame);
            }
            Cmd::Unknown => {
                conn_state.encode(Frame::Error("ERR unknown command".into()));
            }
            cmd => {
                let frame = execute_sync(cmd, &mut art.borrow_mut());
                conn_state.encode(frame);
            }
        },

        _ => {}
    }
    Ok(())
}

// ── Command execution ─────────────────────────────────────────────────────────

fn execute_sync(cmd: Cmd<'_>, art: &mut OxidArt) -> Frame {
    match cmd {
        // ── String / Keys ────────────────────────────────────────────────────
        Cmd::Get(key) => cmd_get(art, key),
        Cmd::Set { key, val, opts } => cmd_set(art, key, val, opts),
        Cmd::MGet(keys) => cmd_mget(art, &keys),
        Cmd::MSet(pairs) => cmd_mset(art, &pairs),
        Cmd::Del(keys) => cmd_del(art, &keys),
        Cmd::Exists(keys) => cmd_exists(art, &keys),
        Cmd::Type(key) => cmd_type(art, key),
        // ── TTL ──────────────────────────────────────────────────────────────
        Cmd::Ttl(key) => cmd_ttl(art, key),
        Cmd::Pttl(key) => cmd_pttl(art, key),
        Cmd::Expire { key, dur } => cmd_expire(art, key, dur),
        Cmd::Persist(key) => cmd_persist(art, key),
        // ── Counters ─────────────────────────────────────────────────────────
        Cmd::Incr(key) => cmd_incr(art, key),
        Cmd::Decr(key) => cmd_decr(art, key),
        Cmd::IncrBy { key, delta } => cmd_incrby(art, key, delta),
        Cmd::DecrBy { key, delta } => cmd_decrby(art, key, delta),
        // ── Server ───────────────────────────────────────────────────────────
        Cmd::DbSize => cmd_dbsize(art),
        Cmd::FlushDb => cmd_flushdb(art),
        // ── Hash ─────────────────────────────────────────────────────────────
        Cmd::HSet { key, fields } => cmd_hset(art, key, &fields),
        Cmd::HGet { key, field } => cmd_hget(art, key, field),
        Cmd::HGetAll(key) => cmd_hgetall(art, key),
        Cmd::HDel { key, fields } => cmd_hdel(art, key, &fields),
        Cmd::HExists { key, field } => cmd_hexists(art, key, field),
        Cmd::HLen(key) => cmd_hlen(art, key),
        Cmd::HKeys(key) => cmd_hkeys(art, key),
        Cmd::HVals(key) => cmd_hvals(art, key),
        Cmd::HMGet { key, fields } => cmd_hmget(art, key, &fields),
        Cmd::HIncrBy { key, field, delta } => cmd_hincrby(art, key, field, delta),
        // ── Set ──────────────────────────────────────────────────────────────
        Cmd::SAdd { key, members } => cmd_sadd(art, key, &members),
        Cmd::SRem { key, members } => cmd_srem(art, key, &members),
        Cmd::SIsMember { key, member } => cmd_sismember(art, key, member),
        Cmd::SCard(key) => cmd_scard(art, key),
        Cmd::SMembers(key) => cmd_smembers(art, key),
        Cmd::SPop { key, count } => cmd_spop(art, key, count),
        // ── ZSet ─────────────────────────────────────────────────────────────
        Cmd::ZAdd { key, members } => cmd_zadd(art, key, &members),
        Cmd::ZCard(key) => cmd_zcard(art, key),
        Cmd::ZRange {
            key,
            start,
            stop,
            with_scores,
        } => cmd_zrange(art, key, start, stop, with_scores),
        Cmd::ZScore { key, member } => cmd_zscore(art, key, member),
        Cmd::ZRem { key, members } => cmd_zrem(art, key, &members),
        Cmd::ZIncrBy { key, delta, member } => cmd_zincrby(art, key, delta, member),
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
        | Cmd::HMSet { .. }
        | Cmd::Unknown => unreachable!(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn resp_pong(msg: Option<&[u8]>) -> Frame {
    match msg {
        Some(m) => Frame::BulkString(SharedByte::from_slice(m)),
        None => Frame::SimpleString(SharedByte::from_slice(b"PONG")),
    }
}

fn resp_ok() -> Frame {
    Frame::SimpleString(SharedByte::from_slice(b"OK"))
}
