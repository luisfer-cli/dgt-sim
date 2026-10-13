# dgt-sim

A single-binary, CLI-only emulator of **DGT electronic chess boards**. It speaks
the real binary DGT serial protocol to its clients over a pseudoterminal or a
TCP socket, so software written for physical DGT hardware can be developed,
tested and load-tested without the hardware.

It is **not** a mock: external applications that open a serial port (or connect
to a socket) and send real `DGT_SEND_BRD`, `DGT_SEND_UPDATE_BRD`, clock and
identification commands get real, correctly framed binary DGT messages back.

```
$ dgt-sim start --boards 100
  ID  TR     ENDPOINT
   1  pty    /dev/pts/3
   2  pty    /dev/pts/4
   ...
control socket: /run/user/1000/dgt-sim.sock   (stop with `dgt-sim shutdown`)
```

## What it does

- Emulates an arbitrary number of independent virtual boards (target: 100, tested
  up to 1000 with TCP).
- Provides a **PTY** (`/dev/pts/N`) or **TCP** endpoint per board.
- Implements the board→PC message framing, the PC→board command set, the 64-field
  sensor model, piece codes, the on-board EEPROM ring buffer and the DGT clock
  messages (`DGT_MSG_BWTIME`, clock commands and ACKs).
- Models **physical** events, not chess moves: lift / place / clear individual
  fields, and turn a UCI move into an ordered sequence of sensor changes
  (including captures, castling, en passant and promotions).
- Loads FEN, replays PGN (single or looping, with delay/speed control) and can run
  many games concurrently.
- Injects faults deterministically: latency, fragmentation, packet loss, truncated
  replies, spontaneous disconnects (seeded, reproducible).
- Exposes a Unix-domain-socket control plane so the CLI drives a running daemon.

## Requirements

- Linux (developed on Arch Linux).
- Rust stable, edition 2024 (MSRV 1.85).
- No root, no Docker, no external services, no real DGT hardware.

## Build

```bash
cargo build --release
# binary: target/release/dgt-sim
```

## Quick start

```bash
# Start 10 boards on pseudoterminals.
dgt-sim start --boards 10

# From another shell: see them, inspect one, drive it.
dgt-sim list
dgt-sim show --board 1            # `status` is accepted as an alias
dgt-sim move --board 1 --uci e2e4

# Point a serial client at a board's /dev/pts/N endpoint and talk DGT bytes.

# Shut everything down (removes the control socket, frees every PTY).
dgt-sim shutdown
```

Use `--json` on any query for machine-readable output and rely on the exit codes
(0 = success, 1 = runtime error, 2 = CLI usage error).

## Command overview

| Command | Purpose |
| --- | --- |
| `start --boards N [--transport pty\|tcp]` | Run the daemon and create boards |
| `info` | Daemon status: pid, socket, board count, uptime |
| `list` | One line per board with its endpoint |
| `show --board N` (alias `status`) | Full board state, clock and FEN |
| `messages --board N` | Recent outbound protocol messages, as hex |
| `move --board N --uci e2e4` | Play a move as a physical sequence |
| `lift/place/clear --board N --square e2` | Manipulate single fields |
| `set-fen --board N --fen "..."` | Load a placement |
| `reset --board N` | Back to the start position, IDLE mode |
| `set-mode --board N --mode update-board` | Force a transfer mode |
| `replay --board N --pgn game.pgn [--delay 500ms] [--speed 2] [--loop]` | Replay a game |
| `disconnect/reconnect --board N` | Drop / re-accept a client |
| `clock --board N --left 5m --run` | Manipulate the emulated clock |
| `config --board N ...` | Change serial, version, trademark, physical style |
| `fault --board N --latency 200ms [--drop-rate 0.05]` | Inject faults |
| `stop --board N` | Terminate one board |
| `shutdown` | Terminate the daemon |

Run `dgt-sim --help` or `dgt-sim <command> --help` for the full list of flags.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — modules, concurrency, actor model.
- [`docs/protocol.md`](docs/protocol.md) — the DGT protocol as implemented, with sources and open questions.
- [`docs/compatibility.md`](docs/compatibility.md) — what "compatible" means here, and what it does not.
- [`docs/examples.md`](docs/examples.md) — copy-pasteable recipes.

## Testing

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --release
cargo run --release --bench boards    # 10 / 100 / 1000 board scaling
```

## Layout

```
src/
  protocol/   pure byte-level DGT protocol (no I/O): commands, messages, codec, pieces
  board/      the physical sensor model: 64 fields, modes, EEPROM, clock
  transport/  PTY and TCP endpoints + deterministic fault injection
  manager.rs  owns every board, routes control requests
  control.rs  Unix-socket JSON control plane
  cli.rs      Clap CLI
  chess.rs    FEN / UCI / PGN (the only place chess rules live)
tests/        protocol, integration, multiboard
benches/      scaling benchmarks
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
