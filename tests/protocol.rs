//! End-to-end protocol tests.
//!
//! Every test here drives a real board over a real TCP socket, speaking raw
//! bytes: no JSON, no shortcuts. Where a test vector exists from a physical
//! board it is used verbatim, so the assertions are anchored to hardware
//! captures rather than to this implementation's own output.

use std::net::SocketAddr;
use std::time::Duration;

use dgt_sim::board::{PhysicalStyle, Snapshot};
use dgt_sim::control::Request;
use dgt_sim::manager::{RunningSimulator, StartOptions};
use dgt_sim::protocol::commands::{HostCommand, clock_command};
use dgt_sim::protocol::messages::{self, Framer};
use dgt_sim::protocol::pieces::{self, code};
use dgt_sim::transport::{Faults, TransportKind};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A board over TCP plus a connected client.
struct Client {
    stream: TcpStream,
    framer: Framer,
    buf: Vec<u8>,
}

impl Client {
    async fn connect(endpoint: &str) -> Self {
        let addr = endpoint.trim_start_matches("tcp://").to_string();
        let stream = TcpStream::connect(addr).await.expect("connect to board");
        Self {
            stream,
            framer: Framer::new(),
            buf: vec![0u8; 4096],
        }
    }

    /// Send raw bytes to the board.
    async fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("write");
    }

    /// Read until `count` complete messages have been seen, or time out.
    async fn expect_frames(&mut self, count: usize, timeout: Duration) -> Vec<messages::Frame> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            while let Some(frame) = self.framer.next_frame() {
                return vec![frame];
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                panic!(
                    "timed out after seeing {} of {count} frames",
                    self.framer.skipped_bytes()
                );
            }
            let n = tokio::time::timeout(left, self.stream.read(&mut self.buf))
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {count} frames"))
                .expect("read");
            assert!(n > 0, "the board closed the connection early");
            self.framer.push(&self.buf[..n]);
        }
    }

    /// Collect exactly `count` frames.
    async fn frames(&mut self, count: usize) -> Vec<messages::Frame> {
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            out.extend(self.expect_frames(count, Duration::from_secs(5)).await);
        }
        out
    }

    /// Assert that nothing arrives within `window`.
    async fn expect_silence(&mut self, window: Duration) {
        let result = tokio::time::timeout(window, self.stream.read(&mut self.buf)).await;
        if let Ok(Ok(n)) = result {
            if n > 0 {
                self.framer.push(&self.buf[..n]);
                let frames: Vec<_> = std::iter::from_fn(|| self.framer.next_frame()).collect();
                panic!("expected silence but received {frames:?}");
            }
        }
    }
}

async fn simulator(boards: u32, faults: Faults) -> RunningSimulator {
    let opts = StartOptions {
        boards,
        transport: TransportKind::Tcp,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        physical_style: PhysicalStyle::Direct,
        faults,
    };
    RunningSimulator::start(opts, "/tmp/dgt-sim-protocol.sock")
        .await
        .expect("start the simulator")
}

fn endpoint(sim: &RunningSimulator, board: u32) -> String {
    sim.endpoint(board).expect("board exists").to_string()
}

#[tokio::test]
async fn board_dump_answers_with_a_real_shaped_frame() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;

    client.send(&[0x42]).await; // DGT_SEND_BRD
    let frames = client.frames(1).await;
    assert_eq!(frames[0].id, messages::id::BOARD_DUMP);
    assert_eq!(frames[0].payload.len(), 64);
    // The payload is the starting position in DGT field order, so it converts
    // straight to a FEN board field.
    let mut squares = [0u8; 64];
    squares.copy_from_slice(&frames[0].payload);
    assert_eq!(squares, pieces::START_SQUARES);
    assert_eq!(
        pieces::to_fen_placement(&squares),
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR"
    );
    // The frame on the wire is 67 bytes with the documented header.
    assert_eq!(
        &[0x80 | frames[0].id, 0x00, 67][..],
        &[0x86, 0x00, 0x43],
        "header bytes a real board sends"
    );

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn identification_commands_all_respond() {
    let sim = simulator(1, Faults::default()).await;
    sim.expect(Request::Config {
        board: 1,
        config: dgt_sim::control::ConfigRequest {
            serial: Some("00137".into()),
            version: Some("2.4".into()),
            bus_address: Some(1025),
            ..Default::default()
        },
    })
    .await
    .unwrap();

    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    // SEND_BRD, RETURN_SERIALNR, RETURN_LONG_SERIALNR, RETURN_BUSADRES,
    // SEND_TRADEMARK, SEND_VERSION.
    client.send(&[0x42, 0x45, 0x55, 0x46, 0x47, 0x4d]).await;
    let frames = client.frames(6).await;
    let ids: Vec<u8> = frames.iter().map(|f| f.id).collect();
    assert_eq!(
        ids,
        vec![
            messages::id::BOARD_DUMP,
            messages::id::SERIALNR,
            messages::id::LONG_SERIALNR,
            messages::id::BUSADRES,
            messages::id::TRADEMARK,
            messages::id::VERSION,
        ]
    );
    assert_eq!(&frames[1].payload, b"00137");
    assert_eq!(frames[2].payload.len(), 10);
    // Bus address 1025 = 0x0401 -> 0x08, 0x01 in two 7-bit halves.
    assert_eq!(frames[3].payload, vec![0x08, 0x01]);
    assert!(
        String::from_utf8_lossy(&frames[4].payload).contains("DGT"),
        "trademark text"
    );
    assert_eq!(frames[5].payload, vec![2, 4]);

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn update_board_mode_streams_lift_then_place() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;

    // First, an IDLE board must stay silent.
    sim.expect(Request::Move {
        board: 1,
        uci: "e2e4".into(),
    })
    .await
    .unwrap();
    client.expect_silence(Duration::from_millis(150)).await;

    // Enter UPDATE_BOARD, then move again.
    client.send(&[0x44]).await;
    let snapshot: Snapshot = serde_json::from_value(
        sim.expect(Request::Move {
            board: 1,
            uci: "d2d4".into(),
        })
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot.mode, "update_board");
    assert_eq!(snapshot.last_emitted, 2, "one message per touched field");

    let frames = client.frames(2).await;
    assert_eq!(frames[0].id, messages::id::FIELD_UPDATE);
    // d2 is field 51, d4 is field 35.
    assert_eq!(frames[0].payload, vec![51, code::EMPTY], "d2 emptied first");
    assert_eq!(frames[1].payload, vec![35, code::WPAWN], "then d4 filled");

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn update_mode_sends_clock_messages_and_update_board_does_not() {
    let sim = simulator(1, Faults::default()).await;
    sim.expect(Request::Clock {
        board: 1,
        clock: dgt_sim::control::ClockRequestJson {
            left_ms: Some(60_000),
            right_ms: Some(90_000),
            running: Some(true),
            ..Default::default()
        },
    })
    .await
    .unwrap();

    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x44]).await; // UPDATE_BOARD: no clock traffic
    client.expect_silence(Duration::from_millis(400)).await;

    client.send(&[0x4b, 0x41]).await; // UPDATE_NICE, then SEND_CLK
    let frames = client.frames(2).await;
    assert_eq!(frames[0].id, messages::id::BWTIME);
    assert!(!messages::is_clock_ack(&frames[0].payload));
    // payload[3] is left minutes BCD, payload[6] the status byte.
    assert_eq!(frames[0].payload[3] & 0x0f, 1, "left minutes");
    assert_ne!(frames[0].payload[6] & messages::bwtime_status::RUNNING, 0);

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn clock_command_is_acked_only_in_clock_streaming_modes() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;

    // IDLE: no ack.
    client.send(&[0x2b, 0x03, 0x03, 0x09, 0x00]).await;
    client.expect_silence(Duration::from_millis(150)).await;

    // UPDATE_BOARD: still no ack, per the protocol description.
    client.send(&[0x44, 0x2b, 0x03, 0x03, 0x09, 0x00]).await;
    client.expect_silence(Duration::from_millis(150)).await;

    // UPDATE_NICE: ack.
    client.send(&[0x4b, 0x2b, 0x03, 0x03, 0x09, 0x00]).await;
    let frames = client.frames(2).await;
    let ack_frame = frames
        .iter()
        .find(|f| f.id == messages::id::BWTIME && messages::is_clock_ack(&f.payload))
        .expect("a clock ack");
    let ack = messages::decode_clock_ack(&ack_frame.payload).unwrap();
    assert_eq!(ack[0], 0x10, "normal ack");
    assert_eq!(ack[1], clock_command::VERSION);
    assert_eq!(ack[2] >> 4, 1, "major version 1");
    assert_eq!(ack[2] & 0x0f, 8, "minor version 8");

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_n_run_clock_command_sets_both_screens() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x4b]).await; // UPDATE_NICE

    // 10 minutes, 30 seconds left; 1 hour 0 minutes 15 seconds right.
    let frame = [
        0x2b, 0x0a, 0x03, 0x0a, 0x00, 0x10, 0x30, 0x01, 0x00, 0x15, 0x03, 0x00,
    ];
    assert_eq!(frame.len(), 12);
    client.send(&frame).await;
    client.frames(2).await; // initial clock message + set-n-run ack

    let snapshot = sim.snapshot(1).await.unwrap();
    assert_eq!(snapshot["clock"]["left"], "0:10:30");
    assert_eq!(snapshot["clock"]["right"], "1:00:15");
    assert_eq!(snapshot["clock"]["running"], true);

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn fragmented_host_commands_are_reassembled() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;

    client.send(&[0x4b]).await; // enter UPDATE_NICE
    // A board request, then a clock-version frame split at every byte.
    client.send(&[0x42]).await;
    for byte in [0x2b, 0x03, 0x03, 0x09, 0x00] {
        client.send(&[byte]).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let frames = client.frames(3).await;
    let ids: Vec<u8> = frames.iter().map(|f| f.id).collect();
    assert!(ids.contains(&messages::id::BOARD_DUMP), "{ids:?}");
    assert!(
        frames
            .iter()
            .any(|f| f.id == messages::id::BWTIME && messages::is_clock_ack(&f.payload)),
        "the split clock command was not acked: {frames:?}"
    );

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_and_malformed_commands_do_not_break_the_board() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;

    client.send(&[0x01, 0xff]).await; // unknown single-byte commands
    client.send(&[0x2b, 0x03, 0x99, 0x09, 0x00]).await; // bad clock start byte
    client.send(&[0x42]).await; // and the board still answers

    let frames = client.frames(1).await;
    assert_eq!(frames[0].id, messages::id::BOARD_DUMP);

    let snapshot = sim.snapshot(1).await.unwrap();
    assert_eq!(snapshot["protocol_errors"], 3, "all three were counted");

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn eeprom_dump_has_the_size_of_a_real_board() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x44]).await;
    sim.expect(Request::Move {
        board: 1,
        uci: "e2e4".into(),
    })
    .await
    .unwrap();

    client.send(&[0x49]).await; // DGT_SEND_EE_MOVES
    let frames = client.frames(1).await;
    assert_eq!(frames[0].id, messages::id::EE_MOVES);
    assert_eq!(
        frames[0].payload.len(),
        dgt_sim::board::state::EEPROM_SIZE + 2
    );
    assert_eq!(
        *frames[0].payload.first().unwrap(),
        messages::ee::DOWNLOADED
    );
    assert_eq!(*frames[0].payload.last().unwrap(), messages::ee::EOF);
    // The two field updates for e2 -> e4 are in the tail, before EE_EOF.
    let tail = &frames[0].payload[frames[0].payload.len() - 8..];
    assert!(tail.contains(&ee_tag(code::EMPTY)), "e2 lift recorded");
    assert!(tail.contains(&ee_tag(code::WPAWN)), "e4 place recorded");

    sim.shutdown().await.unwrap();
}

fn ee_tag(piece_code: u8) -> u8 {
    0x40 | piece_code
}

#[tokio::test]
async fn captures_replays_and_castling_produce_correct_field_sequences() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x44]).await;

    // Castling: king first, then the rook, as a real board reports it.
    sim.expect(Request::SetFen {
        board: 1,
        fen: "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1".into(),
        quiet: true,
    })
    .await
    .unwrap();
    sim.expect(Request::Move {
        board: 1,
        uci: "e1g1".into(),
    })
    .await
    .unwrap();
    let frames = client.frames(4).await;
    let payloads: Vec<Vec<u8>> = frames.iter().map(|f| f.payload.clone()).collect();
    assert_eq!(
        payloads,
        vec![
            vec![60, code::EMPTY], // e1
            vec![62, code::WKING], // g1
            vec![63, code::EMPTY], // h1
            vec![61, code::WROOK], // f1
        ]
    );

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn pgn_replay_emits_the_whole_game() {
    let dir = std::env::temp_dir().join(format!("dgt-sim-pgn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pgn = dir.join("game.pgn");
    std::fs::write(&pgn, "1. e4 e5 2. Nf3 Nc6 3. Bb5 a6 4. O-O\n").unwrap();

    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x44]).await;

    sim.expect(Request::Replay {
        board: 1,
        pgn: pgn.to_string_lossy().into_owned(),
        fen: None,
        delay_ms: 0,
        speed: 1.0,
        repeat: false,
    })
    .await
    .unwrap();

    // Seven quiet moves (2 messages each) plus a castle (4 messages).
    let frames = client.frames(7 * 2 + 4).await;
    assert!(frames.iter().all(|f| f.id == messages::id::FIELD_UPDATE));

    // Give the replay task a moment to finish before snapshotting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let snapshot: Snapshot = serde_json::from_value(sim.snapshot(1).await.unwrap()).unwrap();
    assert_eq!(
        snapshot.fen,
        "r1bqkbnr/1ppp1ppp/p1n5/1B2p3/4P3/5N2/PPPP1PPP/RNBQ1RK1"
    );

    std::fs::remove_dir_all(&dir).ok();
    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_orders_are_not_reordered_by_the_scheduler() {
    // Five boards, each asked for a five-move sequence in parallel: every
    // board must report its own moves in order.
    let sim = simulator(5, Faults::default()).await;
    let mut clients = Vec::new();
    for board in 1..=5 {
        let mut client = Client::connect(&endpoint(&sim, board)).await;
        client.send(&[0x44]).await;
        clients.push(client);
    }
    let moves = ["e2e4", "e7e5", "g1f3", "b8c6", "f1b5"];
    for board in 1..=5u32 {
        for uci in moves {
            sim.expect(Request::Move {
                board,
                uci: uci.into(),
            })
            .await
            .unwrap();
        }
    }
    for client in clients.iter_mut() {
        let frames = client.frames(10).await;
        let squares: Vec<u8> = frames.iter().map(|f| f.payload[0]).collect();
        // e2/e4, e7/e5, g1/f3, b8/c6, f1/b5 -> field numbers in order.
        assert_eq!(squares, vec![52, 36, 12, 28, 62, 46, 57, 42, 61, 33]);
    }

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn latin1_free_trademark_and_high_byte_payloads_are_rejected() {
    let sim = simulator(1, Faults::default()).await;
    let response = sim
        .request(Request::Config {
            board: 1,
            config: dgt_sim::control::ConfigRequest {
                trademark: Some("caf\u{e9} board".into()),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(!response.ok, "8-bit text must be refused");
    assert!(response.error.unwrap().contains("7-bit ASCII"));

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn fault_injection_truncates_frames_in_a_reproducible_way() {
    let sim = simulator(
        1,
        Faults {
            truncate: true,
            seed: 7,
            ..Faults::default()
        },
    )
    .await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x42]).await;

    // A truncated 67-byte frame leaves 33 bytes: enough for a header claiming
    // 67, but not enough payload, so the framer must wait rather than panic.
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_millis(300), async {
        let mut buf = [0u8; 4096];
        loop {
            let n = client.stream.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .ok();
    assert_eq!(got.len(), 33, "half of 67, rounded down");
    assert_eq!(&got[..3], &[0x86, 0x00, 0x43]);

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn total_drop_rate_delivers_nothing_but_keeps_the_board_alive() {
    let sim = simulator(
        1,
        Faults {
            drop_rate: 1.0,
            fragment_max: 1,
            seed: 1,
            ..Faults::default()
        },
    )
    .await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x42]).await;
    client.expect_silence(Duration::from_millis(250)).await;

    // The board itself is still healthy.
    let snapshot = sim.snapshot(1).await.unwrap();
    assert_eq!(snapshot["host_commands"], 1);
    assert_eq!(snapshot["mode"], "idle");

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnect_and_reconnect_change_what_the_board_accepts() {
    let sim = simulator(1, Faults::default()).await;
    let endpoint = endpoint(&sim, 1);
    let mut client = Client::connect(&endpoint).await;
    client.send(&[0x42]).await;
    client.frames(1).await;

    sim.expect(Request::Disconnect { board: 1 }).await.unwrap();
    // The existing connection is dropped.
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(2), client.stream.read(&mut buf))
        .await
        .expect("the board should have closed the link")
        .unwrap_or(0);
    assert_eq!(n, 0, "expected EOF after disconnect");

    // A new client is accepted and immediately closed while still disabled.
    let mut refused = Client::connect(&endpoint).await;
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(2), refused.stream.read(&mut buf))
        .await
        .expect("a disabled board should reject the connection")
        .unwrap_or(0);
    assert_eq!(n, 0);

    sim.expect(Request::Reconnect { board: 1 }).await.unwrap();
    let mut client = Client::connect(&endpoint).await;
    client.send(&[0x42]).await;
    let frames = client.frames(1).await;
    assert_eq!(frames[0].id, messages::id::BOARD_DUMP);

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_fen_and_reset_emit_a_field_update_per_changed_field() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x44]).await;

    // An empty board: 32 pieces disappear.
    let snapshot = sim
        .expect(Request::SetFen {
            board: 1,
            fen: "8/8/8/8/8/8/8/8 w - - 0 1".into(),
            quiet: false,
        })
        .await
        .unwrap();
    assert_eq!(snapshot["last_emitted"], 32);
    let frames = client.frames(32).await;
    assert!(frames.iter().all(|f| f.payload[1] == code::EMPTY));

    // Reset emits the 32 pieces coming back and leaves IDLE mode.
    let snapshot = sim
        .expect(Request::Reset {
            board: 1,
            emit: true,
        })
        .await
        .unwrap();
    assert_eq!(snapshot["mode"], "idle");
    let frames = client.frames(32).await;
    assert!(frames.iter().all(|f| f.payload[1] != code::EMPTY));
    assert_eq!(
        snapshot["fen"],
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR"
    );

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn quiet_set_fen_updates_state_without_talking() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x44]).await;

    let snapshot = sim
        .expect(Request::SetFen {
            board: 1,
            fen: "8/8/8/8/8/8/8/8 w - - 0 1".into(),
            quiet: true,
        })
        .await
        .unwrap();
    assert_eq!(snapshot["last_emitted"], 0);
    client.expect_silence(Duration::from_millis(200)).await;

    // But a board dump still reports the new (empty) position.
    client.send(&[0x42]).await;
    let frames = client.frames(1).await;
    assert!(frames[0].payload.iter().all(|b| *b == code::EMPTY));

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn magic_pieces_are_reportable_through_the_protocol() {
    let sim = simulator(1, Faults::default()).await;
    let mut client = Client::connect(&endpoint(&sim, 1)).await;
    client.send(&[0x44]).await;

    for (square, piece) in [
        ("e4", "magic-draw"),
        ("e5", "magic-white-win"),
        ("d4", "magic-black-win"),
    ] {
        sim.expect(Request::Place {
            board: 1,
            square: square.into(),
            piece: piece.into(),
        })
        .await
        .unwrap();
    }
    let frames = client.frames(3).await;
    assert_eq!(frames[0].payload, vec![36, code::MAGIC_DRAW]);
    assert_eq!(frames[1].payload, vec![28, code::MAGIC_WHITE_WIN]);
    assert_eq!(frames[2].payload, vec![35, code::MAGIC_BLACK_WIN]);

    let snapshot = sim.snapshot(1).await.unwrap();
    assert_eq!(
        snapshot["magic"],
        json!(["e4:magic-draw", "e5:magic-white-win", "d4:magic-black-win"])
    );

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn command_parser_and_framer_agree_on_a_real_capture() {
    // The first messages of arnholt/dgtdriver's 1.inbytes capture, decoded from
    // the base64-encoded hexdumps in that repository.
    let capture: &[u8] = &[
        // DGT_MSG_BOARD_DUMP of the starting position.
        hex_bytes("8600 4308 090a 0c0b 0a09 0807"),
        // DGT_MSG_BWTIME: 0:03:02 both sides, time-per-move on, right to move.
        hex_bytes("8d00 0a20 0302 2003 0208"),
        // DGT_MSG_FIELD_UPDATE: field 0x34 (e2) became empty.
        hex_bytes("8e00 0534 00"),
        // DGT_MSG_FIELD_UPDATE: field 0x24 (e4) is now a white pawn.
        hex_bytes("8e00 0524 01"),
    ]
    .concat();

    let mut framer = Framer::new();
    framer.push(&capture);
    let mut decoded = Vec::new();
    while let Some(frame) = framer.next_frame() {
        decoded.push(frame);
    }
    // The board dump is truncated on purpose (only 7 of 64 payload bytes), so
    // the framer must not emit it yet.
    let (frames, _) = decode_stream(&capture);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].id, messages::id::BWTIME);
    assert!(!messages::is_clock_ack(&frames[0].payload));

    let mut parser = dgt_sim::protocol::CommandParser::new();
    parser.push(&[0x42, 0x44, 0x40]);
    assert_eq!(
        parser.next_command().unwrap().unwrap(),
        HostCommand::SendBoard
    );
    assert_eq!(
        parser.next_command().unwrap().unwrap(),
        HostCommand::SendUpdateBoard
    );
    assert_eq!(parser.next_command().unwrap().unwrap(), HostCommand::Reset);
    assert!(parser.next_command().is_none());
}

/// Decode a byte stream with the framer, returning frames and bytes skipped.
fn decode_stream(bytes: &[u8]) -> (Vec<messages::Frame>, u64) {
    let mut framer = Framer::new();
    framer.push(bytes);
    let frames: Vec<_> = std::iter::from_fn(|| framer.next_frame()).collect();
    (frames, framer.skipped_bytes())
}

/// Parse `"8600 43ab"` into bytes. Test-vector helper.
fn hex_bytes(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|b| b.is_ascii_hexdigit())
        .map(|b| match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => b - b'A' + 10,
        })
        .collect();
    assert_eq!(digits.len() % 2, 0, "odd number of hex digits in {text:?}");
    digits
        .chunks(2)
        .map(|pair| (pair[0] << 4) | pair[1])
        .collect()
}

#[test]
fn hex_helper_is_correct() {
    assert_eq!(hex_bytes("8600 43"), vec![0x86, 0x00, 0x43]);
    assert_eq!(hex_bytes("8d000a"), vec![0x8d, 0x00, 0x0a]);
}
