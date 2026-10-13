//! Integration tests: the control plane, the CLI binary and the PTY transport.
//!
//! These exercise the parts a user actually touches, and they exercise them
//! the way a user does: by running the real binary and talking to the real
//! socket, not by calling into internals.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dgt_sim::board::{BoardTask, PhysicalStyle};
use dgt_sim::control::{self, Request};
use dgt_sim::manager::{RunningSimulator, StartOptions};
use dgt_sim::transport::{TransportKind, pty::PtyLink};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dgt-sim-it-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn tcp_options(boards: u32) -> StartOptions {
    StartOptions {
        boards,
        transport: TransportKind::Tcp,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        physical_style: PhysicalStyle::Direct,
        faults: Default::default(),
    }
}

// ---------------------------------------------------------------- control ---

#[tokio::test]
async fn control_socket_serves_a_real_manager() {
    let dir = temp_dir("ctl");
    let socket = dir.join("dgt.sock");

    let (jobs_tx, mut jobs_rx) = mpsc::channel::<control::Job>(16);
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let server = {
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { control::serve(&socket, jobs_tx, shutdown).await })
    };
    let sim = RunningSimulator::start(tcp_options(2), &socket)
        .await
        .unwrap();

    // Bridge the socket's requests into the manager, exactly as `start` does.
    let bridge = tokio::spawn(async move {
        while let Some((request, reply)) = jobs_rx.recv().await {
            let response = sim
                .request(request)
                .await
                .unwrap_or_else(|err| control::Response::failed(err.to_string()));
            let _ = reply.send(response);
        }
    });

    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let info = control::call(&socket, &Request::Info).await.unwrap();
    assert!(info.ok);
    assert_eq!(info.data.unwrap()["boards"], 2);

    let listed = control::call(&socket, &Request::List).await.unwrap();
    let rows = listed.data.unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["id"], 1);
    assert!(rows[0]["endpoint"].as_str().unwrap().starts_with("tcp://"));

    // A missing board is a clean error, not a panic.
    let missing = control::call(&socket, &Request::Show { board: 99 })
        .await
        .unwrap();
    assert!(!missing.ok);
    assert!(missing.error.unwrap().contains("99"));

    // Two requests over one connection work (the CLI uses one, tests use two).
    let show = control::call(&socket, &Request::Show { board: 1 })
        .await
        .unwrap();
    let fen = show.data.unwrap()["fen"].as_str().unwrap().to_string();
    assert_eq!(fen, "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR");

    let move_response = control::call(
        &socket,
        &Request::Move {
            board: 1,
            uci: "e2e4".into(),
        },
    )
    .await
    .unwrap();
    assert!(move_response.ok);
    let after = move_response.data.unwrap();
    assert_eq!(after["last_emitted"], 0, "idle board streams nothing");

    // Garbage on the wire is answered, not fatal.
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stream.write_all(b"this is not json\n").await.unwrap();
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf[..n]);
    assert!(text.contains("malformed request"), "{text}");

    control::call(&socket, &Request::Shutdown).await.unwrap();
    // Cancel the server *before* joining the bridge: the server owns the last
    // sender on the job channel, so waiting for the bridge first deadlocks.
    shutdown.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the control server did not stop")
        .expect("no panic");
    bridge.await.unwrap();
    assert!(!socket.exists(), "the socket must be removed on shutdown");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn control_socket_permissions_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("perm");
    let socket = dir.join("dgt.sock");
    let (tx, _rx) = mpsc::channel::<control::Job>(1);
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let server = {
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { control::serve(&socket, tx, shutdown).await })
    };
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    shutdown.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await.expect("server stop");
    std::fs::remove_dir_all(&dir).ok();
}

// -------------------------------------------------------------------- pty ---

#[tokio::test]
async fn pty_board_serves_a_serial_client() {
    let sim = RunningSimulator::start(
        StartOptions {
            boards: 1,
            transport: TransportKind::Pty,
            ..tcp_options(1)
        },
        "/tmp/dgt-sim-it-pty.sock",
    )
    .await
    .unwrap();
    let endpoint = sim.endpoint(1).unwrap().to_string();
    assert!(endpoint.starts_with("/dev/pts/"), "{endpoint}");
    assert!(
        Path::new(&endpoint).exists(),
        "the pts device must exist: {endpoint}"
    );

    // A client opens the slave exactly as a serial program would.
    let mut client = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&endpoint)
        .await
        .expect("open the pts");

    client.write_all(&[0x42]).await.unwrap(); // SEND_BRD
    let mut buf = vec![0u8; 67];
    tokio::time::timeout(Duration::from_secs(3), client.read_exact(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed");
    assert_eq!(&buf[..3], &[0x86, 0x00, 0x43]);
    assert_eq!(buf[3], 0x08, "a8 holds a black rook");

    // Update mode works over the pty too.
    client.write_all(&[0x44]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    sim.expect(Request::Move {
        board: 1,
        uci: "e2e4".into(),
    })
    .await
    .unwrap();
    let mut buf = vec![0u8; 10];
    tokio::time::timeout(Duration::from_secs(3), client.read_exact(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed");
    assert_eq!(&buf[..5], &[0x8e, 0x00, 0x05, 52, 0x00], "e2 emptied");
    assert_eq!(&buf[5..], &[0x8e, 0x00, 0x05, 36, 0x01], "e4 filled");

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn pty_endpoints_are_independent() {
    let sim = RunningSimulator::start(
        StartOptions {
            boards: 3,
            transport: TransportKind::Pty,
            ..tcp_options(3)
        },
        "/tmp/dgt-sim-it-pty-multi.sock",
    )
    .await
    .unwrap();
    let paths: Vec<String> = (1..=3)
        .map(|b| sim.endpoint(b).unwrap().to_string())
        .collect();
    let unique: std::collections::BTreeSet<_> = paths.iter().collect();
    assert_eq!(unique.len(), 3, "each board needs its own pty: {paths:?}");

    // A move on board 2 must not appear on board 3.
    let mut clients = Vec::new();
    for path in &paths {
        let mut client = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await
            .unwrap();
        client.write_all(&[0x44]).await.unwrap();
        clients.push(client);
    }
    sim.expect(Request::Move {
        board: 2,
        uci: "e2e4".into(),
    })
    .await
    .unwrap();

    let mut buf = vec![0u8; 10];
    tokio::time::timeout(Duration::from_secs(3), clients[1].read_exact(&mut buf))
        .await
        .expect("board 2 should have produced the move")
        .expect("read failed");

    let mut quiet = vec![0u8; 1];
    let leaked =
        tokio::time::timeout(Duration::from_millis(300), clients[2].read(&mut quiet)).await;
    assert!(leaked.is_err(), "board 3 must stay silent, got {leaked:?}");

    sim.shutdown().await.unwrap();
}

#[tokio::test]
async fn pty_link_reports_and_uses_its_path() {
    let mut link = PtyLink::open().unwrap();
    let path = link.path().to_path_buf();
    assert!(path.exists());
    link.write_all(&[0xAB]).await.unwrap();
    // Writing must not fail even though nobody is reading yet: the pty buffers.
    let mut buf = [0u8; 4];
    let idle = tokio::time::timeout(Duration::from_millis(100), link.read(&mut buf)).await;
    assert!(idle.is_err(), "nothing should be readable yet");
}

// -------------------------------------------------------------------- cli ---

/// Run the real binary and capture stdout, stderr and the exit code.
async fn cli(args: &[&str]) -> (i32, String, String) {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_dgt-sim"))
        .args(args)
        .output()
        .await
        .expect("run dgt-sim");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn cli_end_to_end_over_the_control_socket() {
    let dir = temp_dir("cli");
    let socket = dir.join("dgt.sock");
    let socket_arg = socket.to_string_lossy().into_owned();

    // The daemon must be reachable before any client command works.
    let (code, _, stderr) = cli(&["--socket", &socket_arg, "list"]).await;
    assert_eq!(code, 1, "no daemon yet");
    assert!(stderr.contains("not running"), "{stderr}");

    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_dgt-sim"))
        .args([
            "--socket",
            &socket_arg,
            "start",
            "--boards",
            "2",
            "--transport",
            "tcp",
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the daemon");

    // The endpoints are announced on stdout as one JSON line.
    let stdout = daemon.stdout.take().expect("piped stdout");
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let first = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("daemon did not report its endpoints")
        .unwrap()
        .expect("stdout closed");
    let summaries: Value = serde_json::from_str(&first).expect("valid json");
    assert_eq!(summaries.as_array().unwrap().len(), 2);
    assert!(
        summaries[0]["endpoint"]
            .as_str()
            .unwrap()
            .starts_with("tcp://")
    );

    // info
    let (code, out, _) = cli(&["--socket", &socket_arg, "info", "--json"]).await;
    assert_eq!(code, 0);
    let info: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(info["boards"], 2);

    // list
    let (code, out, _) = cli(&["--socket", &socket_arg, "list", "--json"]).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        serde_json::from_str::<Value>(out.trim())
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // list (human)
    let (code, out, _) = cli(&["--socket", &socket_arg, "list"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("ENDPOINT"), "{out}");

    // show
    let (code, out, _) = cli(&["--socket", &socket_arg, "show", "--board", "1", "--json"]).await;
    assert_eq!(code, 0);
    let shown: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(shown["version"], "1.8");
    assert_eq!(shown["mode"], "idle");
    assert_eq!(shown["physical_style"], "direct");

    // show --ascii
    let (code, out, _) = cli(&["--socket", &socket_arg, "show", "--board", "1", "--ascii"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("a  b  c  d  e  f  g  h"), "{out}");
    assert!(out.contains("rnbqkbnr"), "{out}");

    // a move with no update mode: state changes, nothing streams
    let (code, out, err) = cli(&[
        "--socket",
        &socket_arg,
        "move",
        "--board",
        "1",
        "--uci",
        "e2e4",
    ])
    .await;
    assert_eq!(code, 0);
    assert!(out.contains("emitted=0"), "{out}");
    assert!(err.contains("no protocol messages"), "{err}");

    // set-mode, then the move streams
    let (code, _, _) = cli(&[
        "--socket",
        &socket_arg,
        "set-mode",
        "--board",
        "2",
        "--mode",
        "update-board",
    ])
    .await;
    assert_eq!(code, 0);
    let (code, out, _) = cli(&[
        "--socket",
        &socket_arg,
        "move",
        "--board",
        "2",
        "--uci",
        "d2d4",
    ])
    .await;
    assert_eq!(code, 0);
    assert!(out.contains("emitted=2"), "{out}");

    // messages: the hex log proves real DGT bytes were produced
    let (code, out, _) = cli(&[
        "--socket",
        &socket_arg,
        "messages",
        "--board",
        "2",
        "--json",
    ])
    .await;
    assert_eq!(code, 0);
    let log: Value = serde_json::from_str(out.trim()).unwrap();
    let hexes: Vec<String> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["hex"].as_str().unwrap().to_string())
        .collect();
    assert!(
        hexes.iter().any(|h| h == "8e 00 05 33 00"),
        "d2 (field 51) lift missing: {hexes:?}"
    );
    assert!(
        hexes.iter().any(|h| h == "8e 00 05 23 01"),
        "d4 (field 35) place missing: {hexes:?}"
    );

    // lift / place / clear
    for args in [
        vec!["lift", "--board", "1", "--square", "e2"],
        vec![
            "place",
            "--board",
            "1",
            "--square",
            "e4",
            "--piece",
            "white-pawn",
        ],
        vec!["clear", "--board", "1", "--square", "e4"],
    ] {
        let mut full = vec!["--socket", socket_arg.as_str()];
        full.extend(args.iter().copied());
        let (code, out, err) = cli(&full).await;
        assert_eq!(code, 0, "{out}{err}");
    }

    // set-fen and reset
    let (code, out, _) = cli(&[
        "--socket",
        &socket_arg,
        "set-fen",
        "--board",
        "1",
        "--fen",
        "8/8/8/8/8/8/8/8 w - - 0 1",
    ])
    .await;
    assert_eq!(code, 0, "{out}");
    let (code, out, _) = cli(&["--socket", &socket_arg, "reset", "--board", "1"]).await;
    assert_eq!(code, 0, "{out}");

    // an invalid FEN is a clean failure
    let (code, _, err) = cli(&[
        "--socket",
        &socket_arg,
        "set-fen",
        "--board",
        "1",
        "--fen",
        "not-a-fen",
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("invalid FEN"), "{err}");

    // an unknown board is a clean failure
    let (code, _, err) = cli(&["--socket", &socket_arg, "show", "--board", "42"]).await;
    assert_eq!(code, 1);
    assert!(err.contains("42"), "{err}");

    // clock, config, fault
    let (code, out, _) = cli(&[
        "--socket",
        &socket_arg,
        "clock",
        "--board",
        "1",
        "--left",
        "5m",
        "--run",
    ])
    .await;
    assert_eq!(code, 0, "{out}");
    let (code, _, _) = cli(&[
        "--socket",
        &socket_arg,
        "config",
        "--board",
        "1",
        "--serial",
        "00999",
        "--firmware-version",
        "3.1",
        "--physical-style",
        "verbose",
    ])
    .await;
    assert_eq!(code, 0);
    let (_, out, _) = cli(&["--socket", &socket_arg, "show", "--board", "1", "--json"]).await;
    let shown: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(shown["serial"], "00999");
    assert_eq!(shown["version"], "3.1");
    assert_eq!(shown["physical_style"], "verbose");
    assert_eq!(shown["clock"]["left"], "0:05:00");
    assert_eq!(shown["clock"]["running"], true);

    let (code, _, _) = cli(&[
        "--socket",
        &socket_arg,
        "fault",
        "--board",
        "1",
        "--latency",
        "5ms",
        "--drop-rate",
        "0.1",
    ])
    .await;
    assert_eq!(code, 0);
    let (_, out, _) = cli(&["--socket", &socket_arg, "show", "--board", "1", "--json"]).await;
    let shown: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(shown["faults"]["latency_ms"], 5);

    // disconnect / reconnect / stop
    for args in [
        vec!["disconnect", "--board", "1"],
        vec!["reconnect", "--board", "1"],
        vec!["stop", "--board", "2"],
    ] {
        let mut full = vec!["--socket", socket_arg.as_str()];
        full.extend(args.iter().copied());
        let (code, out, err) = cli(&full).await;
        assert_eq!(code, 0, "{out}{err}");
    }
    let (code, out, _) = cli(&["--socket", &socket_arg, "list", "--json"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(out.trim())
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1,
        "board 2 was stopped"
    );

    // shutdown
    let (code, _, _) = cli(&["--socket", &socket_arg, "shutdown"]).await;
    assert_eq!(code, 0);
    let status = tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("the daemon did not exit")
        .expect("wait");
    assert!(status.success(), "the daemon exited with {status}");

    // SIGTERM path: start again and kill it.
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cli_start_and_stop_leave_no_socket_behind() {
    let dir = temp_dir("cli-stop");
    let socket = dir.join("dgt.sock");
    let socket_arg = socket.to_string_lossy().into_owned();

    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_dgt-sim"))
        .args([
            "--socket",
            &socket_arg,
            "start",
            "--boards",
            "1",
            "--transport",
            "pty",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    let stdout = daemon.stdout.take().unwrap();
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let header = lines.next_line().await.unwrap().unwrap();
    let row = lines.next_line().await.unwrap().unwrap();
    assert!(header.contains("ENDPOINT"), "{header}");
    let endpoint = row.split_whitespace().last().unwrap().to_string();
    assert!(endpoint.starts_with("/dev/pts/"), "{row}");

    let (code, _, _) = cli(&["--socket", &socket_arg, "shutdown"]).await;
    assert_eq!(code, 0);
    daemon.wait().await.unwrap();
    assert!(!socket.exists(), "the socket must be cleaned up");

    // Report the endpoint so a human running the test can inspect the output.
    println!("pty endpoint was {endpoint}");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cli_usage_errors_exit_with_two() {
    let (code, _, stderr) = cli(&["move", "--board", "1"]).await;
    assert_eq!(code, 2, "clap usage errors use exit code 2: {stderr}");

    let (code, out, _) = cli(&["--help"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("set-fen"), "{out}");
    assert!(out.contains("replay"), "{out}");

    let (code, out, _) = cli(&["--version"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("dgt-sim"), "{out}");
}

// ---------------------------------------------------------------- signals ---

#[tokio::test]
async fn sigterm_shuts_the_daemon_down_cleanly() {
    let dir = temp_dir("sigterm");
    let socket = dir.join("dgt.sock");
    let socket_arg = socket.to_string_lossy().into_owned();

    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_dgt-sim"))
        .args([
            "--socket",
            &socket_arg,
            "start",
            "--boards",
            "3",
            "--transport",
            "tcp",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    let stdout = daemon.stdout.take().unwrap();
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    lines.next_line().await.unwrap().unwrap();
    for _ in 0..3 {
        lines.next_line().await.unwrap().unwrap();
    }

    let (code, out, _) = cli(&["--socket", &socket_arg, "info", "--json"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(out.trim()).unwrap()["boards"],
        3
    );

    let pid = daemon.id().expect("pid") as i32;
    // SAFETY: sending a signal to a child process we spawned, using libc
    // through nix, is well-defined. No memory is touched.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("send SIGTERM");

    let status = tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("the daemon ignored SIGTERM")
        .expect("wait");
    assert!(status.success(), "clean exit expected, got {status}");
    assert!(!socket.exists(), "the socket must be removed on SIGTERM");
    std::fs::remove_dir_all(&dir).ok();
}

// --------------------------------------------------------------- board api ---

#[tokio::test]
async fn board_task_stops_when_its_command_channel_closes() {
    use dgt_sim::board::BoardCmd;
    let (cmd_tx, cmd_rx) = mpsc::channel::<BoardCmd>(4);
    let (_link_tx, link_rx) = mpsc::channel(1);
    let task = tokio::spawn(
        BoardTask::new(1, TransportKind::Tcp, "tcp://127.0.0.1:0".into()).run(cmd_rx, link_rx),
    );
    drop(cmd_tx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the board must stop when its channel closes")
        .expect("no panic");
}

#[tokio::test]
async fn snapshot_request_round_trips_through_the_channel() {
    use dgt_sim::board::{BoardCmd, BoardTask};
    let (cmd_tx, cmd_rx) = mpsc::channel::<BoardCmd>(4);
    let (_link_tx, link_rx) = mpsc::channel(1);
    let task = tokio::spawn(
        BoardTask::new(7, TransportKind::Tcp, "tcp://127.0.0.1:0".into()).run(cmd_rx, link_rx),
    );
    let (tx, rx) = oneshot::channel();
    cmd_tx.send(BoardCmd::Snapshot(tx)).await.unwrap();
    let snapshot = rx.await.unwrap();
    assert_eq!(snapshot.id, 7);
    assert_eq!(snapshot.endpoint, "tcp://127.0.0.1:0");

    cmd_tx.send(BoardCmd::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
}
