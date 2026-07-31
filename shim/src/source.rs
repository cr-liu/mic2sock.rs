//! The input end of the pipeline: reassemble fixed-size packets out of a TCP byte
//! stream, reconnect with backoff, and cross-check the result against the sender's
//! own invariants.
//!
//! There is deliberately no `PacketSource` trait. `TcpSource` is the only
//! implementor -- a UDP source is deferred on purpose, because 90% of the shim
//! (jitter buffer, clock, reframing, metrics) is shared between transports and
//! whether UDP is worth adding should be decided from the arrival-delay data this
//! shim collects, not assumed up front. The natural way to write the trait --
//! `fn run(self, tx) -> impl std::future::Future<Output = ()> + Send` in trait
//! position -- needs Rust 1.75 (`impl Trait` in return position in a trait), while
//! `shim/Cargo.toml` declares `rust-version = "1.65"`; compiling it here anyway
//! would make that declaration a silent lie. An inherent `async fn` (stable since
//! 1.39) says the same thing without the false MSRV claim. Add the trait back once
//! a second transport actually exists and its shared shape is known -- not before.

use bytes::Bytes;
use protocol::{Backoff, Header};
use std::io;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

/// What the source reports, in order.
///
/// A reconnect has to travel through the *same* channel as the packets. Signalled
/// out of band, a packet queued before the reconnect could be handled after it, and
/// the jitter buffer would then apply new-generation evidence to old-generation
/// audio -- the ordering hazard is the whole reason this is an enum.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceEvent {
    /// A connection was established, including the first. `JitterBuffer::on_source_reconnect`
    /// needs this: it is the only evidence that a backward packet-id jump is a sender
    /// restart rather than a replay of audio already emitted.
    Connected,
    Packet(Bytes),
}

/// How many framed packets, right after a connect, get cross-checked against the
/// sender's own invariants (`pkt_id` advances by exactly 1, `ms` is a real
/// millisecond value). A wrong `n_ch` / `spp_out` in the config slices the byte
/// stream at the wrong boundaries and turns every "header" into garbage, so this
/// is the only way to catch that mistake -- the config alone cannot, because the
/// consumer it has to match is closed-source. Bounded to a handful of packets: enough
/// that a coincidental match is implausible, small enough not to delay startup or
/// keep re-checking a stream that has already proven itself.
///
/// The check is **fatal**, so "advances by exactly 1" had better not have legitimate
/// exceptions. It does not, and that is a fact about the sender rather than a hope:
/// `mic2sock`'s `SocketHandler::run` does `self.pkt_receiver.recv().await?` on a
/// `broadcast` channel of capacity 16, so a client that falls behind is answered with
/// `Lagged` and the `?` **drops the connection** — it is never served a stream with
/// ids skipped. Within one connection the sequence is therefore contiguous, and across
/// connections this check restarts, so a sender that legitimately restarted its ids is
/// not flagged. (The `ms` bound is likewise the sender's own: it borrows from `secs`
/// when back-dating would make `ms` negative, so `ms` is always in `0..1000`.)
const GEOMETRY_CHECK_PACKETS: usize = 8;

/// Reassembles a fixed-size packet stream from a byte stream.
///
/// TCP gives no message boundaries, so this accumulates until exactly one packet
/// is present. It is a separate type from the IO so it can be tested without a
/// socket.
pub struct Framer {
    pkt_len: usize,
    buf: Vec<u8>,
}

impl Framer {
    pub fn new(pkt_len: usize) -> Self {
        assert!(pkt_len > 0, "pkt_len must be > 0");
        Framer {
            pkt_len,
            buf: Vec::with_capacity(pkt_len * 2),
        }
    }

    /// Appends `data` and returns every complete packet it completed.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Bytes> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while self.buf.len() >= self.pkt_len {
            let rest = self.buf.split_off(self.pkt_len);
            out.push(Bytes::from(std::mem::replace(&mut self.buf, rest)));
        }
        out
    }

    /// Bytes held that do not yet form a packet.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Discards a partial packet. Called on disconnect so a torn packet cannot
    /// fuse onto the next connection's first one.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

/// A running count of the geometry cross-check described at [`GEOMETRY_CHECK_PACKETS`].
///
/// A fresh one is created per connection (see `TcpSource::pump`), which is what
/// makes the check reset on every reconnect: a partial packet is not the only thing
/// that must not survive a dead connection -- neither may its last `pkt_id`, or the
/// first packet of a sender that legitimately restarted (and so legitimately
/// restarted its own id sequence) would be flagged as a geometry mismatch instead of
/// recognised as a new generation.
struct GeometryCheck {
    checked: usize,
    last_pkt_id: Option<i32>,
}

impl GeometryCheck {
    fn new() -> Self {
        GeometryCheck {
            checked: 0,
            last_pkt_id: None,
        }
    }

    /// Validates one more packet, if the budget in [`GEOMETRY_CHECK_PACKETS`] is not
    /// already spent. Returns a description of the mismatch on failure.
    fn check(&mut self, pkt: &Bytes) -> Result<(), String> {
        if self.checked >= GEOMETRY_CHECK_PACKETS {
            return Ok(());
        }
        let h = match Header::parse(pkt) {
            Some(h) => h,
            None => {
                return Err(format!(
                    "source geometry mismatch: a framed packet ({} bytes) is too short \
                     to hold a header; check n_ch / spp_out",
                    pkt.len()
                ))
            }
        };
        if !(0..1000).contains(&h.ms) {
            return Err(format!(
                "source geometry mismatch: header ms={} is not a millisecond value \
                 (pkt_id={}); check n_ch / spp_out",
                h.ms, h.pkt_id
            ));
        }
        if let Some(prev) = self.last_pkt_id {
            // The sender's own wrap rule: i32::MAX never appears, so the successor
            // of i32::MAX - 1 is 0. Only the *step* is checkable, not the absolute
            // value -- the shim joins an already-running stream, so there is no
            // baseline to compare the first id against.
            let expected = if prev == i32::MAX - 1 { 0 } else { prev + 1 };
            if h.pkt_id != expected {
                return Err(format!(
                    "source geometry mismatch: pkt_id jumped from {} to {} (expected {}); \
                     check n_ch / spp_out",
                    prev, h.pkt_id, expected
                ));
            }
        }
        self.last_pkt_id = Some(h.pkt_id);
        self.checked += 1;
        Ok(())
    }
}

/// A `TcpSource::pump` failure.
///
/// `Io` is transient -- the same disconnect/timeout path `run` has always retried.
/// `Fatal` is a geometry mismatch: continuing would mean forwarding mis-framed audio
/// forever, which is worse than stopping, so it propagates out of `run` instead of
/// being retried.
enum PumpError {
    Io(io::Error),
    Fatal(String),
}

impl From<io::Error> for PumpError {
    fn from(e: io::Error) -> Self {
        PumpError::Io(e)
    }
}

/// Reads a fixed-size packet stream from the Pi over TCP, reconnecting forever
/// unless the geometry cross-check fails (see [`GEOMETRY_CHECK_PACKETS`]), in which
/// case retrying would just repeat the same mis-framing.
pub struct TcpSource {
    host: String,
    port: u16,
    pkt_len: usize,
    read_timeout: Duration,
}

impl TcpSource {
    pub fn new(host: String, port: u16, pkt_len: usize) -> Self {
        TcpSource {
            host,
            port,
            pkt_len,
            read_timeout: Duration::from_millis(2000),
        }
    }

    /// Overrides how long a silent link may stay silent before being declared
    /// dead. `SO_KEEPALIVE` is off by default and `tcp_keepalive_time` is two
    /// hours, so without an application-level timeout a silent Wi-Fi drop hangs
    /// the read indefinitely.
    pub fn with_read_timeout(mut self, d: Duration) -> Self {
        self.read_timeout = d;
        self
    }

    /// Drains one connection: frames packets, validates the first few against the
    /// sender's invariants, and forwards the rest untouched. Returns once the peer
    /// closes cleanly, on any IO error (retryable), or on a geometry mismatch (not).
    ///
    /// A fresh `Framer` and `GeometryCheck` are created here, every call, which is
    /// what makes both reset on reconnect: a call happens once per connection.
    async fn pump(
        &self,
        sock: &mut TcpStream,
        tx: &mpsc::Sender<SourceEvent>,
    ) -> Result<(), PumpError> {
        let mut framer = Framer::new(self.pkt_len);
        let mut geometry = GeometryCheck::new();
        let mut chunk = vec![0u8; self.pkt_len.min(65536)];
        loop {
            let n = match time::timeout(self.read_timeout, sock.read(&mut chunk)).await {
                Err(_) => {
                    return Err(PumpError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no data within the read timeout",
                    )))
                }
                Ok(r) => r?,
            };
            if n == 0 {
                return Ok(());
            }
            for p in framer.feed(&chunk[..n]) {
                if let Err(msg) = geometry.check(&p) {
                    return Err(PumpError::Fatal(msg));
                }
                if tx.send(SourceEvent::Packet(p)).await.is_err() {
                    return Ok(()); // pipeline shut down
                }
            }
        }
    }

    /// Runs until shutdown (`Ok`) or a fatal geometry mismatch (`Err`, meaning: do
    /// not retry, let the process exit non-zero instead of forwarding mis-framed
    /// audio forever). Every other failure -- connect refused, timeout, EOF -- is
    /// retried with backoff, as before.
    pub async fn run(self, tx: mpsc::Sender<SourceEvent>) -> Result<(), String> {
        // Seeded by port so several instances on one host do not reconnect in
        // lockstep.
        let mut backoff = Backoff::new(200, 5000, self.port as u64);
        loop {
            let addr = format!("{}:{}", self.host, self.port);
            match TcpStream::connect(&addr).await {
                Ok(mut sock) => {
                    let _ = sock.set_nodelay(true);
                    eprintln!("source: connected to {}", addr);
                    backoff.reset();
                    if tx.send(SourceEvent::Connected).await.is_err() {
                        return Ok(()); // pipeline shut down
                    }
                    match self.pump(&mut sock, &tx).await {
                        Ok(()) => {}
                        Err(PumpError::Io(e)) => eprintln!("source: {} ({})", e, addr),
                        Err(PumpError::Fatal(msg)) => return Err(msg),
                    }
                    eprintln!("source: disconnected from {}", addr);
                }
                Err(e) => eprintln!("source: connect to {} failed: {}", addr, e),
            }
            if tx.is_closed() {
                return Ok(());
            }
            // Backoff covers every disconnect path, not just connect failures.
            let d = backoff.next_delay();
            time::sleep(d).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn assembles_one_packet_from_many_small_reads() {
        let mut f = Framer::new(4);
        assert!(f.feed(&[1, 2]).is_empty());
        assert!(f.feed(&[3]).is_empty());
        let got = f.feed(&[4]);
        assert_eq!(got, vec![Bytes::from(vec![1, 2, 3, 4])]);
    }

    /// A single read may span several packets, and TCP makes no promise about
    /// alignment. All complete packets must come out, with the remainder kept.
    #[test]
    fn splits_a_read_containing_several_packets_and_keeps_the_remainder() {
        let mut f = Framer::new(4);
        let got = f.feed(&[1, 1, 1, 1, 2, 2, 2, 2, 3, 3]);
        assert_eq!(
            got,
            vec![Bytes::from(vec![1, 1, 1, 1]), Bytes::from(vec![2, 2, 2, 2])]
        );
        assert_eq!(f.pending(), 2);
        let got = f.feed(&[3, 3]);
        assert_eq!(got, vec![Bytes::from(vec![3, 3, 3, 3])]);
        assert_eq!(f.pending(), 0);
    }

    #[test]
    fn reset_discards_a_partial_packet() {
        let mut f = Framer::new(4);
        f.feed(&[9, 9]);
        assert_eq!(f.pending(), 2);
        f.reset();
        assert_eq!(f.pending(), 0);
        // A partial packet from a dead connection must not fuse onto the next
        // connection's first packet.
        let got = f.feed(&[1, 2, 3, 4]);
        assert_eq!(got, vec![Bytes::from(vec![1, 2, 3, 4])]);
    }

    /// Builds a real, geometry-valid header-only packet (`pkt_len == HEADER_LEN`,
    /// no payload) so tests can drive `TcpSource` -- which now runs every packet
    /// through the geometry cross-check -- without also carrying real audio.
    fn header_packet(pkt_id: i32, ms: i16) -> [u8; protocol::HEADER_LEN] {
        let mut buf = [0u8; protocol::HEADER_LEN];
        Header {
            device_id: 1,
            secs: 100,
            ms,
            pkt_id,
        }
        .write_to(&mut buf);
        buf
    }

    /// End-to-end over a real socket: a fake sender emits three packets and the
    /// source must deliver a `Connected` first, then exactly those packets.
    #[tokio::test]
    async fn tcp_source_delivers_packets_from_a_real_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            for id in 0i32..3 {
                sock.write_all(&header_packet(id, id as i16)).await.unwrap();
            }
            // Hold the connection open briefly so the reader drains.
            time::sleep(Duration::from_millis(200)).await;
        });

        let (tx, mut rx) = mpsc::channel(16);
        let src = TcpSource::new(addr.ip().to_string(), addr.port(), protocol::HEADER_LEN);
        tokio::spawn(src.run(tx));

        assert_eq!(
            time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("timed out")
                .expect("channel closed"),
            SourceEvent::Connected
        );
        for id in 0i32..3 {
            let got = time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("timed out")
                .expect("channel closed");
            assert_eq!(
                got,
                SourceEvent::Packet(Bytes::copy_from_slice(&header_packet(id, id as i16)))
            );
        }
    }

    /// The whole point of the check: a wrong `n_ch` / `spp_out` slices the stream at
    /// the wrong boundaries, and the resulting "headers" do not advance by 1. This
    /// must be reported as fatal (not retried) rather than forwarded silently.
    #[tokio::test]
    async fn a_pkt_id_gap_is_a_fatal_configuration_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&header_packet(0, 0)).await.unwrap();
            sock.write_all(&header_packet(5, 1)).await.unwrap(); // skips 1..4
            time::sleep(Duration::from_millis(200)).await;
        });

        let (tx, _rx) = mpsc::channel(16);
        let src = TcpSource::new(addr.ip().to_string(), addr.port(), protocol::HEADER_LEN);
        let result = time::timeout(Duration::from_secs(3), src.run(tx))
            .await
            .expect("run did not stop for a fatal error");
        let msg = result.expect_err("a pkt_id gap must be fatal, not silently forwarded");
        assert!(
            msg.contains('5') && msg.contains("n_ch"),
            "message: {}",
            msg
        );
    }

    /// `ms` outside `0..1000` is not a millisecond value and cannot come from a
    /// correctly-framed header; this is the other half of the geometry check.
    #[tokio::test]
    async fn an_out_of_range_ms_is_a_fatal_configuration_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&header_packet(0, 1000)).await.unwrap();
            time::sleep(Duration::from_millis(200)).await;
        });

        let (tx, _rx) = mpsc::channel(16);
        let src = TcpSource::new(addr.ip().to_string(), addr.port(), protocol::HEADER_LEN);
        let result = time::timeout(Duration::from_secs(3), src.run(tx))
            .await
            .expect("run did not stop for a fatal error");
        let msg = result.expect_err("an out-of-range ms must be fatal");
        assert!(
            msg.contains("ms=1000") && msg.contains("n_ch"),
            "message: {}",
            msg
        );
    }

    /// The sender's own wrap rule (i32::MAX never appears, so its predecessor's
    /// successor is 0) must not be mistaken for a gap.
    #[tokio::test]
    async fn the_documented_pkt_id_wraparound_is_not_flagged() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&header_packet(i32::MAX - 1, 0))
                .await
                .unwrap();
            sock.write_all(&header_packet(0, 1)).await.unwrap();
            time::sleep(Duration::from_millis(200)).await;
        });

        let (tx, mut rx) = mpsc::channel(16);
        let src = TcpSource::new(addr.ip().to_string(), addr.port(), protocol::HEADER_LEN);
        tokio::spawn(src.run(tx));

        assert_eq!(rx.recv().await.unwrap(), SourceEvent::Connected);
        assert_eq!(
            rx.recv().await.unwrap(),
            SourceEvent::Packet(Bytes::copy_from_slice(&header_packet(i32::MAX - 1, 0)))
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            SourceEvent::Packet(Bytes::copy_from_slice(&header_packet(0, 1)))
        );
    }

    /// Checking is bounded to the first [`GEOMETRY_CHECK_PACKETS`] packets, so a
    /// stream that has already proven itself is not re-validated forever (and, in
    /// particular, `pkt_id` wrapping normally at `i32::MAX` much later must not be
    /// mistaken for a mismatch of a stream that was never checked there).
    #[tokio::test]
    async fn the_geometry_check_stops_after_the_first_eight_packets() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            for id in 0i32..8 {
                sock.write_all(&header_packet(id, 0)).await.unwrap();
            }
            // A gap that would be fatal if it fell inside the checked window.
            sock.write_all(&header_packet(100, 0)).await.unwrap();
            time::sleep(Duration::from_millis(200)).await;
        });

        let (tx, mut rx) = mpsc::channel(16);
        let src = TcpSource::new(addr.ip().to_string(), addr.port(), protocol::HEADER_LEN);
        tokio::spawn(src.run(tx));

        assert_eq!(rx.recv().await.unwrap(), SourceEvent::Connected);
        for id in 0i32..9 {
            let want = if id < 8 { id } else { 100 };
            let got = time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("timed out: the packet past the checked window was dropped, not forwarded")
                .expect("channel closed");
            assert_eq!(
                got,
                SourceEvent::Packet(Bytes::copy_from_slice(&header_packet(want, 0)))
            );
        }
    }

    /// The validation state must reset on reconnect just like `Framer` does: a
    /// sender restart legitimately restarts its own `pkt_id` sequence, and the first
    /// packet of the new connection must be judged on its own, not against the last
    /// `pkt_id` of a connection that is already gone.
    #[tokio::test]
    async fn geometry_validation_state_resets_on_reconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            {
                let (mut sock, _) = listener.accept().await.unwrap();
                sock.write_all(&header_packet(5000, 0)).await.unwrap();
                // Socket drops here, closing connection 1 (EOF for the client).
            }
            let (mut sock, _) = listener.accept().await.unwrap();
            // Id 0 would be a huge backward jump from 5000 if state carried over.
            sock.write_all(&header_packet(0, 1)).await.unwrap();
            time::sleep(Duration::from_millis(200)).await;
        });

        let (tx, mut rx) = mpsc::channel(16);
        let src = TcpSource::new(addr.ip().to_string(), addr.port(), protocol::HEADER_LEN)
            .with_read_timeout(Duration::from_millis(300));
        tokio::spawn(src.run(tx));

        assert_eq!(
            rx.recv().await.unwrap(),
            SourceEvent::Connected,
            "connection 1"
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            SourceEvent::Packet(Bytes::copy_from_slice(&header_packet(5000, 0)))
        );
        assert_eq!(
            time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("timed out reconnecting")
                .unwrap(),
            SourceEvent::Connected,
            "connection 2"
        );
        assert_eq!(
            time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("timed out")
                .expect(
                    "channel closed: the id reset across reconnect was wrongly flagged as fatal"
                ),
            SourceEvent::Packet(Bytes::copy_from_slice(&header_packet(0, 1)))
        );
    }

    /// A packet shorter than a header cannot have come from a correctly-configured
    /// framer; this is what would have happened to the original (pre-geometry-check)
    /// 4-byte test packets, which is why that test now frames whole headers instead.
    #[tokio::test]
    async fn a_packet_too_short_for_a_header_is_a_fatal_configuration_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&[1, 2, 3, 4]).await.unwrap();
            time::sleep(Duration::from_millis(200)).await;
        });

        let (tx, _rx) = mpsc::channel(16);
        let src = TcpSource::new(addr.ip().to_string(), addr.port(), 4);
        let result = time::timeout(Duration::from_secs(3), src.run(tx))
            .await
            .expect("run did not stop for a fatal error");
        let msg = result.expect_err("an unparsable header must be fatal");
        assert!(
            msg.contains("too short") && msg.contains("n_ch"),
            "message: {}",
            msg
        );
    }
}
