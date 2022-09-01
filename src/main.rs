type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

mod system_call;
mod jack_client;
mod config_file;
mod tcp_server;

mod ring_buf;

use std::sync::Arc;
use arc_swap::ArcSwap;
use ring_buf::RingBuf;

mod connection;
use connection::Connection;

mod shutdown;
use shutdown::Shutdown;

// frames per packet
const PACKET_LEN: usize = 160;
const SAMPLE_RATE: u16 = 16000;
static mut MIC_ID: u16 = 0;

#[tokio::main]
async fn main() {

    use config_file::Config;
    let conf = Arc::new(Config::new());

    use tokio;
    use std::process::Command;

    use system_call::start_jack;
    let conf_copy = conf.clone();
    let jack_server = tokio::spawn( async move {
        start_jack(conf_copy, tokio::signal::ctrl_c()).await;
    }); 

    use jack_client::JackClinet;
    // let audio_client = jack_client::JackClinet::new();

    use tokio::sync::Notify;
    use tcp_server::start_server;
    let (listen_port, max_clients) = (conf.tcp.listen_port, conf.tcp.max_clients);
    let notify_data_ready = Arc::new(Notify::new());
    let tcp_thread = tokio::spawn(async move {
        start_server(listen_port, max_clients, notify_data_ready, tokio::signal::ctrl_c()).await;
    });

    let mut rb = RingBuf::new(6, 0u16);

    let mut audio_buf: Arc<[u16; PACKET_LEN]> = Arc::new([0u16; PACKET_LEN]);
    let packet_buf: Arc<[u16; PACKET_LEN]> = Arc::new([0u16; PACKET_LEN]);
    let mut packet_buf: Arc<ArcSwap<[u16; PACKET_LEN]>> = Arc::new(ArcSwap::new(packet_buf));


    let mut a: Box<[u16]> = Box::new([1u16,2,3,4,5,6]);
    let mut b = Box::new([6u16,7,8,9,0]);
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


    tcp_thread.await;

    jack_server.await;
}

fn print(a: &ArcSwap<[u16; 5]>) {
    println!("{:?}", a);
}