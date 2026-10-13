//! A virtual board: state plus the task that owns it.
//!
//! Each board runs as its own Tokio task. Nothing outside the task can touch
//! the board's state: every mutation goes through an [`BoardCmd`] on a
//! channel, and every read comes back through a `oneshot`. That is what makes
//! hundreds of boards safe to run concurrently without a single lock.
//!
//! An external client's bytes arrive as [`BoardCmd::HostBytes`] from the
//! transport, and the board's replies are returned from the command handler as
//! a list of already-framed protocol messages.

pub mod state;

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::chess;
use crate::error::{Error, Result};
use crate::protocol::commands::{HostCommand, clock_command};
use crate::protocol::messages::{self, bwtime};
use crate::protocol::pieces::{self, BOARD_LEN, Square, code};
use crate::transport::{Faults, Link, Rng, TransportKind, apply_latency, fragment};

pub use state::{
    BoardState, ClockCompat, ClockState, FieldChange, Mode, Side, default_bus_address,
    parse_version,
};

/// How literal the emulated physical sequence should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhysicalStyle {
    /// Minimum sequence: lift the source, put the piece down on the target.
    /// A capture becomes a single "piece landed" update, which the DGT
    /// description explicitly allows.
    Direct,
    /// Also emit the intermediate states a real board sometimes reports:
    /// the target being cleared before a capture, and the pawn arriving
    /// before a promotion.
    Verbose,
}

impl PhysicalStyle {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Verbose => "verbose",
        }
    }

    pub const fn is_verbose(self) -> bool {
        matches!(self, Self::Verbose)
    }
}

impl std::str::FromStr for PhysicalStyle {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "direct" => Ok(Self::Direct),
            "verbose" => Ok(Self::Verbose),
            other => Err(Error::Other(format!(
                "unknown physical style {other:?} (expected direct or verbose)"
            ))),
        }
    }
}

/// How many recently emitted messages are kept for `dgt-sim messages`.
pub const MESSAGE_LOG_CAPACITY: usize = 64;

/// Everything that can be asked of a board.
pub enum BoardCmd {
    /// Raw bytes from the connected client.
    HostBytes(Vec<u8>),
    /// Load a FEN placement, emitting field updates for the difference.
    SetFen { fen: String, quiet: bool },
    /// Full reset: starting position, IDLE mode, power-up EEPROM.
    Reset { emit: bool },
    /// Lift a piece off a field.
    Lift(Square),
    /// Put a piece on a field (raw DGT code).
    Place(Square, u8),
    /// Empty a field.
    Clear(Square),
    /// Play a move, geometrically.
    Move { uci: String },
    /// Force a transfer mode, as if a client had sent the command.
    SetMode(Mode),
    /// Manipulate the emulated clock.
    Clock(ClockRequest),
    /// Change board identity and emulation details.
    Config(BoardConfigPatch),
    /// Replace the fault-injection configuration.
    Fault(Faults),
    /// Explicitly drop the current link.
    Disconnect,
    /// Re-open the link after a [`BoardCmd::Disconnect`].
    Reconnect,
    /// Read the board's state.
    Snapshot(oneshot::Sender<Snapshot>),
    /// Read the recent outbound protocol messages, as hex.
    Messages(oneshot::Sender<Vec<MessageLogEntry>>),
    /// Terminate this board.
    Shutdown,
}

/// Clock manipulation, all fields optional so a caller can change one at a time.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClockRequest {
    pub left: Option<Duration>,
    pub right: Option<Duration>,
    pub running: Option<bool>,
    pub turn: Option<Side>,
    pub connected: Option<bool>,
    pub right_high: Option<bool>,
    pub battery_low: Option<bool>,
}

/// A per-board configuration change.
#[derive(Debug, Clone, Default)]
pub struct BoardConfigPatch {
    pub serial: Option<String>,
    pub long_serial: Option<String>,
    pub version: Option<(u8, u8)>,
    pub trademark: Option<String>,
    pub bus_address: Option<u16>,
    pub scan_delay: Option<Duration>,
    pub clock_compat: Option<ClockCompat>,
    pub physical_style: Option<PhysicalStyle>,
}

/// One entry of the recent-message log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageLogEntry {
    /// Seconds since the board was created.
    pub at_ms: u64,
    /// Protocol message name, e.g. `field-update`.
    pub name: String,
    /// Hex encoding of the full framed message.
    pub hex: String,
    /// Total length in bytes.
    pub len: usize,
    /// Was this message actually handed to the transport?
    pub delivered: bool,
}

/// Everything `show` and `status` report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: u32,
    pub transport: String,
    pub endpoint: String,
    pub connected: bool,
    pub enabled: bool,
    pub peer: Option<String>,
    pub mode: String,
    pub serial: String,
    pub long_serial: String,
    pub bus_address: u16,
    pub version: String,
    pub trademark: String,
    pub physical_style: String,
    pub clock_compat: String,
    pub scan_delay_ms: u64,
    pub fen: String,
    pub ascii: String,
    pub magic: Vec<String>,
    pub clock: ClockView,
    pub faults: Faults,
    pub eeprom_len: usize,
    pub fields_changed: u64,
    pub messages_sent: u64,
    pub host_commands: u64,
    pub protocol_errors: u64,
    /// Protocol messages produced by the most recent command. Zero means the
    /// mutation happened but nothing was streamed (the board is not in an
    /// update mode, or no client is attached).
    pub last_emitted: usize,
}

/// Serialisable view of the emulated clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClockView {
    pub connected: bool,
    pub running: bool,
    pub to_move: String,
    pub right_high: bool,
    pub battery_low: bool,
    pub left_ms: u64,
    pub right_ms: u64,
    pub left: String,
    pub right: String,
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// What a command handler produced.
#[derive(Default)]
struct Outcome {
    messages: Vec<Vec<u8>>,
    stop: bool,
}

/// A live virtual board.
pub struct BoardTask {
    pub id: u32,
    pub state: BoardState,
    pub parser: crate::protocol::CommandParser,
    pub faults: Faults,
    pub rng: Rng,
    pub physical_style: PhysicalStyle,
    pub endpoint: String,
    pub transport: TransportKind,
    /// Link currently attached and usable.
    pub connected: bool,
    /// Is the board willing to accept links at all?
    pub enabled: bool,
    pub peer: Option<String>,
    pub host_commands: u64,
    pub protocol_errors: u64,
    /// How many protocol messages the most recent command produced.
    last_emitted: usize,
    log: VecDeque<MessageLogEntry>,
    started: Instant,
    last_tick: Instant,
}

impl BoardTask {
    pub fn new(id: u32, transport: TransportKind, endpoint: String) -> Self {
        let now = Instant::now();
        let mut state = BoardState::new();
        state.serial = format!("{id:05}");
        state.long_serial = format!("{id:010}");
        state.bus_address = default_bus_address(&state.serial);
        Self {
            id,
            state,
            parser: crate::protocol::CommandParser::new(),
            faults: Faults::default(),
            rng: Rng::new(0),
            physical_style: PhysicalStyle::Direct,
            endpoint,
            transport,
            connected: false,
            enabled: true,
            peer: None,
            host_commands: 0,
            protocol_errors: 0,
            last_emitted: 0,
            log: VecDeque::with_capacity(MESSAGE_LOG_CAPACITY),
            started: now,
            last_tick: now,
        }
    }

    /// Drive the board until it is shut down or its command channel closes.
    pub async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<BoardCmd>,
        mut link_rx: mpsc::Receiver<Link>,
    ) {
        let mut link: Option<Link> = None;
        let mut buf = vec![0u8; 8192];
        // 200 ms matches the real board's full-scan period and is comfortably
        // inside the "at least every second" clock requirement.
        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        tracing::debug!(board = self.id, endpoint = %self.endpoint, "board task started");
        loop {
            tokio::select! {
                biased;
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let outcome = self.handle(cmd);
                    if let Err(err) = self.flush(&mut link, outcome.messages).await {
                        tracing::debug!(board = self.id, error = %err, "link write failed");
                    }
                    if outcome.stop {
                        break;
                    }
                }
                result = read_link(&mut link, &mut buf), if link.is_some() => {
                    match result {
                        Ok(0) => {
                            tracing::debug!(board = self.id, "client closed the connection");
                            link = None;
                            self.connected = false;
                            self.peer = None;
                            self.parser.clear();
                        }
                        Ok(n) => {
                            let bytes = buf[..n].to_vec();
                            let outcome = self.handle(BoardCmd::HostBytes(bytes));
                            if let Err(err) = self.flush(&mut link, outcome.messages).await {
                                tracing::debug!(board = self.id, error = %err, "link write failed");
                            }
                        }
                        Err(err) => {
                            tracing::debug!(board = self.id, error = %err, "link read failed");
                            link = None;
                            self.connected = false;
                            self.peer = None;
                            self.parser.clear();
                        }
                    }
                }
                incoming = link_rx.recv() => {
                    match incoming {
                        None => {}
                        Some(new_link) => {
                            if !self.enabled {
                                tracing::debug!(board = self.id, "link refused: board is disconnected");
                                continue;
                            }
                            if let Some(mut old) = link.take() {
                                old.shutdown().await;
                            }
                            self.peer = Some(new_link.peer());
                            link = Some(new_link);
                            self.connected = true;
                            // A fresh client starts from a clean parser: the
                            // previous session may have died mid-command.
                            self.parser.clear();
                            tracing::debug!(board = self.id, peer = %self.peer.as_deref().unwrap_or(""), "client connected");
                        }
                    }
                }
                _ = ticker.tick() => {
                    let outcome = self.tick(Instant::now());
                    if let Err(err) = self.flush(&mut link, outcome.messages).await {
                        tracing::debug!(board = self.id, error = %err, "link write failed");
                    }
                }
            }
        }
        if let Some(mut l) = link {
            l.shutdown().await;
        }
        tracing::debug!(board = self.id, "board task stopped");
    }

    /// Time-driven work: advance the clock and emit whatever is due.
    fn tick(&mut self, now: Instant) -> Outcome {
        let elapsed = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        let mut out = Outcome::default();

        if let Some(side) = self.state.clock.advance(elapsed) {
            let time = match side {
                Side::Left => self.state.clock.left,
                Side::Right => self.state.clock.right,
            };
            self.state.ee_clock_event(side, time);
            tracing::debug!(board = self.id, side = side.name(), "clock flag fell");
        }
        if let Some(msg) = self.state.clock_message(now) {
            out.messages.push(msg);
        }
        out
    }

    /// Apply a command, returning the messages to send to the client.
    fn handle(&mut self, cmd: BoardCmd) -> Outcome {
        match cmd {
            BoardCmd::HostBytes(bytes) => self.on_host_bytes(&bytes),
            BoardCmd::SetMode(mode) => self.on_set_mode(mode),
            BoardCmd::SetFen { fen, quiet } => match self.on_set_fen(&fen, quiet) {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!(board = self.id, error = %err, "set-fen rejected");
                    Outcome::default()
                }
            },
            BoardCmd::Reset { emit } => self.on_reset(emit),
            BoardCmd::Lift(square) => self.on_plan(vec![FieldChange::new(square, code::EMPTY)]),
            BoardCmd::Place(square, pc) => self.on_plan(vec![FieldChange::new(square, pc)]),
            BoardCmd::Clear(square) => self.on_plan(vec![FieldChange::new(square, code::EMPTY)]),
            BoardCmd::Move { uci } => match self.on_move(&uci) {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!(board = self.id, uci = %uci, error = %err, "move rejected");
                    Outcome::default()
                }
            },
            BoardCmd::Clock(req) => {
                self.on_clock(req);
                Outcome::default()
            }
            BoardCmd::Config(patch) => {
                self.on_config(patch);
                Outcome::default()
            }
            BoardCmd::Fault(faults) => {
                self.faults = faults;
                self.rng = Rng::new(faults.seed);
                Outcome::default()
            }
            BoardCmd::Disconnect => {
                self.enabled = false;
                self.connected = false;
                self.peer = None;
                self.parser.clear();
                Outcome::default()
            }
            BoardCmd::Reconnect => {
                self.enabled = true;
                self.state.mode = Mode::UpdateBoard;
                Outcome::default()
            }
            BoardCmd::Snapshot(reply) => {
                let _ = reply.send(self.snapshot());
                Outcome::default()
            }
            BoardCmd::Messages(reply) => {
                let _ = reply.send(self.log.iter().cloned().collect());
                Outcome::default()
            }
            BoardCmd::Shutdown => Outcome {
                messages: Vec::new(),
                stop: true,
            },
        }
    }

    // ---- command execution ---------------------------------------------

    fn on_host_bytes(&mut self, bytes: &[u8]) -> Outcome {
        self.parser.push(bytes);
        let mut out = Outcome::default();
        loop {
            match self.parser.next_command() {
                None => break,
                Some(Err(err)) => {
                    self.protocol_errors += 1;
                    tracing::warn!(board = self.id, error = %err, "malformed host command");
                }
                Some(Ok(cmd)) => {
                    self.host_commands += 1;
                    self.execute(cmd, &mut out.messages);
                }
            }
        }
        out
    }

    fn execute(&mut self, cmd: HostCommand, out: &mut Vec<Vec<u8>>) {
        tracing::trace!(board = self.id, command = cmd.name(), "host command");
        match cmd {
            HostCommand::Reset => {
                self.state.mode = Mode::Idle;
            }
            HostCommand::SendClock => {
                let times = self.state.clock.times(self.state.clock_compat);
                out.push(bwtime(&times));
            }
            HostCommand::SendBoard => out.push(self.state.board_dump()),
            HostCommand::SendUpdate => {
                self.state.mode = Mode::Update;
                self.state.reset_clock_cache();
                if let Some(msg) = self.state.clock_message(Instant::now()) {
                    out.push(msg);
                }
            }
            HostCommand::SendUpdateNice => {
                self.state.mode = Mode::UpdateNice;
                self.state.reset_clock_cache();
                if let Some(msg) = self.state.clock_message(Instant::now()) {
                    out.push(msg);
                }
            }
            HostCommand::SendUpdateBoard => {
                self.state.mode = Mode::UpdateBoard;
            }
            HostCommand::ReturnSerialNr => match messages::serial_nr(&self.state.serial) {
                Ok(msg) => out.push(msg),
                Err(err) => tracing::warn!(board = self.id, error = %err, "serial number rejected"),
            },
            HostCommand::ReturnLongSerialNr => {
                match messages::long_serial_nr(&self.state.long_serial) {
                    Ok(msg) => out.push(msg),
                    Err(err) => {
                        tracing::warn!(board = self.id, error = %err, "long serial rejected")
                    }
                }
            }
            HostCommand::ReturnBusAdres => out.push(messages::bus_adres(self.state.bus_address)),
            HostCommand::SendTrademark => match messages::trademark(&self.state.trademark) {
                Ok(msg) => out.push(msg),
                Err(err) => tracing::warn!(board = self.id, error = %err, "trademark rejected"),
            },
            HostCommand::SendVersion => {
                out.push(messages::version(
                    self.state.version.0,
                    self.state.version.1,
                ));
            }
            HostCommand::SendEeMoves => match messages::ee_moves(&self.state.eeprom_dump()) {
                Ok(msg) => out.push(msg),
                Err(err) => tracing::warn!(board = self.id, error = %err, "EEPROM dump rejected"),
            },
            HostCommand::SendBatteryStatus => {
                out.push(messages::battery_status(100, 0x7f, 0x7f, 0))
            }
            HostCommand::Clock { command, data } => self.on_clock_command(command, &data, out),
            HostCommand::SetLeds {
                pattern,
                first,
                last,
            } => {
                tracing::debug!(
                    board = self.id,
                    pattern,
                    first,
                    last,
                    "set-leds ignored (Revelation II only)"
                );
            }
            HostCommand::ToBusmode | HostCommand::StartBootloader => {
                tracing::warn!(
                    board = self.id,
                    command = cmd.name(),
                    "command ignored: not supported by this emulator"
                );
            }
            HostCommand::Scan50B
            | HostCommand::Scan50W
            | HostCommand::Scan100
            | HostCommand::SendBoard50B
            | HostCommand::SendBoard50W => {
                tracing::warn!(
                    board = self.id,
                    command = cmd.name(),
                    "draughts command ignored (only 8x8 chess is emulated)"
                );
            }
            HostCommand::Unknown(byte) => {
                self.protocol_errors += 1;
                tracing::warn!(
                    board = self.id,
                    byte = format!("{byte:#04x}"),
                    "unknown command"
                );
            }
        }
    }

    /// The board ACKs clock commands only in UPDATE and UPDATE_NICE mode.
    fn on_clock_command(&mut self, command: u8, data: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut ack2 = 0u8;
        match command {
            clock_command::SETNRUN => {
                if data.len() < 7 {
                    tracing::warn!(board = self.id, "set-n-run with a short payload");
                } else {
                    // hours are binary, minutes and seconds are BCD.
                    let left = Duration::from_secs(
                        u64::from(data[0] & 0x0f) * 3600
                            + decode_bcd(data[1]) * 60
                            + decode_bcd(data[2]),
                    );
                    let right = Duration::from_secs(
                        u64::from(data[3] & 0x0f) * 3600
                            + decode_bcd(data[4]) * 60
                            + decode_bcd(data[5]),
                    );
                    let flags = data[6];
                    self.state.clock.left = left;
                    self.state.clock.right = right;
                    self.state.clock.running = flags & 0x04 == 0;
                    self.state.clock.to_move = if flags & 0x01 != 0 {
                        Side::Left
                    } else {
                        Side::Right
                    };
                    self.state.reset_clock_cache();
                }
            }
            clock_command::VERSION => {
                ack2 = (self.state.version.0 << 4) | (self.state.version.1 & 0x0f);
            }
            clock_command::DISPLAY
            | clock_command::ICONS
            | clock_command::END
            | clock_command::BUTTON
            | clock_command::BEEP
            | clock_command::ASCII => {}
            other => {
                tracing::warn!(board = self.id, command = other, "unknown clock command");
                if self.state.mode.streams_clock() {
                    // Documented error ack for an unsupported command.
                    out.push(messages::build_clock_ack(0x40, 0, 0, 0, false));
                }
                return;
            }
        }

        self.state.clock.last_ack = Some(command);
        if self.state.mode.streams_clock() {
            out.push(messages::clock_ack(command, ack2, 0, false));
        }
    }

    fn on_set_mode(&mut self, mode: Mode) -> Outcome {
        let mut out = Outcome::default();
        self.state.mode = mode;
        if mode.streams_clock() {
            self.state.reset_clock_cache();
            if let Some(msg) = self.state.clock_message(Instant::now()) {
                out.messages.push(msg);
            }
        }
        out
    }

    fn on_set_fen(&mut self, fen: &str, quiet: bool) -> Result<Outcome> {
        let target = chess::parse_fen(fen)?;
        Ok(self.on_position(target, quiet))
    }

    /// Replace the placement, emitting a field update per changed field.
    fn on_position(&mut self, target: [u8; BOARD_LEN], quiet: bool) -> Outcome {
        let changes: Vec<FieldChange> = (0..BOARD_LEN)
            .filter(|i| self.state.squares[*i] != target[*i])
            .map(|i| FieldChange::new(Square(i as u8), target[i]))
            .collect();
        if quiet {
            self.state.set_position(target);
            return Outcome::default();
        }
        self.on_plan(changes)
    }

    /// Apply a physical sequence: update state, then stream it if the mode
    /// calls for it.
    fn on_plan(&mut self, changes: Vec<FieldChange>) -> Outcome {
        let applied = self.state.apply_changes(&changes);
        let mut out = Outcome::default();
        if !self.state.mode.streams_fields() {
            tracing::debug!(
                board = self.id,
                changed = applied.len(),
                mode = self.state.mode.name(),
                "field changes applied but not streamed: board is not in an update mode"
            );
            return out;
        }
        for change in applied {
            out.messages.push(self.state.field_update(change));
        }
        out
    }

    fn on_move(&mut self, uci: &str) -> Result<Outcome> {
        let plan = chess::plan_uci(&self.state.squares, uci, self.physical_style.is_verbose())?;
        Ok(self.on_plan(plan))
    }

    fn on_reset(&mut self, emit: bool) -> Outcome {
        let mut out = Outcome::default();
        if emit && self.state.mode.streams_fields() {
            out = self.on_position(protocol_start(), false);
        } else {
            self.state.set_position(protocol_start());
        }
        self.state.mode = Mode::Idle;
        self.state.eeprom_clear();
        self.state.clock = ClockState::default();
        self.state.reset_clock_cache();
        // A real board writes this sequence at power-up.
        self.state.ee_clock_event(Side::Left, self.state.clock.left);
        out
    }

    fn on_clock(&mut self, req: ClockRequest) {
        let clock = &mut self.state.clock;
        if let Some(v) = req.left {
            clock.left = v;
        }
        if let Some(v) = req.right {
            clock.right = v;
        }
        if let Some(v) = req.running {
            clock.running = v;
        }
        if let Some(v) = req.turn {
            clock.to_move = v;
        }
        if let Some(v) = req.connected {
            clock.connected = v;
        }
        if let Some(v) = req.right_high {
            clock.right_high = v;
        }
        if let Some(v) = req.battery_low {
            clock.battery_low = v;
        }
        self.state.reset_clock_cache();
    }

    fn on_config(&mut self, patch: BoardConfigPatch) {
        if let Some(serial) = patch.serial {
            self.state.bus_address = default_bus_address(&serial);
            self.state.serial = serial;
        }
        if let Some(long) = patch.long_serial {
            self.state.long_serial = long;
        }
        if let Some(bus) = patch.bus_address {
            self.state.bus_address = bus & 0x3fff;
        }
        if let Some(v) = patch.version {
            self.state.version = v;
        }
        if let Some(t) = patch.trademark {
            self.state.trademark = t;
        }
        if let Some(d) = patch.scan_delay {
            self.state.scan_delay = d;
        }
        if let Some(c) = patch.clock_compat {
            self.state.clock_compat = c;
        }
        if let Some(p) = patch.physical_style {
            self.physical_style = p;
        }
    }

    // ---- output --------------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        let clock = &self.state.clock;
        Snapshot {
            id: self.id,
            transport: self.transport.name().to_string(),
            endpoint: self.endpoint.clone(),
            connected: self.connected,
            enabled: self.enabled,
            peer: self.peer.clone(),
            mode: self.state.mode.name().to_string(),
            serial: self.state.serial.clone(),
            long_serial: self.state.long_serial.clone(),
            bus_address: self.state.bus_address,
            version: format!("{}.{}", self.state.version.0, self.state.version.1),
            trademark: self.state.trademark.clone(),
            physical_style: self.physical_style.name().to_string(),
            clock_compat: self.state.clock_compat.name().to_string(),
            scan_delay_ms: self.state.scan_delay.as_millis() as u64,
            fen: self.state.fen_placement(),
            ascii: self.state.ascii(),
            magic: self
                .state
                .magic_squares()
                .into_iter()
                .map(|(sq, c)| format!("{}:{}", sq.uci(), pieces::code_name(c)))
                .collect(),
            clock: ClockView {
                connected: clock.connected,
                running: clock.running,
                to_move: clock.to_move.name().to_string(),
                right_high: clock.right_high,
                battery_low: clock.battery_low,
                left_ms: clock.left.as_millis() as u64,
                right_ms: clock.right.as_millis() as u64,
                left: format_duration(clock.left),
                right: format_duration(clock.right),
            },
            faults: self.faults,
            eeprom_len: self.state.eeprom_len(),
            fields_changed: self.state.fields_changed,
            messages_sent: self.state.messages_sent,
            host_commands: self.host_commands,
            protocol_errors: self.protocol_errors,
            last_emitted: self.last_emitted,
        }
    }

    /// Send the messages produced by a command, applying fault injection.
    async fn flush(&mut self, link: &mut Option<Link>, msgs: Vec<Vec<u8>>) -> io::Result<()> {
        self.last_emitted = msgs.len();
        for msg in msgs {
            let name = message_name(&msg);
            self.log_message(name, &msg, link.is_some() && self.enabled);
            let Some(target) = link.as_mut() else {
                continue;
            };
            if !self.enabled {
                continue;
            }
            apply_latency(&self.faults).await;
            let fragments = fragment(&msg, &self.faults, &mut self.rng);
            let mut broken = false;
            for piece in &fragments {
                if let Err(err) = target.write_all(piece).await {
                    tracing::debug!(board = self.id, error = %err, "write failed");
                    broken = true;
                    break;
                }
                if self.state.scan_delay > Duration::ZERO {
                    tokio::time::sleep(self.state.scan_delay).await;
                }
            }
            if broken
                || (self.faults.disconnect_rate > 0.0
                    && self.rng.next_f64() < self.faults.disconnect_rate)
            {
                *link = None;
                self.connected = false;
                self.peer = None;
                self.parser.clear();
                return Ok(());
            }
        }
        Ok(())
    }

    fn log_message(&mut self, name: &str, msg: &[u8], delivered: bool) {
        if self.log.len() >= MESSAGE_LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(MessageLogEntry {
            at_ms: self.started.elapsed().as_millis() as u64,
            name: name.to_string(),
            hex: hex_encode(msg),
            len: msg.len(),
            delivered,
        });
    }
}

/// The starting position, used by `reset`.
fn protocol_start() -> [u8; BOARD_LEN] {
    pieces::START_SQUARES
}

fn decode_bcd(byte: u8) -> u64 {
    u64::from(byte >> 4) * 10 + u64::from(byte & 0x0f)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Protocol message name from the first byte, for the message log.
pub fn message_name(frame: &[u8]) -> &'static str {
    match frame.first().map(|b| b & 0x7f) {
        Some(messages::id::BOARD_DUMP) => "board-dump",
        Some(messages::id::BWTIME) => {
            if messages::is_clock_ack(&frame[3..]) {
                "clock-ack"
            } else {
                "bw-time"
            }
        }
        Some(messages::id::FIELD_UPDATE) => "field-update",
        Some(messages::id::EE_MOVES) => "ee-moves",
        Some(messages::id::BUSADRES) => "bus-adres",
        Some(messages::id::SERIALNR) => "serial-nr",
        Some(messages::id::TRADEMARK) => "trademark",
        Some(messages::id::VERSION) => "version",
        Some(messages::id::BATTERY_STATUS) => "battery-status",
        Some(messages::id::LONG_SERIALNR) => "long-serial-nr",
        Some(_) => "unknown",
        None => "empty",
    }
}

/// Read from the current link, or wait forever when there is none. The
/// `select!` guard means the `None` branch is unreachable in practice; keeping
/// it here avoids an `unwrap` in the hot path.
async fn read_link(link: &mut Option<Link>, buf: &mut [u8]) -> io::Result<usize> {
    match link {
        Some(l) => l.read(buf).await,
        None => std::future::pending().await,
    }
}

/// Resolve a raw piece code for `place`, including magic pieces.
pub fn resolve_placement(spec: &str) -> Result<u8> {
    pieces::parse_placement(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::id;

    fn task() -> BoardTask {
        BoardTask::new(1, TransportKind::Tcp, "127.0.0.1:0".into())
    }

    fn frames(out: &Outcome) -> Vec<(u8, Vec<u8>)> {
        out.messages
            .iter()
            .map(|m| (m[0] & 0x7f, m[3..].to_vec()))
            .collect()
    }

    fn host(task: &mut BoardTask, bytes: &[u8]) -> Outcome {
        task.on_host_bytes(bytes)
    }

    #[test]
    fn board_dump_is_served_without_entering_update_mode() {
        let mut task = task();
        let out = host(&mut task, &[0x42]);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0][0] & 0x7f, id::BOARD_DUMP);
        assert_eq!(out.messages[0].len(), 67);
        assert_eq!(task.state.mode, Mode::Idle);
    }

    #[test]
    fn identification_commands_are_answered() {
        let mut task = task();
        task.state.serial = "00137".into();
        task.state.long_serial = "0000000137".into();
        task.state.version = (1, 8);
        let out = host(&mut task, &[0x45, 0x55, 0x46, 0x47, 0x4d]);
        let got = frames(&out);
        assert_eq!(
            got.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![
                id::SERIALNR,
                id::LONG_SERIALNR,
                id::BUSADRES,
                id::TRADEMARK,
                id::VERSION
            ]
        );
        assert_eq!(&got[0].1, b"00137");
        assert_eq!(&got[4].1, &[1, 8]);
    }

    #[test]
    fn field_updates_only_stream_in_update_modes() {
        let mut task = task();
        let quiet = task.on_plan(vec![FieldChange::new(
            Square::from_uci("e2").unwrap(),
            code::EMPTY,
        )]);
        assert!(quiet.messages.is_empty(), "IDLE mode must stay silent");
        assert_eq!(
            task.state.code_at(Square::from_uci("e2").unwrap()),
            code::EMPTY
        );

        host(&mut task, &[0x44]);
        let loud = task.on_plan(vec![FieldChange::new(
            Square::from_uci("e2").unwrap(),
            code::WPAWN,
        )]);
        assert_eq!(loud.messages.len(), 1);
        assert_eq!(loud.messages[0][0] & 0x7f, id::FIELD_UPDATE);
    }

    #[test]
    fn move_emits_lift_then_place_in_order() {
        let mut task = task();
        host(&mut task, &[0x44]);
        let out = task.on_move("e2e4").unwrap();
        let got = frames(&out);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, vec![52, code::EMPTY], "e2 emptied first");
        assert_eq!(got[1].1, vec![36, code::WPAWN], "then e4 occupied");
    }

    #[test]
    fn reset_returns_to_start_and_idle() {
        let mut task = task();
        host(&mut task, &[0x44]);
        task.on_move("e2e4").unwrap();
        assert_eq!(
            task.state.squares[Square::from_uci("e4").unwrap().index()],
            code::WPAWN
        );
        let out = task.on_reset(true);
        assert_eq!(task.state.mode, Mode::Idle);
        assert_eq!(
            task.state.fen_placement(),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR"
        );
        // The pawn goes back to e2 and e4 is cleared.
        assert!(out.messages.iter().any(|m| m[3..] == [36, code::EMPTY]));
        assert!(out.messages.iter().any(|m| m[3..] == [52, code::WPAWN]));
    }

    #[test]
    fn update_mode_answers_the_clock_query_immediately() {
        let mut task = task();
        let out = host(&mut task, &[0x43]);
        assert_eq!(task.state.mode, Mode::Update);
        assert_eq!(out.messages[0][0] & 0x7f, id::BWTIME);
        assert_eq!(out.messages[0].len(), 10);
        assert!(!messages::is_clock_ack(&out.messages[0][3..]));
    }

    #[test]
    fn clock_commands_are_acked_only_in_clock_streaming_modes() {
        let clock_version = [0x2b, 0x03, 0x03, 0x09, 0x00];
        let mut task = task();
        task.state.version = (3, 4);
        let out = host(&mut task, &clock_version);
        assert!(out.messages.is_empty(), "IDLE mode sends no ack");

        host(&mut task, &[0x44]);
        let out = host(&mut task, &clock_version);
        assert!(out.messages.is_empty(), "UPDATE_BOARD mode sends no ack");

        host(&mut task, &[0x4b]);
        let out = host(&mut task, &clock_version);
        assert_eq!(out.messages.len(), 1);
        let ack = messages::decode_clock_ack(&out.messages[0][3..]).unwrap();
        assert_eq!(ack[1], clock_command::VERSION);
        assert_eq!(ack[2], 0x34, "3.4 reported as a nibble pair");
    }

    #[test]
    fn set_n_run_clock_command_programs_the_clock() {
        let mut task = task();
        host(&mut task, &[0x4b]);
        // 5 minutes 0 seconds on both sides, running, left to move.
        let frame = [0x2b, 0x0a, 0x03, 0x0a, 0, 5, 0, 0, 5, 0, 0x01, 0x00];
        host(&mut task, &frame);
        assert_eq!(task.state.clock.left, Duration::from_secs(300));
        assert_eq!(task.state.clock.right, Duration::from_secs(300));
        assert!(task.state.clock.running);
        assert_eq!(task.state.clock.to_move, Side::Left);
    }

    #[test]
    fn fractional_host_writes_are_reassembled() {
        let mut task = task();
        // UPDATE_NICE also emits an initial clock message.
        host(&mut task, &[0x4b]);
        assert_eq!(task.host_commands, 1);
        // A clock-version command split across three writes, then a board dump.
        host(&mut task, &[0x2b]);
        host(&mut task, &[0x03, 0x03]);
        assert_eq!(task.host_commands, 1, "the clock frame is still incomplete");
        let out = host(&mut task, &[0x09, 0x00, 0x42]);
        assert_eq!(task.host_commands, 3);
        let got = frames(&out);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, id::BWTIME, "clock ack");
        assert!(messages::is_clock_ack(&got[0].1));
        assert_eq!(got[1].0, id::BOARD_DUMP);
    }

    #[test]
    fn malformed_commands_are_counted_not_fatal() {
        let mut task = task();
        host(&mut task, &[0x01]);
        assert_eq!(task.protocol_errors, 1);
        let out = host(&mut task, &[0x42]);
        assert_eq!(out.messages.len(), 1, "the board keeps working");
    }

    #[test]
    fn eeprom_dump_is_served_and_capped() {
        let mut task = task();
        host(&mut task, &[0x44]);
        task.on_move("e2e4").unwrap();
        let out = host(&mut task, &[0x49]);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0][0] & 0x7f, id::EE_MOVES);
        let payload = &out.messages[0][3..];
        assert_eq!(*payload.first().unwrap(), messages::ee::DOWNLOADED);
        assert_eq!(*payload.last().unwrap(), messages::ee::EOF);
        assert_eq!(payload.len(), crate::board::state::EEPROM_SIZE + 2);
        assert_eq!(out.messages[0].len(), crate::board::state::EEPROM_SIZE + 5);
    }

    #[test]
    fn disconnect_and_reconnect_toggle_link_acceptance() {
        let mut task = task();
        task.handle(BoardCmd::Disconnect);
        assert!(!task.enabled);
        task.handle(BoardCmd::Reconnect);
        assert!(task.enabled);
        assert_eq!(task.state.mode, Mode::UpdateBoard);
    }

    #[test]
    fn config_patch_updates_identity() {
        let mut task = task();
        task.handle(BoardCmd::Config(BoardConfigPatch {
            serial: Some("00999".into()),
            version: Some((2, 7)),
            clock_compat: Some(ClockCompat::Java),
            physical_style: Some(PhysicalStyle::Verbose),
            ..Default::default()
        }));
        assert_eq!(task.state.serial, "00999");
        assert_eq!(task.state.bus_address, 999);
        assert_eq!(task.state.version, (2, 7));
        assert_eq!(task.physical_style, PhysicalStyle::Verbose);
        let snap = task.snapshot();
        assert_eq!(snap.version, "2.7");
        assert_eq!(snap.clock_compat, "java");
    }

    #[test]
    fn snapshot_reports_the_position_and_clock() {
        let task = task();
        let snap = task.snapshot();
        assert_eq!(snap.fen, "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR");
        assert_eq!(snap.clock.left, "0:05:00");
        assert_eq!(snap.mode, "idle");
        assert!(snap.ascii.contains("a  b  c"));
    }

    #[test]
    fn message_log_records_name_hex_and_delivery() {
        let mut task = task();
        let out = host(&mut task, &[0x42]);
        assert_eq!(out.messages.len(), 1);
        task.log_message(message_name(&out.messages[0]), &out.messages[0], true);
        let entry = task.log.back().unwrap();
        assert_eq!(entry.name, "board-dump");
        assert_eq!(entry.len, 67);
        assert!(entry.hex.starts_with("86 00 43"));
    }

    #[test]
    fn unknown_clock_command_gets_an_error_ack() {
        let mut task = task();
        host(&mut task, &[0x4b]);
        let out = host(&mut task, &[0x2b, 0x03, 0x03, 0x7f, 0x00]);
        assert_eq!(out.messages.len(), 1);
        let ack = messages::decode_clock_ack(&out.messages[0][3..]).unwrap();
        assert_eq!(ack[0], 0x40);
    }

    #[test]
    fn verbose_promotion_and_capture_sequences_are_available() {
        let mut task = task();
        host(&mut task, &[0x44]);
        task.physical_style = PhysicalStyle::Verbose;
        task.state.set_position(
            crate::protocol::pieces::from_fen_placement("8/P7/8/8/8/8/8/K6k").unwrap(),
        );
        // Verbose promotion on an empty target: lift the pawn, put it down,
        // then swap it for the promoted piece.
        let out = task.on_move("a7a8q").unwrap();
        assert_eq!(out.messages.len(), 3);
        assert_eq!(frames(&out)[1].1, vec![0, code::WPAWN]);
        assert_eq!(frames(&out)[2].1, vec![0, code::WQUEEN]);

        // A verbose capture additionally clears the occupied target.
        task.state.set_position(
            crate::protocol::pieces::from_fen_placement("8/8/8/3p4/4P3/8/8/K6k").unwrap(),
        );
        let out = task.on_move("e4d5").unwrap();
        assert_eq!(out.messages.len(), 3);
        assert_eq!(frames(&out)[1].1, vec![27, code::EMPTY], "d5 cleared");
        assert_eq!(frames(&out)[2].1, vec![27, code::WPAWN]);
    }
}
