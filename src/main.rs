type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

mod system_call;
use arc_swap::ArcSwap;
use jack::{RingBufferReader, RingBufferWriter};
use system_call::{start_jackd, start_alsa_out};
mod jack_client;
use jack_client::{inspect_device, start_jack_client};
mod config_file;
use config_file::Config;
mod tcp_server;
use tcp_server::start_server;
mod ring_buf;
mod tcp_client;
use tcp_client::start_tcp_client;

use bytes::{BufMut, BytesMut};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio;
use tokio::sync::Notify;
use tokio::time::{sleep, Duration};


#[tokio::main]
async fn main() {
    let mut cfg = Arc::new(Config::new());
    let sample_per_packet = cfg.tcp.sample_per_packet;
    let packet_time_len = (sample_per_packet * 1000 / cfg.mic.sample_rate) as i16;

    let cfg_cp = cfg.clone();
    let device_id = cfg.mic.device_id;
    let mut _jack_server = start_jackd(cfg_cp);

    sleep(Duration::from_secs(1)).await;
    let cfg_cp = cfg.clone();
    let mut _alsa_out = start_alsa_out(cfg_cp);

    sleep(Duration::from_secs(1)).await;
    let (client, n_mic, n_speaker) = inspect_device();
    if n_mic < cfg.mic.n_channel {
        println!("n_mic set to {}", n_mic);
        if let Some(mut cfg_mut) = Arc::<Config>::get_mut(&mut cfg) {
            cfg_mut.mic.n_channel = n_mic;
        }
    }
    if n_speaker < cfg.speaker.n_channel {
        println!("n_speaker set to {}", n_speaker);
        if let Some(mut cfg_mut) = Arc::<Config>::get_mut(&mut cfg) {
            cfg_mut.speaker.n_channel = n_speaker;
        }
    }
    let n_ch = cfg.mic.n_channel + cfg.speaker.n_channel;
    let pkt_len = cfg.tcp.header_len 
        + sample_per_packet * n_ch * 2;

    // let (send, recv) = Channel::<Bytes>();
    // let recv_cp = recv.clone();
    // let (sender, mut receiver) = Channel(pkt_len);
    // let sender_cp = sender.clone();

    let mut capture_buf_readers = Vec::<RingBufferReader>::new();
    let mut capture_buf_writers = Vec::<RingBufferWriter>::new();
    for _ in 0..cfg.mic.n_channel {
        // reserve 0.5s buffer for each mic
        let ringbuf = jack::RingBuffer::new(cfg.mic.sample_rate).unwrap();
        let (reader, writer) = ringbuf.into_reader_writer();
        capture_buf_readers.push(reader);
        capture_buf_writers.push(writer);
    }
    let mut channel_buf = vec![0_u8; sample_per_packet * 2];

    let mut playback_buf_readers = Vec::<RingBufferReader>::new();
    let mut playback_buf_writers = Vec::<RingBufferWriter>::new();
    for _ in 0..cfg.speaker.n_channel {
        // reserve 0.5s buffer for each mic
        let ringbuf = jack::RingBuffer::new(cfg.mic.sample_rate).unwrap();
        let (reader, writer) = ringbuf.into_reader_writer();
        playback_buf_readers.push(reader);
        playback_buf_writers.push(writer);
    }

    let notify_sound_ready = Arc::new(Notify::new());
    let notifyee_sound_ready = notify_sound_ready.clone();

    let packet_buf = Arc::new(
        ArcSwap::from(Arc::new(BytesMut::with_capacity(pkt_len))));
    let sender_buf = packet_buf.clone();
    let mut swap_buf = Arc::new(BytesMut::with_capacity(pkt_len));

    let notify_packet_ready = Arc::new(Notify::new());
    let notifyee_packet_ready = notify_packet_ready.clone();
    let _buf_thread = tokio::spawn(async move {
        let mut pkt_id = 0_i32;
        loop {
            notifyee_sound_ready.notified().await;

            let swap_buf_mut = Arc::get_mut(&mut swap_buf).unwrap();
            swap_buf_mut.clear();
            let unix_time_in_millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
                - 10;
            let mut secs = (unix_time_in_millis / 1000) as u32;
            let mut millis = (unix_time_in_millis % 1000) as i16 - packet_time_len;
            if millis < 0 {
                secs -= 1;
                millis = millis + 1000;
            }
            swap_buf_mut.put_u16_le(device_id as u16);
            swap_buf_mut.put_u32_le(secs);
            swap_buf_mut.put_i16_le(millis);
            swap_buf_mut.put_i32_le(pkt_id);
            // println!("{}:{}:{}", pkt_id, secs, millis);

            for i in 0..capture_buf_readers.len() {
                assert_eq!(capture_buf_readers[i].read_buffer(channel_buf.as_mut_slice()), channel_buf.len());
                swap_buf_mut.put_slice(channel_buf.as_slice());
            }
            swap_buf_mut.put_slice(channel_buf.as_slice());

            swap_buf = sender_buf.swap(swap_buf);
            notify_packet_ready.notify_waiters();
            
            for i in 0..playback_buf_writers.len() {
                playback_buf_writers[i].write_buffer(channel_buf.as_mut_slice());
            }

            pkt_id += 1;
            if pkt_id == std::i32::MAX {
                pkt_id = 0;
            }
        }
    });

    let (listen_port, max_clients) = (cfg.tcp.listen_port, cfg.tcp.max_clients);
    let send_handler = tokio::spawn(async move {
        start_server(
            listen_port,
            max_clients,
            packet_buf,
            notifyee_packet_ready,
            tokio::signal::ctrl_c(),
        )
        .await;
    });

    let cfg_cp = cfg.clone();
    let recv_handler = tokio::spawn(async move {
        sleep(Duration::from_secs(2)).await;
        start_tcp_client(
            cfg_cp,
            "localhost".to_string(),
            7998,
            tokio::signal::ctrl_c()).await;
    });

    let cfg_cp = cfg.clone();
    start_jack_client(
        cfg_cp,
        client,
        notify_sound_ready,
        capture_buf_writers,
        playback_buf_readers,
        tokio::signal::ctrl_c(),
    ).await;

    send_handler.await.unwrap();
    recv_handler.await.unwrap();
    sleep(Duration::from_secs(1)).await;
}
