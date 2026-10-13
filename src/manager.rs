//! The manager: owns every virtual board and answers control requests.
//!
//! Boards are created up front and never share mutable state. Requests are
//! handled one at a time by a single task, which is what keeps identifier
//! allocation and endpoint bookkeeping trivial to reason about; the boards
//! themselves keep running concurrently.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::board::{
    BoardCmd, BoardConfigPatch, BoardTask, ClockRequest, MessageLogEntry, Mode, PhysicalStyle,
    Side, Snapshot, parse_version,
};
use crate::chess;
use crate::control::{ConfigRequest, Info, Job, Request, Response};
use crate::error::{Error, Result};
use crate::protocol::pieces::Square;
use crate::transport::{Faults, Link, TransportKind, accept_loop, pty::PtyLink};

/// Hard ceiling on boards. Each PTY board costs two file descriptors, so the
/// real limit is usually `ulimit -n`; this bound just stops a typo from
/// exhausting the machine.
pub const MAX_BOARDS: u32 = 4096;

/// How the daemon was started.
#[derive(Debug, Clone)]
pub struct StartOptions {
    pub boards: u32,
    pub transport: TransportKind,
    pub bind: SocketAddr,
    pub physical_style: PhysicalStyle,
    pub faults: Faults,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            boards: 1,
            transport: TransportKind::Pty,
            bind: "127.0.0.1:0".parse().expect("valid default bind address"),
            physical_style: PhysicalStyle::Direct,
            faults: Faults::default(),
        }
    }
}

struct BoardHandle {
    cmd_tx: mpsc::Sender<BoardCmd>,
    task: JoinHandle<()>,
    endpoint: String,
    /// Whether the TCP accept loop should hand out links.
    accepting: Arc<AtomicBool>,
}

/// One row of `dgt-sim list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardSummary {
    pub id: u32,
    pub transport: String,
    pub endpoint: String,
    pub connected: bool,
    pub mode: String,
    pub fen: String,
    pub messages_sent: u64,
}

/// Owns the boards and the control-plane state.
pub struct Manager {
    opts: StartOptions,
    socket: PathBuf,
    boards: BTreeMap<u32, BoardHandle>,
    next_id: u32,
    started: Instant,
    replays: JoinSet<()>,
}

impl Manager {
    pub fn new(opts: StartOptions, socket: PathBuf) -> Self {
        Self {
            opts,
            socket,
            boards: BTreeMap::new(),
            next_id: 1,
            started: Instant::now(),
            replays: JoinSet::new(),
        }
    }

    /// Create the initial boards, then serve requests until shutdown.
    ///
    /// `ready` receives one summary per created board so `dgt-sim start` can
    /// print the endpoints without racing the control socket.
    pub async fn run(
        mut self,
        mut rx: mpsc::Receiver<Job>,
        ready: oneshot::Sender<Vec<BoardSummary>>,
    ) -> Result<()> {
        if self.opts.boards == 0 {
            return Err(Error::Limit("--boards must be at least 1".into()));
        }
        if self.opts.boards > MAX_BOARDS {
            return Err(Error::Limit(format!(
                "--boards {} exceeds the hard limit of {MAX_BOARDS}",
                self.opts.boards
            )));
        }
        self.opts.faults.validate()?;

        let mut created = Vec::with_capacity(self.opts.boards as usize);
        for _ in 0..self.opts.boards {
            match self.create_board().await {
                Ok(id) => created.push(BoardSummary {
                    id,
                    transport: self.opts.transport.name().to_string(),
                    endpoint: self.boards[&id].endpoint.clone(),
                    connected: false,
                    mode: Mode::Idle.name().to_string(),
                    fen: crate::protocol::pieces::to_fen_placement(
                        &crate::protocol::pieces::START_SQUARES,
                    ),
                    messages_sent: 0,
                }),
                Err(err) => {
                    tracing::error!(error = %err, "failed to create board, continuing with fewer");
                    break;
                }
            }
        }
        if created.is_empty() {
            return Err(Error::Transport("no board could be created".into()));
        }
        if created.len() < self.opts.boards as usize {
            tracing::warn!(
                requested = self.opts.boards,
                created = created.len(),
                "created fewer boards than requested"
            );
        }
        for summary in &created {
            // One line per endpoint on stderr keeps stdout clean for `--json`.
            tracing::info!(
                board = summary.id,
                endpoint = %summary.endpoint,
                transport = %summary.transport,
                "board ready"
            );
        }
        let _ = ready.send(created);
        tracing::info!(
            boards = self.boards.len(),
            transport = self.opts.transport.name(),
            socket = %self.socket.display(),
            "simulator ready"
        );

        while let Some((request, reply)) = rx.recv().await {
            let is_shutdown = matches!(request, Request::Shutdown);
            let response = self.handle(request).await;
            // A dead CLI must not take the daemon with it.
            let _ = reply.send(response);
            if is_shutdown {
                break;
            }
        }
        self.stop_all().await;
        Ok(())
    }

    async fn create_board(&mut self) -> Result<u32> {
        if self.boards.len() as u32 >= MAX_BOARDS {
            return Err(Error::Limit(format!("cannot exceed {MAX_BOARDS} boards")));
        }
        let id = self.next_id;
        self.next_id += 1;

        let (link_tx, link_rx) = mpsc::channel::<Link>(8);
        let (endpoint, kind, accepting) = match self.opts.transport {
            TransportKind::Pty => {
                let link = PtyLink::open()
                    .map_err(|err| Error::Transport(format!("openpty failed: {err}")))?;
                let endpoint = link.path().to_string_lossy().into_owned();
                link_tx
                    .send(Link::Pty(link))
                    .await
                    .map_err(|_| Error::Transport("board task vanished".into()))?;
                (
                    endpoint,
                    TransportKind::Pty,
                    Arc::new(AtomicBool::new(true)),
                )
            }
            TransportKind::Tcp => {
                let listener = tokio::net::TcpListener::bind(self.opts.bind)
                    .await
                    .map_err(|err| Error::Transport(format!("bind {}: {err}", self.opts.bind)))?;
                let addr = listener
                    .local_addr()
                    .map_err(|err| Error::Transport(err.to_string()))?;
                let accepting = Arc::new(AtomicBool::new(true));
                tokio::spawn(accept_loop(listener, link_tx, accepting.clone()));
                (format!("tcp://{addr}"), TransportKind::Tcp, accepting)
            }
        };

        let (cmd_tx, cmd_rx) = mpsc::channel::<BoardCmd>(64);
        let mut board = BoardTask::new(id, kind, endpoint.clone());
        board.faults = self.opts.faults;
        board.rng = crate::transport::Rng::new(self.opts.faults.seed);
        board.physical_style = self.opts.physical_style;
        let task = tokio::spawn(board.run(cmd_rx, link_rx));

        self.boards.insert(
            id,
            BoardHandle {
                cmd_tx,
                task,
                endpoint,
                accepting,
            },
        );
        Ok(id)
    }

    async fn handle(&mut self, request: Request) -> Response {
        match self.dispatch(request).await {
            Ok(data) => Response {
                ok: true,
                data,
                error: None,
            },
            Err(err) => {
                tracing::debug!(error = %err, "request failed");
                Response::failed(err.to_string())
            }
        }
    }

    async fn dispatch(&mut self, request: Request) -> Result<Option<serde_json::Value>> {
        match request {
            Request::Info => Ok(Some(serde_json::to_value(Info {
                pid: std::process::id(),
                socket: self.socket.display().to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                boards: self.boards.len(),
                uptime_ms: self.started.elapsed().as_millis() as u64,
            })?)),
            Request::List => {
                let mut rows = Vec::with_capacity(self.boards.len());
                for id in self.boards.keys().copied().collect::<Vec<_>>() {
                    let snapshot = self.snapshot(id).await?;
                    rows.push(BoardSummary {
                        id,
                        transport: snapshot.transport,
                        endpoint: snapshot.endpoint,
                        connected: snapshot.connected,
                        mode: snapshot.mode,
                        fen: snapshot.fen,
                        messages_sent: snapshot.messages_sent,
                    });
                }
                Ok(Some(serde_json::to_value(rows)?))
            }
            Request::Show { board } => Ok(Some(serde_json::to_value(self.snapshot(board).await?)?)),
            Request::Messages { board } => Ok(Some(self.board_messages(board).await?)),
            Request::Move { board, uci } => self.mutate(board, BoardCmd::Move { uci }).await,
            Request::Lift { board, square } => {
                let square = Square::from_uci(&square)?;
                self.mutate(board, BoardCmd::Lift(square)).await
            }
            Request::Clear { board, square } => {
                let square = Square::from_uci(&square)?;
                self.mutate(board, BoardCmd::Clear(square)).await
            }
            Request::Place {
                board,
                square,
                piece,
            } => {
                let square = Square::from_uci(&square)?;
                let code = crate::board::resolve_placement(&piece)?;
                self.mutate(board, BoardCmd::Place(square, code)).await
            }
            Request::SetFen { board, fen, quiet } => {
                // Validate here so the CLI gets a useful error message rather
                // than a silent no-op inside the board task.
                chess::parse_fen(&fen)?;
                self.mutate(board, BoardCmd::SetFen { fen, quiet }).await
            }
            Request::Reset { board, emit } => self.mutate(board, BoardCmd::Reset { emit }).await,
            Request::SetMode { board, mode } => {
                let mode = parse_mode(&mode)?;
                self.mutate(board, BoardCmd::SetMode(mode)).await
            }
            Request::Replay {
                board,
                pgn,
                fen,
                delay_ms,
                speed,
                repeat,
            } => {
                self.start_replay(board, pgn, fen, delay_ms, speed, repeat)
                    .await?;
                Ok(Some(json!({ "started": true })))
            }
            Request::Disconnect { board } => self.mutate(board, BoardCmd::Disconnect).await,
            Request::Reconnect { board } => self.mutate(board, BoardCmd::Reconnect).await,
            Request::Stop { board } => {
                self.stop_board(board).await?;
                Ok(Some(json!({ "stopped": board })))
            }
            Request::Shutdown => Ok(None),
            Request::Fault { board, faults } => {
                faults.validate()?;
                self.mutate(board, BoardCmd::Fault(faults)).await
            }
            Request::Config { board, config } => {
                let patch = build_config_patch(&config)?;
                self.mutate(board, BoardCmd::Config(patch)).await
            }
            Request::Clock { board, clock } => {
                let request = build_clock_request(&clock)?;
                self.mutate(board, BoardCmd::Clock(request)).await
            }
        }
    }

    /// Apply a mutation, then report the resulting state. The mutation and the
    /// snapshot travel on the same channel, so ordering is guaranteed.
    async fn mutate(&self, board: u32, cmd: BoardCmd) -> Result<Option<serde_json::Value>> {
        let snapshot = self.send_and_snapshot(board, cmd).await?;
        Ok(Some(serde_json::to_value(snapshot)?))
    }

    async fn send_and_snapshot(&self, board: u32, cmd: BoardCmd) -> Result<Snapshot> {
        let handle = self.boards.get(&board).ok_or(Error::BoardNotFound(board))?;
        handle
            .cmd_tx
            .send(cmd)
            .await
            .map_err(|_| Error::BoardStopped(board))?;
        self.snapshot(board).await
    }

    async fn snapshot(&self, board: u32) -> Result<Snapshot> {
        let handle = self.boards.get(&board).ok_or(Error::BoardNotFound(board))?;
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(BoardCmd::Snapshot(tx))
            .await
            .map_err(|_| Error::BoardStopped(board))?;
        rx.await.map_err(|_| Error::BoardStopped(board))
    }

    async fn board_messages(&self, board: u32) -> Result<serde_json::Value> {
        let handle = self.boards.get(&board).ok_or(Error::BoardNotFound(board))?;
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(BoardCmd::Messages(tx))
            .await
            .map_err(|_| Error::BoardStopped(board))?;
        let entries: Vec<MessageLogEntry> = rx.await.map_err(|_| Error::BoardStopped(board))?;
        Ok(serde_json::to_value(entries)?)
    }

    async fn start_replay(
        &mut self,
        board: u32,
        pgn_path: String,
        fen: Option<String>,
        delay_ms: u64,
        speed: f64,
        repeat: bool,
    ) -> Result<()> {
        if !(speed > 0.0) || !speed.is_finite() {
            return Err(Error::Other(format!(
                "--speed must be a positive number, got {speed}"
            )));
        }
        let handle = self.boards.get(&board).ok_or(Error::BoardNotFound(board))?;
        let text = tokio::fs::read_to_string(&pgn_path).await.map_err(|err| {
            Error::Io(std::io::Error::new(
                err.kind(),
                format!("{pgn_path}: {err}"),
            ))
        })?;
        let moves = chess::pgn_to_uci(&text, fen.as_deref())?;
        if moves.is_empty() {
            return Err(Error::InvalidPgn("no playable moves".into()));
        }

        // A replay is only observable if the board streams field updates, so
        // promote IDLE to UPDATE_BOARD. Any other mode is left alone.
        let snapshot = self.snapshot(board).await?;
        if snapshot.mode == Mode::Idle.name() {
            self.send_and_snapshot(board, BoardCmd::SetMode(Mode::UpdateBoard))
                .await?;
            tracing::info!(board, "replay promoted the board from idle to update_board");
        }

        let tx = handle.cmd_tx.clone();
        let interval = Duration::from_millis(((delay_ms as f64) / speed).round() as u64);
        self.replays.spawn(async move {
            loop {
                for uci in &moves {
                    if tx.send(BoardCmd::Move { uci: uci.clone() }).await.is_err() {
                        return;
                    }
                    if interval > Duration::ZERO {
                        tokio::time::sleep(interval).await;
                    }
                }
                if !repeat {
                    return;
                }
            }
        });
        Ok(())
    }

    async fn stop_board(&mut self, board: u32) -> Result<()> {
        let Some(handle) = self.boards.remove(&board) else {
            return Err(Error::BoardNotFound(board));
        };
        handle.accepting.store(false, Ordering::Relaxed);
        let _ = handle.cmd_tx.send(BoardCmd::Shutdown).await;
        // Give the task a bounded amount of time to finish flushing.
        if tokio::time::timeout(Duration::from_secs(2), handle.task)
            .await
            .is_err()
        {
            tracing::warn!(board, "board task did not stop in time");
        }
        Ok(())
    }

    async fn stop_all(&mut self) {
        self.replays.abort_all();
        let ids: Vec<u32> = self.boards.keys().copied().collect();
        for id in ids {
            let _ = self.stop_board(id).await;
        }
    }
}

fn parse_mode(mode: &str) -> Result<Mode> {
    match mode.to_ascii_lowercase().replace('-', "_").as_str() {
        "idle" => Ok(Mode::Idle),
        "update_board" | "updateboard" | "board" => Ok(Mode::UpdateBoard),
        "update" => Ok(Mode::Update),
        "update_nice" | "updatenice" | "nice" => Ok(Mode::UpdateNice),
        other => Err(Error::Other(format!(
            "unknown mode {other:?} (idle, update-board, update, update-nice)"
        ))),
    }
}

fn build_config_patch(config: &ConfigRequest) -> Result<BoardConfigPatch> {
    let version = match &config.version {
        Some(v) => Some(parse_version(v)?),
        None => None,
    };
    if let Some(serial) = &config.serial
        && (serial.len() > 5 || !serial.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(Error::Other(format!(
            "--serial must be at most 5 ASCII digits, got {serial:?}"
        )));
    }
    if let Some(long) = &config.long_serial
        && (long.len() > 10 || !long.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(Error::Other(format!(
            "--long-serial must be at most 10 ASCII digits, got {long:?}"
        )));
    }
    if let Some(trademark) = &config.trademark
        && (trademark.len() > 256 || trademark.bytes().any(|b| b & 0x80 != 0))
    {
        return Err(Error::Other(
            "--trademark must be at most 256 bytes of 7-bit ASCII".into(),
        ));
    }
    Ok(BoardConfigPatch {
        serial: config.serial.clone(),
        long_serial: config.long_serial.clone(),
        version,
        trademark: config.trademark.clone(),
        bus_address: config.bus_address,
        scan_delay: config.scan_delay_ms.map(Duration::from_millis),
        clock_compat: config.clock_compat,
        physical_style: config.physical_style,
    })
}

fn build_clock_request(clock: &crate::control::ClockRequestJson) -> Result<ClockRequest> {
    let turn = match clock.turn.as_deref() {
        None => None,
        Some("left") => Some(Side::Left),
        Some("right") => Some(Side::Right),
        Some(other) => {
            return Err(Error::Other(format!(
                "unknown clock side {other:?} (left or right)"
            )));
        }
    };
    Ok(ClockRequest {
        left: clock.left_ms.map(Duration::from_millis),
        right: clock.right_ms.map(Duration::from_millis),
        running: clock.running,
        turn,
        connected: clock.connected,
        right_high: None,
        battery_low: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing_accepts_scripts_friendly_spellings() {
        assert_eq!(parse_mode("idle").unwrap(), Mode::Idle);
        assert_eq!(parse_mode("update-board").unwrap(), Mode::UpdateBoard);
        assert_eq!(parse_mode("UPDATE_BOARD").unwrap(), Mode::UpdateBoard);
        assert_eq!(parse_mode("nice").unwrap(), Mode::UpdateNice);
        assert!(parse_mode("turbo").is_err());
    }

    #[test]
    fn config_patch_is_validated() {
        assert!(
            build_config_patch(&ConfigRequest::default())
                .unwrap()
                .serial
                .is_none()
        );

        let ok = ConfigRequest {
            serial: Some("00123".into()),
            version: Some("1.8".into()),
            ..Default::default()
        };
        let patch = build_config_patch(&ok).unwrap();
        assert_eq!(patch.serial.as_deref(), Some("00123"));
        assert_eq!(patch.version, Some((1, 8)));

        for bad in [
            ConfigRequest {
                serial: Some("123456".into()),
                ..Default::default()
            },
            ConfigRequest {
                serial: Some("abc".into()),
                ..Default::default()
            },
            ConfigRequest {
                long_serial: Some("12345678901".into()),
                ..Default::default()
            },
            ConfigRequest {
                version: Some("x.y".into()),
                ..Default::default()
            },
            ConfigRequest {
                trademark: Some("\u{ff}".repeat(4).into()),
                ..Default::default()
            },
        ] {
            assert!(
                build_config_patch(&bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn clock_request_rejects_unknown_sides() {
        assert!(
            build_clock_request(&crate::control::ClockRequestJson::default())
                .unwrap()
                .turn
                .is_none()
        );
        let req = crate::control::ClockRequestJson {
            turn: Some("left".into()),
            left_ms: Some(1000),
            running: Some(true),
            ..Default::default()
        };
        let built = build_clock_request(&req).unwrap();
        assert_eq!(built.turn, Some(Side::Left));
        assert_eq!(built.left, Some(Duration::from_millis(1000)));
        assert_eq!(built.running, Some(true));

        let bad = crate::control::ClockRequestJson {
            turn: Some("up".into()),
            ..Default::default()
        };
        assert!(build_clock_request(&bad).is_err());
    }

    #[test]
    fn board_ceiling_is_enforced() {
        let opts = StartOptions {
            boards: MAX_BOARDS + 1,
            ..StartOptions::default()
        };
        let manager = Manager::new(opts, PathBuf::from("/tmp/never.sock"));
        let (_tx, rx) = mpsc::channel(1);
        let (ready_tx, _ready_rx) = oneshot::channel();
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(manager.run(rx, ready_tx))
            .unwrap_err();
        assert!(matches!(err, Error::Limit(_)), "got {err:?}");
    }
}

/// An embedded simulator: the manager plus its request channel, without the
/// control socket.
///
/// `dgt-sim start` uses [`Manager`] together with [`crate::control::serve`];
/// embedding dgt-sim in another program (or in the test suite) can use this
/// instead and talk to the boards over a plain channel.
pub struct RunningSimulator {
    pub boards: Vec<BoardSummary>,
    jobs: mpsc::Sender<Job>,
    socket: PathBuf,
    join: JoinHandle<Result<()>>,
}

impl RunningSimulator {
    /// Create a fixed number of boards and start serving requests.
    pub async fn start(opts: StartOptions, socket: impl Into<PathBuf>) -> Result<Self> {
        let socket = socket.into();
        let (job_tx, job_rx) = mpsc::channel::<Job>(64);
        let (ready_tx, ready_rx) = oneshot::channel();
        let manager = Manager::new(opts, socket.clone());
        let join = tokio::spawn(async move { manager.run(job_rx, ready_tx).await });
        let boards = ready_rx
            .await
            .map_err(|_| Error::Other("the simulator failed to create its boards".into()))?;
        Ok(Self {
            boards,
            jobs: job_tx,
            socket,
            join,
        })
    }

    /// Send a request straight to the manager, bypassing the socket.
    pub async fn request(&self, request: Request) -> Result<Response> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.jobs
            .send((request, reply_tx))
            .await
            .map_err(|_| Error::Other("the simulator is shutting down".into()))?;
        reply_rx
            .await
            .map_err(|_| Error::Other("the simulator dropped the request".into()))
    }

    /// Send a request and fail on a non-`ok` response, returning its payload.
    pub async fn expect(&self, request: Request) -> Result<serde_json::Value> {
        let response = self.request(request).await?;
        match (response.ok, response.data) {
            (true, data) => Ok(data.unwrap_or(serde_json::Value::Null)),
            (false, _) => Err(Error::Other(
                response.error.unwrap_or_else(|| "unspecified error".into()),
            )),
        }
    }

    /// Snapshot of one board.
    pub async fn snapshot(&self, board: u32) -> Result<serde_json::Value> {
        self.expect(Request::Show { board }).await
    }

    /// The endpoint of a board, as reported at creation.
    pub fn endpoint(&self, board: u32) -> Option<&str> {
        self.boards
            .iter()
            .find(|b| b.id == board)
            .map(|b| b.endpoint.as_str())
    }

    /// The socket path this simulator was configured with (not bound unless
    /// [`crate::control::serve`] was started separately).
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Shut every board down and wait for the manager task to finish.
    pub async fn shutdown(self) -> Result<()> {
        let response = self.request(Request::Shutdown).await?;
        if !response.ok {
            return Err(Error::Other(
                response.error.unwrap_or_else(|| "shutdown failed".into()),
            ));
        }
        self.join
            .await
            .map_err(|err| Error::Other(format!("the simulator task panicked: {err}")))?
    }
}
