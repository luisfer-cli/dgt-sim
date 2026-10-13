//! Pseudoterminal transport.
//!
//! A PTY pair gives an external client a real character device
//! (`/dev/pts/N`) that behaves like a serial port. The board holds the master
//! side and sets the line discipline to raw, so bytes cross the pair
//! unprocessed.
//!
//! This is **not** USB emulation: no USB device appears, and software that
//! enumerates USB VID/PID pairs will not find it. See `docs/compatibility.md`.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::{OpenptyResult, PtyMaster, openpty, ptsname_r};
use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
use tokio::io::unix::AsyncFd;

/// Master side of a pseudoterminal, plus the slave path clients should open.
#[derive(Debug)]
pub struct PtyLink {
    master: AsyncFd<OwnedFd>,
    /// Kept open on purpose. Linux returns `EIO` from a master read as soon as
    /// no process holds the slave open, which would turn this link into a busy
    /// loop. Holding one slave descriptor ourselves keeps the pair stable for
    /// the lifetime of the board.
    _keepalive: OwnedFd,
    path: PathBuf,
}

impl PtyLink {
    /// Create a PTY pair and configure it for binary, unpolluted I/O.
    pub fn open() -> io::Result<Self> {
        let OpenptyResult { master, slave } = openpty(None, None)?;
        // SAFETY: `master` was just returned by `openpty(3)` and is therefore a
        // valid master pty descriptor. The wrapper is consumed immediately and
        // never escapes this function.
        let master = unsafe { PtyMaster::from_owned_fd(master) };
        let path = PathBuf::from(ptsname_r(&master)?);
        let master: OwnedFd = master.into();

        make_raw(master.as_fd())?;
        set_nonblocking(master.as_fd())?;

        let master = AsyncFd::new(master)?;
        Ok(Self {
            master,
            _keepalive: slave,
            path,
        })
    }

    /// Path the client should open, e.g. `/dev/pts/7`.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Read from the client. Blocks until bytes arrive (or forever if the
    /// client is silent, which is exactly what a serial port does).
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.master.readable().await?;
            match guard.try_io(|fd| nix::unistd::read(fd, buf).map_err(io::Error::from)) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// Write to the client, retrying on partial writes.
    pub async fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|fd| nix::unistd::write(fd, buf).map_err(io::Error::from)) {
                Ok(Ok(0)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "pty master accepted zero bytes",
                    ));
                }
                Ok(Ok(n)) => buf = &buf[n..],
                Ok(Err(err)) => return Err(err),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Nothing to flush: the bytes have already been handed to the tty.
    pub async fn shutdown(&mut self) {}
}

/// Put the terminal in raw mode so the line discipline neither interprets nor
/// echoes protocol bytes.
fn make_raw(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut termios = tcgetattr(fd)?;
    cfmakeraw(&mut termios);
    tcsetattr(fd, SetArg::TCSANOW, &termios)?;
    Ok(())
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;
    Ok(())
}

/// Explicitly acknowledge that the async wrapper needs an `AsFd` resource.
const _: fn(&PtyLink) -> bool = |link| {
    let _ = link.master.get_ref().as_fd();
    true
};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pty_pair_transfers_bytes_both_ways() {
        let mut link = PtyLink::open().expect("open pty");
        let path = link.path().to_path_buf();
        assert!(path.starts_with("/dev/pts/"), "unexpected path {path:?}");

        // A client opens the slave by path, exactly as a serial client would.
        let client = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await
            .expect("client opens the pts");

        link.write_all(&[0x8d, 0x00, 0x05, 0x01, 0x08])
            .await
            .unwrap();

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client = client;
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("read timed out")
            .expect("read failed");
        assert_eq!(&buf[..n], &[0x8d, 0x00, 0x05, 0x01, 0x08]);

        client.write_all(&[0x42]).await.unwrap();
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), link.read(&mut buf))
            .await
            .expect("read timed out")
            .expect("read failed");
        assert_eq!(&buf[..n], &[0x42]);
    }

    #[tokio::test]
    async fn raw_mode_prevents_echo_of_our_own_writes() {
        let mut link = PtyLink::open().expect("open pty");
        let path = link.path().to_path_buf();
        let _client = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await
            .expect("client opens the pts");

        link.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 8];
        // Without a raw line discipline the kernel would echo "hello" back to
        // the master and this read would return.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(150), link.read(&mut buf)).await;
        assert!(result.is_err(), "echo leaked back: {:?}", result);
    }

    #[tokio::test]
    async fn separate_ptys_have_separate_paths() {
        let a = PtyLink::open().unwrap();
        let b = PtyLink::open().unwrap();
        assert_ne!(a.path(), b.path());
    }
}
