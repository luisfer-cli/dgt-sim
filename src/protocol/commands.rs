//! Host-to-board commands and the incremental command parser.
//!
//! Most commands are a single byte. Two are length-prefixed frames:
//! `DGT_CLOCK_MESSAGE` (`0x2b`) and `DGT_SET_LEDS` (`0x60`). Source: *DGT
//! Electronic Board Protocol Description*, section "COMMAND CODES FROM PC TO
//! BOARD".

use crate::error::{Error, Result};

/// Commands the host sends to the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCommand {
    /// `0x40` — back to IDLE mode, cancelling any update mode. No reply.
    Reset,
    /// `0x41` — reply with `DGT_MSG_BWTIME`.
    SendClock,
    /// `0x42` — reply with `DGT_MSG_BOARD_DUMP`.
    SendBoard,
    /// `0x43` — UPDATE mode: field updates plus clock messages.
    SendUpdate,
    /// `0x44` — UPDATE_BOARD mode: field updates only.
    SendUpdateBoard,
    /// `0x45` — reply with `DGT_MSG_SERIALNR`.
    ReturnSerialNr,
    /// `0x46` — reply with `DGT_MSG_BUSADRES`.
    ReturnBusAdres,
    /// `0x47` — reply with `DGT_MSG_TRADEMARK`.
    SendTrademark,
    /// `0x49` — reply with `DGT_MSG_EE_MOVES`.
    SendEeMoves,
    /// `0x4a` — switch the (real) RS232 driver to bus mode.
    ToBusmode,
    /// `0x4b` — UPDATE_NICE mode: clock messages only on change.
    SendUpdateNice,
    /// `0x4c` — reply with `DGT_MSG_BATTERY_STATUS` (Bluetooth boards).
    SendBatteryStatus,
    /// `0x4d` — reply with `DGT_MSG_VERSION`.
    SendVersion,
    /// `0x4e` — jump to the bootloader (FLIP).
    StartBootloader,
    /// `0x50` — draughts: board dump of the black squares only.
    SendBoard50B,
    /// `0x51` — draughts: scan black squares only (stored in EEPROM).
    Scan50B,
    /// `0x52` — draughts: board dump of the white squares only.
    SendBoard50W,
    /// `0x53` — draughts: scan white squares only.
    Scan50W,
    /// `0x54` — draughts: scan all squares.
    Scan100,
    /// `0x55` — reply with `DGT_MSG_LONG_SERIALNR`.
    ReturnLongSerialNr,
    /// `0x60` — Revelation II LED pattern. `[0x60, size=4, pattern, first, last, 0x00]`.
    SetLeds { pattern: u8, first: u8, last: u8 },
    /// `0x2b` — a command for the attached clock.
    Clock { command: u8, data: Vec<u8> },
    /// A single byte we do not recognise. A real board ignores these.
    Unknown(u8),
}

impl HostCommand {
    /// Parse a single-byte command. Length-prefixed commands are handled by
    /// [`CommandParser`] and never reach here.
    fn from_byte(byte: u8) -> HostCommand {
        match byte {
            0x40 => Self::Reset,
            0x41 => Self::SendClock,
            0x42 => Self::SendBoard,
            0x43 => Self::SendUpdate,
            0x44 => Self::SendUpdateBoard,
            0x45 => Self::ReturnSerialNr,
            0x46 => Self::ReturnBusAdres,
            0x47 => Self::SendTrademark,
            0x49 => Self::SendEeMoves,
            0x4a => Self::ToBusmode,
            0x4b => Self::SendUpdateNice,
            0x4c => Self::SendBatteryStatus,
            0x4d => Self::SendVersion,
            0x4e => Self::StartBootloader,
            0x50 => Self::SendBoard50B,
            0x51 => Self::Scan50B,
            0x52 => Self::SendBoard50W,
            0x53 => Self::Scan50W,
            0x54 => Self::Scan100,
            0x55 => Self::ReturnLongSerialNr,
            other => Self::Unknown(other),
        }
    }

    /// Short stable name, used in logs, tracing and tests.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Reset => "send-reset",
            Self::SendClock => "send-clock",
            Self::SendBoard => "send-board",
            Self::SendUpdate => "send-update",
            Self::SendUpdateBoard => "send-update-board",
            Self::ReturnSerialNr => "return-serialnr",
            Self::ReturnBusAdres => "return-busadres",
            Self::SendTrademark => "send-trademark",
            Self::SendEeMoves => "send-ee-moves",
            Self::ToBusmode => "to-busmode",
            Self::SendUpdateNice => "send-update-nice",
            Self::SendBatteryStatus => "send-battery-status",
            Self::SendVersion => "send-version",
            Self::StartBootloader => "start-bootloader",
            Self::SendBoard50B => "send-board-50b",
            Self::Scan50B => "scan-50b",
            Self::SendBoard50W => "send-board-50w",
            Self::Scan50W => "scan-50w",
            Self::Scan100 => "scan-100",
            Self::ReturnLongSerialNr => "return-long-serialnr",
            Self::SetLeds { .. } => "set-leds",
            Self::Clock { .. } => "clock-message",
            Self::Unknown(_) => "unknown",
        }
    }
}

/// `DGT_CLOCK_MESSAGE` command ids that we know about.
pub mod clock_command {
    pub const DISPLAY: u8 = 0x01;
    pub const ICONS: u8 = 0x02;
    pub const END: u8 = 0x03;
    pub const BUTTON: u8 = 0x08;
    pub const VERSION: u8 = 0x09;
    pub const SETNRUN: u8 = 0x0a;
    pub const BEEP: u8 = 0x0b;
    pub const ASCII: u8 = 0x0c;
}

/// `DGT_CLOCK_MESSAGE` wrapper constants.
pub const CLOCK_MESSAGE: u8 = 0x2b;
/// `DGT_CMD_CLOCK_START_MESSAGE`.
pub const CLOCK_START: u8 = 0x03;
/// `DGT_CMD_CLOCK_END_MESSAGE`.
pub const CLOCK_END: u8 = 0x00;
/// `DGT_SET_LEDS`.
pub const SET_LEDS: u8 = 0x60;

/// Largest host command we accept, derived from the 7-bit size field.
pub const MAX_COMMAND_SIZE: usize = 2 + 0x7f;

/// Incremental parser for the host-to-board byte stream.
///
/// Bytes may arrive split at any boundary, so a partial length-prefixed
/// command is buffered until it is complete.
#[derive(Debug, Default, Clone)]
pub struct CommandParser {
    buf: Vec<u8>,
}

impl CommandParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Number of buffered bytes. A non-zero value that never shrinks means the
    /// host sent an incomplete frame.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Parse the next complete command, or `None` when more bytes are needed.
    ///
    /// Returns `Err` for a malformed frame. The offending bytes are consumed
    /// so the caller can keep going: the board never stalls on a bad command.
    pub fn next_command(&mut self) -> Option<Result<HostCommand>> {
        let first = *self.buf.first()?;
        match first {
            CLOCK_MESSAGE => self.take_wrapped(CLOCK_MESSAGE, "clock message"),
            SET_LEDS => self.take_wrapped(SET_LEDS, "set-leds"),
            byte => {
                self.buf.remove(0);
                Some(Ok(HostCommand::from_byte(byte)))
            }
        }
    }

    fn take_wrapped(&mut self, tag: u8, what: &str) -> Option<Result<HostCommand>> {
        if self.buf.len() < 2 {
            return None;
        }
        let size = self.buf[1];
        if size & 0x80 != 0 {
            self.buf.drain(..2);
            return Some(Err(Error::Protocol(format!(
                "{what}: size byte {size:#04x} has the MSB set"
            ))));
        }
        let total = usize::from(size) + 2;
        if !(3..=MAX_COMMAND_SIZE).contains(&total) {
            self.buf.drain(..2);
            return Some(Err(Error::Protocol(format!(
                "{what}: declared size {size} is out of range"
            ))));
        }
        if self.buf.len() < total {
            return None;
        }
        let frame: Vec<u8> = self.buf.drain(..total).collect();
        let result = match tag {
            CLOCK_MESSAGE => Self::parse_clock_frame(&frame),
            _ => Self::parse_leds_frame(&frame),
        };
        Some(result)
    }

    /// `[0x2b, size, 0x03, command, data..., 0x00]`
    fn parse_clock_frame(frame: &[u8]) -> Result<HostCommand> {
        let start = frame[2];
        if start != CLOCK_START {
            return Err(Error::Protocol(format!(
                "clock message: expected start byte {CLOCK_START:#04x}, got {start:#04x}"
            )));
        }
        let end = frame[frame.len() - 1];
        if end != CLOCK_END {
            return Err(Error::Protocol(format!(
                "clock message: expected end byte {CLOCK_END:#04x}, got {end:#04x}"
            )));
        }
        if frame.len() < 5 {
            return Err(Error::Protocol("clock message: no clock command id".into()));
        }
        Ok(HostCommand::Clock {
            command: frame[3],
            data: frame[4..frame.len() - 1].to_vec(),
        })
    }

    /// `[0x60, size=4, pattern, first, last, 0x00]`
    fn parse_leds_frame(frame: &[u8]) -> Result<HostCommand> {
        if frame.len() != 6 {
            return Err(Error::Protocol(format!(
                "set-leds: expected 6 bytes, got {}",
                frame.len()
            )));
        }
        if frame[5] != CLOCK_END {
            return Err(Error::Protocol(
                "set-leds: an explicit end byte would be clearer".into(),
            ));
        }
        Ok(HostCommand::SetLeds {
            pattern: frame[2],
            first: frame[3],
            last: frame[4],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(bytes: &[u8]) -> Vec<HostCommand> {
        let mut parser = CommandParser::new();
        parser.push(bytes);
        std::iter::from_fn(|| match parser.next_command() {
            Some(Ok(cmd)) => Some(cmd),
            _ => None,
        })
        .collect()
    }

    #[test]
    fn single_byte_commands() {
        let cmds = parse_all(&[0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x4b, 0x4d, 0x55]);
        assert_eq!(
            cmds,
            vec![
                HostCommand::Reset,
                HostCommand::SendClock,
                HostCommand::SendBoard,
                HostCommand::SendUpdate,
                HostCommand::SendUpdateBoard,
                HostCommand::ReturnSerialNr,
                HostCommand::SendUpdateNice,
                HostCommand::SendVersion,
                HostCommand::ReturnLongSerialNr,
            ]
        );
    }

    #[test]
    fn unknown_commands_are_reported_not_dropped_silently() {
        let cmds = parse_all(&[0x01, 0xff]);
        assert_eq!(
            cmds,
            vec![HostCommand::Unknown(0x01), HostCommand::Unknown(0xff)]
        );
    }

    #[test]
    fn clock_version_command() {
        // DGT_CMD_CLOCK_VERSION: [0x2b, 0x03, 0x03, 0x09, 0x00]
        let cmds = parse_all(&[0x2b, 0x03, 0x03, 0x09, 0x00]);
        assert_eq!(
            cmds,
            vec![HostCommand::Clock {
                command: clock_command::VERSION,
                data: vec![]
            }]
        );
    }

    #[test]
    fn clock_setnrun_command() {
        // DGT_CMD_CLOCK_SETNRUN: size 0x0a, seven data bytes.
        let frame = [0x2b, 0x0a, 0x03, 0x0a, 0, 5, 0, 0, 5, 0, 0x01, 0x00];
        assert_eq!(frame.len(), 12);
        let cmds = parse_all(&frame);
        assert_eq!(
            cmds,
            vec![HostCommand::Clock {
                command: clock_command::SETNRUN,
                data: vec![0, 5, 0, 0, 5, 0, 0x01]
            }]
        );
    }

    #[test]
    fn clock_frames_split_at_every_boundary() {
        let frame = [0x2b, 0x0b, 0x03, 0x01, 1, 2, 3, 4, 5, 6, 7, 0x03, 0x00];
        assert_eq!(frame.len(), 13);
        for split in 0..frame.len() {
            let mut parser = CommandParser::new();
            parser.push(&frame[..split]);
            assert!(parser.next_command().is_none(), "split at {split}");
            parser.push(&frame[split..]);
            let cmd = parser.next_command().expect("complete after second push");
            match cmd.unwrap() {
                HostCommand::Clock { command, data } => {
                    assert_eq!(command, clock_command::DISPLAY);
                    assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 0x03]);
                }
                other => panic!("unexpected command {other:?}"),
            }
        }
    }

    #[test]
    fn set_leds_command() {
        let cmds = parse_all(&[0x60, 0x04, 0x01, 0x00, 0x3f, 0x00]);
        assert_eq!(
            cmds,
            vec![HostCommand::SetLeds {
                pattern: 1,
                first: 0,
                last: 0x3f
            }]
        );
    }

    #[test]
    fn malformed_frames_do_not_stall_the_parser() {
        // Bad start byte, then a valid single-byte command.
        let mut parser = CommandParser::new();
        parser.push(&[0x2b, 0x03, 0x99, 0x09, 0x00, 0x40]);
        let first = parser.next_command().expect("frame is complete");
        assert!(first.is_err());
        assert_eq!(parser.next_command().unwrap().unwrap(), HostCommand::Reset);
        assert_eq!(parser.pending(), 0);

        // Declared size with the MSB set: the two header bytes are dropped and
        // the parser stays usable.
        let mut parser = CommandParser::new();
        parser.push(&[0x2b, 0x83]);
        assert!(parser.next_command().unwrap().is_err());
        parser.clear();
        assert_eq!(parser.pending(), 0);
        parser.push(&[0x42]);
        assert_eq!(
            parser.next_command().unwrap().unwrap(),
            HostCommand::SendBoard
        );
    }

    #[test]
    fn oversized_clock_frame_is_rejected_without_buffering_forever() {
        let mut parser = CommandParser::new();
        parser.push(&[0x2b, 0x7f, 0x03, 0x0c]);
        assert!(parser.next_command().is_none());
        assert!(parser.pending() < MAX_COMMAND_SIZE);
    }
}
