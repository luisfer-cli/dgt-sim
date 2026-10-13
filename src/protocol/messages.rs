//! Board-to-host message framing and encoders.
//!
//! Every message from the board is:
//!
//! ```text
//! byte 0 : message id, MSB (MESSAGE_BIT) always 1
//! byte 1 : message size, bits 13..7, MSB always 0
//! byte 2 : message size, bits 6..0,  MSB always 0
//! byte 3..: payload (0..(2^14 - 3) bytes, each with MSB 0)
//! ```
//!
//! The size **includes** the 3 header bytes. Source: *DGT Electronic Board
//! Protocol Description*, section "DESCRIPTION OF THE MESSAGES FROM BOARD TO
//! PC".

use crate::error::{Error, Result};

use super::pieces::{BOARD_LEN, Square};

/// Set on byte 0 of every board-to-host message.
pub const MESSAGE_BIT: u8 = 0x80;

/// Message identifiers, without [`MESSAGE_BIT`].
pub mod id {
    pub const NONE: u8 = 0x00;
    pub const BOARD_DUMP: u8 = 0x06;
    pub const BWTIME: u8 = 0x0d;
    pub const FIELD_UPDATE: u8 = 0x0e;
    pub const EE_MOVES: u8 = 0x0f;
    pub const BUSADRES: u8 = 0x10;
    pub const SERIALNR: u8 = 0x11;
    pub const TRADEMARK: u8 = 0x12;
    pub const VERSION: u8 = 0x13;
    pub const BOARD_DUMP_50B: u8 = 0x14;
    pub const BOARD_DUMP_50W: u8 = 0x15;
    pub const BATTERY_STATUS: u8 = 0x20;
    pub const LONG_SERIALNR: u8 = 0x22;
}

/// Fixed total message sizes (header included) defined by the protocol.
pub mod size {
    pub const BOARD_DUMP: usize = 67;
    pub const BOARD_DUMP_50: usize = 53;
    pub const BWTIME: usize = 10;
    pub const FIELD_UPDATE: usize = 5;
    pub const BUSADRES: usize = 5;
    pub const SERIALNR: usize = 8;
    pub const VERSION: usize = 5;
    pub const LONG_SERIALNR: usize = 13;
    /// Documented as 7, while the byte-by-byte description lists 9 payload
    /// bytes. Ambiguous upstream; see docs/protocol.md.
    pub const BATTERY_STATUS: usize = 7;
}

/// Largest total message we are willing to emit (0x1f00 payload + 3 header).
pub const MAX_MESSAGE_SIZE: usize = 0x2000;

/// One-byte EEPROM event codes (see docs/protocol.md).
pub mod ee {
    pub const POWERUP: u8 = 0x6a;
    pub const EOF: u8 = 0x6b;
    pub const FOURROWS: u8 = 0x6c;
    pub const EMPTYBOARD: u8 = 0x6d;
    pub const DOWNLOADED: u8 = 0x6e;
    pub const BEGINPOS: u8 = 0x6f;
    pub const BEGINPOS_ROT: u8 = 0x7a;
    pub const START_TAG: u8 = 0x7b;
    pub const WATCHDOG_ACTION: u8 = 0x7c;
    pub const NOP: u8 = 0x7f;
    pub const NOP2: u8 = 0x00;
}

/// Bit D0 of byte 9 of `DGT_MSG_BWTIME`.
pub mod bwtime_status {
    pub const RUNNING: u8 = 0x01;
    pub const RIGHT_HIGH: u8 = 0x02;
    pub const BATTERY_LOW: u8 = 0x04;
    /// D3: right player's turn.
    pub const RIGHT_TO_MOVE: u8 = 0x08;
    /// D4: left player's turn.
    pub const LEFT_TO_MOVE: u8 = 0x10;
    /// D5: set means *no* clock connected.
    pub const NO_CLOCK: u8 = 0x20;
}

/// Bits D4..D6 of the per-player hour byte of `DGT_MSG_BWTIME`.
pub mod bwtime_flags {
    pub const FLAG_FALLEN: u8 = 0x01;
    pub const TIME_PER_MOVE: u8 = 0x02;
    pub const FLAG_INDICATED: u8 = 0x04;
}

/// Build a complete framed message: 3-byte header plus payload.
pub fn frame(message_id: u8, payload: &[u8]) -> Vec<u8> {
    let total = payload.len() + 3;
    debug_assert!(total <= MAX_MESSAGE_SIZE, "message too large");
    let mut out = Vec::with_capacity(total);
    out.push(MESSAGE_BIT | (message_id & 0x7f));
    out.push(((total >> 7) & 0x7f) as u8);
    out.push((total & 0x7f) as u8);
    out.extend_from_slice(payload);
    out
}

/// `DGT_MSG_BOARD_DUMP`: the full state of all 64 fields, `a8` first.
pub fn board_dump(squares: &[u8; BOARD_LEN]) -> Vec<u8> {
    frame(id::BOARD_DUMP, squares)
}

/// `DGT_MSG_FIELD_UPDATE`: one field changed. `code` is a raw DGT piece code,
/// `0x00` meaning the field became empty.
pub fn field_update(square: Square, code: u8) -> Vec<u8> {
    frame(id::FIELD_UPDATE, &[square.0, code & 0x0f])
}

/// `DGT_MSG_VERSION`: major/minor firmware version.
pub fn version(major: u8, minor: u8) -> Vec<u8> {
    frame(id::VERSION, &[major & 0x7f, minor & 0x7f])
}

/// `DGT_MSG_SERIALNR`: five ASCII decimal digits.
pub fn serial_nr(number: &str) -> Result<Vec<u8>> {
    ascii_field(id::SERIALNR, number, 5, size::SERIALNR)
}

/// `DGT_LONG_SERIALNR`: ten ASCII decimal digits.
pub fn long_serial_nr(number: &str) -> Result<Vec<u8>> {
    ascii_field(id::LONG_SERIALNR, number, 10, size::LONG_SERIALNR)
}

fn ascii_field(message_id: u8, value: &str, width: usize, total: usize) -> Result<Vec<u8>> {
    let digits: Vec<u8> = value
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .take(width)
        .collect();
    if digits.is_empty() {
        return Err(Error::Protocol(format!(
            "serial number {value:?} contains no digits"
        )));
    }
    // Right-aligned with leading zeroes, as a real board reports fixed width.
    let mut payload = vec![b'0'; width];
    let start = width.saturating_sub(digits.len());
    payload[start..start + digits.len()].copy_from_slice(&digits);
    debug_assert_eq!(payload.len() + 3, total);
    Ok(frame(message_id, &payload))
}

/// `DGT_MSG_TRADEMARK`: an ASCII identification string, 0..=256 bytes of
/// payload. Bytes above `0x7f` are rejected because the protocol requires the
/// MSB of every payload byte to be zero.
pub fn trademark(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() > 256 {
        return Err(Error::Protocol(format!(
            "trademark message is {} bytes, maximum is 256",
            bytes.len()
        )));
    }
    if bytes.iter().any(|b| *b & 0x80 != 0) {
        return Err(Error::Protocol(
            "trademark message must be 7-bit ASCII".into(),
        ));
    }
    Ok(frame(id::TRADEMARK, bytes))
}

/// `DGT_MSG_BUSADRES`: a 14-bit board address in two 7-bit halves.
pub fn bus_adres(address: u16) -> Vec<u8> {
    let a = address & 0x3fff;
    frame(id::BUSADRES, &[((a >> 7) & 0x7f) as u8, (a & 0x7f) as u8])
}

/// Clock times, in the layout used by `DGT_MSG_BWTIME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockTimes {
    /// Time remaining on the left screen.
    pub left: std::time::Duration,
    /// D0..D2 flags for the left screen (see [`bwtime_flags`]).
    pub left_flags: u8,
    /// Time remaining on the right screen.
    pub right: std::time::Duration,
    /// D0..D2 flags for the right screen.
    pub right_flags: u8,
    /// Status byte, see [`bwtime_status`].
    pub status: u8,
}

fn encode_bcd(value: u64) -> u8 {
    let tens = ((value / 10) % 10) as u8;
    let units = (value % 10) as u8;
    (tens << 4) | units
}

fn hours_of(d: std::time::Duration) -> u8 {
    ((d.as_secs() / 3600).min(9)) as u8
}

/// `DGT_MSG_BWTIME` carrying clock times.
///
/// Layout (payload byte -> meaning):
/// `0` right hours (D0-D3) + right flags (D4-D6), `1` right minutes BCD,
/// `2` right seconds BCD, `3` left hours + flags, `4` left minutes BCD,
/// `5` left seconds BCD, `6` clock status.
pub fn bwtime(times: &ClockTimes) -> Vec<u8> {
    let bcd = |d: std::time::Duration| {
        let secs = d.as_secs();
        let m = (secs / 60) % 60;
        let s = secs % 60;
        (encode_bcd(m), encode_bcd(s))
    };
    let (rm, rs) = bcd(times.right);
    let (lm, ls) = bcd(times.left);
    let payload = [
        ((times.right_flags & 0x07) << 4) | hours_of(times.right),
        rm,
        rs,
        ((times.left_flags & 0x07) << 4) | hours_of(times.left),
        lm,
        ls,
        times.status,
    ];
    frame(id::BWTIME, &payload)
}

/// Is this `DGT_MSG_BWTIME` payload a clock ACK rather than clock times?
pub fn is_clock_ack(payload: &[u8]) -> bool {
    payload.len() >= 4 && ((payload[0] & 0x0f) == 0x0a || (payload[3] & 0x0f) == 0x0a)
}

/// The 4 ack bytes recovered from a `DGT_MSG_BWTIME` ACK payload.
pub fn decode_clock_ack(payload: &[u8]) -> Result<[u8; 4]> {
    if payload.len() != 7 {
        return Err(Error::Protocol(format!(
            "clock ack payload is {} bytes, expected 7",
            payload.len()
        )));
    }
    let d = payload;
    Ok([
        (d[1] & 0x7f) | ((d[3] << 3) & 0x80),
        (d[2] & 0x7f) | ((d[3] << 2) & 0x80),
        (d[4] & 0x7f) | ((d[0] << 3) & 0x80),
        (d[5] & 0x7f) | ((d[0] << 2) & 0x80),
    ])
}

/// Build a `DGT_MSG_BWTIME` clock ACK.
///
/// `command` is the clock command being acknowledged (`0x01` display, `0x08`
/// button, `0x09` version, `0x0a` set-n-run, `0x0b` beep). `ack2` carries
/// command-specific data (the clock version for a version ack); `ack3` carries
/// the button code for a button ack. `auto_generated` sets the D7 bit of the
/// ack1 byte.
pub fn clock_ack(command: u8, ack2: u8, ack3: u8, auto_generated: bool) -> Vec<u8> {
    build_clock_ack(0x10, command, ack2, ack3, auto_generated)
}

/// Build a `DGT_MSG_BWTIME` clock ACK with an explicit `ack0` byte (use
/// `0x40` to report an error).
pub fn build_clock_ack(ack0: u8, ack1: u8, ack2: u8, ack3: u8, auto_generated: bool) -> Vec<u8> {
    let ack1 = if auto_generated { ack1 | 0x80 } else { ack1 };
    // Invert the documented mapping:
    //   ack0 = (b1 & 0x7f) | ((b3 << 3) & 0x80)
    //   ack1 = (b2 & 0x7f) | ((b3 << 2) & 0x80)
    //   ack2 = (b4 & 0x7f) | ((b0 << 3) & 0x80)
    //   ack3 = (b5 & 0x7f) | ((b0 << 2) & 0x80)
    //
    // b0 carries the two spare bits of ack2/ack3 and must keep bits 4 and 5
    // clear so it does not perturb them; its low nibble of 0x0a is what marks
    // the message as an ACK rather than clock times.
    let b0 = 0x0a;
    let b1 = ack0 & 0x7f;
    let b2 = ack1 & 0x7f;
    // b3 carries the top bits of ack0 and ack1, one shift apart.
    let b3 = ((ack0 & 0x80) >> 3) | ((ack1 & 0x80) >> 2);
    let b4 = ack2 & 0x7f;
    let b5 = ack3 & 0x7f;
    frame(id::BWTIME, &[b0, b1, b2, b3, b4, b5, 0])
}

/// `DGT_MSG_BATTERY_STATUS` (Bluetooth boards only).
///
/// Emitted as a 4-byte payload even though the upstream description is
/// internally inconsistent about the size. See docs/protocol.md.
pub fn battery_status(
    capacity_percent: u8,
    hours_left: u8,
    minutes_left: u8,
    status: u8,
) -> Vec<u8> {
    frame(
        id::BATTERY_STATUS,
        &[capacity_percent, hours_left, minutes_left, status],
    )
}

/// `DGT_MSG_EE_MOVES`: the raw contents of the on-board EEPROM ring buffer.
pub fn ee_moves(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() + 3 > MAX_MESSAGE_SIZE {
        return Err(Error::Protocol(format!(
            "EEPROM dump of {} bytes exceeds the message size limit",
            payload.len()
        )));
    }
    Ok(frame(id::EE_MOVES, payload))
}

/// A parsed raw frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Message id with [`MESSAGE_BIT`] stripped.
    pub id: u8,
    /// Payload bytes (everything after the 3-byte header).
    pub payload: Vec<u8>,
}

/// Incremental frame reader, for decoding the board's byte stream. Used by the
/// test-suite and by any tool that consumes the simulator's output.
#[derive(Debug, Default, Clone)]
pub struct Framer {
    buf: Vec<u8>,
    skipped: u64,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes discarded while resynchronising (a healthy stream reports 0).
    pub fn skipped_bytes(&self) -> u64 {
        self.skipped
    }

    /// Buffered bytes that do not yet form a complete message.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Yield the next complete message, resynchronising on malformed headers.
    ///
    /// On a malformed header exactly one byte is discarded, then parsing is
    /// retried. Dropping *one* byte rather than scanning ahead for the next
    /// byte with the message bit set matters: every payload byte has its MSB
    /// clear by definition, so a "find the next header" heuristic happily
    /// skips over the real header that follows one garbage byte.
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if self.buf.len() < 3 {
                return None;
            }
            let id = self.buf[0];
            let size_msb = self.buf[1];
            let size_lsb = self.buf[2];
            let well_formed =
                id & MESSAGE_BIT != 0 && size_msb & MESSAGE_BIT == 0 && size_lsb & MESSAGE_BIT == 0;
            let len = (((size_msb & 0x7f) as usize) << 7) | (size_lsb & 0x7f) as usize;
            if !well_formed || !(3..=MAX_MESSAGE_SIZE).contains(&len) {
                self.skipped += 1;
                self.buf.remove(0);
                continue;
            }
            if self.buf.len() < len {
                return None;
            }
            let payload = self.buf[3..len].to_vec();
            self.buf.drain(..len);
            return Some(Frame {
                id: id & !MESSAGE_BIT,
                payload,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_dump_frame_matches_real_capture() {
        // "8600 43" + the 64 starting-position codes, as captured in
        // arnholt/dgtdriver test/resources/1.inbytes.
        let msg = board_dump(&super::super::pieces::START_SQUARES);
        assert_eq!(msg.len(), 67);
        assert_eq!(&msg[..3], &[0x86, 0x00, 0x43]);
        assert_eq!(msg[3], 0x08);
        assert_eq!(msg[66], 0x02);
    }

    #[test]
    fn field_update_frame_matches_real_capture() {
        // "8e00 05 34 00": field 0x34 (= e2) became empty.
        let msg = field_update(Square::from_uci("e2").unwrap(), 0x00);
        assert_eq!(msg, vec![0x8e, 0x00, 0x05, 0x34, 0x00]);
    }

    #[test]
    fn version_and_serial_frames_match_real_capture() {
        // "9300 05 01 08" -> version 1.8
        assert_eq!(version(1, 8), vec![0x93, 0x00, 0x05, 0x01, 0x08]);
        // "9100 08 30 30 31 32 33" -> serial 00123
        assert_eq!(
            serial_nr("00123").unwrap(),
            vec![0x91, 0x00, 0x08, b'0', b'0', b'1', b'2', b'3']
        );
        assert_eq!(long_serial_nr("123").unwrap().len(), 13);
        assert!(serial_nr("no-digits").is_err());
    }

    #[test]
    fn bwtime_frame_matches_real_capture() {
        // "8d00 0a 20 03 02 20 03 02 08": both screens at 3 minutes 2 seconds
        // with the time-per-move indicator on, running, right to move. Taken
        // verbatim from arnholt/dgtdriver test/resources/1.inbytes.
        let times = ClockTimes {
            left: std::time::Duration::from_secs(3 * 60 + 2),
            left_flags: bwtime_flags::TIME_PER_MOVE,
            right: std::time::Duration::from_secs(3 * 60 + 2),
            right_flags: bwtime_flags::TIME_PER_MOVE,
            // The captured byte is 0x08: right to move, clock not running.
            status: bwtime_status::RIGHT_TO_MOVE,
        };
        let msg = bwtime(&times);
        assert_eq!(
            msg,
            vec![0x8d, 0x00, 0x0a, 0x20, 0x03, 0x02, 0x20, 0x03, 0x02, 0x08]
        );
        assert!(!is_clock_ack(&msg[3..]));
    }

    #[test]
    fn clock_ack_roundtrips_through_the_documented_mapping() {
        for (cmd, ack2, ack3) in [
            (0x01u8, 0x00u8, 0x00u8),
            (0x09, 0x18, 0x00),
            (0x08, 0x00, 0x35),
        ] {
            let msg = clock_ack(cmd, ack2, ack3, false);
            assert_eq!(msg.len(), 10);
            let payload = &msg[3..];
            assert!(is_clock_ack(payload), "must be detected as an ack");
            let ack = decode_clock_ack(payload).unwrap();
            assert_eq!(ack[0], 0x10, "normal ack");
            assert_eq!(ack[1], cmd);
            assert_eq!(ack[2], ack2);
            assert_eq!(ack[3], ack3);
        }
    }

    #[test]
    fn auto_generated_ack_sets_the_high_bit() {
        let msg = clock_ack(0x81, 0, 0, true);
        let ack = decode_clock_ack(&msg[3..]).unwrap();
        assert_eq!(ack[1], 0x81);
    }

    #[test]
    fn framer_handles_fragmentation_and_resync() {
        let a = board_dump(&super::super::pieces::START_SQUARES);
        let b = version(1, 8);
        let c = field_update(Square(52), 0x00);
        let stream: Vec<u8> = [a.clone(), b.clone(), c.clone()].concat();

        for chunk in 1..=7 {
            let mut framer = Framer::new();
            for piece in stream.chunks(chunk) {
                framer.push(piece);
            }
            let frames: Vec<Frame> = std::iter::from_fn(|| framer.next_frame()).collect();
            assert_eq!(frames.len(), 3, "chunk size {chunk}");
            assert_eq!(frames[0].id, id::BOARD_DUMP);
            assert_eq!(frames[1].id, id::VERSION);
            assert_eq!(frames[2].id, id::FIELD_UPDATE);
        }
    }

    #[test]
    fn framer_resyncs_after_garbage() {
        let mut framer = Framer::new();
        framer.push(&[0x01, 0x02, 0x83]); // garbage whose bytes look like bad headers
        let good = version(2, 1);
        framer.push(&good);
        let frames: Vec<Frame> = std::iter::from_fn(|| framer.next_frame()).collect();
        assert_eq!(frames.len(), 1, "the real frame must survive the garbage");
        assert_eq!(frames[0].payload, vec![2, 1]);
        assert_eq!(framer.skipped_bytes(), 3);
    }

    #[test]
    fn framer_rejects_impossible_lengths() {
        let mut framer = Framer::new();
        framer.push(&[0x80 | id::BOARD_DUMP, 0x00, 0x02]); // length < 3
        framer.push(&version(1, 0));
        let frames: Vec<Frame> = std::iter::from_fn(|| framer.next_frame()).collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, id::VERSION);
    }

    #[test]
    fn trademark_and_bus_adres_rules() {
        assert_eq!(trademark("DGT Projects").unwrap()[3..], *b"DGT Projects");
        assert!(trademark(&"x".repeat(257)).is_err());
        assert!(trademark("caf\u{e9}").is_err());
        assert_eq!(bus_adres(0x0810), vec![0x90, 0x00, 0x05, 0x10, 0x10]);
    }

    #[test]
    fn ee_moves_size_limit() {
        assert!(ee_moves(&vec![0u8; 0x1f00]).is_ok());
        assert!(ee_moves(&vec![0u8; MAX_MESSAGE_SIZE]).is_err());
    }
}
