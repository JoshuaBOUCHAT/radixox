use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use crossbeam::channel::{bounded, Receiver, Sender};
use oxidart::{OxidArt, TtlResult};
use redis_protocol::resp2::decode::decode_bytes_mut;
use redis_protocol::resp2::encode::extend_encode;
use redis_protocol::resp2::types::BytesFrame as Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BUFFER_SIZE: usize = 64 * 1024;
const COMMAND_CHANNEL_SIZE: usize = 16_384; // ~2Mo de buffer pour les commands
const RESPONSE_CHANNEL_SIZE: usize = 8; // petit buffer par connexion

// Static responses (avoid allocation)
static PONG: Bytes = Bytes::from_static(b"PONG");
static OK: Bytes = Bytes::from_static(b"OK");

// =============================================================================
// Command & Response types
// =============================================================================

/// Command envoyé au worker OxidArt
enum Command {
    /// Opération client
    Op { id: usize, op: Op },
    /// Tick pour mettre à jour le temps (TTL)
    Tick,
    /// Evict les clés expirées
    Evict,
}

/// Opérations supportées - tout pré-parsé en Bytes (zero-copy)
enum Op {
    // Meta (pas besoin de data)
    Ping,
    Quit,
    Select,
    Echo { msg: Bytes },

    // Data operations
    Get { key: Bytes },
    Set { key: Bytes, value: Bytes, ttl: Option<Duration>, condition: SetCondition },
    Del { keys: Vec<Bytes> },
    Keys { prefix: Bytes },
    Ttl { key: Bytes },
    Pttl { key: Bytes },
    Expire { key: Bytes, secs: u64 },
    Pexpire { key: Bytes, ms: u64 },
    Persist { key: Bytes },
    Exists { keys: Vec<Bytes> },
    Mget { keys: Vec<Bytes> },
    Mset { pairs: Vec<(Bytes, Bytes)> },
    Setnx { key: Bytes, value: Bytes },
    Setex { key: Bytes, secs: u64, value: Bytes },
    Type { key: Bytes },
    DbSize,
    FlushDb,
}

#[derive(Default, Clone, Copy)]
enum SetCondition {
    #[default]
    Always,
    IfNotExists, // NX
    IfExists,    // XX
}

/// Réponse du worker OxidArt
enum Response {
    Frame(Frame),
}

// =============================================================================
// Channels registry (lock-free via boxcar)
// =============================================================================

struct ChannelRegistry {
    senders: boxcar::Vec<Sender<Response>>,
}

impl ChannelRegistry {
    fn new() -> Self {
        Self {
            senders: boxcar::Vec::new(),
        }
    }

    /// Alloue un nouveau slot et retourne (id, receiver)
    fn allocate(&self) -> (usize, Receiver<Response>) {
        let (tx, rx) = bounded(RESPONSE_CHANNEL_SIZE);
        let id = self.senders.push(tx);
        (id, rx)
    }

    /// Envoie une réponse au slot donné
    fn send(&self, id: usize, response: Response) {
        if let Some(sender) = self.senders.get(id) {
            let _ = sender.send(response);
        }
    }
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:6379").await?;
    println!("RadixOx RESP Multithread Server listening on 0.0.0.0:6379");

    // Channel pour les commands vers le worker OxidArt
    let (cmd_tx, cmd_rx): (Sender<Command>, Receiver<Command>) = bounded(COMMAND_CHANNEL_SIZE);

    // Registry des channels de réponse (lock-free)
    let registry = Arc::new(ChannelRegistry::new());

    // Spawn le worker OxidArt (single thread, bloquant, lock-free)
    let registry_clone = registry.clone();
    std::thread::spawn(move || {
        oxidart_worker(cmd_rx, registry_clone);
    });

    // Spawn ticker thread (envoie Tick toutes les 100ms)
    let cmd_tx_ticker = cmd_tx.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if cmd_tx_ticker.send(Command::Tick).is_err() {
                break;
            }
        }
    });

    // Spawn evictor thread (envoie Evict toutes les secondes)
    let cmd_tx_evictor = cmd_tx.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if cmd_tx_evictor.send(Command::Evict).is_err() {
                break;
            }
        }
    });

    // Accept loop
    loop {
        let (stream, addr) = listener.accept().await?;
        println!("New connection from {}", addr);

        let cmd_tx = cmd_tx.clone();
        let (id, resp_rx) = registry.allocate();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, id, cmd_tx, resp_rx).await {
                eprintln!("Connection error: {}", e);
            }
            println!("Connection {} closed", addr);
        });
    }
}

// =============================================================================
// OxidArt Worker (single thread, bloquant, lock-free hot path)
// =============================================================================

fn oxidart_worker(cmd_rx: Receiver<Command>, registry: Arc<ChannelRegistry>) {
    let mut art = OxidArt::new();
    art.tick();

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::Tick => {
                art.tick();
            }
            Command::Evict => {
                art.evict_expired();
            }
            Command::Op { id, op } => {
                let frame = execute_op(&mut art, op);
                registry.send(id, Response::Frame(frame));
            }
        }
    }
}

fn execute_op(art: &mut OxidArt, op: Op) -> Frame {
    match op {
        // Meta commands
        Op::Ping => Frame::SimpleString(PONG.clone()),
        Op::Quit => Frame::SimpleString(OK.clone()),
        Op::Select => Frame::SimpleString(OK.clone()),
        Op::Echo { msg } => Frame::BulkString(msg),

        // Data commands
        Op::Get { key } => match art.get(key) {
            Some(val) => Frame::BulkString(val),
            None => Frame::Null,
        },

        Op::Set { key, value, ttl, condition } => {
            let key_exists = art.get(key.clone()).is_some();
            match condition {
                SetCondition::IfNotExists if key_exists => return Frame::Null,
                SetCondition::IfExists if !key_exists => return Frame::Null,
                _ => {}
            }
            match ttl {
                Some(duration) => art.set_ttl(key, duration, value),
                None => art.set(key, value),
            }
            Frame::SimpleString(OK.clone())
        }

        Op::Del { keys } => {
            let mut count = 0i64;
            for key in keys {
                if art.del(key).is_some() {
                    count += 1;
                }
            }
            Frame::Integer(count)
        }

        Op::Keys { prefix } => {
            let pairs = art.getn(prefix);
            let keys: Vec<Frame> = pairs
                .into_iter()
                .map(|(k, _)| Frame::BulkString(k))
                .collect();
            Frame::Array(keys)
        }

        Op::Ttl { key } => match art.get_ttl(key) {
            TtlResult::KeyNotExist => Frame::Integer(-2),
            TtlResult::KeyWithoutTtl => Frame::Integer(-1),
            TtlResult::KeyWithTtl(secs) => Frame::Integer(secs as i64),
        },

        Op::Pttl { key } => match art.get_ttl(key) {
            TtlResult::KeyNotExist => Frame::Integer(-2),
            TtlResult::KeyWithoutTtl => Frame::Integer(-1),
            TtlResult::KeyWithTtl(secs) => Frame::Integer((secs * 1000) as i64),
        },

        Op::Expire { key, secs } => {
            if art.expire(key, Duration::from_secs(secs)) {
                Frame::Integer(1)
            } else {
                Frame::Integer(0)
            }
        }

        Op::Pexpire { key, ms } => {
            if art.expire(key, Duration::from_millis(ms)) {
                Frame::Integer(1)
            } else {
                Frame::Integer(0)
            }
        }

        Op::Persist { key } => {
            if art.persist(key) {
                Frame::Integer(1)
            } else {
                Frame::Integer(0)
            }
        }

        Op::Exists { keys } => {
            let mut count = 0i64;
            for key in keys {
                if art.get(key).is_some() {
                    count += 1;
                }
            }
            Frame::Integer(count)
        }

        Op::Mget { keys } => {
            let results: Vec<Frame> = keys
                .into_iter()
                .map(|key| match art.get(key) {
                    Some(val) => Frame::BulkString(val),
                    None => Frame::Null,
                })
                .collect();
            Frame::Array(results)
        }

        Op::Mset { pairs } => {
            for (key, value) in pairs {
                art.set(key, value);
            }
            Frame::SimpleString(OK.clone())
        }

        Op::Setnx { key, value } => {
            if art.get(key.clone()).is_some() {
                Frame::Integer(0)
            } else {
                art.set(key, value);
                Frame::Integer(1)
            }
        }

        Op::Setex { key, secs, value } => {
            art.set_ttl(key, Duration::from_secs(secs), value);
            Frame::SimpleString(OK.clone())
        }

        Op::Type { key } => match art.get(key) {
            Some(_) => Frame::SimpleString(Bytes::from_static(b"string")),
            None => Frame::SimpleString(Bytes::from_static(b"none")),
        },

        Op::DbSize => {
            let count = art.getn(Bytes::new()).len() as i64;
            Frame::Integer(count)
        }

        Op::FlushDb => {
            art.deln(Bytes::new());
            Frame::SimpleString(OK.clone())
        }
    }
}

// =============================================================================
// Connection Handler (parse RESP, send commands, await response)
// =============================================================================

async fn handle_connection(
    mut stream: TcpStream,
    id: usize,
    cmd_tx: Sender<Command>,
    resp_rx: Receiver<Response>,
) -> std::io::Result<()> {
    let mut read_buf = BytesMut::with_capacity(BUFFER_SIZE);
    let mut write_buf = BytesMut::with_capacity(BUFFER_SIZE);
    let mut temp_buf = vec![0u8; BUFFER_SIZE];

    loop {
        // Read from socket
        let n = stream.read(&mut temp_buf).await?;
        if n == 0 {
            return Ok(()); // Connection closed
        }

        read_buf.extend_from_slice(&temp_buf[..n]);

        // Parse and dispatch commands
        loop {
            match decode_bytes_mut(&mut read_buf) {
                Ok(Some((frame, _consumed, _buf))) => {
                    match parse_command(frame) {
                        Ok(op) => {
                            // Send to OxidArt worker
                            if cmd_tx.send(Command::Op { id, op }).is_err() {
                                return Ok(()); // Worker shutdown
                            }

                            // Wait for response
                            match resp_rx.recv() {
                                Ok(Response::Frame(response)) => {
                                    extend_encode(&mut write_buf, &response)
                                        .expect("encode should not fail");
                                }
                                Err(_) => return Ok(()), // Channel closed
                            }
                        }
                        Err(err_frame) => {
                            extend_encode(&mut write_buf, &err_frame)
                                .expect("encode should not fail");
                        }
                    }
                }
                Ok(None) => break, // Need more data
                Err(e) => {
                    eprintln!("Parse error: {:?}", e);
                    let err = Frame::Error(format!("ERR parse error: {:?}", e).into());
                    extend_encode(&mut write_buf, &err).expect("encode should not fail");
                    break;
                }
            }
        }

        // Write all responses
        if !write_buf.is_empty() {
            stream.write_all(&write_buf).await?;
            write_buf.clear();
        }
    }
}

// =============================================================================
// RESP Parsing -> Op (zero-copy avec Bytes)
// =============================================================================

fn parse_command(frame: Frame) -> Result<Op, Frame> {
    let args = frame_to_args(frame).ok_or_else(|| Frame::Error("ERR invalid command".into()))?;

    if args.is_empty() {
        return Err(Frame::Error("ERR empty command".into()));
    }

    let cmd = &args[0];

    // Match command (case-insensitive)
    if cmd.eq_ignore_ascii_case(b"PING") {
        Ok(Op::Ping)
    } else if cmd.eq_ignore_ascii_case(b"QUIT") {
        Ok(Op::Quit)
    } else if cmd.eq_ignore_ascii_case(b"SELECT") {
        Ok(Op::Select)
    } else if cmd.eq_ignore_ascii_case(b"ECHO") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'ECHO' command".into()));
        }
        Ok(Op::Echo { msg: args[1].clone() })
    } else if cmd.eq_ignore_ascii_case(b"GET") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'GET' command".into()));
        }
        Ok(Op::Get { key: args[1].clone() })
    } else if cmd.eq_ignore_ascii_case(b"SET") {
        if args.len() < 3 {
            return Err(Frame::Error("ERR wrong number of arguments for 'SET' command".into()));
        }
        let key = args[1].clone();
        let value = args[2].clone();
        let (ttl, condition) = parse_set_options(&args[3..])?;
        Ok(Op::Set { key, value, ttl, condition })
    } else if cmd.eq_ignore_ascii_case(b"DEL") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'DEL' command".into()));
        }
        Ok(Op::Del { keys: args[1..].to_vec() })
    } else if cmd.eq_ignore_ascii_case(b"KEYS") {
        let prefix = if args.len() > 1 {
            let mut p = args[1].clone();
            if p.ends_with(b"*") {
                p = p.slice(..p.len() - 1);
            }
            p
        } else {
            Bytes::new()
        };
        Ok(Op::Keys { prefix })
    } else if cmd.eq_ignore_ascii_case(b"TTL") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'TTL' command".into()));
        }
        Ok(Op::Ttl { key: args[1].clone() })
    } else if cmd.eq_ignore_ascii_case(b"PTTL") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'PTTL' command".into()));
        }
        Ok(Op::Pttl { key: args[1].clone() })
    } else if cmd.eq_ignore_ascii_case(b"EXPIRE") {
        if args.len() < 3 {
            return Err(Frame::Error("ERR wrong number of arguments for 'EXPIRE' command".into()));
        }
        let secs = parse_int(&args[2])
            .ok_or_else(|| Frame::Error("ERR value is not an integer".into()))?;
        Ok(Op::Expire { key: args[1].clone(), secs })
    } else if cmd.eq_ignore_ascii_case(b"PEXPIRE") {
        if args.len() < 3 {
            return Err(Frame::Error("ERR wrong number of arguments for 'PEXPIRE' command".into()));
        }
        let ms = parse_int(&args[2])
            .ok_or_else(|| Frame::Error("ERR value is not an integer".into()))?;
        Ok(Op::Pexpire { key: args[1].clone(), ms })
    } else if cmd.eq_ignore_ascii_case(b"PERSIST") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'PERSIST' command".into()));
        }
        Ok(Op::Persist { key: args[1].clone() })
    } else if cmd.eq_ignore_ascii_case(b"EXISTS") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'EXISTS' command".into()));
        }
        Ok(Op::Exists { keys: args[1..].to_vec() })
    } else if cmd.eq_ignore_ascii_case(b"MGET") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'MGET' command".into()));
        }
        Ok(Op::Mget { keys: args[1..].to_vec() })
    } else if cmd.eq_ignore_ascii_case(b"MSET") {
        if args.len() < 3 || (args.len() - 1) % 2 != 0 {
            return Err(Frame::Error("ERR wrong number of arguments for 'MSET' command".into()));
        }
        let pairs: Vec<(Bytes, Bytes)> = args[1..]
            .chunks_exact(2)
            .map(|c| (c[0].clone(), c[1].clone()))
            .collect();
        Ok(Op::Mset { pairs })
    } else if cmd.eq_ignore_ascii_case(b"SETNX") {
        if args.len() < 3 {
            return Err(Frame::Error("ERR wrong number of arguments for 'SETNX' command".into()));
        }
        Ok(Op::Setnx { key: args[1].clone(), value: args[2].clone() })
    } else if cmd.eq_ignore_ascii_case(b"SETEX") {
        if args.len() < 4 {
            return Err(Frame::Error("ERR wrong number of arguments for 'SETEX' command".into()));
        }
        let secs = parse_int(&args[2])
            .ok_or_else(|| Frame::Error("ERR value is not an integer or out of range".into()))?;
        Ok(Op::Setex { key: args[1].clone(), secs, value: args[3].clone() })
    } else if cmd.eq_ignore_ascii_case(b"TYPE") {
        if args.len() < 2 {
            return Err(Frame::Error("ERR wrong number of arguments for 'TYPE' command".into()));
        }
        Ok(Op::Type { key: args[1].clone() })
    } else if cmd.eq_ignore_ascii_case(b"DBSIZE") {
        Ok(Op::DbSize)
    } else if cmd.eq_ignore_ascii_case(b"FLUSHDB") {
        Ok(Op::FlushDb)
    } else {
        Err(Frame::Error(
            format!("ERR unknown command '{}'", String::from_utf8_lossy(cmd)).into(),
        ))
    }
}

fn frame_to_args(frame: Frame) -> Option<Vec<Bytes>> {
    match frame {
        Frame::Array(arr) => {
            let mut args = Vec::with_capacity(arr.len());
            for f in arr {
                match f {
                    Frame::BulkString(b) => args.push(b),
                    Frame::SimpleString(s) => args.push(s),
                    _ => return None,
                }
            }
            Some(args)
        }
        Frame::BulkString(b) => Some(vec![b]),
        Frame::SimpleString(s) => Some(vec![s]),
        _ => None,
    }
}

fn parse_set_options(args: &[Bytes]) -> Result<(Option<Duration>, SetCondition), Frame> {
    let mut ttl = None;
    let mut condition = SetCondition::Always;
    let mut i = 0;

    while i < args.len() {
        if args[i].eq_ignore_ascii_case(b"EX") {
            i += 1;
            if i >= args.len() {
                return Err(Frame::Error("ERR syntax error".into()));
            }
            let secs: u64 = parse_int(&args[i])
                .ok_or_else(|| Frame::Error("ERR value is not an integer or out of range".into()))?;
            ttl = Some(Duration::from_secs(secs));
        } else if args[i].eq_ignore_ascii_case(b"PX") {
            i += 1;
            if i >= args.len() {
                return Err(Frame::Error("ERR syntax error".into()));
            }
            let ms: u64 = parse_int(&args[i])
                .ok_or_else(|| Frame::Error("ERR value is not an integer or out of range".into()))?;
            ttl = Some(Duration::from_millis(ms));
        } else if args[i].eq_ignore_ascii_case(b"NX") {
            condition = SetCondition::IfNotExists;
        } else if args[i].eq_ignore_ascii_case(b"XX") {
            condition = SetCondition::IfExists;
        }
        i += 1;
    }

    Ok((ttl, condition))
}

fn parse_int<T: std::str::FromStr>(arg: &Bytes) -> Option<T> {
    std::str::from_utf8(arg).ok().and_then(|s| s.parse().ok())
}
