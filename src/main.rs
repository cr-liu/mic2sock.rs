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
mod ring_buf;
mod socket;

use arc_swap::ArcSwap;
use bytes::{BufMut, BytesMut};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio;
use tokio::sync::Notify;
use tokio::time::{sleep, Duration};

// frameo per packet
const HEADER_LEN: usize = 12;
const PACKET_N_SAMPLE: usize = 160;

#[tokio::main]
async fn main() {
    let cfg = Arc::new(Config::new());
    let cfg_cp = cfg.clone();
    let device_id = cfg.mic.device_id;
    let mut jack_server = start_jack(cfg_cp);

    sleep(Duration::from_secs(1)).await;
    let (client, n_mic) = inspect_device();
    if n_mic != cfg.mic.n_channel {
        println!("n_channel set to {}", n_mic);
    }
    let n_ch = std::cmp::min(n_mic, cfg.mic.n_channel);

    let pkt_len = HEADER_LEN + PACKET_N_SAMPLE * n_ch * 2;
    let mut audio_data_buf = BytesMut::zeroed(PACKET_N_SAMPLE * n_ch * 2);
    let mut swap_buf = Arc::new(BytesMut::zeroed(pkt_len));
    // let pkt_buf = ;
    let atomic_pkt_buf = Arc::new(ArcSwap::new(Arc::new(BytesMut::zeroed(pkt_len))));
    let ringbuf = jack::RingBuffer::new(cfg.mic.sample_rate as usize * n_ch * 2).unwrap();
    let (mut ringbuf_reader, ringbuf_writer) = ringbuf.into_reader_writer();

    let notify_dump_data = Arc::new(Notify::new());
    let notify_data_ready = Arc::new(Notify::new());

    let pkt_buf_mut = atomic_pkt_buf.clone();
    let notify_dump_data_cp = notify_dump_data.clone();
    let notify_data_ready_cp = notify_data_ready.clone();
    let _buf_thread = tokio::spawn(async move {
        let mut pkt_id = 0_u32;
        loop {
            notify_dump_data_cp.notified().await;
            // println!("ringbuf len: {}", ringbuf_reader.space());
            let swap_buf_mut = Arc::get_mut(&mut swap_buf).unwrap();
            swap_buf_mut.clear();
            let unix_time_in_millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
                - 10;
            let secs = (unix_time_in_millis / 1000) as u32;
            let millis = (unix_time_in_millis % 1000) as u16;
            swap_buf_mut.put_u16(device_id as u16);
            swap_buf_mut.put_u32(secs);
            swap_buf_mut.put_u16(millis);
            swap_buf_mut.put_u32(pkt_id);

            let _read_size = ringbuf_reader.read_buffer(audio_data_buf.as_mut());
            swap_buf_mut.extend_from_slice(audio_data_buf.as_ref());

            swap_buf = pkt_buf_mut.swap(swap_buf);
            notify_data_ready_cp.notify_waiters();

            pkt_id += 1;
            if pkt_id == std::u32::MAX {
                pkt_id = 0;
            }
        }
    });

    let (listen_port, max_clients) = (cfg.tcp.listen_port, cfg.tcp.max_clients);
    let tcp_thread = tokio::spawn(async move {
        start_server(
            listen_port,
            max_clients,
            notify_data_ready,
            atomic_pkt_buf,
            tokio::signal::ctrl_c(),
        )
        .await;
    });

    let cfg_cp = cfg.clone();
    start_jack_client(
        cfg_cp,
        client,
        notify_dump_data,
        ringbuf_writer,
        tokio::signal::ctrl_c(),
    )
    .await;

    tcp_thread.await.unwrap();
    sleep(Duration::from_secs(1)).await;
    jack_server.kill().await.unwrap();
}
