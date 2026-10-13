# The DGT protocol as implemented

This document records what `dgt-sim` actually implements, byte by byte, and what
it deliberately does not. The goal is that a reader can verify the emulator
against the protocol rather than trust it.

## Sources

Everything below is derived from public material, primarily:

- *DGT Electronic Board Protocol Description* (the header/document distributed by
  DGT; the internal changelog referenced in the code is dated 2015-07-20).
- [`emeijdam/dgtboard-rs`](https://github.com/emeijdam/dgtboard-rs)
- [`fnogatz/dgtchess`](https://github.com/fnogatz/dgtchess)
- [`arnsholt/dgtdriver`](https://github.com/arnsholt/dgtdriver) — its
  `test/resources` corpus (`1.inbytes`, `1.uci`) is used verbatim as test vectors.

**Nothing in this project has been verified against physical hardware.** Where the
sources disagree or are silent, that is stated explicitly under
[Pending / unverified](#pending--unverified) and the emulator offers a switch
rather than a guess.

## Message framing (board → PC)

Every message from the board is:

```
byte 0 : message id | 0x80        (the MSB of byte 0 is always set)
byte 1 : size bits 13..7          (MSB always 0)
byte 2 : size bits 6..0           (MSB always 0)
byte 3..: payload                 (every payload byte has its MSB clear)
```

The `size` is the **total** message length, header included. Payloads are
therefore `size - 3` bytes, up to `0x1f00`.

The incremental reader (`protocol::messages::Framer`) tolerates fragmentation at
any boundary and resynchronises after garbage by discarding one byte at a time —
never by scanning for the next byte with the MSB set, because that heuristic
skips over the real header that follows a single garbage byte.

### Message identifiers (without the `0x80` bit)

| Id | Name | Fixed size | Payload |
| --- | --- | --- | --- |
| `0x06` | `DGT_MSG_BOARD_DUMP` | 67 | 64 sensor codes, field 0 (`a8`) first |
| `0x0d` | `DGT_MSG_BWTIME` | 10 | clock times **or** a clock ACK (7 payload bytes) |
| `0x0e` | `DGT_MSG_FIELD_UPDATE` | 5 | `[field, piece_code]` |
| `0x0f` | `DGT_MSG_EE_MOVES` | variable | `EE_DOWNLOADED`, `0x1f00`-byte ring, `EE_EOF` |
| `0x10` | `DGT_MSG_BUSADRES` | 5 | 14-bit address in two 7-bit halves |
| `0x11` | `DGT_MSG_SERIALNR` | 8 | 5 ASCII decimal digits |
| `0x12` | `DGT_MSG_TRADEMARK` | variable | 7-bit ASCII identification string |
| `0x13` | `DGT_MSG_VERSION` | 5 | `[major, minor]` |
| `0x14` | `DGT_MSG_BOARD_DUMP_50B` | 53 | draughts, black squares |
| `0x15` | `DGT_MSG_BOARD_DUMP_50W` | 53 | draughts, white squares |
| `0x20` | `DGT_MSG_BATTERY_STATUS` | 7* | Bluetooth boards |
| `0x22` | `DGT_LONG_SERIALNR` | 13 | 10 ASCII decimal digits |

\* The battery-status size is internally inconsistent upstream; see
[Pending / unverified](#pending--unverified).

The emulator emits `0x06`, `0x0d`, `0x0e`, `0x0f`, `0x10`, `0x11`, `0x12`,
`0x13`, `0x20` and `0x22`. The draughts dumps (`0x14`, `0x15`) are decoded by the
framer but not produced, because only 8×8 chess is emulated.

## Commands (PC → board)

Single-byte commands are handled directly; `0x2b` and `0x60` are length-prefixed
frames.

| Byte | Name | Reply |
| --- | --- | --- |
| `0x40` | `DGT_SEND_RESET` | none; back to IDLE |
| `0x41` | `DGT_SEND_CLK` | `DGT_MSG_BWTIME` |
| `0x42` | `DGT_SEND_BRD` | `DGT_MSG_BOARD_DUMP` |
| `0x43` | `DGT_SEND_UPDATE` | enters UPDATE (fields + clock) |
| `0x44` | `DGT_SEND_UPDATE_BRD` | enters UPDATE_BOARD (fields only) |
| `0x45` | `DGT_RETURN_SERIALNR` | `DGT_MSG_SERIALNR` |
| `0x46` | `DGT_RETURN_BUSADRES` | `DGT_MSG_BUSADRES` |
| `0x47` | `DGT_SEND_TRADEMARK` | `DGT_MSG_TRADEMARK` |
| `0x49` | `DGT_SEND_EE_MOVES` | `DGT_MSG_EE_MOVES` |
| `0x4a` | `DGT_TO_BUSMODE` | ignored (documented as unsupported) |
| `0x4b` | `DGT_SEND_UPDATE_NICE` | enters UPDATE_NICE (clock on change) |
| `0x4c` | `DGT_SEND_BATTERY_STATUS` | `DGT_MSG_BATTERY_STATUS` |
| `0x4d` | `DGT_SEND_VERSION` | `DGT_MSG_VERSION` |
| `0x4e` | `DGT_STARTBOOTLOADER` | ignored (documented as unsupported) |
| `0x50`–`0x54` | draughts scan/board commands | ignored (chess only) |
| `0x55` | `DGT_RETURN_LONG_SERIALNR` | `DGT_LONG_SERIALNR` |
| `0x60` | `DGT_SET_LEDS` | none (Revelation II only) |
| `0x2b` | `DGT_CLOCK_MESSAGE` | see below |

### Transfer modes

The board only streams field updates in UPDATE, UPDATE_BOARD and UPDATE_NICE;
clock messages are streamed in UPDATE (at least once per second) and UPDATE_NICE
(only when the payload changes). IDLE streams nothing. This matches the protocol
description and is enforced by the emulator's state machine, so a client that
never sends a mode command sees silence — exactly like real hardware.

### `DGT_CLOCK_MESSAGE` (`0x2b`)

```
0x2b, size, 0x03 (DGT_CMD_CLOCK_START), command, data..., 0x00 (DGT_CMD_CLOCK_END)
```

Known clock command ids:

| Id | Name |
| --- | --- |
| `0x01` | `DGT_CMD_CLOCK_DISPLAY` |
| `0x02` | `DGT_CMD_CLOCK_ICONS` |
| `0x03` | `DGT_CMD_CLOCK_END` |
| `0x08` | `DGT_CMD_CLOCK_BUTTON` |
| `0x09` | `DGT_CMD_CLOCK_VERSION` |
| `0x0a` | `DGT_CMD_CLOCK_SETNRUN` |
| `0x0b` | `DGT_CMD_CLOCK_BEEP` |
| `0x0c` | `DGT_CMD_CLOCK_ASCII` |

The emulator ACKs clock commands only in UPDATE and UPDATE_NICE. `SETNRUN`
programs both screens (`hours` binary, `minutes`/`seconds` BCD) and the running /
side-to-move flags. An unsupported clock command is answered with the documented
error ACK (`ack0 = 0x40`).

### `DGT_SET_LEDS` (`0x60`)

```
0x60, size = 4, pattern, first, last, 0x00
```

Parsed and validated, but a no-op: only the Revelation II has addressable LEDs.

## Fields, piece codes and numbering

Fields are numbered exactly like FEN ranks: field `0` is `a8`, field `63` is
`h1`. This makes board dumps and FEN placements trivially convertible, and is
why the emulator never has to reverse rows.

| Code | Piece | Code | Piece |
| --- | --- | --- | --- |
| `0x00` | empty | `0x07` | black pawn |
| `0x01` | white pawn | `0x08` | black rook |
| `0x02` | white rook | `0x09` | black knight |
| `0x03` | white knight | `0x0a` | black bishop |
| `0x04` | white bishop | `0x0b` | black king |
| `0x05` | white king | `0x0c` | black queen |
| `0x06` | white queen | `0x0d` | "magic" piece: draw |
|  |  | `0x0e` | "magic" piece: white win |
|  |  | `0x0f` | "magic" piece: black win |

The three magic pieces are exposed by `dgt-sim place --piece magic-draw` (etc.)
and reported in `show`, but cannot be expressed in FEN, so they render as empty
fields there. Any code above `0x0f` is rejected as malformed.

## `DGT_MSG_BWTIME`

Payload layout (7 bytes):

| Byte | Meaning |
| --- | --- |
| 0 | right hours (D0–D3) + right flags (D4–D6) |
| 1 | right minutes, BCD |
| 2 | right seconds, BCD |
| 3 | left hours (D0–D3) + left flags (D4–D6) |
| 4 | left minutes, BCD |
| 5 | left seconds, BCD |
| 6 | status |

Status bits: `RUNNING 0x01`, `RIGHT_HIGH 0x02`, `BATTERY_LOW 0x04`,
`RIGHT_TO_MOVE 0x08`, `LEFT_TO_MOVE 0x10`, `NO_CLOCK 0x20`.
Per-screen flag bits: `FLAG_FALLEN 0x01`, `TIME_PER_MOVE 0x02`,
`FLAG_INDICATED 0x04`.

A clock **ACK** is also a `DGT_MSG_BWTIME`; it is recognised by a low nibble of
`0x0a` in payload byte 0 or 3. The documented bit-packing of the four ACK bytes
across the seven payload bytes is implemented and round-trips in the tests.

## EEPROM

The on-board ring buffer is `0x1f00` bytes, reported in full for
`DGT_SEND_EE_MOVES` as `EE_DOWNLOADED`, the ring, then `EE_EOF`.

One-byte tags: `EE_POWERUP 0x6a`, `EE_EOF 0x6b`, `EE_FOURROWS 0x6c`,
`EE_EMPTYBOARD 0x6d`, `EE_DOWNLOADED 0x6e`, `EE_BEGINPOS 0x6f`,
`EE_BEGINPOS_ROT 0x7a`, `EE_START_TAG 0x7b`, `EE_WATCHDOG_ACTION 0x7c`,
`EE_NOP 0x7f`, `EE_NOP2 0x00`.

Two-byte field tag: `0t0r nnnn`, `00ii iiii` — the `r` bit (D4) means "less than
roughly 250 ms since the previous tag". Three-byte clock tag:
`0uuf hhhh`, minutes BCD, seconds BCD.

At construction the emulator writes the power-up sequence the hardware writes:
three `EE_NOP`, one `EE_POWERUP`, then the position tag for the starting
position.

## Pending / unverified

These are the parts that could **not** be verified and are therefore either
switchable or explicitly marked as not implemented. None of them is guessed.

1. **`DGT_MSG_BATTERY_STATUS` size.** The description states a 7-byte total while
   its byte-by-byte list implies a 9-byte payload. The emulator emits a 4-byte
   payload (total 7). Bluetooth boards only; no board without Bluetooth needs it.

2. **`DGT_MSG_BWTIME` status bit polarity.** The 2015-07-20 changelog swaps D3 and
   D4 while describing D5 with inverted polarity, and `dgtdriver` (Java) reads
   them differently from the header. Because this cannot be checked against
   hardware here, both readings are supported:
   `--clock-compat header` (D3 = right to move, D5 set = *no* clock) and
   `--clock-compat java` (D3 = left to move, D5 set = clock connected).

3. **Clock message timing.** "At least once a second" in UPDATE mode is
   implemented as a 200 ms board tick with a 1000 ms clock-message cadence; the
   exact hardware cadence is not documented.

4. **`DGT_TO_BUSMODE` / `DGT_STARTBOOTLOADER`.** Recognised and logged, but not
   emulated: there is no RS-485 bus nor a bootloader to switch to.

5. **Draughts commands** (`0x50`–`0x54`) are recognised and ignored.

6. **USB hardware emulation (Level 3)** is not implemented. See
   [`compatibility.md`](compatibility.md).

7. **Compatibility with DGT LiveChess or any specific third-party application has
   not been tested.** The emulator targets the wire protocol (Level 1) and serial
   / TCP integration (Level 2); it does not claim application-level compatibility
   for software that discovers DGT devices over USB.
