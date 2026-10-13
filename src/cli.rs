//! `dgt-sim` command line interface.
//!
//! Every subcommand other than `start` is a thin client of the control
//! socket, so the same automation works from Bash, from `xargs`, or from a
//! test harness: pass `--json` for machine-readable output, and rely on the
//! exit code (`0` success, `1` failure, `2` usage error).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;

use crate::board::{ClockCompat, PhysicalStyle};
use crate::control::{self, ClockRequestJson, ConfigRequest, Request};
use crate::error::{Error, Result};
use crate::manager::{BoardSummary, MAX_BOARDS, Manager, StartOptions};
use crate::transport::{Faults, TransportKind};

/// Emulate DGT electronic chess boards over PTY or TCP.
///
/// The daemon speaks the real binary DGT protocol to its clients; JSON is used
/// only on the control socket between this CLI and the running simulator.
#[derive(Debug, Parser)]
#[command(
    name = "dgt-sim",
    version,
    about = "Emulate DGT electronic chess boards",
    long_about = None,
    propagate_version = true,
)]
pub struct Cli {
    /// Control socket path. Defaults to $DGT_SIM_SOCKET, then
    /// $XDG_RUNTIME_DIR/dgt-sim.sock, then /tmp/dgt-sim-<uid>.sock.
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Increase log verbosity: -v for debug, -vv for trace.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Suppress all logging on stderr.
    #[arg(long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Which transport the boards expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TransportArg {
    /// Pseudoterminal per board (`/dev/pts/N`), for real serial clients.
    Pty,
    /// Loopback TCP listener per board, for scripts and tests.
    Tcp,
}

impl From<TransportArg> for TransportKind {
    fn from(value: TransportArg) -> Self {
        match value {
            TransportArg::Pty => Self::Pty,
            TransportArg::Tcp => Self::Tcp,
        }
    }
}

/// `--json` for machine-readable output.
#[derive(Debug, Clone, Copy, Args)]
pub struct OutputArgs {
    /// Print a single line of JSON instead of a human-readable summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the simulator in the foreground and create virtual boards.
    Start(StartArgs),
    /// Daemon status: socket, pid, board count.
    Info(OutputArgs),
    /// List every virtual board with its endpoint.
    List(OutputArgs),
    /// Show one board's full state.
    #[command(visible_alias = "status")]
    Show(ShowArgs),
    /// Show recently emitted protocol messages as hex.
    Messages {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Play a move on a board (geometric, legality not enforced).
    Move {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// Move in UCI form, e.g. e2e4 or e7e8q.
        #[arg(long)]
        uci: String,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Lift the piece standing on a field.
    Lift {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// Field, e.g. e2.
        #[arg(long)]
        square: String,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Put a piece on a field.
    Place {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// Field, e.g. e4.
        #[arg(long)]
        square: String,
        /// Piece: white-pawn, bp, N, magic-draw, empty.
        #[arg(long)]
        piece: String,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Empty a field.
    Clear {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// Field, e.g. e4.
        #[arg(long)]
        square: String,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Load a FEN into the board's sensors.
    SetFen {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// Full or placement-only FEN.
        #[arg(long)]
        fen: String,
        /// Change the state without emitting field updates.
        #[arg(long)]
        quiet: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Return a board to the starting position, IDLE mode and a fresh EEPROM.
    Reset {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// Do not emit the field updates for the repositioned pieces.
        #[arg(long)]
        no_emit: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Force a protocol transfer mode without a connected client.
    SetMode {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// idle, update-board, update or update-nice.
        #[arg(long, value_name = "MODE")]
        mode: String,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Replay a PGN file move by move.
    Replay {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        /// Path to a PGN file.
        #[arg(long, value_name = "PATH")]
        pgn: PathBuf,
        /// Starting position for the PGN; defaults to the standard start.
        #[arg(long)]
        fen: Option<String>,
        /// Delay between moves, e.g. 500ms or 1s.
        #[arg(long, value_parser = parse_duration, default_value = "0ms")]
        delay: Duration,
        /// Multiplier applied to --delay.
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Restart the game forever, until the board is stopped.
        #[arg(long = "loop")]
        repeat: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Drop the current client connection.
    Disconnect {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Accept clients again after `disconnect`.
    Reconnect {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Terminate one board and free its endpoint.
    Stop {
        /// Board identifier.
        #[arg(long)]
        board: u32,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Terminate the simulator and every board.
    Shutdown(OutputArgs),
    /// Inject faults into one board's links.
    Fault(FaultArgs),
    /// Change a board's identity and emulation details.
    Config(ConfigArgs),
    /// Manipulate a board's emulated clock.
    Clock(ClockArgs),
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// How many virtual boards to create.
    #[arg(long, default_value_t = 1)]
    pub boards: u32,

    /// Transport every board exposes.
    #[arg(long, value_enum, default_value_t = TransportArg::Pty)]
    pub transport: TransportArg,

    /// Address TCP boards bind to. Loopback by default.
    #[arg(long, default_value = "127.0.0.1:0")]
    pub bind: SocketAddr,

    /// Physical event style for moves.
    #[arg(long, value_enum, default_value_t = PhysicalStyleArg::Direct)]
    pub physical_style: PhysicalStyleArg,

    /// How the two side-dependent BWTime status bits are encoded.
    #[arg(long, value_enum, default_value_t = ClockCompatArg::Header)]
    pub clock_compat: ClockCompatArg,

    /// Refuse to start above this many boards.
    #[arg(long, default_value_t = MAX_BOARDS)]
    pub max_boards: u32,

    /// Fault injection: delay before every write, e.g. 200ms.
    #[arg(long, value_parser = parse_duration, default_value = "0ms")]
    pub latency: Duration,

    /// Fault injection: fraction of fragments dropped, 0.0..=1.0.
    #[arg(long, default_value_t = 0.0)]
    pub drop_rate: f64,

    /// Fault injection: maximum fragment size in bytes, 0 disables splitting.
    #[arg(long, default_value_t = 0)]
    pub fragment_max: usize,

    /// Fault injection: truncate every outgoing message.
    #[arg(long)]
    pub truncate: bool,

    /// Fault injection: seed for the deterministic drop PRNG.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,

    /// Print the created endpoints as JSON on stdout and keep running.
    #[arg(long)]
    pub json: bool,
}

/// Mirror of [`PhysicalStyle`] so clap can render the possible values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PhysicalStyleArg {
    /// Minimum message sequence.
    Direct,
    /// Also emit intermediate physical states.
    Verbose,
}

impl From<PhysicalStyleArg> for PhysicalStyle {
    fn from(value: PhysicalStyleArg) -> Self {
        match value {
            PhysicalStyleArg::Direct => Self::Direct,
            PhysicalStyleArg::Verbose => Self::Verbose,
        }
    }
}

/// Mirror of [`ClockCompat`] for clap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ClockCompatArg {
    /// Follow the DGT header (D3 = right to move, D5 set = no clock).
    Header,
    /// Follow arnsholt/dgtdriver (D3 = left to move, D5 set = connected).
    Java,
}

impl From<ClockCompatArg> for ClockCompat {
    fn from(value: ClockCompatArg) -> Self {
        match value {
            ClockCompatArg::Header => Self::Header,
            ClockCompatArg::Java => Self::Java,
        }
    }
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Board identifier.
    #[arg(long)]
    pub board: u32,
    /// Print the ASCII diagram too.
    #[arg(long)]
    pub ascii: bool,
    #[command(flatten)]
    pub out: OutputArgs,
}

#[derive(Debug, Args)]
pub struct FaultArgs {
    /// Board identifier.
    #[arg(long)]
    pub board: u32,
    /// Delay before every write, e.g. 200ms. Use 0ms to clear.
    #[arg(long, value_parser = parse_duration, default_value = "0ms")]
    pub latency: Duration,
    /// Fraction of fragments to drop, 0.0..=1.0.
    #[arg(long, default_value_t = 0.0)]
    pub drop_rate: f64,
    /// Maximum fragment size in bytes, 0 disables splitting.
    #[arg(long, default_value_t = 0)]
    pub fragment_max: usize,
    /// Truncate every outgoing message mid-frame.
    #[arg(long)]
    pub truncate: bool,
    /// Drop the link after a write, 0.0..=1.0.
    #[arg(long, default_value_t = 0.0)]
    pub disconnect_rate: f64,
    /// Seed for the deterministic fault PRNG.
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    #[command(flatten)]
    pub out: OutputArgs,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Board identifier.
    #[arg(long)]
    pub board: u32,
    /// Short serial number: up to 5 ASCII digits.
    #[arg(long)]
    pub serial: Option<String>,
    /// Long serial number: up to 10 ASCII digits.
    #[arg(long)]
    pub long_serial: Option<String>,
    /// Firmware version as major.minor.
    #[arg(long = "firmware-version", value_name = "MAJOR.MINOR")]
    pub firmware_version: Option<String>,
    /// Identification string returned for `DGT_SEND_TRADEMARK`.
    #[arg(long)]
    pub trademark: Option<String>,
    /// 14-bit bus address.
    #[arg(long)]
    pub bus_address: Option<u16>,
    /// Extra delay between consecutive field updates, e.g. 5ms.
    #[arg(long, value_parser = parse_duration)]
    pub scan_delay: Option<Duration>,
    /// BWTime status-bit interpretation.
    #[arg(long, value_enum)]
    pub clock_compat: Option<ClockCompatArg>,
    /// Physical event style for moves.
    #[arg(long, value_enum)]
    pub physical_style: Option<PhysicalStyleArg>,
    #[command(flatten)]
    pub out: OutputArgs,
}

#[derive(Debug, Args)]
pub struct ClockArgs {
    /// Board identifier.
    #[arg(long)]
    pub board: u32,
    /// Time on the left screen, e.g. 5m or 1h30m.
    #[arg(long, value_parser = parse_duration)]
    pub left: Option<Duration>,
    /// Time on the right screen.
    #[arg(long, value_parser = parse_duration)]
    pub right: Option<Duration>,
    /// Start the clock counting down.
    #[arg(long, conflicts_with = "pause")]
    pub run: bool,
    /// Stop the clock.
    #[arg(long)]
    pub pause: bool,
    /// Which screen is on the move.
    #[arg(long, value_enum)]
    pub turn: Option<TurnArg>,
    /// Is a clock attached at all?
    #[arg(long)]
    pub connected: Option<bool>,
    #[command(flatten)]
    pub out: OutputArgs,
}

/// Clock side for `--turn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TurnArg {
    Left,
    Right,
}

impl TurnArg {
    const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// Resolve the control socket, honouring `--socket` before the default.
pub fn socket_path(cli: &Cli) -> PathBuf {
    cli.socket
        .clone()
        .unwrap_or_else(control::default_socket_path)
}

/// Run the chosen subcommand, returning the process exit code.
pub async fn run(cli: Cli) -> Result<i32> {
    let socket = socket_path(&cli);
    match cli.command {
        Command::Start(args) => run_daemon(&socket, args).await,
        command => {
            let (request, out) = to_request(command)?;
            dispatch(&socket, request, out).await
        }
    }
}

/// Map a CLI subcommand onto a control request.
fn to_request(command: Command) -> Result<(Request, OutputArgs)> {
    let pair = match command {
        Command::Start(_) => unreachable!("start is handled before this point"),
        Command::Info(out) => (Request::Info, out),
        Command::List(out) => (Request::List, out),
        Command::Show(args) => (Request::Show { board: args.board }, args.out),
        Command::Messages { board, out } => (Request::Messages { board }, out),
        Command::Move { board, uci, out } => (Request::Move { board, uci }, out),
        Command::Lift { board, square, out } => (Request::Lift { board, square }, out),
        Command::Place {
            board,
            square,
            piece,
            out,
        } => (
            Request::Place {
                board,
                square,
                piece,
            },
            out,
        ),
        Command::Clear { board, square, out } => (Request::Clear { board, square }, out),
        Command::SetFen {
            board,
            fen,
            quiet,
            out,
        } => (Request::SetFen { board, fen, quiet }, out),
        Command::Reset {
            board,
            no_emit,
            out,
        } => (
            Request::Reset {
                board,
                emit: !no_emit,
            },
            out,
        ),
        Command::SetMode { board, mode, out } => (Request::SetMode { board, mode }, out),
        Command::Replay {
            board,
            pgn,
            fen,
            delay,
            speed,
            repeat,
            out,
        } => (
            Request::Replay {
                board,
                pgn: pgn.to_string_lossy().into_owned(),
                fen,
                delay_ms: delay.as_millis() as u64,
                speed,
                repeat,
            },
            out,
        ),
        Command::Disconnect { board, out } => (Request::Disconnect { board }, out),
        Command::Reconnect { board, out } => (Request::Reconnect { board }, out),
        Command::Stop { board, out } => (Request::Stop { board }, out),
        Command::Shutdown(out) => (Request::Shutdown, out),
        Command::Fault(args) => {
            let faults = Faults {
                latency_ms: args.latency.as_millis() as u64,
                drop_rate: args.drop_rate,
                fragment_max: args.fragment_max,
                truncate: args.truncate,
                seed: args.seed,
                disconnect_rate: args.disconnect_rate,
            };
            faults.validate()?;
            (
                Request::Fault {
                    board: args.board,
                    faults,
                },
                args.out,
            )
        }
        Command::Config(args) => {
            let config = ConfigRequest {
                serial: args.serial,
                long_serial: args.long_serial,
                version: args.firmware_version,
                trademark: args.trademark,
                bus_address: args.bus_address,
                scan_delay_ms: args.scan_delay.map(|d| d.as_millis() as u64),
                clock_compat: args.clock_compat.map(Into::into),
                physical_style: args.physical_style.map(Into::into),
            };
            if config.is_empty() {
                return Err(Error::Other(
                    "config: pass at least one option (see --help)".into(),
                ));
            }
            (
                Request::Config {
                    board: args.board,
                    config,
                },
                args.out,
            )
        }
        Command::Clock(args) => {
            let clock = ClockRequestJson {
                left_ms: args.left.map(|d| d.as_millis() as u64),
                right_ms: args.right.map(|d| d.as_millis() as u64),
                running: if args.run {
                    Some(true)
                } else if args.pause {
                    Some(false)
                } else {
                    None
                },
                turn: args.turn.map(|t| t.name().to_string()),
                connected: args.connected,
            };
            if clock.is_empty() {
                return Err(Error::Other(
                    "clock: pass at least one option (see --help)".into(),
                ));
            }
            (
                Request::Clock {
                    board: args.board,
                    clock,
                },
                args.out,
            )
        }
    };
    Ok(pair)
}

async fn dispatch(socket: &Path, request: Request, out: OutputArgs) -> Result<i32> {
    let shutdown = matches!(request, Request::Shutdown);
    let response = control::call(socket, &request).await?;
    if !response.ok {
        let message = response
            .error
            .unwrap_or_else(|| "the simulator reported an error".into());
        eprintln!("dgt-sim: {message}");
        return Ok(1);
    }
    let data = response.data.unwrap_or(Value::Null);
    if out.json {
        println!("{}", serde_json::to_string(&data)?);
        return Ok(0);
    }
    render(&request, &data)?;
    if shutdown {
        eprintln!("dgt-sim: simulator stopped");
    }
    Ok(0)
}

/// Human-readable rendering for the non-`--json` case.
fn render(request: &Request, data: &Value) -> Result<()> {
    match request {
        Request::Info => {
            println!("pid       {}", stringify_field(data, "pid"));
            println!("socket    {}", stringify_field(data, "socket"));
            println!("version   {}", stringify_field(data, "version"));
            println!("boards    {}", stringify_field(data, "boards"));
            println!("uptime    {} ms", stringify_field(data, "uptime_ms"));
        }
        Request::List => {
            let rows: Vec<BoardSummary> = serde_json::from_value(data.clone())?;
            if rows.is_empty() {
                println!("no boards");
                return Ok(());
            }
            println!(
                "{:>4}  {:<5}  {:<22}  {:<9}  {:<12}  FEN",
                "ID", "TR", "ENDPOINT", "CONNECTED", "MODE"
            );
            for row in rows {
                println!(
                    "{:>4}  {:<5}  {:<22}  {:<9}  {:<12}  {}",
                    row.id,
                    row.transport,
                    row.endpoint,
                    if row.connected { "yes" } else { "no" },
                    row.mode,
                    row.fen
                );
            }
        }
        Request::Show { .. } => {
            let fields: [(&str, &str); 16] = [
                ("id", "id"),
                ("transport", "transport"),
                ("endpoint", "endpoint"),
                ("connected", "connected"),
                ("peer", "peer"),
                ("mode", "mode"),
                ("serial", "serial"),
                ("long_serial", "long_serial"),
                ("bus_address", "bus_address"),
                ("version", "version"),
                ("physical_style", "physical_style"),
                ("clock_compat", "clock_compat"),
                ("scan_delay_ms", "scan_delay_ms"),
                ("eeprom_len", "eeprom_len"),
                ("fields_changed", "fields_changed"),
                ("messages_sent", "messages_sent"),
            ];
            for (label, key) in fields {
                println!("{label:<15} {}", stringify_field(data, key));
            }
            let clock = &data["clock"];
            println!(
                "clock           left {} right {} {} to-move={} connected={}",
                stringify_field(clock, "left"),
                stringify_field(clock, "right"),
                if clock["running"] == Value::Bool(true) {
                    "running"
                } else {
                    "stopped"
                },
                stringify_field(clock, "to_move"),
                stringify_field(clock, "connected")
            );
            println!("fen             {}", stringify_field(data, "fen"));
            if let Some(ascii) = data["ascii"].as_str() {
                print!("{ascii}");
            }
        }
        Request::Messages { .. } => {
            let entries = data.as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                println!("no protocol messages emitted yet");
                return Ok(());
            }
            for entry in entries {
                println!(
                    "{:>8}ms  {:<14} {:>4}B  {}",
                    stringify_field(&entry, "at_ms"),
                    stringify_field(&entry, "name"),
                    stringify_field(&entry, "len"),
                    stringify_field(&entry, "hex")
                );
            }
        }
        Request::Move { .. }
        | Request::Lift { .. }
        | Request::Place { .. }
        | Request::Clear { .. } => {
            emit_summary(data, true);
        }
        Request::SetFen { .. } | Request::Reset { .. } => emit_summary(data, true),
        Request::SetMode { .. } => emit_summary(data, false),
        Request::Replay { .. } => {
            if data["started"] == Value::Bool(true) {
                println!("replay started");
            }
        }
        Request::Stop { .. } => println!("board {} stopped", stringify_field(data, "stopped")),
        Request::Shutdown => println!("shutting down"),
        Request::Disconnect { .. } | Request::Reconnect { .. } => emit_summary(data, false),
        Request::Fault { .. } => emit_summary(data, false),
        Request::Config { .. } => emit_summary(data, false),
        Request::Clock { .. } => emit_summary(data, false),
    }
    Ok(())
}

/// Compact one-line report for mutations, including how many protocol
/// messages the change produced.
///
/// `hint` adds an explanation when nothing was streamed; it is set only for the
/// commands where streaming was actually expected, so that mode changes and
/// configuration commands do not print a misleading warning.
fn emit_summary(data: &Value, hint: bool) {
    let emitted = data["last_emitted"].as_u64().unwrap_or(0);
    println!(
        "board {} ({}) mode={} emitted={} fen={}",
        stringify_field(data, "id"),
        stringify_field(data, "endpoint"),
        stringify_field(data, "mode"),
        emitted,
        stringify_field(data, "fen")
    );
    if hint && emitted == 0 {
        eprintln!(
            "dgt-sim: no protocol messages were streamed (the board is not in an update mode, \
             or no client is attached); use `dgt-sim set-mode --mode update-board` or connect a client"
        );
    }
}

fn stringify_field(value: &Value, key: &str) -> String {
    match &value[key] {
        Value::Null => "-".into(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Run the daemon in the foreground until SIGINT, SIGTERM or `shutdown`.
async fn run_daemon(socket: &Path, args: StartArgs) -> Result<i32> {
    if args.max_boards == 0 {
        return Err(Error::Limit("--max-boards must be at least 1".into()));
    }
    if args.boards > args.max_boards {
        return Err(Error::Limit(format!(
            "--boards {} exceeds --max-boards {}",
            args.boards, args.max_boards
        )));
    }
    let faults = Faults {
        latency_ms: args.latency.as_millis() as u64,
        drop_rate: args.drop_rate,
        fragment_max: args.fragment_max,
        truncate: args.truncate,
        seed: args.seed,
        disconnect_rate: 0.0,
    };
    faults.validate()?;

    let opts = StartOptions {
        boards: args.boards,
        transport: args.transport.into(),
        bind: args.bind,
        physical_style: args.physical_style.into(),
        faults,
    };

    let (job_tx, job_rx) = tokio::sync::mpsc::channel(64);
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());

    // The control server and the manager run side by side: the server only
    // forwards requests, the manager owns every board.
    let server = {
        let socket = socket.to_path_buf();
        let shutdown = shutdown.clone();
        let job_tx = job_tx.clone();
        tokio::spawn(async move { control::serve(&socket, job_tx, shutdown).await })
    };

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let manager = Manager::new(opts, socket.to_path_buf());
    let manager_task = tokio::spawn(async move { manager.run(job_rx, ready_tx).await });

    let summaries = ready_rx
        .await
        .map_err(|_| Error::Other("the simulator failed to create its boards".into()))?;

    if args.json {
        println!("{}", serde_json::to_string(&summaries)?);
    } else {
        println!("{:>4}  {:<5}  ENDPOINT", "ID", "TR");
        for summary in &summaries {
            println!(
                "{:>4}  {:<5}  {}",
                summary.id, summary.transport, summary.endpoint
            );
        }
        println!(
            "control socket: {}   (stop with `dgt-sim shutdown`)",
            socket.display()
        );
    }

    spawn_signal_handler(job_tx, shutdown.clone());

    let outcome = manager_task
        .await
        .map_err(|err| Error::Other(format!("the simulator task panicked: {err}")))?;
    shutdown.notify_one();
    let _ = server.await;
    outcome?;
    Ok(0)
}

/// Translate SIGINT/SIGTERM into a graceful shutdown.
fn spawn_signal_handler(
    job_tx: tokio::sync::mpsc::Sender<control::Job>,
    shutdown: std::sync::Arc<tokio::sync::Notify>,
) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, "cannot install SIGINT handler");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, "cannot install SIGTERM handler");
                return;
            }
        };
        let name = tokio::select! {
            _ = sigint.recv() => "SIGINT",
            _ = sigterm.recv() => "SIGTERM",
        };
        tracing::info!(signal = name, "shutting down");
        let (reply, _drop) = tokio::sync::oneshot::channel();
        let _ = job_tx.send((Request::Shutdown, reply)).await;
        shutdown.notify_one();
    });
}

/// Parse `500ms`, `1s`, `2m`, `1h30m` or a bare number of milliseconds.
pub fn parse_duration(input: &str) -> std::result::Result<Duration, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(ms) = text.parse::<u64>() {
        return Ok(Duration::from_millis(ms));
    }
    let mut total = Duration::ZERO;
    let mut number = String::new();
    let mut unit = String::new();
    let mut any = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            if !unit.is_empty() {
                total += unit_duration(&number, &unit)?;
                number.clear();
                unit.clear();
            }
            number.push(ch);
        } else if ch.is_ascii_alphabetic() {
            if number.is_empty() {
                return Err(format!("invalid duration {input:?}"));
            }
            unit.push(ch);
        } else if ch.is_whitespace() {
            continue;
        } else {
            return Err(format!("invalid duration {input:?}"));
        }
    }
    if !number.is_empty() {
        total += unit_duration(&number, &unit)?;
        any = true;
    }
    if !any {
        return Err(format!("invalid duration {input:?}"));
    }
    Ok(total)
}

fn unit_duration(number: &str, unit: &str) -> std::result::Result<Duration, String> {
    let value: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration number {number:?}"))?;
    let millis = match unit.to_ascii_lowercase().as_str() {
        "" | "ms" => value,
        "s" | "sec" | "secs" | "second" | "seconds" => value * 1_000,
        "m" | "min" | "mins" | "minute" | "minutes" => value * 60_000,
        "h" | "hr" | "hour" | "hours" => value * 3_600_000,
        "us" | "usec" => return Ok(Duration::from_micros(value)),
        other => return Err(format!("unknown duration unit {other:?}")),
    };
    Ok(Duration::from_millis(millis))
}

/// Convenience for scripts: `dgt-sim list --json | jq`.
pub fn json_lines(value: &Value) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

/// Build a [`json!`] object with the resolved socket, used in tests.
#[cfg(test)]
fn describe_socket(socket: &Path) -> Value {
    serde_json::json!({ "socket": socket.display().to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration("500").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("0ms").unwrap(), Duration::ZERO);
        for bad in ["", "ms", "abc", "1x", "-5s", "1..2s"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn subcommands_parse_into_requests() {
        let cli =
            Cli::try_parse_from(["dgt-sim", "move", "--board", "2", "--uci", "e2e4"]).unwrap();
        let (request, out) = to_request(cli.command).unwrap();
        assert!(!out.json);
        match request {
            Request::Move { board, uci } => {
                assert_eq!(board, 2);
                assert_eq!(uci, "e2e4");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn json_flag_is_propagated() {
        let cli = Cli::try_parse_from(["dgt-sim", "list", "--json"]).unwrap();
        let (request, out) = to_request(cli.command).unwrap();
        assert!(out.json);
        assert!(matches!(request, Request::List));
    }

    #[test]
    fn place_accepts_a_piece_name() {
        let cli = Cli::try_parse_from([
            "dgt-sim",
            "place",
            "--board",
            "1",
            "--square",
            "e4",
            "--piece",
            "white-pawn",
        ])
        .unwrap();
        match to_request(cli.command).unwrap().0 {
            Request::Place { piece, square, .. } => {
                assert_eq!(piece, "white-pawn");
                assert_eq!(square, "e4");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn reset_defaults_to_emitting_updates() {
        let cli = Cli::try_parse_from(["dgt-sim", "reset", "--board", "1"]).unwrap();
        match to_request(cli.command).unwrap().0 {
            Request::Reset { emit, .. } => assert!(emit),
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from(["dgt-sim", "reset", "--board", "1", "--no-emit"]).unwrap();
        match to_request(cli.command).unwrap().0 {
            Request::Reset { emit, .. } => assert!(!emit),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn clock_requires_at_least_one_option() {
        let cli = Cli::try_parse_from(["dgt-sim", "clock", "--board", "1"]).unwrap();
        assert!(to_request(cli.command).is_err());

        let cli =
            Cli::try_parse_from(["dgt-sim", "clock", "--board", "1", "--left", "5m", "--run"])
                .unwrap();
        match to_request(cli.command).unwrap().0 {
            Request::Clock { clock, .. } => {
                assert_eq!(clock.left_ms, Some(300_000));
                assert_eq!(clock.running, Some(true));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn config_requires_at_least_one_option() {
        let cli = Cli::try_parse_from(["dgt-sim", "config", "--board", "1"]).unwrap();
        assert!(to_request(cli.command).is_err());

        let cli = Cli::try_parse_from([
            "dgt-sim",
            "config",
            "--board",
            "1",
            "--serial",
            "00042",
            "--firmware-version",
            "2.1",
        ])
        .unwrap();
        match to_request(cli.command).unwrap().0 {
            Request::Config { config, .. } => {
                assert_eq!(config.serial.as_deref(), Some("00042"));
                assert_eq!(config.version.as_deref(), Some("2.1"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn fault_flags_map_onto_the_fault_config() {
        let cli = Cli::try_parse_from([
            "dgt-sim",
            "fault",
            "--board",
            "3",
            "--latency",
            "200ms",
            "--drop-rate",
            "0.05",
            "--seed",
            "9",
        ])
        .unwrap();
        match to_request(cli.command).unwrap().0 {
            Request::Fault { board, faults } => {
                assert_eq!(board, 3);
                assert_eq!(faults.latency_ms, 200);
                assert!((faults.drop_rate - 0.05).abs() < 1e-9);
                assert_eq!(faults.seed, 9);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn replay_maps_path_delay_and_speed() {
        let cli = Cli::try_parse_from([
            "dgt-sim", "replay", "--board", "1", "--pgn", "game.pgn", "--delay", "500ms",
            "--speed", "2", "--loop",
        ])
        .unwrap();
        match to_request(cli.command).unwrap().0 {
            Request::Replay {
                pgn,
                delay_ms,
                speed,
                repeat,
                ..
            } => {
                assert_eq!(pgn, "game.pgn");
                assert_eq!(delay_ms, 500);
                assert_eq!(speed, 2.0);
                assert!(repeat);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn start_parses_transport_and_faults() {
        let cli = Cli::try_parse_from([
            "dgt-sim",
            "start",
            "--boards",
            "100",
            "--transport",
            "tcp",
            "--latency",
            "1ms",
        ])
        .unwrap();
        match cli.command {
            Command::Start(args) => {
                assert_eq!(args.boards, 100);
                assert_eq!(args.transport, TransportArg::Tcp);
                assert_eq!(args.latency, Duration::from_millis(1));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn socket_flag_overrides_the_default() {
        let cli = Cli::try_parse_from(["dgt-sim", "--socket", "/tmp/x.sock", "list"]).unwrap();
        assert_eq!(socket_path(&cli), PathBuf::from("/tmp/x.sock"));
        assert!(socket_path(&Cli::try_parse_from(["dgt-sim", "list"]).unwrap()).is_absolute());
    }

    #[test]
    fn render_is_safe_for_empty_payloads() {
        // Rendering must not panic on a null payload (defensive: a future
        // daemon version might omit a field).
        render(&Request::Info, &Value::Null).unwrap();
        render(&Request::List, &serde_json::json!([])).unwrap();
        render(&Request::Show { board: 1 }, &serde_json::json!({"id": 1})).unwrap();
        render(&Request::Messages { board: 1 }, &serde_json::json!([])).unwrap();
        let _ = describe_socket(Path::new("/tmp/x.sock"));
        assert_eq!(
            json_lines(&serde_json::json!({"a": 1})).unwrap(),
            "{\"a\":1}"
        );
    }

    #[test]
    fn exit_code_helpers() {
        assert_eq!(Error::BoardNotFound(3).exit_code(), 1);
    }
}
