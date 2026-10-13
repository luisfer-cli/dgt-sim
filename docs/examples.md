# Examples

All examples assume `dgt-sim` is on `PATH` (or use `target/release/dgt-sim`).
Every command supports `--json` for scripts; the exit codes are `0` success,
`1` runtime error, `2` CLI usage error.

## Start some boards

```bash
# 10 boards on pseudoterminals (the default transport).
dgt-sim start --boards 10

# 100 boards on loopback TCP.
dgt-sim start --boards 100 --transport tcp

# Print the endpoints as one line of JSON (handy for scripts) and keep running.
dgt-sim start --boards 4 --json | jq -r '.[].endpoint'
```

`start` runs in the foreground. Use a second shell (or a service unit) for the
commands below. A different control socket can be chosen with `--socket PATH` or
`$DGT_SIM_SOCKET`.

## Inspect boards

```bash
dgt-sim info                       # pid, socket, board count, uptime
dgt-sim list                       # one row per board
dgt-sim show --board 1             # full state + FEN (alias: `dgt-sim status`)
dgt-sim show --board 1 --ascii     # also print the board diagram
dgt-sim show --board 1 --json | jq .clock
dgt-sim messages --board 1         # recent outbound protocol messages, as hex
```

## Connect a real serial client

A PTY board publishes a `/dev/pts/N` endpoint (shown by `list`/`start`). Anything
that opens a serial port can use it:

```bash
# Print the endpoint of board 1.
EP=$(dgt-sim list --json | jq -r '.[] | select(.id==1) | .endpoint')

# Raw: send DGT_SEND_BRD (0x42) and read the 67-byte board dump back.
printf '\x42' > "$EP" & cat "$EP" | xxd | head
```

A more comfortable option is `socat` if your client wants a real serial device,
or just open `$EP` directly from your program. Over TCP:

```bash
# board 1's endpoint looks like tcp://127.0.0.1:45123
ADDR=$(dgt-sim list --json | jq -r '.[] | select(.id==1) | .endpoint' | sed 's#tcp://##')
printf '\x42' | nc -q1 "$ADDR" | xxd
```

### Talk the protocol by hand

```bash
# Enter UPDATE_BOARD, then request a board dump, in one write.
printf '\x44\x42' > "$EP"; cat "$EP" | xxd
```

The board only streams field updates once a mode command has put it in an update
mode; until then it stays silent, exactly like real hardware.

## Play moves

```bash
dgt-sim set-mode --board 1 --mode update-board   # so updates are streamed
dgt-sim move --board 1 --uci e2e4                # two field updates: e2, e4
dgt-sim move --board 1 --uci e7e8q               # promotion
dgt-sim move --board 1 --uci e1g1                # castling: king then rook
```

Moves are geometric physical sequences, not legality-checked:

```
$ dgt-sim messages --board 1
     12ms  field-update    5B  8e 00 05 34 00
     12ms  field-update    5B  8e 00 05 24 01
```

`--physical-style verbose` additionally emits the intermediate states a real
board sometimes reports (clearing an occupied destination before a capture, the
promoted pawn arriving before it is swapped):

```bash
dgt-sim config --board 1 --physical-style verbose
```

## Manipulate fields directly

```bash
dgt-sim lift  --board 1 --square e2
dgt-sim place --board 1 --square e4 --piece white-pawn
dgt-sim clear --board 1 --square e4
```

`--piece` accepts `white-knight`, `wn`, a FEN letter (`N` = white knight,
`n` = black), `empty`, or a magic piece (`magic-draw`, `magic-white-win`,
`magic-black-win`).

## FEN and reset

```bash
dgt-sim set-fen --board 1 --fen "r1bqkbnr/pppp1ppp/2n5/4p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4"
dgt-sim set-fen --board 1 --fen "8/8/8/8/8/8/8/8 w - - 0 1" --quiet   # silent reposition
dgt-sim reset   --board 1                                             # start position + IDLE
```

## Replay a PGN

```bash
dgt-sim replay --board 1 --pgn game.pgn
dgt-sim replay --board 1 --pgn game.pgn --delay 500ms          # 0.5 s between moves
dgt-sim replay --board 1 --pgn game.pgn --delay 1s --speed 2   # effectively 0.5 s
dgt-sim replay --board 1 --pgn game.pgn --loop                 # forever
dgt-sim replay --board 2 --pgn other.pgn --fen "<start FEN>"   # from a custom position
```

Replaying promotes an idle board to `update-board` so the moves are observable.
Different boards replay concurrently:

```bash
for b in 1 2 3 4 5; do dgt-sim replay --board "$b" --pgn game.pgn --delay 250ms --loop & done
```

## Emulated clock

```bash
dgt-sim clock --board 1 --left 5m --right 5m --run
dgt-sim clock --board 1 --pause
dgt-sim clock --board 1 --turn right
dgt-sim clock --board 1 --connected false       # simulate no clock attached
```

In `update`/`update-nice` mode the board streams `DGT_MSG_BWTIME` messages to the
client as the clock runs.

## Fault injection

Faults are off by default. Each board is independent, and a seed makes the
random ones reproducible:

```bash
dgt-sim fault --board 1 --latency 200ms
dgt-sim fault --board 2 --drop-rate 0.05 --seed 42
dgt-sim fault --board 3 --fragment-max 8          # split writes into ≤8-byte chunks
dgt-sim fault --board 4 --truncate                 # cut every message in half
dgt-sim fault --board 5 --disconnect-rate 0.01     # 1% chance of dropping the link
dgt-sim fault --board 1 --latency 0ms              # clear
```

Faults can also be set for every board at start:

```bash
dgt-sim start --boards 20 --latency 5ms --drop-rate 0.01 --seed 7
```

## Disconnect, reconnect, stop

```bash
dgt-sim disconnect --board 1     # drops the current client immediately (EOF)
dgt-sim reconnect  --board 1     # accepts clients again
dgt-sim stop       --board 1     # terminates the board and frees its endpoint
dgt-sim shutdown                 # terminates the daemon and every board
```

`SIGINT`/`SIGTERM` to the daemon behave like `shutdown`: boards are stopped, PTYs
and sockets are released, and the control socket is removed.

## Scripting

```bash
# Endpoints of every TCP board.
dgt-sim list --json | jq -r '.[] | select(.transport=="tcp") | .endpoint'

# Fail if any board has left the starting position.
dgt-sim list --json | jq -e 'all(.[]; .fen=="rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR")'

# Wait until a board has processed a mode command.
until [ "$(dgt-sim show --board 1 --json | jq -r .mode)" != idle ]; do sleep 0.05; done
```

## Benchmarks

```bash
cargo bench --bench boards        # 10 / 100 / 1000 boards, protocol throughput
```

The benchmark reports creation time, resident memory and `RLIMIT_NOFILE` (a PTY
board costs two file descriptors, so that value is the practical ceiling).
