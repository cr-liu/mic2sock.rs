use arc_swap::ArcSwap;
use bytes::BytesMut;
use std::future::Future;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Notify, Semaphore};
use tokio::time::{self, Duration};

use crate::Connection;

pub struct TcpServer {
    port: u16,
    listener: TcpListener,
    limit_connections: Arc<Semaphore>,
    notify_data_ready: Arc<Notify>,
    data_to_send: Arc<ArcSwap<BytesMut>>,
    notify_shutdown: broadcast::Sender<()>,
    shutdown_complete_tx: mpsc::Sender<()>,
    shutdown_complete_rx: mpsc::Receiver<()>,
}

impl TcpServer {
    pub async fn new(
        port: u16,
        max_clients: u16,
        notify_data_ready: Arc<Notify>,
        data_to_send: Arc<ArcSwap<BytesMut>>,
    ) -> crate::Result<TcpServer> {
        let addr = format!("{}:{}", "127.0.0.1", port);
        let listener = TcpListener::bind(&addr).await?;
        let (notify_shutdown, _) = broadcast::channel(1);
        let (shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel(1);

        let server = TcpServer {
            port,
            listener,
            limit_connections: Arc::new(Semaphore::new(max_clients.into())),
            notify_data_ready,
            data_to_send,
            notify_shutdown,
            shutdown_complete_tx,
            shutdown_complete_rx,
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
            let connection = Connection::new(socket, self.data_to_send.clone());

            let mut handler = ConnectionHandler {
                connection,
                ip_addr,
                notified_data_ready: self.notify_data_ready.clone(),
                shutdown: false,
                shutdown_signal: self.notify_shutdown.subscribe(),
                _shutdown_complete: self.shutdown_complete_tx.clone(),
            };

            tokio::spawn(async move {
                if let Err(err) = handler.run().await {
                    println!("Error! Connection error. {}", err);
                }
                drop(permit);
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

pub struct ConnectionHandler {
    connection: Connection,
    ip_addr: String,
    notified_data_ready: Arc<Notify>,
    shutdown: bool,
    shutdown_signal: broadcast::Receiver<()>,
    _shutdown_complete: mpsc::Sender<()>,
}

impl ConnectionHandler {
    // todo: return Result<()>
    async fn run(&mut self) -> crate::Result<()> {
        while !self.shutdown {
            self.notified_data_ready.notified().await;
            tokio::select! {
                _res = self.connection.write_packet() => {
                    // return res;
                }
                _ = self.shutdown_signal.recv() => {
                    self.shutdown = true;
                    return Ok(());
                }
            };
        }
        Ok(())
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        println!("{} disconnected", self.ip_addr);
    }
}

// Run tcp server; SIGINT ('tokio::signal::ctrl_c()') can be used as 'shutdown' argument.
pub async fn start_server(
    port: u16,
    max_clients: u16,
    notify_data_ready: Arc<Notify>,
    data_to_send: Arc<ArcSwap<BytesMut>>,
    shutdown: impl Future,
) {
    let mut server = TcpServer::new(port, max_clients, notify_data_ready, data_to_send)
        .await
        .unwrap();
    tokio::select! {
        res = server.run() => {
            if let Err(err) = res {
                println!("Error! Failed to accept connection. {}", err);
            }
        }
        _ = shutdown => {
            println!("cleaning up tcp server");
        }
    }

    let TcpServer {
        mut shutdown_complete_rx,
        shutdown_complete_tx,
        notify_shutdown,
        ..
    } = server;
    drop(notify_shutdown);
    drop(shutdown_complete_tx);
    shutdown_complete_rx.recv().await;
}
