//! Piece codes, square numbering and board-field FEN helpers.
//!
//! Square numbering is the DGT one: field `0` is `a8`, field `63` is `h1`,
//! row by row as seen by White with the connector on White's left. This is
//! exactly FEN row order, which makes board dumps trivially convertible.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Number of fields on a chess board.
pub const BOARD_LEN: usize = 64;

/// Colours as the *board* sees them. The board knows nothing about whose turn
/// it is or about any game state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    White,
    Black,
}

impl Color {
    pub const fn name(self) -> &'static str {
        match self {
            Self::White => "white",
            Self::Black => "black",
        }
    }

    pub const fn opponent(self) -> Self {
        match self {
            Self::White => Self::Black,
            Self::Black => Self::White,
        }
    }
}

/// Piece role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Pawn,
    Rook,
    Knight,
    Bishop,
    King,
    Queen,
}

impl Role {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pawn => "pawn",
            Self::Rook => "rook",
            Self::Knight => "knight",
            Self::Bishop => "bishop",
            Self::King => "king",
            Self::Queen => "queen",
        }
    }

    /// Lowercase FEN letter.
    pub const fn fen_char(self) -> char {
        match self {
            Self::Pawn => 'p',
            Self::Rook => 'r',
            Self::Knight => 'n',
            Self::Bishop => 'b',
            Self::King => 'k',
            Self::Queen => 'q',
        }
    }

    pub const fn from_fen_char(c: char) -> Option<Self> {
        match c.to_ascii_lowercase() {
            'p' => Some(Self::Pawn),
            'r' => Some(Self::Rook),
            'n' => Some(Self::Knight),
            'b' => Some(Self::Bishop),
            'k' => Some(Self::King),
            'q' => Some(Self::Queen),
            _ => None,
        }
    }
}

/// A chess piece: colour + role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Piece {
    pub color: Color,
    pub role: Role,
}

impl Piece {
    pub const fn new(color: Color, role: Role) -> Self {
        Self { color, role }
    }

    /// Decode a DGT piece code. Magic pieces (`0x0d..=0x0f`) and empty
    /// (`0x00`) decode to `None`; use [`code_name`] for those.
    pub const fn from_code(code: u8) -> Option<Self> {
        use Color::{Black as B, White as W};
        use Role::*;
        let (color, role) = match code {
            0x01 => (W, Pawn),
            0x02 => (W, Rook),
            0x03 => (W, Knight),
            0x04 => (W, Bishop),
            0x05 => (W, King),
            0x06 => (W, Queen),
            0x07 => (B, Pawn),
            0x08 => (B, Rook),
            0x09 => (B, Knight),
            0x0a => (B, Bishop),
            0x0b => (B, King),
            0x0c => (B, Queen),
            _ => return None,
        };
        Some(Self { color, role })
    }

    /// Encode as a DGT piece code.
    pub const fn code(self) -> u8 {
        use Color::{Black as B, White as W};
        use Role::*;
        match (self.color, self.role) {
            (W, Pawn) => 0x01,
            (W, Rook) => 0x02,
            (W, Knight) => 0x03,
            (W, Bishop) => 0x04,
            (W, King) => 0x05,
            (W, Queen) => 0x06,
            (B, Pawn) => 0x07,
            (B, Rook) => 0x08,
            (B, Knight) => 0x09,
            (B, Bishop) => 0x0a,
            (B, King) => 0x0b,
            (B, Queen) => 0x0c,
        }
    }

    /// FEN letter (`P`, `n`, ...).
    pub fn fen_char(self) -> char {
        let c = self.role.fen_char();
        match self.color {
            Color::White => c.to_ascii_uppercase(),
            Color::Black => c,
        }
    }

    /// Display name such as `white-pawn`.
    pub fn display_name(self) -> String {
        format!("{}-{}", self.color.name(), self.role.name())
    }
}

/// DGT piece/sensor codes. `EMPTY` means no piece detected.
pub mod code {
    pub const EMPTY: u8 = 0x00;
    pub const WPAWN: u8 = 0x01;
    pub const WROOK: u8 = 0x02;
    pub const WKNIGHT: u8 = 0x03;
    pub const WBISHOP: u8 = 0x04;
    pub const WKING: u8 = 0x05;
    pub const WQUEEN: u8 = 0x06;
    pub const BPAWN: u8 = 0x07;
    pub const BROOK: u8 = 0x08;
    pub const BKNIGHT: u8 = 0x09;
    pub const BBISHOP: u8 = 0x0a;
    pub const BKING: u8 = 0x0b;
    pub const BQUEEN: u8 = 0x0c;
    /// Magic piece: draw.
    pub const MAGIC_DRAW: u8 = 0x0d;
    /// Magic piece: white win.
    pub const MAGIC_WHITE_WIN: u8 = 0x0e;
    /// Magic piece: black win.
    pub const MAGIC_BLACK_WIN: u8 = 0x0f;
}

/// Is `code` a valid DGT sensor code (0x00..=0x0f)? The protocol reserves the
/// top nibble, so anything above 0x0f is a malformed field/piece byte.
pub const fn is_valid_code(code: u8) -> bool {
    code <= 0x0f
}

/// Human name for a raw DGT code, including empty and the magic pieces.
pub const fn code_name(code: u8) -> &'static str {
    match code {
        code::EMPTY => "empty",
        code::WPAWN => "white-pawn",
        code::WROOK => "white-rook",
        code::WKNIGHT => "white-knight",
        code::WBISHOP => "white-bishop",
        code::WKING => "white-king",
        code::WQUEEN => "white-queen",
        code::BPAWN => "black-pawn",
        code::BROOK => "black-rook",
        code::BKNIGHT => "black-knight",
        code::BBISHOP => "black-bishop",
        code::BKING => "black-king",
        code::BQUEEN => "black-queen",
        code::MAGIC_DRAW => "magic-draw",
        code::MAGIC_WHITE_WIN => "magic-white-win",
        code::MAGIC_BLACK_WIN => "magic-black-win",
        _ => "invalid",
    }
}

/// Parse a placement specification into a raw DGT code.
///
/// Accepts long names (`white-knight`), short names (`wn`), single FEN letters
/// (`N` for a white knight, `n` for a black one), `empty`, and the three magic
/// pieces.
pub fn parse_placement(s: &str) -> Result<u8> {
    let raw = s.trim();

    // A single character is a FEN letter, and FEN letters are case-sensitive:
    // handle this before any lowercasing.
    let mut chars = raw.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && let Some(role) = Role::from_fen_char(c)
    {
        let color = if c.is_ascii_uppercase() {
            Color::White
        } else {
            Color::Black
        };
        return Ok(Piece::new(color, role).code());
    }

    let t = raw.to_ascii_lowercase();
    let direct = match t.as_str() {
        "empty" | "none" | "clear" => Some(code::EMPTY),
        "magic-draw" | "draw" => Some(code::MAGIC_DRAW),
        "magic-white-win" | "white-win" => Some(code::MAGIC_WHITE_WIN),
        "magic-black-win" | "black-win" => Some(code::MAGIC_BLACK_WIN),
        _ => None,
    };
    if let Some(c) = direct {
        return Ok(c);
    }

    // Long name: "<color>-<role>".
    if let Some((color, role)) = t.split_once('-') {
        let color = match color {
            "white" | "w" => Color::White,
            "black" | "b" => Color::Black,
            _ => return Err(Error::InvalidPiece(s.to_string())),
        };
        let role = match role {
            "pawn" | "p" => Role::Pawn,
            "rook" | "r" => Role::Rook,
            "knight" | "n" => Role::Knight,
            "bishop" | "b" => Role::Bishop,
            "king" | "k" => Role::King,
            "queen" | "q" => Role::Queen,
            _ => return Err(Error::InvalidPiece(s.to_string())),
        };
        return Ok(Piece::new(color, role).code());
    }

    // Short name: "wn", "bp", or a single FEN letter, or a hex code "0x0d".
    if let Some(hex) = t.strip_prefix("0x") {
        let v = u8::from_str_radix(hex, 16).map_err(|_| Error::InvalidHex(s.to_string()))?;
        if is_valid_code(v) {
            return Ok(v);
        }
        return Err(Error::InvalidPiece(s.to_string()));
    }
    let chars: Vec<char> = t.chars().collect();
    match chars.as_slice() {
        [color, role] => {
            let color = match color {
                'w' => Color::White,
                'b' => Color::Black,
                _ => return Err(Error::InvalidPiece(s.to_string())),
            };
            let role =
                Role::from_fen_char(*role).ok_or_else(|| Error::InvalidPiece(s.to_string()))?;
            Ok(Piece::new(color, role).code())
        }
        _ => Err(Error::InvalidPiece(s.to_string())),
    }
}

/// A board field, `0` = `a8` .. `63` = `h1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Square(pub u8);

impl Square {
    pub fn new(index: u8) -> Result<Self> {
        if (index as usize) < BOARD_LEN {
            Ok(Self(index))
        } else {
            Err(Error::Protocol(format!("field {index} out of range 0..63")))
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// `0` = file a .. `7` = file h.
    pub const fn file(self) -> u8 {
        self.0 % 8
    }

    /// Chess rank, `1..=8`.
    pub const fn rank(self) -> u8 {
        8 - self.0 / 8
    }

    pub const fn from_file_rank(file: u8, rank: u8) -> Self {
        Self((8 - rank) * 8 + file)
    }

    /// Parse `e2` style coordinates.
    pub fn from_uci(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        if b.len() != 2 {
            return Err(Error::InvalidSquare(s.to_string()));
        }
        let file = b[0];
        let rank = b[1];
        if !(b'a'..=b'h').contains(&file) || !(b'1'..=b'8').contains(&rank) {
            return Err(Error::InvalidSquare(s.to_string()));
        }
        Ok(Self::from_file_rank(file - b'a', rank - b'0'))
    }

    /// Render as `e2`.
    pub fn uci(self) -> String {
        let file = (b'a' + self.file()) as char;
        let rank = (b'0' + self.rank()) as char;
        format!("{file}{rank}")
    }

    /// The square mirrored through the board centre (a1 <-> h8).
    pub const fn rotated(self) -> Self {
        Self(63 - self.0)
    }
}

/// Board field, or `None` if no piece is present.
pub fn code_to_piece(code: u8) -> Option<Piece> {
    Piece::from_code(code)
}

/// Render 64 sensor codes as a FEN board field. Magic pieces are rendered as
/// `.` because FEN cannot express them; use [`magic_squares`] to find them.
pub fn to_fen_placement(squares: &[u8; BOARD_LEN]) -> String {
    let mut out = String::with_capacity(72);
    for rank in 0..8 {
        let mut empty = 0;
        for file in 0..8 {
            let code = squares[rank * 8 + file];
            match Piece::from_code(code) {
                Some(p) => {
                    if empty > 0 {
                        out.push(char::from(b'0' + empty));
                        empty = 0;
                    }
                    out.push(p.fen_char());
                }
                None => empty += 1,
            }
        }
        if empty > 0 {
            out.push(char::from(b'0' + empty));
        }
        if rank != 7 {
            out.push('/');
        }
    }
    out
}

/// Fields occupied by a magic piece (`0x0d..=0x0f`), in field order.
pub fn magic_squares(squares: &[u8; BOARD_LEN]) -> Vec<(Square, u8)> {
    squares
        .iter()
        .enumerate()
        .filter(|(_, c)| (code::MAGIC_DRAW..=code::MAGIC_BLACK_WIN).contains(*c))
        .map(|(i, c)| (Square(i as u8), *c))
        .collect()
}

/// Parse a FEN board field (the first space-separated field of a FEN) into
/// 64 sensor codes. Only the placement is used: the board has no concept of
/// side to move, castling rights or counters.
pub fn from_fen_placement(placement: &str) -> Result<[u8; BOARD_LEN]> {
    let mut squares = [code::EMPTY; BOARD_LEN];
    let mut rank: i32 = 0;
    let mut file: i32 = 0;
    for ch in placement.chars() {
        match ch {
            '/' => {
                if file != 8 {
                    return Err(Error::InvalidFen(format!(
                        "rank {} has {file} files, expected 8",
                        rank + 1
                    )));
                }
                rank += 1;
                file = 0;
                if rank > 7 {
                    return Err(Error::InvalidFen("too many ranks".into()));
                }
            }
            '1'..='8' => {
                file += ch.to_digit(10).unwrap() as i32;
                if file > 8 {
                    return Err(Error::InvalidFen(format!("rank {} overflows", rank + 1)));
                }
            }
            _ => {
                let role = Role::from_fen_char(ch)
                    .ok_or_else(|| Error::InvalidFen(format!("unknown piece {ch:?}")))?;
                if file >= 8 {
                    return Err(Error::InvalidFen(format!("rank {} overflows", rank + 1)));
                }
                let color = if ch.is_ascii_uppercase() {
                    Color::White
                } else {
                    Color::Black
                };
                squares[(rank * 8 + file) as usize] = Piece::new(color, role).code();
                file += 1;
            }
        }
    }
    if rank != 7 || file != 8 {
        return Err(Error::InvalidFen("expected 8 ranks of 8 files".into()));
    }
    Ok(squares)
}

/// Extract the placement field from a full FEN string.
pub fn fen_placement_of(fen: &str) -> &str {
    fen.split_whitespace().next().unwrap_or("")
}

/// Render a compact ASCII diagram, `1..8` on the left, `a..h` below.
pub fn to_ascii(squares: &[u8; BOARD_LEN]) -> String {
    let mut out = String::with_capacity(400);
    out.push_str("  +------------------------+\n");
    for rank in 0..8 {
        out.push(char::from(b'8' - rank as u8));
        out.push_str(" |");
        for file in 0..8 {
            let code = squares[rank * 8 + file];
            let ch = match Piece::from_code(code) {
                Some(p) => p.fen_char(),
                None if (code::MAGIC_DRAW..=code::MAGIC_BLACK_WIN).contains(&code) => '*',
                None => ' ',
            };
            out.push(' ');
            out.push(ch);
            out.push(' ');
        }
        out.push_str("|\n");
    }
    out.push_str("  +------------------------+\n");
    out.push_str("    a  b  c  d  e  f  g  h\n");
    out
}

/// The standard chess starting position in DGT field order.
pub const START_SQUARES: [u8; BOARD_LEN] = [
    code::BROOK,
    code::BKNIGHT,
    code::BBISHOP,
    code::BQUEEN,
    code::BKING,
    code::BBISHOP,
    code::BKNIGHT,
    code::BROOK,
    code::BPAWN,
    code::BPAWN,
    code::BPAWN,
    code::BPAWN,
    code::BPAWN,
    code::BPAWN,
    code::BPAWN,
    code::BPAWN,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::EMPTY,
    code::WPAWN,
    code::WPAWN,
    code::WPAWN,
    code::WPAWN,
    code::WPAWN,
    code::WPAWN,
    code::WPAWN,
    code::WPAWN,
    code::WROOK,
    code::WKNIGHT,
    code::WBISHOP,
    code::WQUEEN,
    code::WKING,
    code::WBISHOP,
    code::WKNIGHT,
    code::WROOK,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_position_matches_known_board_dump() {
        // Real capture from arnholt/dgtdriver's test corpus (1.inbytes), a
        // DGT_MSG_BOARD_DUMP of the initial position.
        let dump: [u8; 64] = [
            0x08, 0x09, 0x0a, 0x0c, 0x0b, 0x0a, 0x09, 0x08, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
            0x07, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x02, 0x03, 0x04, 0x06, 0x05, 0x04, 0x03, 0x02,
        ];
        assert_eq!(dump, START_SQUARES);
        assert_eq!(
            to_fen_placement(&dump),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR"
        );
        assert_eq!(
            from_fen_placement("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR").unwrap(),
            dump
        );
    }

    #[test]
    fn fen_roundtrip_random_placement() {
        let placement = "r3k2r/1pp2ppp/p1n1b3/8/2B1P3/P2P1N2/1P3PPP/R3K2R";
        let squares = from_fen_placement(placement).unwrap();
        assert_eq!(to_fen_placement(&squares), placement);
    }

    #[test]
    fn fen_rejects_bad_placements() {
        assert!(from_fen_placement("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP").is_err());
        assert!(from_fen_placement("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNX").is_err());
        assert!(from_fen_placement("9/8/8/8/8/8/8/8").is_err());
    }

    #[test]
    fn square_coordinates() {
        assert_eq!(Square(0).uci(), "a8");
        assert_eq!(Square(63).uci(), "h1");
        assert_eq!(Square::from_uci("e2").unwrap(), Square(52));
        assert_eq!(Square::from_uci("e4").unwrap(), Square(36));
        assert_eq!(Square::from_uci("a8").unwrap(), Square(0));
        assert_eq!(Square(36).rotated(), Square(27));
        assert!(Square::from_uci("i9").is_err());
        assert_eq!(
            Square::from_file_rank(4, 2),
            Square::from_uci("e2").unwrap()
        );
    }

    #[test]
    fn piece_codes_roundtrip() {
        for code in 0x00..=0x0f {
            if let Some(p) = Piece::from_code(code) {
                assert_eq!(p.code(), code, "{code:#04x}");
            }
        }
        assert_eq!(
            Piece::from_code(0x03).unwrap().display_name(),
            "white-knight"
        );
        // Single letters are FEN letters: case carries the colour.
        assert_eq!(parse_placement("N").unwrap(), 0x03);
        assert_eq!(parse_placement("n").unwrap(), 0x09);
        assert_eq!(parse_placement("P").unwrap(), 0x01);
        assert_eq!(parse_placement("p").unwrap(), 0x07);
        assert_eq!(parse_placement("wn").unwrap(), 0x03);
        assert_eq!(parse_placement("white-knight").unwrap(), 0x03);
        assert_eq!(parse_placement("magic-draw").unwrap(), 0x0d);
        assert_eq!(parse_placement("0x0f").unwrap(), 0x0f);
        assert!(parse_placement("purple-dragon").is_err());
        assert!(parse_placement("").is_err());
    }

    #[test]
    fn magic_squares_are_reported_and_never_in_fen() {
        let mut squares = START_SQUARES;
        squares[Square::from_uci("e2").unwrap().index()] = code::MAGIC_DRAW;
        assert_eq!(
            magic_squares(&squares),
            vec![(Square::from_uci("e2").unwrap(), 0x0d)]
        );
        // FEN cannot express a magic piece, so it renders as an empty field.
        assert_ne!(to_fen_placement(&squares), to_fen_placement(&START_SQUARES));
        assert_eq!(
            to_fen_placement(&squares),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPP1PPP/RNBQKBNR"
        );
    }
}
