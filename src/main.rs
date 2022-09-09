type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

mod system_call;
use system_call::start_jack;
mod jack_client;
use jack_client::{inspect_device, start_jack_client};
mod config_file;
use config_file::Config;
mod tcp_server;
use tcp_server::start_server;
mod connection;
use connection::Connection;
mod shutdown;
use shutdown::Shutdown;
mod ring_buf;
use ring_buf::RingBuf;

use arc_swap::ArcSwap;
use bytes::{BufMut, BytesMut};
use std::sync::Arc;
use tokio;
use tokio::sync::Notify;
use tokio::time::{sleep, Duration};

// frames per packet
const PACKET_LEN: usize = 160;
static mut MIC_ID: u16 = 0;

#[tokio::main]
async fn main() {
    let cfg = Arc::new(Config::new());
    let cfg_cp = cfg.clone();
    let mut jack_server = start_jack(cfg_cp);

    let (listen_port, max_clients) = (cfg.tcp.listen_port, cfg.tcp.max_clients);
    let notify_data_ready = Arc::new(Notify::new());
    let notify_data_ready_cp = notify_data_ready.clone();
    let tcp_thread = tokio::spawn(async move {
        start_server(
            listen_port,
            max_clients,
            notify_data_ready_cp,
            tokio::signal::ctrl_c(),
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;
    let (client, n_mic) = inspect_device();
    if n_mic != cfg.mic.n_channel {
        println!("n_channel set to {}", n_mic);
    }
    let n_ch = std::cmp::min(n_mic, cfg.mic.n_channel);

    let mut buf = BytesMut::with_capacity(PACKET_LEN * n_ch * 2);
    // fill() method does not work;
    buf.put_bytes(0, PACKET_LEN * n_ch * 2);
    let mut swap_buf = Arc::new(buf);
    let mut buf = BytesMut::with_capacity(PACKET_LEN * n_ch * 2);
    buf.put_bytes(0, PACKET_LEN * n_ch * 2);
    let pkt_buf = Arc::new(buf);
    let atomic_pkt_buf = Arc::new(ArcSwap::new(pkt_buf));
    let pkt_buf_mut = atomic_pkt_buf.clone();

    let ringbuf = jack::RingBuffer::new(cfg.mic.sample_rate as usize * n_ch * 2).unwrap();
    let (mut ringbuf_reader, ringbuf_writer) = ringbuf.into_reader_writer();
    let notify_dump_data = Arc::new(Notify::new());
    let notify_dump_data_cp = notify_dump_data.clone();

    let _buf_thread = tokio::spawn(async move {
        loop {
            notify_dump_data.notified().await;
            // println!("ringbuf len: {}", ringbuf_reader.space());
            let _read_size =
                ringbuf_reader.read_buffer(Arc::get_mut(&mut swap_buf).unwrap().as_mut());
            swap_buf = pkt_buf_mut.swap(swap_buf);
            notify_data_ready.notify_waiters();
        }
    });

    let cfg_cp = cfg.clone();
    start_jack_client(
        cfg_cp,
        client,
        notify_dump_data_cp,
        ringbuf_writer,
        tokio::signal::ctrl_c(),
    )
    .await;

    tcp_thread.await.unwrap();
    sleep(Duration::from_secs(1)).await;
    jack_server.kill().await.unwrap();
}
