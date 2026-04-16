use bytes::Bytes;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{broadcast, Semaphore};
use tokio::time::{Duration, Instant};

// ── UDP Server ──

pub async fn start_udp_server(
    port: usize,
    max_clients: usize,
    pkt_sender: broadcast::Sender<Bytes>,
    shutdown: impl Future,
) {
    let socket = Arc::new(
        UdpSocket::bind(format!("0.0.0.0:{}", port))
            .await
            .expect("Failed to bind UDP socket"),
    );
    println!("UDP server listening on port {}", port);

    let clients: Arc<tokio::sync::Mutex<HashMap<SocketAddr, Instant>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // Registration listener task
    let reg_socket = socket.clone();
    let reg_clients = clients.clone();
    let reg_handle = tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            match reg_socket.recv_from(&mut buf).await {
                Ok((_, addr)) => {
                    let mut map = reg_clients.lock().await;
                    if map.len() < max_clients || map.contains_key(&addr) {
                        map.insert(addr, Instant::now());
                    }
                }
                Err(e) => {
                    eprintln!("UDP registration error: {}", e);
                }
            }
        }
    });

    // Broadcast sender task
    let send_socket = socket.clone();
    let send_clients = clients.clone();
    let mut receiver = pkt_sender.subscribe();
    let send_handle = tokio::spawn(async move {
        loop {
            let packet = match receiver.recv().await {
                Ok(pkt) => pkt,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("UDP broadcast lagged by {} packets", n);
                    continue;
                }
                Err(_) => break,
            };

            let mut map = send_clients.lock().await;
            let now = Instant::now();
            map.retain(|_, last_seen| now.duration_since(*last_seen) < Duration::from_secs(5));

            for addr in map.keys() {
                let _ = send_socket.send_to(&packet, addr).await;
            }
        }
    });

    shutdown.await;
    println!("Shutting down UDP server");
    reg_handle.abort();
    send_handle.abort();
}

// ── TCP Server ──

pub async fn start_tcp_server(
    port: usize,
    max_clients: usize,
    pkt_sender: broadcast::Sender<Bytes>,
    shutdown: impl Future,
) {
    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("Failed to bind TCP listener");
    println!("TCP server listening on port {}", port);

    let semaphore = Arc::new(Semaphore::new(max_clients));
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    let accept_handle = tokio::spawn({
        let shutdown_tx = shutdown_tx.clone();
        let pkt_sender = pkt_sender.clone();
        let semaphore = semaphore.clone();
        async move {
            loop {
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let (socket, addr) = match listener.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("TCP accept error: {}", e);
                        continue;
                    }
                };
                println!("TCP connection from {}", addr);
                let _ = socket.set_nodelay(true);

                let mut receiver = pkt_sender.subscribe();
                let mut shutdown_rx = shutdown_tx.subscribe();

                tokio::spawn(async move {
                    let mut socket = socket;
                    loop {
                        tokio::select! {
                            result = receiver.recv() => {
                                match result {
                                    Ok(packet) => {
                                        if socket.write_all(&packet).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                    Err(_) => break,
                                }
                            }
                            _ = shutdown_rx.recv() => break,
                        }
                    }
                    println!("{} disconnected", addr);
                    drop(permit);
                });
            }
        }
    });

    shutdown.await;
    println!("Shutting down TCP server");
    let _ = shutdown_tx.send(());
    accept_handle.abort();
}

/// Start the appropriate server based on protocol config.
pub async fn start_server(
    protocol: &str,
    port: usize,
    max_clients: usize,
    pkt_sender: broadcast::Sender<Bytes>,
    shutdown: impl Future,
) {
    match protocol {
        "udp" => start_udp_server(port, max_clients, pkt_sender, shutdown).await,
        "tcp" => start_tcp_server(port, max_clients, pkt_sender, shutdown).await,
        other => panic!("Unknown sender protocol: {}", other),
    }
}
