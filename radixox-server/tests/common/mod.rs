use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Spawn the radixox-resp binary on `port` and wait until it accepts connections.
///
/// Any orphaned radixox-resp processes still listening on `port` from a previous
/// test run are killed first (via `fuser -k`), so each test binary always starts
/// with a fresh server and clean state.
pub fn start_server(port: u16) {
    // Kill any orphan from a previous run holding this port.
    let _ = Command::new("fuser")
        .args(["-k", &format!("{port}/tcp")])
        .stderr(Stdio::null())
        .status();
    // Give the OS a moment to release the port.
    std::thread::sleep(Duration::from_millis(150));

    let bin = env!("CARGO_BIN_EXE_radixox-resp");
    let child = Command::new(bin)
        .env("RADIXOX_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn radixox-resp on port {port}: {e}"));
    std::mem::forget(child);
    wait_for_port(port);
}

/// Open a fresh synchronous Redis connection to `port`.
pub fn conn(port: u16) -> redis::Connection {
    redis::Client::open(format!("redis://127.0.0.1:{port}"))
        .expect("invalid redis URL")
        .get_connection()
        .unwrap_or_else(|e| panic!("failed to connect to port {port}: {e}"))
}

/// Assert that a RedisError is a WRONGTYPE protocol error.
/// In redis crate 0.27, WRONGTYPE errors are classified as ExtensionError, not TypeError.
#[allow(dead_code)]
pub fn assert_wrongtype(err: &redis::RedisError) {
    assert!(
        err.to_string().contains("WRONGTYPE"),
        "expected WRONGTYPE error, got: {err}"
    );
}

fn wait_for_port(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("radixox-resp did not start on port {port} within 5 s");
}
