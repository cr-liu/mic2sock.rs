//! The output end of the pipeline: serve the black-box consumer over localhost.

use bytes::Bytes;
use std::io;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

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
    /// Kept small on purpose, so backpressure appears in our code rather than
    /// inside a multi-megabyte kernel buffer.
    sndbuf_packets: usize,
    pkt_len: usize,
}

impl Sink {
    pub async fn bind(addr: &str, sndbuf_packets: usize, pkt_len: usize) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Sink {
            listener,
            sndbuf_packets,
            pkt_len,
        })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts one consumer at a time and forwards everything `rx` yields.
    ///
    /// While no consumer is attached the stream is discarded rather than queued:
    /// there is no release clock without a consumer, so queueing would grow
    /// without bound.
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
                            let _ = sock.set_nodelay(true);
                            let want = self.pkt_len * self.sndbuf_packets;
                            if let Err(e) =
                                socket2::SockRef::from(&sock).set_send_buffer_size(want)
                            {
                                eprintln!("sink: could not set SO_SNDBUF to {}: {}", want, e);
                            }
                            if current.is_some() {
                                eprintln!("sink: {} replaces the previous consumer", peer);
                            } else {
                                eprintln!("sink: consumer connected from {}", peer);
                            }
                            // Reassigning `current` drops the old stream here,
                            // which closes it -- that close is what tells a
                            // displaced consumer it has been replaced.
                            current = Some(sock);
                        }
                        Err(e) => eprintln!("sink: accept failed: {}", e),
                    }
                }
                item = rx.recv() => {
                    let Some(pkt) = item else { return };
                    if let Some(sock) = current.as_mut() {
                        // write_all is what applies backpressure, and with a
                        // deliberately small SO_SNDBUF it returns only once the
                        // consumer has actually taken the data. That is the
                        // release clock.
                        if let Err(e) = sock.write_all(&pkt).await {
                            eprintln!("sink: consumer write failed: {}", e);
                            current = None;
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
        let sink = Sink::bind("127.0.0.1:0", 3, 4).await.unwrap();
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
        let sink = Sink::bind("127.0.0.1:0", 3, 4).await.unwrap();
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
        let sink = Sink::bind("127.0.0.1:0", 3, 4).await.unwrap();
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
