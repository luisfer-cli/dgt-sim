//! Chess vocabulary used *around* the emulator: FEN, UCI and PGN.
//!
//! This module is deliberately separate from [`crate::board`] and
//! [`crate::protocol`]. The sensor core knows nothing about legal moves; this
//! is where legality is allowed to exist, and only for the operations that
//! genuinely need it (`set-fen` validation and `replay`).
//!
//! `shakmaty` is used for exactly two things: validating a full FEN, and
//! resolving SAN tokens from a PGN into concrete moves. Turning a move into a
//! *physical* sequence of field changes is done here by geometry, so that
//! illegal-but-real board states are still emulatable.

use shakmaty::fen::Fen;
use shakmaty::san::San;
use shakmaty::{CastlingMode, Chess, Move, Position, Role as SRole, Square as SSquare};

use crate::board::FieldChange;
use crate::error::{Error, Result};
use crate::protocol::pieces::{self, BOARD_LEN, Color, Square, code};

/// Validate a FEN and return only its placement as DGT sensor codes.
///
/// Only the placement is used: the board has no concept of side to move,
/// castling rights or move counters. The remaining FEN fields are validated
/// when present, but a *legal* chess position is not required — an emulator
/// must be able to load an empty board or two white kings, because a physical
/// board can hold exactly that.
pub fn parse_fen(fen: &str) -> Result<[u8; BOARD_LEN]> {
    let placement = pieces::fen_placement_of(fen);
    if placement.is_empty() {
        return Err(Error::InvalidFen("empty FEN".into()));
    }
    let squares = pieces::from_fen_placement(placement)
        .map_err(|e| Error::InvalidFen(format!("{fen:?}: {e}")))?;
    if fen.split_whitespace().count() > 1 {
        Fen::from_ascii(fen.as_bytes()).map_err(|e| Error::InvalidFen(format!("{fen:?}: {e}")))?;
    }
    Ok(squares)
}

/// Position from a FEN, for move generation during `replay`.
pub fn position(fen: &str) -> Result<Chess> {
    Fen::from_ascii(fen.as_bytes())
        .map_err(|e| Error::InvalidFen(format!("{fen:?}: {e}")))?
        .into_position::<Chess>(CastlingMode::Standard)
        .map_err(|e| Error::InvalidFen(format!("{fen:?}: {e}")))
}

/// Translate a `shakmaty` square into a DGT field.
fn field_of(square: SSquare) -> Square {
    Square::from_file_rank(u8::from(square.file()), u8::from(square.rank()) + 1)
}
fn shakmaty_square(square: Square) -> SSquare {
    // Both coordinates are always in range: `Square` is 0..=63 by construction.
    SSquare::new(u32::from(square.file()) + 8 * u32::from(square.rank() - 1))
}

fn code_of(color: shakmaty::Color, role: SRole) -> u8 {
    let color = match color {
        shakmaty::Color::White => Color::White,
        shakmaty::Color::Black => Color::Black,
    };
    let role = match role {
        SRole::Pawn => pieces::Role::Pawn,
        SRole::Knight => pieces::Role::Knight,
        SRole::Bishop => pieces::Role::Bishop,
        SRole::Rook => pieces::Role::Rook,
        SRole::Queen => pieces::Role::Queen,
        SRole::King => pieces::Role::King,
    };
    pieces::Piece::new(color, role).code()
}

/// A physical move plan: the ordered sequence of field changes a human would
/// produce when playing `mv` on a real board.
///
/// `verbose` adds the intermediate states a real board sometimes reports:
///
/// * a capture clears the destination before the capturing piece lands,
/// * a promotion shows the pawn arriving before it is exchanged.
///
/// `direct` (the default) emits the minimum sequence, which is what the DGT
/// description says a board *may* report ("the board may generate a single
/// message (piece B on square)").
pub fn plan_move(squares: &[u8; BOARD_LEN], mv: Move, verbose: bool) -> Vec<FieldChange> {
    let mut plan = Vec::with_capacity(4);
    let piece_code = code_of_shakmaty(mv, squares);
    match mv {
        Move::Normal {
            from,
            to,
            promotion,
            ..
        } => {
            let from_field = field_of(from);
            let to_field = field_of(to);
            let target_occupied = squares[to_field.index()] != code::EMPTY;
            plan.push(FieldChange::new(from_field, code::EMPTY));
            if verbose && target_occupied {
                plan.push(FieldChange::new(to_field, code::EMPTY));
            }
            if verbose && promotion.is_some() {
                // A pawn arrives, then is swapped for the promoted piece.
                plan.push(FieldChange::new(
                    to_field,
                    pawn_code_current(squares, from_field),
                ));
            }
            plan.push(FieldChange::new(
                to_field,
                promotion.map_or(piece_code, |role| {
                    code_of(color_of(squares, from_field), role)
                }),
            ));
        }
        Move::EnPassant { from, to } => {
            let to_field = field_of(to);
            plan.push(FieldChange::new(field_of(from), code::EMPTY));
            // The captured pawn sits beside the destination, on the rank the
            // moving pawn came from.
            let captured = Square::from_file_rank(to_field.file(), field_of(from).rank());
            if verbose {
                plan.push(FieldChange::new(captured, code::EMPTY));
            }
            plan.push(FieldChange::new(to_field, piece_code));
        }
        Move::Castle { king, rook } => {
            // `to()` for a castle is the rook square, which is what tells us
            // the direction independently of which side the king stands on.
            let king_from = field_of(king);
            let rook_from = field_of(rook);
            let kingside = rook_from.file() > king_from.file();
            let (king_to, rook_to) = castle_targets(king_from, kingside);
            plan.push(FieldChange::new(king_from, code::EMPTY));
            plan.push(FieldChange::new(
                king_to,
                code_of(color_of(squares, king_from), SRole::King),
            ));
            plan.push(FieldChange::new(rook_from, code::EMPTY));
            plan.push(FieldChange::new(
                rook_to,
                code_of(color_of(squares, king_from), SRole::Rook),
            ));
        }
        Move::Put { role, to } => {
            let to_field = field_of(to);
            // Crazyhouse drops do not exist on a DGT board; the colour is
            // unknowable from the move alone, so assume white.
            plan.push(FieldChange::new(
                to_field,
                code_of(shakmaty::Color::White, role),
            ));
        }
    }
    plan
}

/// New field code for the pawn that is about to be promoted.
fn pawn_code_current(squares: &[u8; BOARD_LEN], from: Square) -> u8 {
    let current = squares[from.index()];
    if current == code::EMPTY {
        code::WPAWN
    } else {
        current
    }
}

/// Colour of the piece standing on a field, defaulting to White when empty.
fn color_of(squares: &[u8; BOARD_LEN], square: Square) -> shakmaty::Color {
    match pieces::Piece::from_code(squares[square.index()]) {
        Some(p) => match p.color {
            Color::White => shakmaty::Color::White,
            Color::Black => shakmaty::Color::Black,
        },
        None => shakmaty::Color::White,
    }
}

fn code_of_shakmaty(mv: Move, squares: &[u8; BOARD_LEN]) -> u8 {
    let role = mv.role();
    if let Some(from) = mv.from() {
        let from_field = field_of(from);
        let color = color_of(squares, from_field);
        // Trust the board for the colour; trust the move for the role.
        code_of(color, role)
    } else {
        code_of(shakmaty::Color::White, role)
    }
}

/// King and rook destinations for a castle, given the king's origin and the
/// direction.
fn castle_targets(king_from: Square, kingside: bool) -> (Square, Square) {
    let rank = king_from.rank();
    if kingside {
        // e1g1 / e8g8, rook to f1 / f8.
        (
            Square::from_file_rank(6, rank),
            Square::from_file_rank(5, rank),
        )
    } else {
        // e1c1 / e8c8, rook to d1 / d8.
        (
            Square::from_file_rank(2, rank),
            Square::from_file_rank(3, rank),
        )
    }
}

/// Plan a move given in UCI (e.g. `e2e4`, `e7e8q`).
///
/// The move is *not* checked for legality: a board emulator must accept
/// whatever a client asks for, including moves that have no legal
/// interpretation. Missing origin squares *are* rejected, because that would
/// silently produce a nonsense sequence.
pub fn plan_uci(squares: &[u8; BOARD_LEN], uci: &str, verbose: bool) -> Result<Vec<FieldChange>> {
    let mv = parse_uci(squares, uci)?;
    Ok(plan_move(squares, mv, verbose))
}

/// Parse a UCI move against a placement, without checking legality.
pub fn parse_uci(squares: &[u8; BOARD_LEN], uci: &str) -> Result<Move> {
    let t = uci.trim();
    if t.len() < 4 || t.len() > 5 {
        return Err(Error::InvalidUci(uci.to_string()));
    }
    let from = Square::from_uci(&t[0..2]).map_err(|_| Error::InvalidUci(uci.to_string()))?;
    let to = Square::from_uci(&t[2..4]).map_err(|_| Error::InvalidUci(uci.to_string()))?;
    let promotion = match t.as_bytes().get(4) {
        None => None,
        Some(b'q') | Some(b'Q') => Some(SRole::Queen),
        Some(b'r') | Some(b'R') => Some(SRole::Rook),
        Some(b'b') | Some(b'B') => Some(SRole::Bishop),
        Some(b'n') | Some(b'N') => Some(SRole::Knight),
        Some(_) => return Err(Error::InvalidUci(uci.to_string())),
    };
    let from_ss = shakmaty_square(from);
    let to_ss = shakmaty_square(to);

    let moved = pieces::Piece::from_code(squares[from.index()])
        .ok_or_else(|| Error::InvalidUci(format!("{uci}: no piece on {}", from.uci())))?;
    let captured = pieces::Piece::from_code(squares[to.index()]);

    // A king moving two files is a castle, whatever the client thinks.
    if moved.role == pieces::Role::King
        && from.file().abs_diff(to.file()) == 2
        && from.rank() == to.rank()
    {
        let rook_file: u8 = if to.file() < from.file() { 0 } else { 7 };
        let rook_from = Square::from_file_rank(rook_file, from.rank());
        return Ok(Move::Castle {
            king: from_ss,
            rook: shakmaty_square(rook_from),
        });
    }

    if moved.role == pieces::Role::Pawn
        && captured.is_none()
        && from.file() != to.file()
        && from.index() != to.index()
    {
        return Ok(Move::EnPassant {
            from: from_ss,
            to: to_ss,
        });
    }

    Ok(Move::Normal {
        role: match moved.role {
            pieces::Role::Pawn => SRole::Pawn,
            pieces::Role::Knight => SRole::Knight,
            pieces::Role::Bishop => SRole::Bishop,
            pieces::Role::Rook => SRole::Rook,
            pieces::Role::Queen => SRole::Queen,
            pieces::Role::King => SRole::King,
        },
        from: from_ss,
        to: to_ss,
        capture: captured.map(|p| match p.role {
            pieces::Role::Pawn => SRole::Pawn,
            pieces::Role::Knight => SRole::Knight,
            pieces::Role::Bishop => SRole::Bishop,
            pieces::Role::Rook => SRole::Rook,
            pieces::Role::Queen => SRole::Queen,
            pieces::Role::King => SRole::King,
        }),
        promotion: if moved.role == pieces::Role::Pawn && (to.rank() == 8 || to.rank() == 1) {
            promotion.or(Some(SRole::Queen))
        } else {
            None
        },
    })
}

/// Split PGN movetext into SAN tokens, discarding headers, comments, NAGs and
/// variations.
pub fn pgn_san_tokens(pgn: &str) -> Vec<String> {
    let bytes = pgn.as_bytes();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut i = 0usize;
    let mut variation_depth = 0i32;

    let flush = |tokens: &mut Vec<String>, current: &mut String| {
        if !current.is_empty() {
            tokens.push(std::mem::take(current));
        }
    };

    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '{' => {
                flush(&mut tokens, &mut current);
                while i < bytes.len() && bytes[i] != b'}' {
                    i += 1;
                }
            }
            ';' => {
                flush(&mut tokens, &mut current);
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '[' => {
                flush(&mut tokens, &mut current);
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
            }
            '(' => {
                flush(&mut tokens, &mut current);
                variation_depth += 1;
            }
            ')' => {
                flush(&mut tokens, &mut current);
                variation_depth -= 1;
            }
            '$' => {
                flush(&mut tokens, &mut current);
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            c if c.is_whitespace() => {
                flush(&mut tokens, &mut current);
            }
            _ => {
                if variation_depth <= 0 {
                    current.push(c);
                }
            }
        }
        i += 1;
    }
    flush(&mut tokens, &mut current);

    tokens.into_iter().filter(|t| !is_pgn_noise(t)).collect()
}

/// Move numbers, results and the like: everything that is not a SAN move.
fn is_pgn_noise(token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    if matches!(token, "1-0" | "0-1" | "1/2-1/2" | "*") {
        return true;
    }
    // "12.", "12...", "12" as a move number.
    let trimmed = token.trim_end_matches('.');
    if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    // Symbols like "--" or "Z0".
    token.starts_with("--")
}

/// Resolve a PGN into a list of UCI moves.
///
/// `start_fen` defaults to the standard starting position. The resulting list
/// is what `replay` feeds to the board one move at a time, so the board never
/// has to know about chess rules.
pub fn pgn_to_uci(pgn: &str, start_fen: Option<&str>) -> Result<Vec<String>> {
    let mut pos = match start_fen {
        Some(fen) => position(fen)?,
        None => Chess::default(),
    };
    let tokens = pgn_san_tokens(pgn);
    if tokens.is_empty() {
        return Err(Error::InvalidPgn("no moves found".into()));
    }
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let cleaned: String = token.chars().filter(|c| *c != '!' && *c != '?').collect();
        let san = San::from_ascii(cleaned.as_bytes())
            .map_err(|_| Error::InvalidPgn(format!("cannot parse SAN token {token:?}")))?;
        let mv = san
            .to_move(&pos)
            .map_err(|e| Error::InvalidPgn(format!("illegal move {token:?}: {e}")))?;
        out.push(uci_of(&pos, mv));
        pos = pos
            .play(mv)
            .map_err(|e| Error::InvalidPgn(format!("cannot play {token:?}: {e}")))?;
    }
    Ok(out)
}

/// UCI string for a `shakmaty` move, with castling written as a king move so
/// that the same geometry as `move --uci` applies.
pub fn uci_of(_pos: &Chess, mv: Move) -> String {
    match mv {
        Move::Normal {
            from,
            to,
            promotion,
            ..
        } => {
            let mut s = format!("{}{}", square_uci(field_of(from)), square_uci(field_of(to)));
            if let Some(role) = promotion {
                s.push(match role {
                    SRole::Knight => 'n',
                    SRole::Bishop => 'b',
                    SRole::Rook => 'r',
                    _ => 'q',
                });
            }
            s
        }
        Move::EnPassant { from, to } => {
            format!("{}{}", square_uci(field_of(from)), square_uci(field_of(to)))
        }
        Move::Castle { king, rook } => {
            // Encode the king move only: `plan_uci` derives the rook half from
            // the geometry, so a castle stays a single logical move.
            let king_from = field_of(king);
            let kingside = field_of(rook).file() > king_from.file();
            let (king_to, _) = castle_targets(king_from, kingside);
            format!("{}{}", square_uci(king_from), square_uci(king_to))
        }
        Move::Put { to, .. } => format!("@{}", square_uci(field_of(to))),
    }
}

fn square_uci(square: Square) -> String {
    square.uci()
}

/// Promotion piece code for a given colour, used by `place` ergonomics.
pub fn promotion_code(color: Color, role: pieces::Role) -> u8 {
    pieces::Piece::new(color, role).code()
}

/// Precomputed plan for clearing every field: `dgt-sim reset --empty`.
pub fn empty_board_changes(squares: &[u8; BOARD_LEN]) -> Vec<FieldChange> {
    (0..BOARD_LEN)
        .filter(|i| squares[*i] != code::EMPTY)
        .map(|i| FieldChange::new(Square(i as u8), code::EMPTY))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::pieces::{START_SQUARES, from_fen_placement};

    fn apply(squares: &mut [u8; BOARD_LEN], plan: &[FieldChange]) {
        for change in plan {
            squares[change.square.index()] = change.code;
        }
    }

    #[test]
    fn quiet_move_clears_then_places() {
        let mut squares = START_SQUARES;
        let plan = plan_uci(&squares, "e2e4", false).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].square.uci(), "e2");
        assert_eq!(plan[0].code, code::EMPTY);
        assert_eq!(plan[1].square.uci(), "e4");
        assert_eq!(plan[1].code, code::WPAWN);
        apply(&mut squares, &plan);
        assert_eq!(
            squares[Square::from_uci("e2").unwrap().index()],
            code::EMPTY
        );
        assert_eq!(
            squares[Square::from_uci("e4").unwrap().index()],
            code::WPAWN
        );
    }

    #[test]
    fn capture_replaces_in_one_message_by_default() {
        let squares = from_fen_placement("8/8/8/3p4/4P3/8/8/K6k").unwrap();
        let plan = plan_uci(&squares, "e4d5", false).unwrap();
        assert_eq!(plan.len(), 2, "clear source, then land on the target");
        assert_eq!(plan[1].square.uci(), "d5");
        assert_eq!(plan[1].code, code::WPAWN);
    }

    #[test]
    fn verbose_capture_clears_the_target_first() {
        let squares = from_fen_placement("8/8/8/3p4/4P3/8/8/K6k").unwrap();
        let plan = plan_uci(&squares, "e4d5", true).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(
            (plan[1].square.uci().as_str(), plan[1].code),
            ("d5", code::EMPTY)
        );
        assert_eq!(plan[2].code, code::WPAWN);
    }

    #[test]
    fn promotion_defaults_to_queen_and_honours_an_explicit_piece() {
        let squares = from_fen_placement("8/P7/8/8/8/8/8/K6k").unwrap();
        let plan = plan_uci(&squares, "a7a8", false).unwrap();
        assert_eq!(plan.last().unwrap().code, code::WQUEEN);

        let plan = plan_uci(&squares, "a7a8n", false).unwrap();
        assert_eq!(plan.last().unwrap().code, code::WKNIGHT);
    }

    #[test]
    fn verbose_promotion_shows_the_pawn_arriving_first() {
        let squares = from_fen_placement("8/P7/8/8/8/8/8/K6k").unwrap();
        let plan = plan_uci(&squares, "a7a8q", true).unwrap();
        // Lift the pawn, put it on the last rank, swap it for the promotion.
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[1].code, code::WPAWN);
        assert_eq!(plan[2].code, code::WQUEEN);

        // On an occupied target the capture is cleared in between.
        let squares = from_fen_placement("1r6/P7/8/8/8/8/8/K6k").unwrap();
        let plan = plan_uci(&squares, "a7b8q", true).unwrap();
        assert_eq!(plan.len(), 4);
        assert_eq!(plan[1].code, code::EMPTY, "b8 cleared first");
        assert_eq!(plan[2].code, code::WPAWN);
        assert_eq!(plan[3].code, code::WQUEEN);
    }

    #[test]
    fn castling_moves_both_king_and_rook() {
        let squares = from_fen_placement("r3k2r/8/8/8/8/8/8/R3K2R").unwrap();
        let plan = plan_uci(&squares, "e1g1", false).unwrap();
        assert_eq!(plan.len(), 4);
        assert_eq!(
            plan.iter().map(|c| c.square.uci()).collect::<Vec<_>>(),
            vec!["e1", "g1", "h1", "f1"]
        );
        assert_eq!(plan[1].code, code::WKING);
        assert_eq!(plan[3].code, code::WROOK);

        let plan = plan_uci(&squares, "e1c1", false).unwrap();
        assert_eq!(
            plan.iter().map(|c| c.square.uci()).collect::<Vec<_>>(),
            vec!["e1", "c1", "a1", "d1"]
        );
    }

    #[test]
    fn en_passant_removes_the_pawn_beside_the_target() {
        let squares = from_fen_placement("8/8/8/3pP3/8/8/8/K6k").unwrap();
        let plan = plan_uci(&squares, "e5d6", true).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].square.uci(), "e5");
        assert_eq!(plan[1].square.uci(), "d5", "the captured pawn");
        assert_eq!(plan[2].square.uci(), "d6");
        assert_eq!(plan[2].code, code::WPAWN);
    }

    #[test]
    fn moving_from_an_empty_square_is_rejected() {
        let squares = START_SQUARES;
        let err = plan_uci(&squares, "e5e4", false).unwrap_err();
        assert!(matches!(err, Error::InvalidUci(_)), "got {err:?}");
        assert!(plan_uci(&squares, "zz99", false).is_err());
        assert!(plan_uci(&squares, "e2e4x", false).is_err());
    }

    #[test]
    fn illegal_moves_are_still_planned() {
        // A rook teleporting across the board is not legal chess, but a board
        // emulator must be able to show it.
        let squares = START_SQUARES;
        let plan = plan_uci(&squares, "a1h8", false).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[1].code, code::WROOK);
    }

    #[test]
    fn pgn_tokenizer_skips_noise() {
        let pgn = r#"
[Event "Test"]
[Result "1-0"]

1. e4 e5 {a comment (with parens)} 2. Nf3 $1 Nc6 (2... d6 3. d4) 3. Bb5 1-0
"#;
        let tokens = pgn_san_tokens(pgn);
        assert_eq!(tokens, vec!["e4", "e5", "Nf3", "Nc6", "Bb5"]);
    }

    #[test]
    fn pgn_replays_into_uci_moves() {
        let pgn = "1. e4 e5 2. Nf3 Nc6 3. Bb5 a6";
        let moves = pgn_to_uci(pgn, None).unwrap();
        assert_eq!(moves, vec!["e2e4", "e7e5", "g1f3", "b8c6", "f1b5", "a7a6"]);
    }

    #[test]
    fn pgn_handles_castling_and_promotion() {
        let pgn = "1. e4 e5 2. Nf3 Nc6 3. Bc4 Bc5 4. O-O";
        let moves = pgn_to_uci(pgn, None).unwrap();
        assert_eq!(moves.len(), 7);
        // A castle is one logical move; the rook half comes from geometry.
        assert_eq!(moves[6], "e1g1");
        let squares =
            from_fen_placement("r1bqk1nr/pppp1ppp/2n5/2b1p3/2B1P3/5N2/PPPP1PPP/RNBQK2R").unwrap();
        let plan = plan_uci(&squares, &moves[6], false).unwrap();
        assert_eq!(
            plan.iter().map(|c| c.square.uci()).collect::<Vec<_>>(),
            vec!["e1", "g1", "h1", "f1"]
        );
    }

    #[test]
    fn pgn_rejects_broken_input() {
        assert!(pgn_to_uci("1. e9 e5", None).is_err());
        assert!(pgn_to_uci("", None).is_err());
    }

    #[test]
    fn fen_validation_accepts_and_rejects() {
        assert!(parse_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1").is_ok());
        // Placement only is enough; the board does not need the other fields.
        assert!(parse_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR").is_ok());
        // Positions no chess engine would accept are still loadable.
        assert!(parse_fen("8/8/8/8/8/8/8/8 w - - 0 1").is_ok());
        assert!(parse_fen("8/8/8/8/8/8/8/K6K w - - 0 1").is_ok());

        assert!(parse_fen("nonsense").is_err());
        assert!(parse_fen("").is_err());
        assert!(parse_fen("8/8/8/8/8/8/8").is_err());
        assert!(parse_fen("8/8/8/8/8/8/8/8 w KQkq - notanumber 0").is_err());
    }

    #[test]
    fn a_full_pgn_from_the_dgt_reference_corpus_replays() {
        // From arnsholt/dgtdriver test/resources/1.uci (Italian game).
        let pgn = "1. e4 e5 2. Nf3 Nc6 3. Bb5 Nf6 4. d3 Bc5 5. Bxc6 dxc6 6. O-O Qe7";
        let moves = pgn_to_uci(pgn, None).unwrap();
        assert_eq!(
            &moves[..10],
            &[
                "e2e4", "e7e5", "g1f3", "b8c6", "f1b5", "g8f6", "d2d3", "f8c5", "b5c6", "d7c6"
            ]
        );
        assert_eq!(moves[10], "e1g1");
        assert_eq!(moves[11], "d8e7");
    }
}
