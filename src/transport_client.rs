use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncReadExt;
use tokio::net::{self, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

// ── UDP Client ──

async fn udp_client_loop(
    host: &str,
    port: usize,
    pkt_size: usize,
    sender: mpsc::Sender<Vec<u8>>,
    shutdown: &AtomicBool,
) {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind UDP client socket");

    let server_addr = format!("{}:{}", host, port);
    // Connect to server so we can use recv() instead of recv_from()
    socket.connect(&server_addr).await.expect("Failed to connect UDP socket");
    // Send initial registration
    let _ = socket.send(b"register").await;

    let mut buf = vec![0u8; pkt_size];
    let mut registration_interval = time::interval(Duration::from_secs(2));

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            result = socket.recv(&mut buf) => {
                match result {
                    Ok(n) if n == pkt_size => {
                        let _ = sender.send(buf[..n].to_vec()).await;
                    }
                    Ok(n) => {
                        eprintln!("UDP client: unexpected packet size {} (expected {})", n, pkt_size);
                    }
                    Err(e) => {
                        eprintln!("UDP client recv error: {}", e);
                    }
                }
            }
            _ = registration_interval.tick() => {
                let _ = socket.send(b"register").await;
            }
        }
    }
}

// ── TCP Client ──

async fn tcp_client_loop(
    host: &str,
    port: usize,
    pkt_size: usize,
    sender: mpsc::Sender<Vec<u8>>,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let addr = format!("{}:{}", host, port);
        match TcpStream::connect(&addr).await {
            Ok(mut stream) => {
                println!("TCP client connected to {}", addr);
                let mut pkt_buf = Vec::<u8>::with_capacity(pkt_size * 2);
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    match stream.read_buf(&mut pkt_buf).await {
                        Ok(0) => break,
                        Ok(_) => {
                            // Process all complete packets in the buffer
                            while pkt_buf.len() >= pkt_size {
                                let _ = sender.send(pkt_buf[..pkt_size].to_vec()).await;
                                pkt_buf.drain(..pkt_size);
                            }
                        }
                        Err(e) => {
                            eprintln!("TCP client read error: {}", e);
                            break;
                        }
                    }
                }
                println!("TCP client disconnected from {}", addr);
            }
            Err(_) => {
                if net::lookup_host(&addr).await.is_err() {
                    return;
                }
                time::sleep(Duration::from_secs(2)).await;
                println!("TCP client reconnecting...");
            }
        }
    }
}

/// Start the appropriate client based on protocol config.
pub async fn start_client(
    protocol: &str,
    host: &str,
    port: usize,
    pkt_size: usize,
    sender: mpsc::Sender<Vec<u8>>,
    shutdown: impl Future,
) {
    let stop = AtomicBool::new(false);

    tokio::select! {
        _ = async {
            match protocol {
                "udp" => udp_client_loop(host, port, pkt_size, sender, &stop).await,
                "tcp" => tcp_client_loop(host, port, pkt_size, sender, &stop).await,
                other => panic!("Unknown receiver protocol: {}", other),
            }
        } => {}
        _ = shutdown => {
            stop.store(true, Ordering::Relaxed);
            println!("Transport client shutting down");
        }
    }
}
