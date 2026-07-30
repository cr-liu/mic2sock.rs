use crate::Config;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
// use crossbeam::channel::Sender;
use tokio::sync::mpsc::Sender;
use tokio::io::AsyncReadExt;
use tokio::time::{self, Duration};
use tokio::net::{self, TcpStream};


pub struct TcpClient {
    host: String,
    port: usize,
    pkt_size: usize,
    resend: Sender<Vec<u8>>,
    shutdown: AtomicBool,
}

impl TcpClient {
    pub async fn inf_run(&mut self) -> crate::Result<()> {
        while self.shutdown.load(Ordering::Relaxed) != true {
            let addr = format!("{}:{}", self.host, self.port);
            match TcpStream::connect(&addr).await {
                Ok(tcp_stream) => {
                    println!("Connected to {}", self.host);
                    self.inner_loop(tcp_stream,).await;
                    println!("Disconnected from {}", self.host);
                }
                Err(_) => {
                    if let Err(_) = net::lookup_host(&addr).await {
                        return Ok(());
                    } else {
                        time::sleep(Duration::from_secs(2)).await;
                        println!("Try to reconnect");
                    }
                }
            }
        }
        Ok(())
    }
    
    async fn inner_loop(
        &mut self,
        mut tcp_stream: TcpStream,
    ) {
        let mut pkt_buf = Vec::<u8>::with_capacity(self.pkt_size);
        while self.shutdown.load(Ordering::Relaxed) != true {
            match tcp_stream.read_buf(&mut pkt_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if pkt_buf.len() < self.pkt_size {
                        continue;
                    }
                    assert_eq!(pkt_buf.len(), self.pkt_size);
                    let resend_buf = pkt_buf.clone();
                    let _ = self.resend.send(resend_buf).await;

                    pkt_buf.clear();
                }
                Err(_) => {
                    println!("TCP client read data error");
                    return
                }
            }
        }
        println!("inner loop finished");
        drop(tcp_stream);
    }
}

pub(crate) async fn start_tcp_client(
    cfg: Arc<Config>,
    resend: Sender<Vec<u8>>,
    shutdown: impl Future,
) {
    let (host, port) = (cfg.tcp_receiver.host.clone(), cfg.tcp_receiver.port);
    let pkt_size = cfg.tcp_receiver.header_len + 
        cfg.tcp_receiver.n_channel * cfg.tcp_receiver.sample_per_packet * 2;

    let mut client = TcpClient{
        host, 
        port, 
        pkt_size,
        resend,
        shutdown: AtomicBool::new(false)};
    tokio::select! {
        res = client.inf_run() => {
            if let Err(_) = res {
                println!("Failed to start tcp client");
            }
        }
        _ = shutdown => {
            client.shutdown.store(true, Ordering::Relaxed);
            println!("Try to disconnect");
            drop(client);
        }
    }
}