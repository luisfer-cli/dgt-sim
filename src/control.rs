//! Control plane: a Unix domain socket carrying one JSON request per line.
//!
//! The daemon owns the socket; the CLI is a thin client. DGT clients keep
//! speaking the real binary protocol over PTY or TCP — JSON never reaches
//! them.
//!
//! The socket is created `0600` and lives in `$XDG_RUNTIME_DIR` when that
//! exists, so other users cannot drive a simulator they do not own.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::board::{ClockCompat, PhysicalStyle};
use crate::error::{Error, Result};
use crate::transport::Faults;

/// Default socket path: `$XDG_RUNTIME_DIR/dgt-sim.sock`, else
/// `/tmp/dgt-sim-<uid>.sock`.
///
/// `DGT_SIM_SOCKET` overrides it; `--socket` overrides that.
pub fn default_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DGT_SIM_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("dgt-sim.sock");
        }
    }
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/dgt-sim-{}.sock", uid.as_raw()))
}

/// A request from the CLI to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Daemon status: socket, pid, board count, uptime.
    Info,
    /// All boards, one summary line each.
    List,
    /// One board's details.
    Show { board: u32 },
    /// Recent outbound protocol messages, as hex.
    Messages { board: u32 },
    /// Play a move, geometrically (no legality check).
    Move { board: u32, uci: String },
    /// Lift the piece off a field.
    Lift { board: u32, square: String },
    /// Put a piece on a field.
    Place {
        board: u32,
        square: String,
        piece: String,
    },
    /// Empty a field.
    Clear { board: u32, square: String },
    /// Load a FEN placement.
    SetFen {
        board: u32,
        fen: String,
        #[serde(default)]
        quiet: bool,
    },
    /// Starting position, IDLE mode, fresh EEPROM.
    Reset {
        board: u32,
        #[serde(default = "default_true")]
        emit: bool,
    },
    /// Force a transfer mode without a connected client.
    SetMode { board: u32, mode: String },
    /// Replay a PGN file.
    Replay {
        board: u32,
        /// Path to a PGN file.
        pgn: String,
        /// Starting position; defaults to the standard start.
        fen: Option<String>,
        #[serde(default)]
        delay_ms: u64,
        #[serde(default = "default_speed")]
        speed: f64,
        #[serde(default, rename = "loop")]
        repeat: bool,
    },
    /// Drop the current client connection.
    Disconnect { board: u32 },
    /// Accept clients again after `Disconnect`.
    Reconnect { board: u32 },
    /// Terminate one board, freeing its endpoint.
    Stop { board: u32 },
    /// Terminate the daemon and every board.
    Shutdown,
    /// Replace fault injection for one board.
    Fault { board: u32, faults: Faults },
    /// Change board identity and emulation details.
    Config { board: u32, config: ConfigRequest },
    /// Manipulate the emulated clock.
    Clock { board: u32, clock: ClockRequestJson },
}

fn default_true() -> bool {
    true
}

fn default_speed() -> f64 {
    1.0
}

/// `config` subcommand payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trademark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus_address: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_compat: Option<ClockCompat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_style: Option<PhysicalStyle>,
}

impl ConfigRequest {
    /// Is there anything to apply?
    pub fn is_empty(&self) -> bool {
        self.serial.is_none()
            && self.long_serial.is_none()
            && self.version.is_none()
            && self.trademark.is_none()
            && self.bus_address.is_none()
            && self.scan_delay_ms.is_none()
            && self.clock_compat.is_none()
            && self.physical_style.is_none()
    }
}

/// `clock` subcommand payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClockRequestJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
}

impl ClockRequestJson {
    pub fn is_empty(&self) -> bool {
        self.left_ms.is_none()
            && self.right_ms.is_none()
            && self.running.is_none()
            && self.turn.is_none()
            && self.connected.is_none()
    }
}

/// The daemon's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: impl Serialize) -> Self {
        Self {
            ok: true,
            data: serde_json::to_value(data).ok(),
            error: None,
        }
    }

    pub fn empty() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }

    /// Turn a `Result` into a response, keeping the error message.
    pub fn from_result(result: Result<Option<Value>>) -> Self {
        match result {
            Ok(data) => Self {
                ok: true,
                data,
                error: None,
            },
            Err(err) => Self::failed(err.to_string()),
        }
    }
}

/// A request plus the channel to answer on.
pub type Job = (Request, oneshot::Sender<Response>);

/// Run the control server until `shutdown` is notified.
///
/// `jobs` is the manager's command channel. Each accepted connection may carry
/// any number of newline-delimited requests.
pub async fn serve(
    path: &Path,
    jobs: mpsc::Sender<Job>,
    shutdown: std::sync::Arc<tokio::sync::Notify>,
) -> Result<()> {
    if path.exists() {
        // A stale socket from a killed daemon would block us forever.
        match UnixStream::connect(path).await {
            Ok(_) => {
                return Err(Error::AlreadyRunning(path.display().to_string()));
            }
            Err(_) => {
                tracing::warn!(path = %path.display(), "removing stale control socket");
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let listener = UnixListener::bind(path)?;
    restrict_permissions(path).await?;
    tracing::info!(socket = %path.display(), "control socket ready");

    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let jobs = jobs.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, jobs).await {
                                tracing::debug!(error = %err, "control connection ended");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "control accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            _ = shutdown.notified() => break Ok(()),
        }
    };
    let _ = tokio::fs::remove_file(path).await;
    result
}

/// Only the owner may connect. The daemon never needs root.
async fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = tokio::fs::metadata(path).await?;
    let mut perms = metadata.permissions();
    perms.set_mode(0o600);
    tokio::fs::set_permissions(path, perms).await?;
    Ok(())
}

async fn handle_connection(stream: UnixStream, jobs: mpsc::Sender<Job>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(line) {
            Ok(request) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                if jobs.send((request, reply_tx)).await.is_err() {
                    Response::failed("the simulator is shutting down")
                } else {
                    match reply_rx.await {
                        Ok(response) => response,
                        Err(_) => Response::failed("the simulator dropped the request"),
                    }
                }
            }
            Err(err) => Response::failed(format!("malformed request: {err}")),
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write_half.write_all(&encoded).await?;
    }
    Ok(())
}

/// Send one request and read one response.
pub async fn call(path: &Path, request: &Request) -> Result<Response> {
    let stream = UnixStream::connect(path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound
            || err.kind() == std::io::ErrorKind::ConnectionRefused
        {
            Error::NotRunning(path.display().to_string())
        } else {
            Error::Io(err)
        }
    })?;
    let (read_half, mut write_half) = stream.into_split();
    let mut payload = serde_json::to_vec(request)?;
    payload.push(b'\n');
    write_half.write_all(&payload).await?;
    write_half.flush().await?;

    let mut lines = BufReader::new(read_half).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| Error::Control("the simulator closed the connection".into()))?;
    Ok(serde_json::from_str(&line)?)
}

/// Daemon status reported by [`Request::Info`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub pid: u32,
    pub socket: String,
    pub version: String,
    pub boards: usize,
    pub uptime_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape_is_stable() {
        let value = serde_json::to_value(Request::Move {
            board: 3,
            uci: "e2e4".into(),
        })
        .unwrap();
        assert_eq!(value["cmd"], "move");
        assert_eq!(value["board"], 3);
        assert_eq!(value["uci"], "e2e4");

        // Optional fields default rather than being required, so scripts can
        // send the minimum.
        let parsed: Request = serde_json::from_str(r#"{"cmd":"reset","board":1}"#).unwrap();
        match parsed {
            Request::Reset { emit, .. } => assert!(emit),
            other => panic!("unexpected {other:?}"),
        }
        let parsed: Request = serde_json::from_str(r#"{"cmd":"show","board":2}"#).unwrap();
        assert!(matches!(parsed, Request::Show { board: 2 }));
    }

    #[test]
    fn replay_accepts_the_reserved_word_loop() {
        let parsed: Request =
            serde_json::from_str(r#"{"cmd":"replay","board":1,"pgn":"g.pgn","loop":true}"#)
                .unwrap();
        match parsed {
            Request::Replay {
                repeat,
                delay_ms,
                speed,
                ..
            } => {
                assert!(repeat);
                assert_eq!(delay_ms, 0);
                assert_eq!(speed, 1.0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn response_roundtrip() {
        let response = Response::ok(Info {
            pid: 1,
            socket: "/tmp/x.sock".into(),
            version: "0.1.0".into(),
            boards: 10,
            uptime_ms: 42,
        });
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: Response = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.data.unwrap()["boards"], 10);

        let failed = Response::failed("nope");
        let encoded = serde_json::to_string(&failed).unwrap();
        assert!(!encoded.contains("data"));
        assert_eq!(
            serde_json::from_str::<Response>(&encoded)
                .unwrap()
                .error
                .unwrap(),
            "nope"
        );
    }

    #[tokio::test]
    async fn server_and_client_roundtrip_over_a_real_socket() {
        let dir = std::env::temp_dir().join(format!("dgt-sim-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("t{}.sock", line!()));

        let (tx, mut rx) = mpsc::channel::<Job>(8);
        let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        let server = {
            let path = path.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { serve(&path, tx, shutdown).await })
        };
        let handler = tokio::spawn(async move {
            while let Some((request, reply)) = rx.recv().await {
                let payload = serde_json::to_value(&request).unwrap();
                reply.send(Response::ok(payload)).unwrap();
            }
        });

        // Wait for the socket to appear.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the control socket must be owner-only");
        }

        let response = call(&path, &Request::Info).await.unwrap();
        assert!(response.ok);
        assert_eq!(response.data.unwrap()["cmd"], "info");

        let response = call(&path, &Request::Show { board: 7 }).await.unwrap();
        assert_eq!(response.data.unwrap()["board"], 7);

        shutdown.notify_waiters();
        handler.abort();
        let _ = server.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn calling_a_missing_socket_reports_not_running() {
        let path = std::env::temp_dir().join("dgt-sim-does-not-exist.sock");
        let err = call(&path, &Request::List).await.unwrap_err();
        assert!(matches!(err, Error::NotRunning(_)), "got {err:?}");
    }

    #[test]
    fn empty_config_and_clock_payloads_are_detected() {
        assert!(ConfigRequest::default().is_empty());
        assert!(
            !ConfigRequest {
                serial: Some("1".into()),
                ..Default::default()
            }
            .is_empty()
        );
        assert!(ClockRequestJson::default().is_empty());
    }

    #[test]
    fn default_socket_path_is_absolute() {
        assert!(default_socket_path().is_absolute());
    }
}
