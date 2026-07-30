use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
// use arc_swap::ArcSwap;
// use tokio::sync::Notify;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::time::{self, Duration};
use tokio::io::AsyncWriteExt;

pub struct TcpServer {
    port: usize,
    listener: TcpListener,
    limit_connections: Arc<Semaphore>,
    // packet_buf: Arc<ArcSwap<Vec<u8>>>,
    // notifyee: Arc<Notify>,
    pkt_sender: broadcast::Sender<Vec<u8>>,
    notify_shutdown: broadcast::Sender<()>,
}

impl TcpServer {
    pub async fn new(
        port: usize,
        max_clients: usize,
        // packet_buf: Arc<ArcSwap<Vec<u8>>>,
        // notifyee: Arc<Notify>,
        pkt_sender: broadcast::Sender<Vec<u8>>,
    ) -> crate::Result<TcpServer> {
        let addr = format!("{}:{}", "0.0.0.0", port);
        let listener = TcpListener::bind(&addr).await?;
        let (notify_shutdown, _) = broadcast::channel(1);

        let server = TcpServer {
            port,
            listener,
            limit_connections: Arc::new(Semaphore::new(max_clients.into())),
            // packet_buf,
            // notifyee,
            pkt_sender,
            notify_shutdown,
        };
        Ok(server)
    }
    async fn run(&mut self) -> crate::Result<()> {
        println!("listen on port: {}", self.port);

        loop {
            let permit = self
                .limit_connections
                .clone()
                .acquire_owned()
                .await
                .unwrap();
            let socket = self.accept().await?;
            socket.set_nodelay(true)?;
            let ip_addr = socket.peer_addr().unwrap().to_string();

            let mut handler = SocketHandler {
                ip_addr,
                socket,
                // packet_buf: self.packet_buf.clone(),
                // notifyee: self.notifyee.clone(),
                pkt_receiver: self.pkt_sender.subscribe(),
                shutdown: AtomicBool::new(false),
                shutdown_signal: self.notify_shutdown.subscribe(),
            };

            tokio::spawn(async move {
                if let Err(err) = handler.run().await {
                    println!("Error! Connection error. {}", err);
                }
                drop(permit);
                drop(handler);
            });
        }
    }

    async fn accept(&mut self) -> crate::Result<TcpStream> {
        let mut backoff = 1;

        loop {
            match self.listener.accept().await {
                Ok((socket, addr)) => {
                    println!("connection from {}", addr);
                    return Ok(socket);
                }
                Err(err) => {
                    if backoff > 64 {
                        return Err(err.into());
                    }
                }
            }

            time::sleep(Duration::from_secs(backoff)).await;
            backoff *= 2;
        }
    }
}

pub struct SocketHandler {
    ip_addr: String,
    socket: TcpStream,
    // packet_buf: Arc<ArcSwap<Vec<u8>>>,
    // notifyee: Arc<Notify>,
    pkt_receiver: broadcast::Receiver<Vec<u8>>,
    shutdown: AtomicBool,
    shutdown_signal: broadcast::Receiver<()>,
}

impl SocketHandler {
    async fn run(&mut self) -> crate::Result<()> {
        while self.shutdown.load(Ordering::Relaxed) != true {
            // self.notifyee.notified().await;
            // let packet = self.packet_buf.load();
            let packet = self.pkt_receiver.recv().await?;
            tokio::select! {
                // res = self.socket.write_all(packet.as_ref()) => {
                res = self.socket.write_all(&packet) => {
                    if let Err(_) = res {
                        self.shutdown.store(true, Ordering::Relaxed);
                    }
                }
                _ = self.shutdown_signal.recv() => {
                    self.shutdown.store(true, Ordering::Relaxed);
                    return Ok(());
                }
            };
        }
        self.socket.shutdown().await?; //.unwrap();
        Ok(())
    }
}

impl Drop for SocketHandler {
    fn drop(&mut self) {
        println!("{} disconnected", self.ip_addr);
    }
}

// Run tcp server; SIGINT ('tokio::signal::ctrl_c()') can be used as 'shutdown' argument.
pub async fn start_server(
    port: usize,
    max_clients: usize,
    // packet_buf: Arc<ArcSwap<Vec<u8>>>,
    // notifyee: Arc<Notify>,
    packet_sender: broadcast::Sender<Vec<u8>>,
    shutdown: impl Future,
) {
    let mut server = TcpServer::new(
        port,
        max_clients,
        // packet_buf,
        // notifyee)
        packet_sender,)
        .await
        .unwrap();
    tokio::select! {
        res = server.run() => {
            if let Err(err) = res {
                println!("Error! Failed to accept connection. {}", err);
            }
        }
        _ = shutdown => {
            println!("Cleaning up tcp server");
        }
    }

    let TcpServer {
        // notifyee,
        notify_shutdown,
        ..
    } = server;
    // drop(notifyee);
    drop(notify_shutdown);
}
