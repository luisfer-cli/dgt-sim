//! Transports: how an external client talks to a virtual board.
//!
//! Two transports exist:
//!
//! * **PTY** — a pseudoterminal pair. The board owns the master side; the
//!   slave side (`/dev/pts/N`) is the endpoint a client opens, so anything
//!   that speaks to a serial port works unchanged.
//! * **TCP** — a loopback listener, one per board. Easier to script against and
//!   the only transport that reports genuine connection lifecycle.
//!
//! Neither is USB emulation. See `docs/compatibility.md`.

pub mod pty;
pub mod tcp;

use std::time::Duration;

use crate::error::Result;

use self::pty::PtyLink;
use self::tcp::TcpLink;

/// Which transport a board exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Pty,
    Tcp,
}

impl TransportKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Tcp => "tcp",
        }
    }
}

impl std::str::FromStr for TransportKind {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pty" => Ok(Self::Pty),
            "tcp" => Ok(Self::Tcp),
            other => Err(crate::error::Error::Transport(format!(
                "unknown transport {other:?} (expected pty or tcp)"
            ))),
        }
    }
}

/// A live byte pipe to a client.
#[derive(Debug)]
pub enum Link {
    Pty(PtyLink),
    Tcp(TcpLink),
}

impl Link {
    /// Read some bytes. `Ok(0)` means the peer went away.
    pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Pty(link) => link.read(buf).await,
            Self::Tcp(link) => link.read(buf).await,
        }
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Pty(link) => link.write_all(buf).await,
            Self::Tcp(link) => link.write_all(buf).await,
        }
    }

    /// Human-readable endpoint of this particular connection.
    pub fn peer(&self) -> String {
        match self {
            Self::Pty(link) => link.path().to_string_lossy().into_owned(),
            Self::Tcp(link) => link
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "tcp".into()),
        }
    }

    /// Ask the peer to flush before dropping the link.
    pub async fn shutdown(&mut self) {
        match self {
            Self::Pty(link) => link.shutdown().await,
            Self::Tcp(link) => {
                let _ = link.shutdown().await;
            }
        }
    }
}

/// Per-board fault injection. All fields default to "off" so that a freshly
/// created board behaves ideally.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Faults {
    /// Delay before every write.
    pub latency_ms: u64,
    /// Probability in `[0, 1]` that a fragment of the outgoing stream is lost.
    pub drop_rate: f64,
    /// Maximum fragment size; `0` disables fragmentation.
    pub fragment_max: usize,
    /// Truncate every outgoing message in the middle, as a flaky read would.
    pub truncate: bool,
    /// Seed for the deterministic fault PRNG.
    pub seed: u64,
    /// Probability in `[0, 1]` of a spontaneous link drop after a write.
    pub disconnect_rate: f64,
}

impl Default for Faults {
    fn default() -> Self {
        Self {
            latency_ms: 0,
            drop_rate: 0.0,
            fragment_max: 0,
            truncate: false,
            seed: 0,
            disconnect_rate: 0.0,
        }
    }
}

impl Faults {
    /// Are any faults active?
    pub fn is_clean(&self) -> bool {
        self.latency_ms == 0
            && self.drop_rate <= 0.0
            && self.fragment_max == 0
            && !self.truncate
            && self.disconnect_rate <= 0.0
    }

    pub fn validate(&self) -> Result<()> {
        for (name, rate) in [
            ("drop-rate", self.drop_rate),
            ("disconnect-rate", self.disconnect_rate),
        ] {
            if !(0.0..=1.0).contains(&rate) || rate.is_nan() {
                return Err(crate::error::Error::Other(format!(
                    "--{name} must be within 0.0..=1.0, got {rate}"
                )));
            }
        }
        if self.fragment_max > 1024 {
            return Err(crate::error::Error::Other(
                "--fragment-max must be between 0 and 1024".into(),
            ));
        }
        Ok(())
    }
}

/// Deterministic PRNG (xorshift64*). Reproducible fault injection is the whole
/// point, so a hand-rolled generator beats pulling in `rand` plus its seeding
/// rules.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[lo, hi]`.
    pub fn range_inclusive(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo + 1) as u64) as usize
    }
}

/// Split `data` into fragments according to the fault configuration. Returns
/// the fragments that survive the drop rule, plus whether the trailing bytes
/// were cut off.
pub fn fragment(data: &[u8], faults: &Faults, rng: &mut Rng) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    let pieces: Vec<&[u8]> = if faults.fragment_max == 0 {
        vec![data]
    } else {
        let max = faults.fragment_max.max(1);
        data.chunks(max).collect()
    };

    let mut out = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.iter().enumerate() {
        if faults.drop_rate > 0.0 && rng.next_f64() < faults.drop_rate {
            continue;
        }
        let last = index == pieces.len() - 1;
        if last && faults.truncate && piece.len() > 1 {
            out.push(piece[..piece.len() / 2].to_vec());
        } else {
            out.push(piece.to_vec());
        }
        if faults.fragment_max > 0 && !last {
            // Leave the fragment boundary visible to jittery clients.
            continue;
        }
    }
    out
}

/// Sleep helper so the board task only pays for latency when configured.
pub async fn apply_latency(faults: &Faults) {
    if faults.latency_ms > 0 {
        tokio::time::sleep(Duration::from_millis(faults.latency_ms)).await;
    }
}

/// A `TcpListener` accept loop turned into a stream of [`Link`]s.
pub async fn accept_loop(
    listener: tokio::net::TcpListener,
    link_tx: tokio::sync::mpsc::Sender<Link>,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let _ = stream.set_nodelay(true);
                if !enabled.load(std::sync::atomic::Ordering::Relaxed) {
                    // The board is "disconnected": accept and immediately drop
                    // so clients see a closed connection rather than hanging.
                    drop(stream);
                    continue;
                }
                if link_tx.send(Link::Tcp(TcpLink::new(stream))).await.is_err() {
                    break;
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "tcp accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_for_a_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = Rng::new(43);
        assert_ne!(Rng::new(42).next_u64(), c.next_u64());
    }

    #[test]
    fn clean_faults_pass_the_stream_through_untouched() {
        let faults = Faults::default();
        assert!(faults.is_clean());
        let mut rng = Rng::new(1);
        let data = vec![1u8, 2, 3, 4, 5];
        assert_eq!(fragment(&data, &faults, &mut rng), vec![data.clone()]);
    }

    #[test]
    fn fragmentation_splits_into_bounded_chunks() {
        let faults = Faults {
            fragment_max: 2,
            ..Faults::default()
        };
        let mut rng = Rng::new(1);
        let out = fragment(&[0, 1, 2, 3, 4], &faults, &mut rng);
        assert_eq!(out, vec![vec![0, 1], vec![2, 3], vec![4]]);
    }

    #[test]
    fn total_drop_rate_loses_everything_and_zero_drop_rate_loses_nothing() {
        let all = Faults {
            drop_rate: 1.0,
            fragment_max: 3,
            ..Faults::default()
        };
        let mut rng = Rng::new(7);
        assert!(fragment(&[0; 9], &all, &mut rng).is_empty());

        let none = Faults {
            drop_rate: 0.0,
            ..Faults::default()
        };
        let mut rng = Rng::new(7);
        assert_eq!(fragment(&[0; 9], &none, &mut rng), vec![vec![0; 9]]);
    }

    #[test]
    fn drop_rate_is_reproducible_for_a_seed() {
        let faults = Faults {
            drop_rate: 0.5,
            fragment_max: 1,
            ..Faults::default()
        };
        let collect = || {
            let mut rng = Rng::new(0xdead_beef);
            fragment(&[0u8; 64], &faults, &mut rng)
        };
        assert_eq!(collect(), collect());
    }

    #[test]
    fn truncate_cuts_the_final_fragment() {
        let faults = Faults {
            truncate: true,
            ..Faults::default()
        };
        let mut rng = Rng::new(3);
        let out = fragment(&[0, 1, 2, 3, 4, 5], &faults, &mut rng);
        assert_eq!(out, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn fault_config_is_validated() {
        assert!(Faults::default().validate().is_ok());
        assert!(
            Faults {
                drop_rate: 1.5,
                ..Faults::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Faults {
                drop_rate: -0.1,
                ..Faults::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Faults {
                fragment_max: 9999,
                ..Faults::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn transport_kind_parsing() {
        use std::str::FromStr;
        assert_eq!(TransportKind::from_str("pty").unwrap(), TransportKind::Pty);
        assert_eq!(TransportKind::from_str("TCP").unwrap(), TransportKind::Tcp);
        assert!(TransportKind::from_str("usb").is_err());
    }
}
