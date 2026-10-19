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
use monoio::net::{ListenerOpts, TcpListener, TcpStream};
use monoio::time::TimeDriver;
use monoio::{IoUringDriver, Runtime, RuntimeBuilder};

use oxidart::OxidArt;
use radixox_lib::cmd::{Cmd, Parse};
use radixox_lib::shared_byte::SharedByte;
pub(crate) use radixox_lib::shared_frame::SharedFrame as Frame;

use resp_cmd::delayed::{cmd_flushdb, cmd_keys, cmd_unlink};
use resp_cmd::pub_sub::{cmd_publish, cmd_subscribe, cmd_unsubscribe};
use radixox::exec::{
    encode_krange, encode_mget, encode_scan, execute_sync, resp_ok, resp_pong,
};
use radixox::exec::cmd_hset;
use crate::resp_cmd::client::Client;
use crate::utils::{ConnState, SubRegistry};

pub(crate) type IOResult<T> = std::io::Result<T>;
type SharedART = Rc<RefCell<OxidArt>>;
pub(crate) type SharedRegistry = Rc<RefCell<SubRegistry>>;

/// Capacité initiale des buffers d'une connexion (lecture ×2, écriture). Ils
/// grandissent au besoin (grosse valeur, gros pipeline). À 512 Kio, 80
/// connexions memtier gardaient ~48 Mo de pages écrites en RSS.
const BUFFER_SIZE: usize = 64 * 1024;
/// Taille maximale du tampon d'entrée d'une connexion (frame en cours de
/// réception), comme `client-query-buffer-limit` de Redis. Au-delà : fermeture.
const QUERY_BUFFER_LIMIT: usize = 1 << 30;
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
        let shared_art =
            OxidArt::shared_with_evictor(Duration::from_millis(100), Duration::from_secs(1));
        resp_cmd::bgsave::load_at_startup(&shared_art)?;
        resp_cmd::shutdown::install(shared_art.clone());

        // 127.0.0.1 par défaut : sans `RADIXOX_BIND`, le serveur n'est pas
        // joignable depuis le réseau.
        let bind = env::var("RADIXOX_BIND").unwrap_or_else(|_| "127.0.0.1".into());
        let addr = if bind.contains(':') { format!("[{bind}]:{port}") } else { format!("{bind}:{port}") };
        if resp_cmd::auth::password().is_none() && bind != "127.0.0.1" && bind != "::1" && bind != "localhost" {
            eprintln!("WARNING: listening on {bind} without RADIXOX_PASSWORD: anyone who can reach this port has full access");
        }
        // monoio active SO_REUSEPORT par défaut : un second serveur lancé sur
        // le même port démarrait sans erreur et le noyau lui envoyait une
        // partie des connexions (données séparées). Ici, EADDRINUSE.
        let opts = ListenerOpts::new().reuse_port(false);
        let listener = Rc::new(TcpListener::bind_with_config(&addr, &opts)?);
        println!("RadixOx RESP Server listening on {addr}");

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
            // Sans ça, Nagle retient une écriture tant que la précédente n'est
            // pas acquittée (jusqu'à ~40 ms avec le delayed ACK du client).
            if let Err(e) = stream.set_nodelay(true) {
                eprintln!("set_nodelay: {e}");
            }
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
    let mut client = Client::new();

    loop {
        // Commande incomplète qui remplit tout le buffer (grosse valeur) : sans
        // place libre, la lecture rendrait 0 et la connexion serait fermée.
        if io_buf_offset == io_buf.capacity() {
            if io_buf_offset >= QUERY_BUFFER_LIMIT {
                return Err(std::io::Error::other("query buffer limit reached"));
            }
            io_buf.reserve(io_buf.capacity().min(QUERY_BUFFER_LIMIT - io_buf_offset));
        }
        let io_buf_cap = io_buf.capacity();
        let slice = SliceMut::new(io_buf, io_buf_offset, io_buf_cap);
        let (n, returned) = conn_state.read_and_flush(slice, registry, read).await?;
        (io_buf, read_buf) = (read_buf, returned.into_inner());

        if n == 0 {
            return Ok(());
        }
        resp_cmd::bgsave::note_read();

        let consume_byte_count =
            handle_buffer(&mut read_buf, conn_state, registry, art, &mut client).await?;
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
    client: &mut Client,
) -> IOResult<usize> {
    let window = prefetch_window().max(1);
    // Par fenêtres : parse jusqu'à `window` commandes, précharge leurs clés
    // ensemble (miss superposés), puis exécution dans l'ordre, inchangée.
    let read_buf = &*read_buf;
    let mut offset = 0;
    let mut batch: Vec<Cmd<'_>> = Vec::with_capacity(window);
    let mut keys: Vec<&[u8]> = Vec::with_capacity(window);
    let mut invalid = None;
    loop {
        while batch.len() < window {
            match Cmd::parse(&read_buf[offset..]) {
                Parse::Done(cmd, n) => {
                    offset += n;
                    keys.extend(cmd.main_key());
                    batch.push(cmd);
                }
                Parse::Incomplete => break,
                Parse::Invalid(msg) => {
                    invalid = Some(msg);
                    break;
                }
            }
        }
        if batch.is_empty() {
            break;
        }
        if keys.len() > 1 {
            art.borrow().prefetch_keys(&keys);
        }
        keys.clear();
        for cmd in batch.drain(..) {
            dispatch(cmd, conn_state, registry, art, client).await?;
        }
        if invalid.is_some() {
            break;
        }
    }
    match invalid {
        None => Ok(offset),
        // Flux invalide : les commandes valides d'avant ont répondu ; erreur
        // puis fermeture, comme Redis.
        Some(msg) => {
            let err = Frame::Error(format!("ERR Protocol error: {msg}").into());
            conn_state.send(err, registry).await?;
            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, msg))
        }
    }
}

/// Taille des fenêtres de préchargement (`RADIXOX_PREFETCH`, 8 par défaut,
/// 0 = désactivé).
fn prefetch_window() -> usize {
    static W: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *W.get_or_init(|| {
        std::env::var("RADIXOX_PREFETCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
            .min(oxidart::PREFETCH_BATCH)
    })
}

async fn dispatch(
    cmd: Cmd<'_>,
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
    art: &SharedART,
    client: &mut Client,
) -> IOResult<()> {
    // Non authentifié : forcément en mode Normal (SUBSCRIBE est refusé).
    if matches!(cmd, Cmd::Empty) {
        return Ok(());
    }
    if !client.authed && !cmd.allowed_unauthenticated() {
        conn_state.encode(resp_cmd::auth::noauth());
        return Ok(());
    }
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
                let info = format!(
                    "# Server\r\nredis_version:7.0.0\r\nradixox_version:{}\r\n\r\n# Stats\r\nscan_cursors:{}\r\n",
                    env!("CARGO_PKG_VERSION"),
                    resp_cmd::scan::active_cursors()
                );
                conn_state.encode(Frame::BulkString(SharedByte::from_slice(info.as_bytes())));
            }
            Cmd::Ping(msg) => conn_state.encode(resp_pong(msg)),
            Cmd::Auth { user, pass } => {
                conn_state.encode(resp_cmd::auth::cmd_auth(user, pass, &mut client.authed));
            }
            Cmd::Hello(args) => conn_state.encode(resp_cmd::client::cmd_hello(&args, client)),
            Cmd::Client(args) => conn_state.encode(resp_cmd::client::cmd_client(&args, client)),
            Cmd::Command(args) => conn_state.encode(resp_cmd::client::cmd_command(&args)),
            Cmd::Quit => {
                // +OK (et les réponses pipelinées avant) partent avant la fermeture.
                conn_state.encode(resp_ok());
                conn_state.flush().await?;
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
                let resp = cmd_keys(pattern, art).await;
                conn_state.encode_with(|buf| buf.extend_from_slice(&resp));
            }
            Cmd::BgSave => {
                let frame = if resp_cmd::bgsave::start_bgsave(&art) {
                    Frame::SimpleString(SharedByte::from_slice(b"Background saving started"))
                } else {
                    Frame::Error("ERR Background save already in progress".into())
                };
                conn_state.encode(frame);
            }
            Cmd::Save => {
                let frame = match resp_cmd::bgsave::save_sync(&art, resp_cmd::bgsave::sync_level()) {
                    Ok(st) => {
                        eprintln!(
                            "[save] {:.1} MB ({:.1} MB bruts) en {:.2?} (DFS {:.2?})",
                            st.written as f64 / 1e6,
                            st.raw as f64 / 1e6,
                            st.total,
                            st.walk
                        );
                        resp_ok()
                    }
                    Err(e) => Frame::Error(format!("ERR {e}").into()),
                };
                conn_state.encode(frame);
            }
            Cmd::FlushDb => {
                // Base détachée tout de suite ; libération en fond (après la
                // save si une save tourne : sinon tout l'arbre serait capturé).
                cmd_flushdb(art);
                conn_state.encode(resp_ok());
            }
            Cmd::Unlink(keys) => {
                let count = cmd_unlink(&keys, art).await;
                conn_state.encode(Frame::Integer(count));
            }
            Cmd::MGet(keys) => {
                // Descentes entrelacées + valeurs par référence, encodées
                // pendant l'emprunt (ni clone ni Vec<Frame>).
                let mut expired = Vec::new();
                {
                    let art = art.borrow();
                    let vals = art.mget_ref(&keys, &mut expired);
                    conn_state.encode_with(|buf| encode_mget(buf, &vals));
                }
                // Suppression à la volée des expirées croisées, après l'encodage.
                if !expired.is_empty() {
                    art.borrow_mut().purge_expired(&keys, &expired);
                }
            }
            Cmd::KRange { prefix, from, count, rev } => {
                // Encodé pendant l'emprunt de l'arbre : les valeurs sont des
                // références (pas de clone/refcount), préchargées au parcours.
                let art = art.borrow();
                let range = art.krange(prefix, from, count, rev);
                conn_state.encode_with(|buf| encode_krange(buf, &range));
            }
            Cmd::Scan { cursor, pattern, count, ty } => {
                let art = art.borrow();
                match resp_cmd::scan::cmd_scan(&art, client.id, cursor, pattern, count, ty) {
                    Ok((next, page)) => conn_state.encode_with(|buf| encode_scan(buf, next, &page)),
                    Err(e) => conn_state.encode(e),
                }
            }
            Cmd::Invalid { name, err } => conn_state.encode(Frame::Error(err.message(name).into())),
            cmd => {
                let frame = execute_sync(cmd, &mut art.borrow_mut());
                conn_state.encode(frame);
            }
        },

        _ => {}
    }
    Ok(())
}
