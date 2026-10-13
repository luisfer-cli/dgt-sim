# Running dgt-sim as a systemd service

`dgt-sim start` runs in the foreground and shuts down cleanly on `SIGTERM`, so it
maps directly onto a systemd unit. Because the emulator needs no privileges and
its control socket lives in `$XDG_RUNTIME_DIR`, the recommended setup is a
**user service**.

Files:

- [`dgt-sim.service`](dgt-sim.service) — user unit (recommended).

## 1. Install the binary

Either:

```bash
cargo build --release
install -Dm755 target/release/dgt-sim ~/.local/bin/dgt-sim
```

or, if you use `cargo install --path .` (which installs to `~/.cargo/bin`),
either symlink it or adjust `ExecStart` in the unit:

```bash
ln -sf ~/.cargo/bin/dgt-sim ~/.local/bin/dgt-sim
```

## 2. Install the user service

```bash
mkdir -p ~/.config/systemd/user
cp packaging/systemd/dgt-sim.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dgt-sim.service
```

If you want the boards to keep running after you log out, enable lingering:

```bash
loginctl enable-linger "$USER"
```

## 3. Check it

```bash
systemctl --user status dgt-sim.service
journalctl --user -u dgt-sim.service -f      # board endpoints appear here
dgt-sim list                                  # the CLI talks to the same socket
```

The daemon uses `$XDG_RUNTIME_DIR/dgt-sim.sock` by default, which is exactly
where a user service points `%t`. The CLI run from your shell finds it with no
extra flags.

## 4. Configure boards / transport

The unit reads two environment variables, so a drop-in is enough — no need to
edit the unit file:

```bash
systemctl --user edit dgt-sim.service
```

```ini
[Service]
Environment=DGT_SIM_BOARDS=100
Environment=DGT_SIM_TRANSPORT=tcp
```

```bash
systemctl --user restart dgt-sim.service
```

`DGT_SIM_TRANSPORT` is `pty` (default) or `tcp`.

## 5. Stop / remove

```bash
systemctl --user stop dgt-sim.service          # graceful; same as `dgt-sim shutdown`
systemctl --user disable --now dgt-sim.service
rm ~/.config/systemd/user/dgt-sim.service
systemctl --user daemon-reload
```

`dgt-sim shutdown` (or `SIGTERM`) stops every board, releases the PTYs and
removes the control socket, so a restart never hits a stale socket.

## System-wide variant (optional)

If you really need a system service (for example to serve TCP boards to other
hosts), run it as a dedicated unprivileged user and give it a private runtime
directory. Create `/etc/systemd/system/dgt-sim.service`:

```ini
[Unit]
Description=DGT electronic chess board emulator
Documentation=https://github.com/luisfer-cli/dgt-sim
After=network.target

[Service]
Type=simple
User=dgt-sim
Group=dgt-sim
# A private /run/dgt-sim directory is created and removed automatically.
RuntimeDirectory=dgt-sim
Environment=DGT_SIM_SOCKET=/run/dgt-sim/dgt-sim.sock
Environment=DGT_SIM_BOARDS=10
Environment=DGT_SIM_TRANSPORT=tcp
ExecStart=/usr/local/bin/dgt-sim start --boards ${DGT_SIM_BOARDS} --transport ${DGT_SIM_TRANSPORT}
Restart=on-failure
RestartSec=2
KillSignal=SIGTERM
TimeoutStopSec=15
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/run/dgt-sim

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin dgt-sim
sudo systemctl daemon-reload
sudo systemctl enable --now dgt-sim
```

Client commands then need the socket path explicitly:

```bash
dgt-sim --socket /run/dgt-sim/dgt-sim.sock list
```

TCP boards bind to `127.0.0.1:0` by default; add `--bind 0.0.0.0:0` only if you
intend to expose them, and mind that anyone who can reach a board endpoint can
drive it.
