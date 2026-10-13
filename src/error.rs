//! Error type shared by the whole crate.

use thiserror::Error;

/// Every fallible operation in `dgt-sim` returns this.
#[derive(Debug, Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("control error: {0}")]
    Control(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("invalid square {0:?} (expected a1..h8)")]
    InvalidSquare(String),

    #[error("invalid piece {0:?} (try: white-pawn, bp, magic-draw)")]
    InvalidPiece(String),

    #[error("invalid FEN: {0}")]
    InvalidFen(String),

    #[error("invalid PGN: {0}")]
    InvalidPgn(String),

    #[error("invalid UCI move {0:?} (expected e2e4 or e7e8q)")]
    InvalidUci(String),

    #[error("invalid hex byte: {0}")]
    InvalidHex(String),

    #[error("board {0} does not exist")]
    BoardNotFound(u32),

    #[error("board {0} is already stopped")]
    BoardStopped(u32),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("the simulator is not running on socket {0}")]
    NotRunning(String),

    #[error("the simulator is already running on socket {0}")]
    AlreadyRunning(String),

    #[error("limit exceeded: {0}")]
    Limit(String),

    #[error("{0}")]
    Other(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Process exit code for this error. `0` is reserved for success and `2`
    /// for CLI usage errors (produced by clap itself), so every runtime error
    /// maps to `1`.
    pub fn exit_code(&self) -> i32 {
        1
    }
}
