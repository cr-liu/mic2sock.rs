//! The output end of the pipeline: serve the black-box consumer over localhost.

use bytes::Bytes;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// SO_SNDBUF for the consumer's socket, in packets.
///
/// As load-bearing as the two-slot pipeline queue, and small for the same reason:
/// backpressure has to appear in our code rather than inside a multi-megabyte
/// kernel buffer, or the write stops being the release clock. The kernel may round
/// or double the byte value. One named constant, because retuning the clock's
/// tightness must not mean hunting bare `3`s across call sites and tests.
pub const SNDBUF_PACKETS: usize = 3;

/// Serves the consumer over localhost.
///
/// The release clock is the consumer's read rate: a packet is requested from the
/// pipeline only once the socket has accepted the previous one. That has three
/// benefits — no timer is needed, so Windows' 15.6 ms default timer resolution
/// is irrelevant; the pipeline locks automatically to the consumer's true audio
/// clock, since its internal buffer is bounded and therefore its long-run read
/// rate equals its device rate; and buffer depth becomes directly observable.
pub struct Sink {
    listener: TcpListener,
    pkt_len: usize,
    connected: Arc<AtomicBool>,
}

impl Sink {
    pub async fn bind(addr: &str, pkt_len: usize) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Sink {
            listener,
            pkt_len,
            connected: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Whether a consumer is attached right now.
    ///
    /// The pipeline needs this because a *discard* is indistinguishable from a write
    /// as far as the queue is concerned: with no consumer the sink drains everything
    /// it is given, which frees the queue, which the pipeline would read as permission
    /// to release the next packet. It would then empty the jitter buffer into a
    /// discard and arrive at the moment the consumer connects with nothing buffered —
    /// the exact opposite of spec §6.5, which is to hold a rolling window so the
    /// buffer is already primed when the consumer appears.
    ///
    /// Deliberately coarse. It gates whether to release at all, not *when*; the
    /// release timing is still the socket write. A stale read costs at most one
    /// discarded packet or one extra wait.
    pub fn connected(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.connected)
    }

    /// Configures and installs an accepted consumer.
    ///
    /// The one place `current` is ever set, paired with `drop_consumer` below, so
    /// the invariant `connected == current.is_some()` cannot be broken by a call
    /// site updating one without the other — it once held at only three of the
    /// four mutation sites, correct by coincidence at the fourth.
    fn install(&self, current: &mut Option<TcpStream>, sock: TcpStream) {
        let _ = sock.set_nodelay(true);
        let want = self.pkt_len * SNDBUF_PACKETS;
        if let Err(e) = socket2::SockRef::from(&sock).set_send_buffer_size(want) {
            eprintln!("sink: could not set SO_SNDBUF to {}: {}", want, e);
        }
        // Reassigning `current` drops any previous stream, which closes it — that
        // close is what tells a displaced consumer it has been replaced.
        *current = Some(sock);
        self.connected.store(true, Ordering::Relaxed);
    }

    fn drop_consumer(&self, current: &mut Option<TcpStream>) {
        *current = None;
        self.connected.store(false, Ordering::Relaxed);
    }

    /// Accepts one consumer at a time and forwards everything `rx` yields.
    ///
    /// While no consumer is attached the stream is discarded rather than queued:
    /// there is no release clock without a consumer, so queueing would grow without
    /// bound. The pipeline is told (see [`Sink::connected`]) so that it holds a
    /// rolling window instead of feeding this discard.
    pub async fn run(self, mut rx: mpsc::Receiver<Bytes>) {
        let mut current: Option<TcpStream> = None;
        loop {
            // `biased` is load-bearing, not a style choice: without it, select!
            // polls ready branches in random order, and if a connection is
            // already sitting in the accept backlog *and* a packet is already
            // queued by the time this task is first polled, it can process the
            // packet before the accept -- silently dropping it against a stale
            // `current` even though a consumer had, in fact, already connected.
            // Checking accept first makes connection state authoritative before
            // any packet-routing decision, every iteration.
            tokio::select! {
                biased;
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((sock, peer)) => {
                            if current.is_some() {
                                eprintln!("sink: {} replaces the previous consumer", peer);
                            } else {
                                eprintln!("sink: consumer connected from {}", peer);
                            }
                            self.install(&mut current, sock);
                        }
                        Err(e) => eprintln!("sink: accept failed: {}", e),
                    }
                }
                item = rx.recv() => {
                    let Some(pkt) = item else { return };
                    if let Some(sock) = current.as_mut() {
                        // The write is what applies backpressure, and with a
                        // deliberately small SO_SNDBUF it returns close to when the
                        // consumer takes the data. That is the release clock.
                        //
                        // It races `accept` because a consumer that connects and then
                        // stops reading blocks this write indefinitely, and without the
                        // race its replacement would wait behind it forever -- exactly
                        // the case newest-wins exists to recover from. Abandoning a
                        // half-written packet is safe *because* we are discarding that
                        // consumer: the partial bytes die with its socket, and a
                        // consumer that is being replaced cannot be corrupted by them.
                        let write = sock.write_all(&pkt);
                        tokio::select! {
                            biased;
                            res = write => {
                                if let Err(e) = res {
                                    eprintln!("sink: consumer write failed: {}", e);
                                    self.drop_consumer(&mut current);
                                }
                            }
                            accepted = self.listener.accept() => {
                                match accepted {
                                    Ok((sock, peer)) => {
                                        eprintln!(
                                            "sink: {} replaces a consumer that stopped reading",
                                            peer
                                        );
                                        self.install(&mut current, sock);
                                    }
                                    Err(e) => eprintln!("sink: accept failed: {}", e),
                                }
                            }
                        }
                    }
                    // else: no consumer, so discard.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn forwards_packets_to_a_connected_consumer() {
        let sink = Sink::bind("127.0.0.1:0", 4).await.unwrap();
        let addr = sink.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        tokio::spawn(sink.run(rx));

        let mut client = TcpStream::connect(addr).await.unwrap();
        for n in 1u8..=3 {
            tx.send(Bytes::from(vec![n; 4])).await.unwrap();
        }
        let mut got = [0u8; 12];
        timeout(Duration::from_secs(3), client.read_exact(&mut got))
            .await
            .expect("timed out")
            .unwrap();
        assert_eq!(got, [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]);
    }

    /// A second consumer replaces the first. The release clock is defined by the
    /// consumer, so two consumers would mean two conflicting clocks. Replacing
    /// rather than rejecting means a crashed consumer is not locked out by its
    /// own stale connection.
    #[tokio::test]
    async fn a_new_consumer_replaces_the_old_one() {
        let sink = Sink::bind("127.0.0.1:0", 4).await.unwrap();
        let addr = sink.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        tokio::spawn(sink.run(rx));

        let mut first = TcpStream::connect(addr).await.unwrap();
        tx.send(Bytes::from(vec![1u8; 4])).await.unwrap();
        let mut b = [0u8; 4];
        timeout(Duration::from_secs(3), first.read_exact(&mut b))
            .await
            .expect("timed out")
            .unwrap();

        let mut second = TcpStream::connect(addr).await.unwrap();

        // Do not send packet 2 right after `connect`: that would race `run`'s
        // select! between its accept branch (which reassigns `current` to
        // `second`) and its recv branch (which would write to whatever
        // `current` is at that moment). Both can be simultaneously ready on a
        // loopback connect, and select! picks a ready branch at random, so
        // sending immediately is nondeterministic -- occasionally packet 2
        // would land on the stale `first` instead of `second`.
        //
        // `run` drops the old stream synchronously while handling the accept
        // branch, so waiting for `first` to observe EOF is waiting for the
        // actual thing (the swap), not a fixed delay: once this read returns,
        // `current` is guaranteed to already be `second`.
        let n = timeout(Duration::from_secs(3), first.read(&mut b))
            .await
            .expect("the old consumer was never displaced")
            .unwrap();
        assert_eq!(n, 0, "the displaced consumer was not disconnected");

        tx.send(Bytes::from(vec![2u8; 4])).await.unwrap();
        timeout(Duration::from_secs(3), second.read_exact(&mut b))
            .await
            .expect("second consumer got nothing")
            .unwrap();
        assert_eq!(b, [2u8; 4]);
    }

    /// With no consumer attached, sends must not block the pipeline forever.
    #[tokio::test]
    async fn discards_while_no_consumer_is_attached() {
        let sink = Sink::bind("127.0.0.1:0", 4).await.unwrap();
        let (tx, rx) = mpsc::channel::<Bytes>(4);
        tokio::spawn(sink.run(rx));
        for n in 0u8..20 {
            timeout(Duration::from_millis(500), tx.send(Bytes::from(vec![n; 4])))
                .await
                .expect("pipeline blocked with no consumer attached")
                .unwrap();
        }
    }
}
