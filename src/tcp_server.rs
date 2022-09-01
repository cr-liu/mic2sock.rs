use std::sync::Arc;
use std::future::Future;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Semaphore, Notify};
use tokio::time::{self, Duration};

use crate::{ Connection, Shutdown };

pub struct TcpServer {
    port: u16,
    listener: TcpListener,
    limit_connections: Arc<Semaphore>,
    notify_data_ready: Arc<Notify>,
    notify_shutdown: broadcast::Sender<()>,
}

impl TcpServer {
    pub async fn new(port: u16, max_clients: u16, notify_data_ready: Arc<Notify>) -> crate::Result<TcpServer> {
        let addr = format!("{}:{}", "127.0.0.1", port);
        let listener = TcpListener::bind(addr).await?;
        let (notify_shutdown, _) = broadcast::channel(1);

        let server = TcpServer {
            port,
            listener,
            limit_connections: Arc::new(Semaphore::new(max_clients.into())),
            notify_data_ready,
            notify_shutdown,
        };
        Ok(server)
    }
    async fn run(&mut self) -> crate::Result<()> {
        // info!("listen on port: {}", self.port;
        println!("listen on port: {}", self.port);

        loop {
            let permit = self.limit_connections.clone().acquire_owned().await.unwrap();
            let socket = self.accept().await?;
            socket.set_nodelay(true)?;
            let ip_addr = socket.peer_addr().unwrap().to_string();
            let notified_data_ready = self.notify_data_ready.clone();
            let mut connection = Connection::new(socket);
            connection.set_mic_id();

            let mut handler = ConnectionHandler {
                connection,
                ip_addr,
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),
                notified_data_ready,
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
    shutdown: Shutdown,
    notified_data_ready: Arc<Notify>,
}

impl ConnectionHandler {
    // todo: return Result<()>
    async fn run(&mut self) -> crate::Result<()> {
        while !self.shutdown.is_shutdown() {
            // self.notified_data_ready.notified().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            tokio::select! {
                res = self.connection.write_packet() => { return res; }
                _ = self.shutdown.recv() => { return Ok(()); }
            };
        }
        return Ok(());
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        println!("{} disconnected", self.ip_addr);
    }
}

// Run tcp server; SIGINT ('tokio::signal::ctrl_c()') can be used as 'shutdown' argument.
pub async fn start_server(port: u16, max_clients: u16, notify_data_ready: Arc<Notify>, shutdown: impl Future) {
    let mut server = TcpServer::new(port, max_clients, notify_data_ready).await.unwrap();
    tokio::select! {
        res = server.run() => {
            if let Err(err) = res {
                println!("Error! Failed to accept connection. {}", err);
            }
        }
        _ = shutdown => {
            println!("\ncleaning up tcp server");
        }
    }
}
