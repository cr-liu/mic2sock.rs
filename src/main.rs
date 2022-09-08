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

use std::sync::Arc;

use tokio;
use tokio::sync::Notify;
use tokio::time::{sleep, Duration};

use arc_swap::ArcSwap;
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

    let mut rb = RingBuf::new(6, 0u16);

    let mut audio_buf: Arc<[u16; PACKET_LEN]> = Arc::new([0u16; PACKET_LEN]);
    let packet_buf: Arc<[u16; PACKET_LEN]> = Arc::new([0u16; PACKET_LEN]);
    let mut packet_buf: Arc<ArcSwap<[u16; PACKET_LEN]>> = Arc::new(ArcSwap::new(packet_buf));

    let mut a: Box<[u16]> = Box::new([1u16, 2, 3, 4, 5, 6]);
    let mut b = Box::new([6u16, 7, 8, 9, 0]);
    // let mut b: Arc<[u16]> = Arc::from(a);

    let mut p: Arc<[u16; 3]> = Arc::new([0u16; 3]);
    let pp: Arc<[u16; 3]> = Arc::new([0u16; 3]);

    rb.append(&a);
    println!("{}", rb.len());
    rb.pop(Arc::get_mut(&mut p).unwrap());
    println!("{:?}", &p);

    println!("{}", rb.len());
    let c = Arc::new(ArcSwap::new(pp));
    let cc = c.clone();
    p = c.swap(p);
    println!("{:?}", &p);

    rb.pop(Arc::get_mut(&mut p).unwrap());
    println!("{:?}", &p);
    p = c.swap(p);
    println!("{:?}", &p);

    // let d = ArcSwap::new(a_clone);
    // b = c.swap(b);
    // let h = tokio::spawn(async {
    //     print(&c);
    // });
    // h.await;
    // print(&d);

    sleep(Duration::from_secs(1)).await;
    let (client, n_mic) = inspect_device();
    if n_mic != cfg.mic.n_channel {
        println!("n_channel set to {}", n_mic);
    }
    let n_ch = std::cmp::min(n_mic, cfg.mic.n_channel);

    let ringbuf = jack::RingBuffer::new(cfg.mic.sample_rate as usize * n_ch * 2).unwrap();
    let (ringbuf_reader, ringbuf_writer) = ringbuf.into_reader_writer();
    let notify_dump_data = Arc::new(Notify::new());
    let notify_dump_data_cp = notify_dump_data.clone();

    let _buf_thread = tokio::spawn(async move {
        loop {
            notify_dump_data.notified().await;
            println!("{}", ringbuf_reader.space());
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

fn print(a: &ArcSwap<[u16; 5]>) {
    println!("{:?}", a);
}
