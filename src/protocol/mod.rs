//! DGT protocol: piece codes, square numbering, message framing, commands.
//!
//! Everything in this module is pure logic. It performs no I/O and never
//! touches a serial port or a socket, so it can be unit tested directly.
//!
//! Primary source: *DGT Electronic Board Protocol Description* by DGT Projects
//! B.V. (the long circulated `dgtbrd.h` header, revision 20120309 plus the
//! 2014-2017 addenda). Cross-checked against `arnsholt/dgtdriver` (Java) and
//! `emeijdam/dgtboard-rs` (Rust). See `docs/protocol.md`.

pub mod commands;
pub mod messages;
pub mod pieces;

pub use commands::{CommandParser, HostCommand};
pub use messages::{Frame, Framer, frame};
pub use pieces::{Color, Piece, Role, Square};
