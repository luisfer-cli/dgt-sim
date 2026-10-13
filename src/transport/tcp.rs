//! TCP transport: one loopback listener per board.
//!
//! Used for scripting, for integration tests and — unlike the PTY transport —
//! for genuine connection lifecycle: the board knows when a client arrives and
//! when it leaves.

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A connected TCP client.
#[derive(Debug)]
pub struct TcpLink {
    stream: TcpStream,
}

impl TcpLink {
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// `Ok(0)` means the client closed the connection.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf).await
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.stream.write_all(buf).await
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tcp_link_roundtrips_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(b"board?").await.unwrap();
            let mut buf = [0u8; 6];
            stream.read_exact(&mut buf).await.unwrap();
            buf
        });

        let (stream, _) = listener.accept().await.unwrap();
        let mut link = TcpLink::new(stream);
        let mut buf = [0u8; 6];
        assert_eq!(link.read(&mut buf).await.unwrap(), 6);
        assert_eq!(&buf, b"board?");
        link.write_all(b"dump!!").await.unwrap();
        assert_eq!(&client.await.unwrap(), b"dump!!");
    }

    #[tokio::test]
    async fn client_close_is_reported_as_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            drop(stream);
        });
        let (stream, _) = listener.accept().await.unwrap();
        let mut link = TcpLink::new(stream);
        client.await.unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(link.read(&mut buf).await.unwrap(), 0);
    }
}
