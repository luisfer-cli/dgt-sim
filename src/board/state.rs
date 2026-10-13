//! Physical state of a virtual board.
//!
//! A [`BoardState`] is a *sensor* model, not a chess position. It records what
//! a real DGT board would report: which piece code sits on each field, which
//! mode the board is in, what its firmware claims to be, what the attached
//! clock shows, and what is stored in the on-board EEPROM.
//!
//! Nothing here enforces the laws of chess. Illegally placed pieces, a lone
//! knight, a king captured on the wrong square — all are representable, which
//! is exactly what a board emulator must support.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::protocol::messages::{self, ClockTimes, bwtime_flags, bwtime_status, ee};
use crate::protocol::pieces::{
    self, BOARD_LEN, Color, Role, START_SQUARES, Square, code, to_ascii, to_fen_placement,
};

/// Size of the on-board EEPROM ring buffer (`0x1f00` bytes).
pub const EEPROM_SIZE: usize = 0x1f00;

/// Number of pieces the board must see on files a, b, g, h for `EE_FOURROWS`.
const FOUR_ROWS_PIECES: usize = 16;

/// Which of the four documented transfer modes the board is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `DGT_SEND_RESET`: no automatic transfer at all.
    Idle,
    /// `DGT_SEND_UPDATE_BRD`: field updates only.
    UpdateBoard,
    /// `DGT_SEND_UPDATE`: field updates plus clock messages.
    Update,
    /// `DGT_SEND_UPDATE_NICE`: clock messages only when they change.
    UpdateNice,
}

impl Mode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::UpdateBoard => "update_board",
            Self::Update => "update",
            Self::UpdateNice => "update_nice",
        }
    }

    /// Are field updates streamed in this mode?
    pub const fn streams_fields(self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// Are clock messages streamed in this mode?
    pub const fn streams_clock(self) -> bool {
        matches!(self, Self::Update | Self::UpdateNice)
    }
}

/// Which clock screen is on the move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    pub const fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// How the two side-dependent bits of the `DGT_MSG_BWTIME` status byte are
/// interpreted.
///
/// The DGT header is self-contradictory here (its 20150720 changelog swaps D3
/// and D4 while describing D5 with inverted polarity), and `dgtdriver` in Java
/// resolves it differently from the header. Since we cannot check against real
/// hardware, both readings are available. See docs/protocol.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockCompat {
    /// Follow the DGT header: D3 = right to move, D4 = left to move,
    /// D5 set means *no* clock connected.
    Header,
    /// Follow arnsholt/dgtdriver: D3 = left to move, D4 = right to move,
    /// D5 set means the clock *is* connected.
    Java,
}

impl ClockCompat {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Java => "java",
        }
    }
}

/// Emulated DGT clock. The clock is a separate device attached to the board;
/// the board merely relays its messages.
#[derive(Debug, Clone, Serialize)]
pub struct ClockState {
    pub connected: bool,
    pub running: bool,
    pub left: Duration,
    pub right: Duration,
    pub left_flags: u8,
    pub right_flags: u8,
    /// Which screen is on the move.
    pub to_move: Side,
    /// D1 tumbler position: is the *right* side high?
    pub right_high: bool,
    pub battery_low: bool,
    /// Active clock command ACK bookkeeping: ack of the last clock command.
    pub last_ack: Option<u8>,
}

impl Default for ClockState {
    fn default() -> Self {
        Self {
            connected: true,
            running: false,
            left: Duration::from_secs(5 * 60),
            right: Duration::from_secs(5 * 60),
            left_flags: 0,
            right_flags: 0,
            to_move: Side::Left,
            right_high: false,
            battery_low: false,
            last_ack: None,
        }
    }
}

impl ClockState {
    /// Status byte for `DGT_MSG_BWTIME`, interpreted according to `compat`.
    pub fn status_byte(&self, compat: ClockCompat) -> u8 {
        let mut status = 0u8;
        if self.running {
            status |= bwtime_status::RUNNING;
        }
        if self.right_high {
            status |= bwtime_status::RIGHT_HIGH;
        }
        if self.battery_low {
            status |= bwtime_status::BATTERY_LOW;
        }
        match compat {
            ClockCompat::Header => {
                match self.to_move {
                    Side::Right => status |= bwtime_status::RIGHT_TO_MOVE,
                    Side::Left => status |= bwtime_status::LEFT_TO_MOVE,
                }
                if !self.connected {
                    status |= bwtime_status::NO_CLOCK;
                }
            }
            ClockCompat::Java => {
                match self.to_move {
                    // dgtdriver reads 0x08 as "left" and 0x20 as "connected".
                    Side::Left => status |= bwtime_status::RIGHT_TO_MOVE,
                    Side::Right => status |= bwtime_status::LEFT_TO_MOVE,
                }
                if self.connected {
                    status |= bwtime_status::NO_CLOCK;
                }
            }
        }
        status
    }

    /// Advance the running clock. Returns the *pre-tick* side that elapsed, if
    /// any.
    pub fn advance(&mut self, elapsed: Duration) -> Option<Side> {
        if !self.running || !self.connected {
            return None;
        }
        let side = self.to_move;
        let target = match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        };
        if *target > elapsed {
            *target -= elapsed;
            return None;
        }
        *target = Duration::ZERO;
        self.running = false;
        let flags = match side {
            Side::Left => &mut self.left_flags,
            Side::Right => &mut self.right_flags,
        };
        *flags |= bwtime_flags::FLAG_FALLEN | bwtime_flags::FLAG_INDICATED;
        Some(side)
    }

    /// Snapshot in the wire layout used by `DGT_MSG_BWTIME`.
    pub fn times(&self, compat: ClockCompat) -> ClockTimes {
        ClockTimes {
            left: self.left,
            left_flags: self.left_flags,
            right: self.right,
            right_flags: self.right_flags,
            status: self.status_byte(compat),
        }
    }
}

/// An appended change log used by the EEPROM dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldChange {
    /// Field that changed.
    pub square: Square,
    /// New raw DGT piece code (`0x00` = empty).
    pub code: u8,
}

impl FieldChange {
    pub fn new(square: Square, code: u8) -> Self {
        Self { square, code }
    }
}

/// Everything a virtual board knows about itself.
#[derive(Debug, Clone)]
pub struct BoardState {
    /// Raw sensor codes, field 0 = a8 .. field 63 = h1.
    pub squares: [u8; BOARD_LEN],
    pub mode: Mode,
    pub clock: ClockState,
    pub clock_compat: ClockCompat,
    /// Short serial number (5 ASCII digits on the wire).
    pub serial: String,
    /// Long serial number (10 ASCII digits on the wire, `0x55` command).
    pub long_serial: String,
    /// 14-bit bus address reported by `DGT_RETURN_BUSADRES`.
    pub bus_address: u16,
    pub version: (u8, u8),
    pub trademark: String,
    /// Extra latency between consecutive field updates, emulating scan timing.
    pub scan_delay: Duration,
    /// Signalled by placing a magic piece; exposed for scripting.
    eeprom: VecDeque<u8>,
    last_field_event: Option<Instant>,
    last_bwtime: Option<[u8; 7]>,
    last_bwtime_at: Option<Instant>,
    /// Total field updates emitted since creation.
    pub fields_changed: u64,
    /// Total protocol messages handed to the transport.
    pub messages_sent: u64,
}

impl Default for BoardState {
    fn default() -> Self {
        Self::new()
    }
}

impl BoardState {
    /// A board sitting in the standard starting position, in IDLE mode, with
    /// the power-up EEPROM sequence already written.
    pub fn new() -> Self {
        let mut state = Self {
            squares: START_SQUARES,
            mode: Mode::Idle,
            clock: ClockState::default(),
            clock_compat: ClockCompat::Header,
            serial: "00123".into(),
            long_serial: "0000000123".into(),
            bus_address: 0x0123,
            version: (1, 8),
            trademark: DEFAULT_TRADEMARK.into(),
            scan_delay: Duration::ZERO,
            eeprom: VecDeque::with_capacity(EEPROM_SIZE),
            last_field_event: None,
            last_bwtime: None,
            last_bwtime_at: None,
            fields_changed: 0,
            messages_sent: 0,
        };
        // "At power-on, three tags EE_NOP are written, followed by a one-byte
        // EE_POWERUP message."
        state.ee_push(&[ee::NOP, ee::NOP, ee::NOP, ee::POWERUP]);
        state.record_position_tags();
        state
    }

    /// Sensor code on a field.
    pub fn code_at(&self, square: Square) -> u8 {
        self.squares[square.index()]
    }

    pub fn piece_at(&self, square: Square) -> Option<pieces::Piece> {
        pieces::Piece::from_code(self.code_at(square))
    }

    /// FEN board field for the current placement (magic pieces are omitted).
    pub fn fen_placement(&self) -> String {
        to_fen_placement(&self.squares)
    }

    pub fn ascii(&self) -> String {
        to_ascii(&self.squares)
    }

    pub fn magic_squares(&self) -> Vec<(Square, u8)> {
        pieces::magic_squares(&self.squares)
    }

    pub fn set_position(&mut self, squares: [u8; BOARD_LEN]) {
        self.squares = squares;
    }

    /// Replace the whole placement, returning the field changes needed to get
    /// there. Used by `set-fen` and `reset`, which model a human placing and
    /// removing pieces.
    pub fn diff_to(&self, target: &[u8; BOARD_LEN]) -> Vec<FieldChange> {
        (0..BOARD_LEN)
            .filter(|i| self.squares[*i] != target[*i])
            .map(|i| FieldChange::new(Square(i as u8), target[i]))
            .collect()
    }

    /// Write one field, recording it in the EEPROM if the code actually
    /// changed. Returns `true` when something changed.
    pub fn set_field(&mut self, square: Square, code: u8) -> bool {
        if self.squares[square.index()] == code {
            return false;
        }
        self.squares[square.index()] = code;
        self.fields_changed += 1;
        self.ee_field_event(square, code);
        self.record_position_tags();
        true
    }

    /// Apply a batch of field changes in order, recording each one.
    pub fn apply_changes(&mut self, changes: &[FieldChange]) -> Vec<FieldChange> {
        let mut applied = Vec::with_capacity(changes.len());
        for change in changes {
            if self.set_field(change.square, change.code) {
                applied.push(*change);
            }
        }
        applied
    }

    // ---- EEPROM ---------------------------------------------------------

    pub fn eeprom_len(&self) -> usize {
        self.eeprom.len()
    }

    /// The recorded events, oldest first, without any padding.
    pub fn eeprom_bytes(&self) -> Vec<u8> {
        self.eeprom.iter().copied().collect()
    }

    /// The payload a real board returns for `DGT_SEND_EE_MOVES`:
    /// `EE_DOWNLOADED`, the ring buffer contents (oldest first), then
    /// `EE_EOF`.
    ///
    /// A real board always streams its whole 0x1f00-byte ring, so the message
    /// has a fixed size. Regions our emulator never wrote are filled with
    /// `0x00`, which the protocol defines as `EE_NOP2` — a no-op, so a client
    /// replaying the stream sees the events and nothing else.
    pub fn eeprom_dump(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(EEPROM_SIZE + 2);
        out.push(ee::DOWNLOADED);
        out.resize(1 + (EEPROM_SIZE - self.eeprom.len()), 0);
        out.extend(self.eeprom.iter().copied());
        out.push(ee::EOF);
        debug_assert_eq!(out.len(), EEPROM_SIZE + 2);
        out
    }

    pub fn eeprom_clear(&mut self) {
        self.eeprom.clear();
        self.last_field_event = None;
    }

    fn ee_push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.eeprom.len() >= EEPROM_SIZE {
                self.eeprom.pop_front();
            }
            self.eeprom.push_back(*byte);
        }
    }

    /// Two-byte field update tag: `0t0r nnnn`, `00ii iiii`.
    ///
    /// The `r` bit means "less than roughly 250 ms since the previous tag".
    fn ee_field_event(&mut self, square: Square, code: u8) {
        let now = Instant::now();
        let recent = self
            .last_field_event
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(250));
        self.last_field_event = Some(now);
        let first = 0x40 | if recent { 0x10 } else { 0 } | (code & 0x0f);
        self.ee_push(&[first, square.0]);
    }

    /// Three-byte clock tag: `0uuf hhhh`, minutes BCD, seconds BCD.
    pub fn ee_clock_event(&mut self, side: Side, time: Duration) {
        let secs = time.as_secs();
        let hours = (secs / 3600).min(9) as u8;
        let minutes = ((secs / 60) % 60) as u8;
        let seconds = (secs % 60) as u8;
        let first = 0x60 | if side == Side::Left { 0x10 } else { 0 } | hours;
        self.ee_push(&[
            first,
            ((minutes / 10) << 4) | (minutes % 10),
            ((seconds / 10) << 4) | (seconds % 10),
        ]);
    }

    /// Write the one-byte position tags a real board would write.
    ///
    /// The hardware applies hysteresis ("only after more than two pieces have
    /// been out of the begin position is the search restarted") so it does not
    /// flood the buffer. We re-evaluate on every change instead and rely on the
    /// `set_field` guard to only write when a field actually changed.
    fn record_position_tags(&mut self) {
        if self.squares == START_SQUARES {
            self.ee_push(&[ee::BEGINPOS]);
            return;
        }
        if self.squares == rotated_start() {
            self.ee_push(&[ee::BEGINPOS_ROT]);
            return;
        }
        if self.squares.iter().all(|c| *c == code::EMPTY) {
            self.ee_push(&[ee::EMPTYBOARD]);
            return;
        }
        if self.four_rows_occupied() {
            self.ee_push(&[ee::FOURROWS]);
        }
    }

    /// "16 chess figures ... all in the A, B, G and H row", not in the
    /// starting position.
    fn four_rows_occupied(&self) -> bool {
        let occupied = self
            .squares
            .iter()
            .enumerate()
            .filter(|(i, c)| {
                let file = *i % 8;
                matches!(file, 0 | 1 | 6 | 7) && **c != code::EMPTY
            })
            .count();
        occupied == FOUR_ROWS_PIECES
    }

    // ---- clock message emission ----------------------------------------

    /// Should a `DGT_MSG_BWTIME` carrying the given payload be emitted now?
    ///
    /// In UPDATE mode clock data are sent at least once a second. In
    /// UPDATE_NICE mode only on change.
    fn clock_due(&self, payload: &[u8; 7], now: Instant) -> bool {
        match self.mode {
            Mode::Update => match self.last_bwtime_at {
                None => true,
                Some(t) => now.duration_since(t) >= Duration::from_millis(1000),
            },
            Mode::UpdateNice => self.last_bwtime.as_ref() != Some(payload),
            _ => false,
        }
    }

    /// Produce the clock message for the current state, if the mode calls for
    /// one.
    pub fn clock_message(&mut self, now: Instant) -> Option<Vec<u8>> {
        if !self.mode.streams_clock() {
            return None;
        }
        let times = self.clock.times(self.clock_compat);
        let msg = messages::bwtime(&times);
        let payload: [u8; 7] = msg[3..].try_into().ok()?;
        if !self.clock_due(&payload, now) {
            return None;
        }
        self.last_bwtime = Some(payload);
        self.last_bwtime_at = Some(now);
        self.messages_sent += 1;
        Some(msg)
    }

    /// Forget the cached clock message so the next tick emits unconditionally.
    pub fn reset_clock_cache(&mut self) {
        self.last_bwtime = None;
        self.last_bwtime_at = None;
    }

    /// Build a full board dump.
    pub fn board_dump(&mut self) -> Vec<u8> {
        self.messages_sent += 1;
        messages::board_dump(&self.squares)
    }

    /// Build a field update message.
    pub fn field_update(&mut self, change: FieldChange) -> Vec<u8> {
        self.messages_sent += 1;
        messages::field_update(change.square, change.code)
    }
}

/// The standard starting position as seen from the opposite side of the board.
pub fn rotated_start() -> [u8; BOARD_LEN] {
    let mut out = [code::EMPTY; BOARD_LEN];
    for (i, c) in START_SQUARES.iter().enumerate() {
        out[63 - i] = *c;
    }
    out
}

/// The identification string a real DGT board reports.
pub const DEFAULT_TRADEMARK: &str = "DGT Projects - This DGT board is produced by DGT Projects.\n\
     DGT Projects is a registered trade mark.\n\
     220798 ISP/bus/8KP/8KE/P6/Fritz5 Vs 1.00. Serial nr. 00123 1.0";

/// Reasonable defaults so a virtual board behaves like a real one.
pub fn default_bus_address(serial: &str) -> u16 {
    serial
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .fold(0u16, |acc, b| {
            acc.saturating_mul(10).saturating_add(u16::from(b - b'0'))
        })
        & 0x3fff
}

/// Parse `major.minor` for the `config` command.
pub fn parse_version(s: &str) -> Result<(u8, u8)> {
    let mut parts = s.split(['.', ':', '-']);
    let major = parts.next().unwrap_or("").trim();
    let minor = parts.next().unwrap_or("0").trim();
    let major: u8 = major
        .parse()
        .map_err(|_| crate::error::Error::Other(format!("invalid version major {major:?}")))?;
    let minor: u8 = minor
        .parse()
        .map_err(|_| crate::error::Error::Other(format!("invalid version minor {minor:?}")))?;
    Ok((major, minor))
}

/// `white-pawn` style description of a square's contents, for logs.
pub fn describe_square(state: &BoardState, square: Square) -> String {
    let code = state.code_at(square);
    if code == code::EMPTY {
        "empty".into()
    } else {
        format!("{} ({code:#04x})", pieces::code_name(code))
    }
}

/// Piece roles ordered by DGT code, for `list`-style output.
pub const ROLE_ORDER: [Role; 6] = [
    Role::Pawn,
    Role::Rook,
    Role::Knight,
    Role::Bishop,
    Role::King,
    Role::Queen,
];

/// Colour/role pair used when synthesising a promotion.
pub const fn piece_code(color: Color, role: Role) -> u8 {
    pieces::Piece::new(color, role).code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(name: &str) -> Square {
        Square::from_uci(name).unwrap()
    }

    #[test]
    fn fresh_board_is_in_the_starting_position_and_wrote_a_powerup_tag() {
        let state = BoardState::new();
        assert_eq!(state.mode, Mode::Idle);
        assert_eq!(
            state.fen_placement(),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR"
        );
        // NOP, NOP, NOP, POWERUP, BEGINPOS
        assert_eq!(state.eeprom_len(), 5);
        assert_eq!(
            state.eeprom_iter_last(5),
            vec![ee::NOP, ee::NOP, ee::NOP, ee::POWERUP, ee::BEGINPOS]
        );
    }

    #[test]
    fn field_changes_are_recorded_in_the_eeprom() {
        let mut state = BoardState::new();
        state.eeprom_clear();
        assert!(state.set_field(sq("e2"), code::EMPTY));
        // First byte 0x40 | code, second byte field index. e2 is field 52.
        // The position is no longer the starting position but still has 16
        // pieces on files a, b, g and h, so EE_FOURROWS follows.
        assert_eq!(state.eeprom_bytes(), vec![0x40, 52, ee::FOURROWS]);
        // A no-op write must not be recorded.
        let before = state.eeprom_len();
        assert!(!state.set_field(sq("e2"), code::EMPTY));
        assert_eq!(state.eeprom_len(), before);
    }

    #[test]
    fn eeprom_dump_has_the_fixed_size_of_a_real_board() {
        let mut state = BoardState::new();
        let dump = state.eeprom_dump();
        assert_eq!(dump.len(), EEPROM_SIZE + 2, "DOWNLOADED + ring + EOF");
        assert_eq!(*dump.first().unwrap(), ee::DOWNLOADED);
        assert_eq!(*dump.last().unwrap(), ee::EOF);
        // Unwritten regions read as EE_NOP2 (0x00), then the real events.
        assert_eq!(dump[1], ee::NOP2);
        assert_eq!(
            &dump[dump.len() - 1 - state.eeprom_len()..dump.len() - 1],
            &[ee::NOP, ee::NOP, ee::NOP, ee::POWERUP, ee::BEGINPOS]
        );

        state.eeprom_clear();
        assert_eq!(state.eeprom_dump().len(), EEPROM_SIZE + 2);
    }

    #[test]
    fn eeprom_is_a_bounded_ring_buffer() {
        let mut state = BoardState::new();
        state.eeprom_clear();
        for i in 0..(EEPROM_SIZE as u64 / 2 + 16) {
            let square = Square((i % 64) as u8);
            let code = if i % 2 == 0 { code::WPAWN } else { code::EMPTY };
            state.set_field(square, code);
        }
        assert!(state.eeprom_len() <= EEPROM_SIZE);
        let dump = state.eeprom_dump();
        assert_eq!(*dump.first().unwrap(), ee::DOWNLOADED);
        assert_eq!(*dump.last().unwrap(), ee::EOF);
    }

    #[test]
    fn clock_events_use_the_three_byte_tag_layout() {
        let mut state = BoardState::new();
        state.eeprom_clear();
        state.ee_clock_event(Side::Left, Duration::from_secs(2 * 3600 + 34 * 60 + 56));
        // uu = 11, f = 1 (left), hours = 2 -> 0x72; minutes 34 -> 0x34; seconds 56 -> 0x56
        assert_eq!(state.eeprom_bytes(), vec![0x72, 0x34, 0x56]);
        state.eeprom_clear();
        state.ee_clock_event(Side::Right, Duration::from_secs(9 * 60 + 5));
        assert_eq!(state.eeprom_bytes(), vec![0x60, 0x09, 0x05]);
    }

    #[test]
    fn four_rows_and_empty_board_tags() {
        let mut state = BoardState::new();
        state.eeprom_clear();
        // Move the two centre-row pawns of each side outwards: still 16 pieces
        // on files a, b, g, h, but not a starting position.
        state.set_field(sq("c2"), code::EMPTY);
        state.set_field(sq("d2"), code::EMPTY);
        state.set_field(sq("c7"), code::EMPTY);
        state.set_field(sq("d7"), code::EMPTY);
        state.set_field(sq("e2"), code::WQUEEN);
        state.set_field(sq("e7"), code::BQUEEN);
        assert!(
            state.eeprom_iter_contains(ee::FOURROWS),
            "expected an EE_FOURROWS tag"
        );

        state.eeprom_clear();
        let empty = [code::EMPTY; BOARD_LEN];
        for (i, target) in empty.iter().enumerate() {
            state.set_field(Square(i as u8), *target);
        }
        assert!(state.eeprom_iter_contains(ee::EMPTYBOARD));
    }

    #[test]
    fn rotated_start_position_is_tagged_separately() {
        let mut state = BoardState::new();
        state.eeprom_clear();
        let rotated = rotated_start();
        let changes = state.diff_to(&rotated);
        // Only the two occupied back ranks differ; the empty middle 32 fields
        // stay put.
        assert_eq!(changes.len(), 32);
        state.apply_changes(&changes);
        assert!(state.eeprom_iter_contains(ee::BEGINPOS_ROT));
    }

    #[test]
    fn update_modes_gate_what_is_streamed() {
        assert!(!Mode::Idle.streams_fields());
        assert!(!Mode::Idle.streams_clock());
        assert!(Mode::UpdateBoard.streams_fields());
        assert!(!Mode::UpdateBoard.streams_clock());
        assert!(Mode::Update.streams_clock());
        assert!(Mode::UpdateNice.streams_clock());
    }

    #[test]
    fn clock_status_bits_follow_the_selected_compatibility_reading() {
        let mut clock = ClockState::default();
        clock.to_move = Side::Right;
        clock.running = true;

        let header = clock.status_byte(ClockCompat::Header);
        assert_eq!(header & bwtime_status::RIGHT_TO_MOVE, 0x08);
        assert_eq!(header & bwtime_status::LEFT_TO_MOVE, 0);
        assert_eq!(header & bwtime_status::NO_CLOCK, 0, "clock is connected");

        clock.connected = false;
        assert_eq!(
            clock.status_byte(ClockCompat::Header) & bwtime_status::NO_CLOCK,
            bwtime_status::NO_CLOCK
        );
        assert_eq!(
            clock.status_byte(ClockCompat::Java) & bwtime_status::NO_CLOCK,
            0,
            "the Java reading inverts D5"
        );
    }

    #[test]
    fn running_clock_counts_down_and_flags() {
        let mut clock = ClockState {
            running: true,
            left: Duration::from_secs(2),
            to_move: Side::Left,
            ..ClockState::default()
        };
        assert_eq!(clock.advance(Duration::from_secs(1)), None);
        assert_eq!(clock.left, Duration::from_secs(1));
        assert_eq!(clock.advance(Duration::from_secs(5)), Some(Side::Left));
        assert_eq!(clock.left, Duration::ZERO);
        assert!(!clock.running);
        assert_eq!(
            clock.left_flags,
            bwtime_flags::FLAG_FALLEN | bwtime_flags::FLAG_INDICATED
        );
    }

    #[test]
    fn update_mode_emits_clock_at_most_once_per_second() {
        let mut state = BoardState::new();
        state.mode = Mode::Update;
        let t0 = Instant::now();
        assert!(state.clock_message(t0).is_some(), "first tick emits");
        assert!(state.clock_message(t0).is_none(), "not again immediately");
        assert!(
            state
                .clock_message(t0 + Duration::from_millis(500))
                .is_none()
        );
        assert!(
            state
                .clock_message(t0 + Duration::from_millis(1100))
                .is_some()
        );
    }

    #[test]
    fn nice_mode_emits_clock_only_on_change() {
        let mut state = BoardState::new();
        state.mode = Mode::UpdateNice;
        let t0 = Instant::now();
        assert!(state.clock_message(t0).is_some());
        assert!(state.clock_message(t0 + Duration::from_secs(5)).is_none());
        state.clock.left -= Duration::from_secs(1);
        assert!(state.clock_message(t0 + Duration::from_secs(6)).is_some());
    }

    #[test]
    fn idle_mode_never_emits_clock() {
        let mut state = BoardState::new();
        state.mode = Mode::Idle;
        assert!(state.clock_message(Instant::now()).is_none());
    }

    #[test]
    fn default_bus_address_is_the_serial_number() {
        assert_eq!(default_bus_address("00123"), 123);
        assert_eq!(default_bus_address("01025"), 1025);
        // The header's worked example: serial 01025 -> 0x08, 0x01.
        assert_eq!(
            messages::bus_adres(default_bus_address("01025"))[3..],
            [0x08, 0x01]
        );
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("1.8").unwrap(), (1, 8));
        assert_eq!(parse_version("3").unwrap(), (3, 0));
        assert!(parse_version("x.y").is_err());
    }

    impl BoardState {
        fn eeprom_iter_last(&self, n: usize) -> Vec<u8> {
            self.eeprom
                .iter()
                .skip(self.eeprom.len().saturating_sub(n))
                .copied()
                .collect()
        }

        fn eeprom_iter_contains(&self, tag: u8) -> bool {
            self.eeprom.iter().any(|b| *b == tag)
        }
    }
}
