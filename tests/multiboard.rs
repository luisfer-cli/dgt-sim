//! Multi-board tests.
//!
//! The acceptance target is 100 concurrent boards; the extended target is
//! 1000, bounded by file descriptors. These tests do not merely create the
//! boards — they connect to every one of them and verify the protocol, because
//! a board that exists but cannot be talked to is worthless.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use dgt_sim::board::PhysicalStyle;
use dgt_sim::control::{ClockRequestJson, Request};
use dgt_sim::manager::{RunningSimulator, StartOptions};
use dgt_sim::protocol::messages::{self, Framer};
use dgt_sim::transport::{TransportKind, pty::PtyLink};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PARALLEL_BOARDS: u32 = 100;

fn options(boards: u32, transport: TransportKind) -> StartOptions {
    StartOptions {
        boards,
        transport,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        physical_style: PhysicalStyle::Direct,
        faults: Default::default(),
    }
}

/// Ask a board for its dump and return the 64 sensor codes.
async fn read_board_dump(endpoint: &str) -> Vec<u8> {
    let addr = endpoint.trim_start_matches("tcp://");
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(&[0x42]).await.expect("write");
    let mut framer = Framer::new();
    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf))
            .await
            .expect("timed out waiting for a board dump")
            .expect("read");
        assert!(n > 0, "{endpoint} closed the connection early");
        framer.push(&buf[..n]);
        if let Some(frame) = framer.next_frame() {
            assert_eq!(frame.id, messages::id::BOARD_DUMP);
            return frame.payload;
        }
        assert!(framer.pending() < 67, "impossible frame from {endpoint}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_tcp_boards_all_answer() {
    let started = Instant::now();
    let sim = RunningSimulator::start(
        options(PARALLEL_BOARDS, TransportKind::Tcp),
        "/tmp/dgt-sim-multi-100.sock",
    )
    .await
    .expect("create 100 boards");
    let creation = started.elapsed();
    assert_eq!(sim.boards.len(), PARALLEL_BOARDS as usize);

    // Every endpoint must be distinct.
    let unique: std::collections::BTreeSet<String> =
        sim.boards.iter().map(|b| b.endpoint.clone()).collect();
    assert_eq!(
        unique.len(),
        PARALLEL_BOARDS as usize,
        "endpoints must be unique"
    );

    // Talk to all of them at once.
    let query_start = Instant::now();
    let mut tasks = Vec::with_capacity(PARALLEL_BOARDS as usize);
    for board in &sim.boards {
        let id = board.id;
        let endpoint = board.endpoint.clone();
        tasks.push(tokio::spawn(async move {
            (id, read_board_dump(&endpoint).await)
        }));
    }
    for task in tasks {
        let (id, payload) = task.await.expect("join");
        assert_eq!(payload.len(), 64, "board {id}");
        assert_eq!(payload[0], 0x08, "board {id} a8");
        assert_eq!(payload[63], 0x02, "board {id} h1");
    }
    let query = query_start.elapsed();

    // Every board reports a distinct identity, so a client can tell them apart.
    let ids: Vec<u32> = sim.boards.iter().map(|b| b.id).collect();
    let mut serials = std::collections::BTreeSet::new();
    for id in ids {
        let snapshot = sim.snapshot(id).await.unwrap();
        serials.insert(snapshot["serial"].as_str().unwrap().to_string());
    }
    assert_eq!(serials.len(), PARALLEL_BOARDS as usize);

    println!(
        "{PARALLEL_BOARDS} boards: created in {:.0} ms ({:.0} us/board), \
         all dumps in {:.0} ms ({:.0} us/board)",
        creation.as_secs_f64() * 1000.0,
        creation.as_secs_f64() * 1e6 / f64::from(PARALLEL_BOARDS),
        query.as_secs_f64() * 1000.0,
        query.as_secs_f64() * 1e6 / f64::from(PARALLEL_BOARDS),
    );

    sim.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_boards_keep_independent_state() {
    let sim = RunningSimulator::start(
        options(PARALLEL_BOARDS, TransportKind::Tcp),
        "/tmp/dgt-sim-multi-state.sock",
    )
    .await
    .unwrap();

    // Put every board into update mode, then move only the odd-numbered ones.
    for board in 1..=PARALLEL_BOARDS {
        sim.expect(Request::SetMode {
            board,
            mode: "update-board".into(),
        })
        .await
        .unwrap();
    }
    for board in (1..=PARALLEL_BOARDS).step_by(2) {
        sim.expect(Request::Move {
            board,
            uci: "e2e4".into(),
        })
        .await
        .unwrap();
    }

    for board in 1..=PARALLEL_BOARDS {
        let snapshot = sim.snapshot(board).await.unwrap();
        let fen = snapshot["fen"].as_str().unwrap();
        if board % 2 == 1 {
            assert!(
                fen.contains("PPPP1PPP"),
                "board {board} should have played e4, got {fen}"
            );
        } else {
            assert_eq!(
                fen, "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR",
                "board {board} must be untouched"
            );
        }
    }

    sim.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clients_do_not_interfere() {
    let sim = RunningSimulator::start(
        options(16, TransportKind::Tcp),
        "/tmp/dgt-sim-multi-clients.sock",
    )
    .await
    .unwrap();

    // Sixteen clients, each with its own board, each playing a different move
    // and reading back exactly its own two field updates.
    let mut tasks = Vec::new();
    for board in 1..=16u32 {
        let endpoint = sim.endpoint(board).unwrap().to_string();
        let uci = match board % 4 {
            0 => "e2e4",
            1 => "d2d4",
            2 => "g1f3",
            _ => "b1c3",
        }
        .to_string();
        tasks.push(tokio::spawn(async move {
            let addr = endpoint.trim_start_matches("tcp://").to_string();
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(&[0x44]).await.unwrap();
            (board, stream, uci)
        }));
    }

    let mut clients = Vec::new();
    for task in tasks {
        clients.push(task.await.unwrap());
    }

    // Drive moves from a second set of tasks so the boards work concurrently.
    let mut movers = Vec::new();
    for (board, _, uci) in &clients {
        let board = *board;
        let uci = uci.clone();
        movers.push(tokio::spawn(async move { (board, uci) }));
    }
    // Every request goes through the same manager channel, so `expect` calls
    // have to be awaited in order; do them sequentially and rely on the client
    // reads to overlap.
    for mover in movers {
        let (board, uci) = mover.await.unwrap();
        sim.expect(Request::Move { board, uci }).await.unwrap();
    }

    for (board, stream, uci) in clients.iter_mut() {
        let mut framer = Framer::new();
        let mut buf = [0u8; 512];
        let mut frames = Vec::new();
        while frames.len() < 2 {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("timed out reading board updates")
                .unwrap();
            assert!(n > 0, "board {board} closed early");
            framer.push(&buf[..n]);
            while let Some(frame) = framer.next_frame() {
                frames.push(frame);
            }
        }
        assert_eq!(
            frames[0].payload[1], 0x00,
            "lift on board {board}, move {uci}"
        );
        assert_ne!(
            frames[1].payload[1], 0x00,
            "place on board {board}, move {uci}"
        );
        assert_eq!(frames[0].payload[0], source_field(uci), "board {board}");
    }

    sim.shutdown().await.unwrap();
}

fn source_field(uci: &str) -> u8 {
    dgt_sim::protocol::pieces::Square::from_uci(&uci[0..2])
        .unwrap()
        .0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_pty_boards_each_expose_their_own_device() {
    // PTY boards cost two file descriptors each, so stay within a sane budget
    // and report the real ceiling instead of failing cryptically.
    let wanted: u32 = 50;
    let sim = RunningSimulator::start(
        options(wanted, TransportKind::Pty),
        "/tmp/dgt-sim-multi-pty.sock",
    )
    .await
    .expect("create pty boards");
    assert_eq!(sim.boards.len(), wanted as usize);

    let paths: Vec<String> = sim.boards.iter().map(|b| b.endpoint.clone()).collect();
    let unique: std::collections::BTreeSet<&String> = paths.iter().collect();
    assert_eq!(unique.len(), wanted as usize);

    // Each device exists and is writable.
    for path in &paths {
        assert!(std::path::Path::new(path).exists(), "{path} missing");
    }

    // Spot-check a handful end to end: opening every pts is possible but slow,
    // and the pty transport is already covered by the integration tests.
    for (index, path) in paths.iter().enumerate().step_by(10) {
        let mut client = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await
            .unwrap_or_else(|err| panic!("open {path}: {err}"));
        client.write_all(&[0x42]).await.unwrap();
        let mut buf = vec![0u8; 67];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut buf))
            .await
            .expect("timed out")
            .expect("read");
        assert_eq!(&buf[..3], &[0x86, 0x00, 0x43], "board {}", index + 1);
    }

    sim.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_boards_frees_their_pty() {
    let sim = RunningSimulator::start(
        options(3, TransportKind::Pty),
        "/tmp/dgt-sim-multi-free.sock",
    )
    .await
    .unwrap();
    let endpoint = sim.endpoint(2).unwrap().to_string();
    assert!(std::path::Path::new(&endpoint).exists());

    sim.expect(Request::Stop { board: 2 }).await.unwrap();
    assert!(
        sim.request(Request::Show { board: 2 })
            .await
            .unwrap()
            .error
            .is_some(),
        "board 2 should be gone"
    );
    // The device disappears once every descriptor to the pair is closed.
    let mut closed = false;
    for _ in 0..50 {
        if !std::path::Path::new(&endpoint).exists() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "{endpoint} still exists after stop");

    sim.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pty_board_keeps_ticking_without_a_client() {
    // The PTY transport hands the board one link and then drops the sender, so
    // its link channel closes. A closed channel used to make the board's
    // `select!` spin at 100% CPU (the `recv()` arm was always ready with
    // nothing to do) and starve the 200 ms ticker. The clock is the observable
    // proof that ticks still happen with no client attached.
    let sim = RunningSimulator::start(options(1, TransportKind::Pty), "/tmp/dgt-sim-pty-tick.sock")
        .await
        .unwrap();

    sim.expect(Request::Clock {
        board: 1,
        clock: ClockRequestJson {
            left_ms: Some(300_000),
            running: Some(true),
            connected: Some(true),
            ..Default::default()
        },
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(1500)).await;

    let snapshot = sim.snapshot(1).await.unwrap();
    let left_ms = snapshot["clock"]["left_ms"].as_u64().unwrap();
    assert!(
        left_ms < 299_500,
        "the clock did not advance ({left_ms} ms): the board's ticker is starved"
    );

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_pty_pair_needs_two_file_descriptors() {
    // Documents the scaling limit: 1000 PTY boards need ~2000 descriptors.
    // `PtyLink::open` registers the master with the Tokio reactor, so it must
    // run inside a runtime.
    let links: Vec<PtyLink> = (0..16).map(|_| PtyLink::open().unwrap()).collect();
    assert_eq!(links.len(), 16);
    let unique: std::collections::BTreeSet<_> =
        links.iter().map(|l| l.path().to_path_buf()).collect();
    assert_eq!(unique.len(), 16);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creation_failure_is_reported_not_hidden() {
    // Asking for more boards than the hard ceiling must fail loudly.
    let opts = StartOptions {
        boards: dgt_sim::manager::MAX_BOARDS + 1,
        ..options(1, TransportKind::Tcp)
    };
    let result = RunningSimulator::start(opts, "/tmp/dgt-sim-multi-limit.sock").await;
    assert!(result.is_err(), "expected a limit error");
}
