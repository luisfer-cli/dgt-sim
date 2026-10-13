# Architecture

`dgt-sim` is a single binary with a small library core. Everything is layered so
that the pure protocol logic can be tested without opening a device or a socket,
and so that no layer reaches into another's state.

```
                 ┌──────────────────────────────────────────────┐
   CLI client ──▶│ control.rs  (Unix socket, JSON request/line)  │
                 └───────────────┬──────────────────────────────┘
                                 │ mpsc::channel<Job>
                 ┌───────────────▼──────────────────────────────┐
                 │ manager.rs   (owns boards, routes requests)   │
                 └───┬───────────────────────────────────┬──────┘
       one task per  │ BoardCmd channel                  │ link channel
       board         ▼                                   ▼
                 ┌────────────────────────┐   ┌───────────────────────┐
 DGT client ────▶│ board/  BoardTask::run │◀──│ transport/ (PTY, TCP) │
 (raw bytes)     └───────────┬────────────┘   └───────────────────────┘
                             │ uses
                 ┌───────────▼────────────┐   ┌───────────────────────┐
                 │ protocol/ (pure bytes) │   │ chess.rs (FEN/UCI/PGN)│
                 └────────────────────────┘   └───────────────────────┘
```

## Modules

| Module | Responsibility | Touches I/O? |
| --- | --- | --- |
| `protocol` | Framing, commands, messages, piece codes, incremental parse/frame. | No |
| `board` | The physical sensor model: 64 fields, modes, EEPROM, clock, and the task that owns them. | via channels only |
| `transport` | `Link` over PTY or TCP, plus deterministic fault injection. | Yes |
| `manager` | Creates/removes boards, allocates identifiers, routes control requests. | Sockets |
| `control` | Unix-domain-socket JSON control plane. | Unix socket |
| `cli` | Clap CLI; every command but `start` is a control-socket client. | Yes |
| `chess` | FEN validation, UCI parsing, PGN→UCI. The only place chess rules exist. | No |

The `protocol` module is deliberately pure: `CommandParser` and `Framer` are
plain state machines over `Vec<u8>`. The tests drive them directly with hardware
captures, no device required.

## The actor model: one task per board

Each board is a Tokio task (`BoardTask::run`) that **owns** its state. Nothing
outside the task can read or mutate that state directly:

- every mutation is a `BoardCmd` sent over an `mpsc` channel;
- every read is a `Snapshot` / `Messages` request answered over a `oneshot`;
- the board's replies are returned from the command handler as a list of already
  framed protocol messages.

This is what makes hundreds of boards safe: there are **no locks and no shared
mutable state**, so there is no lock ordering to get wrong and no data race to
reason about. Ownership is explicit and cancellation is explicit.

`BoardTask::run` multiplexes four sources with `tokio::select!`:

1. control commands (`BoardCmd`),
2. bytes from the connected client (`Link::read`),
3. newly accepted links (so a client can attach at any time),
4. a 200 ms tick that advances the clock and emits due clock messages.

The manager handles control requests one at a time from a single task, which is
why identifier allocation and endpoint bookkeeping need no synchronisation. The
boards themselves run concurrently and never coordinate with each other.

### Ordering caveat

The `select!` is `biased` and prefers the control channel. A command injected
through the control plane can therefore be processed before bytes a client has
already written to its socket, because the two travel over different channels.
Tests that need them ordered synchronise on the observed board state rather than
on a sleep. In normal use a script drives a board either through its client
socket *or* through the control plane, not both at once.

## Transports

- **PTY** (`transport/pty.rs`): the board holds the master side of a
  pseudoterminal pair; the client opens the slave (`/dev/pts/N`). The line
  discipline is set to raw so nothing echoes or interprets protocol bytes. One
  slave descriptor is kept open for the board's lifetime, because Linux returns
  `EIO` from a master read as soon as no process holds the slave — holding it
  keeps the link stable.
- **TCP** (`transport/tcp.rs`): one loopback listener per board. Unlike PTY it
  reports genuine connection lifecycle, which is what several tests rely on.

Both hide behind `Link`, so the board task never knows which one it is talking
through. Neither is USB emulation; see [`compatibility.md`](compatibility.md).

## Fault injection

`transport::Faults` is a per-board configuration applied when messages are
written: latency, fragmentation (`fragment_max`), probabilistic fragment loss,
mid-message truncation, and spontaneous disconnects. All randomness comes from a
small xorshift RNG seeded from `Faults::seed`, so a failure mode is reproducible
given a seed. Faults are off by default.

## Control plane

`control.rs` binds a Unix-domain socket (`$XDG_RUNTIME_DIR/dgt-sim.sock`,
`/tmp/dgt-sim-<uid>.sock`, or `--socket`) with mode `0600`, and speaks
newline-delimited JSON — one request per line, one response per line. A stale
socket from a killed daemon is detected and removed. The JSON never reaches a DGT
client: those only ever see the binary protocol.

## Errors and shutdown

All fallible operations return one `Error` type (`error.rs`). Runtime errors exit
`1`; Clap usage errors exit `2`; success exits `0`. `SIGINT` and `SIGTERM` are
translated into a graceful shutdown that stops every board task, drops every PTY
and removes the control socket. Boards are given a bounded time to flush before
being abandoned, and a dead CLI never takes the daemon with it (a failed reply
send is ignored).

## Performance

Measured on the development machine with `cargo bench --bench boards`:

| Boards | Create (ms) | RSS (KiB) |
| --- | --- | --- |
| 10 | 0.67 | 4 480 |
| 100 | 9.29 | 7 076 |
| 1 000 | 62.71 | 29 576 |

Pure protocol work is on the order of tens of nanoseconds: encoding + framing +
parsing a board dump is ~30 ns/op, and parsing a fragmented clock command is
~70 ns/op. The per-board dump round-trip (~77–161 µs) is dominated by a fresh
`connect()` per board, not by the emulator.

The practical ceiling is file descriptors: a PTY board costs two. The benchmark
reports the process's `RLIMIT_NOFILE` so the real ceiling is visible.
