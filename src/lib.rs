//! # dgt-sim
//!
//! An emulator of DGT electronic chess boards. It speaks the real binary DGT
//! serial protocol to its clients over a pseudoterminal or a TCP socket, so
//! software written for physical DGT hardware can be developed and tested
//! without the hardware.
//!
//! The crate is split into layers that do not leak into each other:
//!
//! | Module | Responsibility |
//! | --- | --- |
//! | [`protocol`] | Pure byte-level DGT protocol: framing, commands, messages, piece codes. No I/O. |
//! | [`board`] | The physical sensor model: 64 fields, transfer modes, EEPROM, clock. |
//! | [`transport`] | PTY and TCP endpoints, plus deterministic fault injection. |
//! | [`manager`] | Owns every board and routes control requests. |
//! | [`control`] | Unix-socket JSON control plane between the CLI and the daemon. |
//! | [`chess`] | FEN/UCI/PGN. The only place chess *rules* are allowed to exist. |
//!
//! The emulator deliberately does not validate the laws of chess: a physical
//! board can hold any arrangement of pieces, and so can a virtual one.
//!
//! ```no_run
//! use dgt_sim::protocol::{CommandParser, HostCommand, Framer};
//!
//! // Host side: what a client sends.
//! let mut parser = CommandParser::new();
//! parser.push(&[0x42]);
//! assert_eq!(parser.next_command().unwrap().unwrap(), HostCommand::SendBoard);
//!
//! // Board side: what the client reads back.
//! let mut framer = Framer::new();
//! framer.push(&[0x86, 0x00, 0x43]);
//! assert!(framer.next_frame().is_none(), "payload not here yet");
//! ```

pub mod board;
pub mod chess;
pub mod cli;
pub mod control;
pub mod error;
pub mod manager;
pub mod protocol;
pub mod transport;

pub use error::{Error, Result};
