use crate::Config;

use tokio::io::AsyncReadExt;
use tokio::time::{self, Duration};
use tokio::net::TcpStream ;
use bytes::{BytesMut, Buf};
use std::future::Future;
use std::sync::Arc;

pub struct TcpClient {
    host: String,
    port: usize,
    header_size: usize,
    n_ch: usize,
    sample_per_packet: usize,
    shutdown: bool,
}

impl TcpClient {
    pub async fn inf_run(&mut self) -> crate::Result<()> {
        while !self.shutdown {
            let addr = format!("{}:{}", self.host, self.port);
            match TcpStream::connect(addr).await {
                Ok(tcp_stream) => {
                    println!("Connected to {}", self.host);
                    self.inner_loop(
                        tcp_stream,
                        self.header_size,
                        self.n_ch,
                        self.sample_per_packet).await;
                    println!("disconnected from {}", self.host);
                }
                Err(_) => {
                    time::sleep(Duration::from_secs(10)).await;
                    println!("Try to reconnect");
                }
            }
        }
        Ok(())
    }
    
    async fn inner_loop(
        &mut self,
        mut tcp_stream: TcpStream,
        header_size: usize,
        n_ch: usize,
        sample_per_packet: usize,
    ) {
        let pkt_size = header_size + sample_per_packet * n_ch * 2;
        let mut sound_buf = 
            vec![vec![0_u8; sample_per_packet * 2]; n_ch];
        let mut pkt_buf = BytesMut::with_capacity(pkt_size);
        while !self.shutdown {
            match tcp_stream.read_buf(&mut pkt_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if pkt_buf.len() < pkt_size {
                        continue;
                    }
                    let _device_id = pkt_buf.get_u16_le();
                    let secs = pkt_buf.get_u32_le();
                    let millis = pkt_buf.get_u16_le();
                    let pkt_id = pkt_buf.get_i32_le();
                    println!("{}:{}:{}", pkt_id, secs, millis);

                    for i in 0..n_ch {
                        pkt_buf.copy_to_slice(&mut sound_buf[i]);
                    }
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
    host: String,
    port: usize,
    shutdown: impl Future,
) {
    let (header_size, n_ch, sample_per_packet)
        = (cfg.tcp.header_len, cfg.mic.n_channel + cfg.speaker.n_channel, cfg.tcp.sample_per_packet);
    let mut client = TcpClient{
        host, 
        port, 
        header_size, 
        n_ch,
        sample_per_packet,
        shutdown: false};
    tokio::select! {
        res = client.inf_run() => {
            if let Err(_) = res {
                println!("Failed to start tcp client");
            }
        }
        _ = shutdown => {
            client.shutdown = true;
            println!("Try to disconnect");
            drop(client);
        }
    }
}