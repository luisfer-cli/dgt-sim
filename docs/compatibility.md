# Compatibility

"Compatible with DGT" means different things to different applications. This
project is explicit about which of the three levels it reaches.

## Level 1 — Protocol

**Done.** The emulator produces correct binary DGT messages: framing, message
identifiers, the command set, field updates, board dumps, serial/version/
trademark/bus-address replies, the EEPROM dump and the clock messages. The byte
layout is documented with sources in [`protocol.md`](protocol.md), and the test
suite pins several messages to captures from `arnsholt/dgtdriver`'s hardware
corpus rather than to this implementation's own output.

## Level 2 — Serial / TCP integration

**Done, within the limits below.** An external client can connect and exchange
real protocol bytes:

- **PTY**: the board exposes a `/dev/pts/N` character device. Anything that opens
  a serial port and reads/writes bytes works unchanged — the line discipline is
  raw, so no echo or line editing interferes.
- **TCP**: one loopback listener per board, for scripts and test harnesses. It is
  the transport that reports genuine connect/disconnect events.

Verified by the integration and multiboard test suites: a serial client over PTY,
raw TCP clients, 100 concurrent TCP boards, many PTY boards with independent
endpoints, and per-board state isolation.

### Limits

- A PTY is **not** a USB serial adapter. It behaves like a serial port at the
  character-device level, but no USB device appears and no USB VID/PID is
  presented.
- TCP is loopback-only by default (`127.0.0.1:0`).
- **No compatibility with any specific third-party application has been tested.**
  In particular, DGT LiveChess is *not* claimed to work: it discovers DGT boards
  as USB devices, which is Level 3.

## Level 3 — USB hardware emulation

**Not implemented, and not part of the MVP.**

Applications that discover DGT boards over USB will not find an emulated board
here. Reaching Level 3 on Linux would require one of:

- **USB Gadget / `dummy_hcd` / ConfigFS.** A Linux USB device controller in
  gadget mode can present a vendor/class USB device from userspace. It needs a
  UDC — either real hardware (a single-board computer in device mode) or the
  `dummy_hcd`/`raw-gadget` kernel modules — and often root. This is the most
  faithful route for host-side discovery, but it is hardware- and
  kernel-dependent, so it cannot be a required part of the MVP.
- **USB/IP (`usbip`).** A gadget exposed on one machine can be forwarded to
  another over the network with `usbip`, which again requires a UDC
  (or `dummy_hcd`) as the source. Adds an external dependency.
- **Driver shimming / `LD_PRELOAD`.** Intercepting `libusb` calls in a specific
  application is fragile, application-specific, and explicitly out of scope.

None of these is implemented. If Level 3 is pursued later, the `transport`
abstraction is the place to add it, and it must remain optional so the default
build needs no special hardware or privileges.

## What the emulator is not

- It is not a chess engine and does not generate moves; it reports sensor
  changes. Illegally placed pieces, two kings, or a piece lifted and never put
  back are all representable, because a physical board can hold exactly that.
- It does not enforce the laws of chess anywhere except `set-fen` validation and
  PGN replay (where a real engine, `shakmaty`, is used to resolve SAN tokens).
- It is not a mock: clients get real framed DGT bytes, not JSON or UCI.
