//! Benchmarks for board scaling: 10, 100 and 1000 boards.
//!
//! Run with `cargo run --release --bench boards` (or `cargo bench`). There is
//! deliberately no `criterion` dependency: the numbers that matter here are
//! memory per board, time to create N boards, and protocol throughput, all of
//! which `std::time::Instant` measures perfectly well.
//!
//! The benchmark is honest about its own limits: what it can measure is
//! *creation cost*, *resident memory* and *per-message round-trip cost*. It
//! cannot measure whether the host kernel can keep 1000 concurrent PTY pairs
//! alive on your machine — the file-descriptor check below reports the
//! `RLIMIT_NOFILE` you actually have so you can see the real ceiling.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use dgt_sim::board::PhysicalStyle;
use dgt_sim::control::{Job, Request};
use dgt_sim::manager::{Manager, StartOptions};
use dgt_sim::protocol::commands::HostCommand;
use dgt_sim::protocol::messages::{self, Framer};
use dgt_sim::protocol::{CommandParser, pieces};
use dgt_sim::transport::TransportKind;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

const SCALES: [u32; 3] = [10, 100, 1000];

fn rlimit_nofile() -> Option<u64> {
    // /proc/self/limits is the cheapest way to read this without libc.
    let text = std::fs::read_to_string("/proc/self/limits").ok()?;
    for line in text.lines() {
        if line.starts_with("Max open files") {
            return line.split_whitespace().nth(3)?.parse().ok();
        }
    }
    None
}

fn resident_kib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Time to create `n` TCP boards and answer one `DGT_SEND_BRD` on each.
async fn bench_tcp(n: u32) -> (Duration, Duration) {
    let opts = StartOptions {
        boards: n,
        transport: TransportKind::Tcp,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        physical_style: PhysicalStyle::Direct,
        faults: Default::default(),
    };
    let (job_tx, job_rx) = mpsc::channel::<Job>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let manager = Manager::new(opts, "/tmp/dgt-sim-bench.sock".into());
    let task = tokio::spawn(manager.run(job_rx, ready_tx));

    let created_at = Instant::now();
    let summaries = ready_rx.await.expect("boards created");
    let creation = created_at.elapsed();

    // One request per board, sequentially, so the number is a per-request cost
    // rather than a measure of the scheduler.
    let query_at = Instant::now();
    for summary in &summaries {
        let addr = summary.endpoint.trim_start_matches("tcp://");
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(&[0x42]).await.expect("write");
        let mut framer = Framer::new();
        let mut buf = [0u8; 1024];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let n = stream.read(&mut buf).await.expect("read");
            assert!(n > 0, "connection closed early");
            framer.push(&buf[..n]);
            if let Some(frame) = framer.next_frame() {
                assert_eq!(frame.id, messages::id::BOARD_DUMP);
                assert_eq!(frame.payload.len(), 64);
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for a board dump");
        }
    }
    let per_board = query_at.elapsed();

    let _ = job_tx.send((Request::Shutdown, oneshot::channel().0)).await;
    let _ = task.await;
    (creation, per_board)
}

/// Pure protocol cost: encode a board dump, frame it, and parse it back.
fn bench_protocol(iterations: u32) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        let dump = messages::board_dump(&pieces::START_SQUARES);
        let mut framer = Framer::new();
        framer.push(&dump);
        let frame = framer.next_frame().expect("one frame");
        assert_eq!(frame.payload.len(), 64);
    }
    start.elapsed()
}

/// Parse cost for a command stream with fragmented writes.
fn bench_command_parsing(iterations: u32) -> Duration {
    let clock = [0x2b, 0x0b, 0x03, 0x01, 1, 2, 3, 4, 5, 6, 7, 0x03, 0x00];
    let start = Instant::now();
    for _ in 0..iterations {
        let mut parser = CommandParser::new();
        for chunk in clock.chunks(3) {
            parser.push(chunk);
            while let Some(cmd) = parser.next_command() {
                assert!(matches!(cmd.unwrap(), HostCommand::Clock { .. }));
            }
        }
    }
    start.elapsed()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("dgt-sim benchmarks");
    match rlimit_nofile() {
        Some(limit) => println!("RLIMIT_NOFILE: {limit} (a PTY board costs 2 fds)"),
        None => println!("RLIMIT_NOFILE: unknown"),
    }
    if let Some(rss) = resident_kib() {
        println!("baseline RSS: {rss} KiB");
    }

    println!("\n-- protocol (no I/O) --");
    let iterations = 200_000;
    let encode = bench_protocol(iterations);
    println!(
        "board dump encode+frame+parse: {:>8.2} ns/op  ({iterations} iterations)",
        encode.as_nanos() as f64 / f64::from(iterations)
    );
    let parse = bench_command_parsing(iterations / 10);
    println!(
        "fragmented clock command parse: {:>7.2} ns/op  ({} iterations)",
        parse.as_nanos() as f64 / f64::from(iterations / 10),
        iterations / 10
    );

    println!("\n-- boards --");
    println!(
        "{:>6}  {:>14}  {:>16}  {:>16}  {:>12}",
        "BOARDS", "CREATE (ms)", "PER-BOARD (us)", "DUMP RT (us)", "RSS (KiB)"
    );
    for n in SCALES {
        let (creation, queries) = bench_tcp(n).await;
        let rss = resident_kib().unwrap_or(0);
        println!(
            "{n:>6}  {:>14.2}  {:>16.2}  {:>16.2}  {:>12}",
            creation.as_secs_f64() * 1000.0,
            creation.as_secs_f64() * 1e6 / f64::from(n),
            queries.as_secs_f64() * 1e6 / f64::from(n),
            rss
        );
    }

    println!(
        "\nThe board-dump round trip includes a fresh TCP connection per board,\n\
         so it is an upper bound dominated by connect() rather than by the emulator."
    );
}
