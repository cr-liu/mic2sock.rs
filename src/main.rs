type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

mod system_call;
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

use std::cmp::{max, min};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use jack::{RingBufferReader, RingBufferWriter};
use tokio::{self, sync::Notify};
use tokio::time::{sleep, Duration};
use crossbeam::channel::bounded;
use arc_swap::ArcSwap;


#[tokio::main]
async fn main() {
    let mut cfg = Arc::new(Config::new());
    let send_header_len = cfg.tcp_sender.header_len;
    let recv_header_len = cfg.tcp_receiver.header_len;
    let sample_per_send_packet = cfg.tcp_sender.sample_per_packet;
    let sample_per_recv_packet = cfg.tcp_receiver.sample_per_packet;
    let sample_per_packet = max(sample_per_send_packet, sample_per_recv_packet);
    let packet_time_len = (sample_per_send_packet * 1000 / cfg.mic.sample_rate) as i16;
    let recv_n_ch = cfg.tcp_receiver.n_channel;
    let recv_pkt_len = recv_header_len + 
        recv_n_ch * sample_per_recv_packet *2;
    let device_id = cfg.mic.device_id as u16;

    let cfg_cp = cfg.clone();
    let mut jack_server = start_jackd(cfg_cp);
    sleep(Duration::from_secs(1)).await;
    let cfg_cp = cfg.clone();
    let mut alsa_out = start_alsa_out(cfg_cp);
    sleep(Duration::from_secs(1)).await;
 
    let (client, mut n_mic, mut n_speaker) = inspect_device();
    if n_mic < cfg.mic.n_channel {
        println!("n_mic set to {}", n_mic);
        if let Some(mut cfg_mut) = Arc::<Config>::get_mut(&mut cfg) {
            cfg_mut.mic.n_channel = n_mic;
        }
    }
    n_speaker = min(n_speaker, cfg.tcp_receiver.n_channel);
    if n_speaker < cfg.speaker.n_channel {
        println!("n_speaker set to {}", n_speaker);
        if let Some(mut cfg_mut) = Arc::<Config>::get_mut(&mut cfg) {
            cfg_mut.speaker.n_channel = n_speaker;
        }
    }
    (n_mic, n_speaker) = (cfg.mic.n_channel, cfg.speaker.n_channel);
    let n_ch = n_mic + n_speaker;
    let send_pkt_len = send_header_len + sample_per_send_packet * n_ch * 2;
    println!("Send {n_ch} channels with packet length {send_pkt_len}");

    let mut capture_buf_readers = Vec::<RingBufferReader>::new();
    let mut capture_buf_writers = Vec::<RingBufferWriter>::new();
    for _ in 0..n_mic {
        // reserve 0.5s buffer for each mic
        let ringbuf = jack::RingBuffer::new(cfg.mic.sample_rate).unwrap();
        let (reader, writer) = ringbuf.into_reader_writer();
        capture_buf_readers.push(reader);
        capture_buf_writers.push(writer);
    }
    let mut send_channel_buf = vec![0_u8; sample_per_send_packet * 2];
    let zeroed_channel_buf = vec![0_u8; sample_per_send_packet *2];

    let mut resend_buf_readers = Vec::<RingBufferReader>::new();
    let mut resend_buf_writers = Vec::<RingBufferWriter>::new();
    for _ in 0..n_speaker {
        // reserve 0.5s buffer for each mic
        let ringbuf = jack::RingBuffer::new(sample_per_packet * 4).unwrap();
        let (reader, writer) = ringbuf.into_reader_writer();
        resend_buf_readers.push(reader);
        resend_buf_writers.push(writer);
    }


    let mut playback_buf_readers = Vec::<RingBufferReader>::new();
    let mut playback_buf_writers = Vec::<RingBufferWriter>::new();
    for _ in 0..n_speaker {
        // reserve 0.5s buffer for each mic
        let ringbuf = jack::RingBuffer::new(cfg.mic.sample_rate).unwrap();
        let (reader, writer) = ringbuf.into_reader_writer();
        playback_buf_readers.push(reader);
        playback_buf_writers.push(writer);
    }

    let (resend, incoming_socket) = bounded::<Vec<u8>>(send_pkt_len * 2);

    let notify_sound_ready = Arc::new(Notify::new());
    let notifyee_sound_ready = notify_sound_ready.clone();

    let send_packet_buf = Arc::new(
        ArcSwap::from(Arc::new(vec![0_u8; send_pkt_len])));
    let sender_buf = send_packet_buf.clone();
    let mut swap_buf = Arc::new(vec![0_u8; send_pkt_len]);

    let notify_packet_ready = Arc::new(Notify::new());
    let notifyee_packet_ready = notify_packet_ready.clone();

    let _process_sender_buf = tokio::spawn(async move {
        let mut pkt_id = 0_i32;
        loop {
            notifyee_sound_ready.notified().await;

            let swap_buf_mut = Arc::get_mut(&mut swap_buf).unwrap();

            let unix_time_in_millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
                - 10;
            let mut secs = (unix_time_in_millis / 1000) as u32;
            let mut ms = (unix_time_in_millis % 1000) as i16 - packet_time_len;
            if ms < 0 {
                secs -= 1;
                ms = ms + 1000;
            }

            swap_buf_mut[0..2].copy_from_slice(&device_id.to_le_bytes());
            swap_buf_mut[2..6].copy_from_slice(&secs.to_le_bytes());
            swap_buf_mut[6..8].copy_from_slice(&ms.to_le_bytes());
            swap_buf_mut[8..12].copy_from_slice(&pkt_id.to_le_bytes());
            
            let mut s_idx = send_header_len;
            for i in 0..capture_buf_readers.len() {
                let n_bytes = capture_buf_readers[i].read_buffer(send_channel_buf.as_mut_slice());
                assert_eq!(n_bytes, send_channel_buf.len());
                let e_idx = s_idx + send_channel_buf.len();

                swap_buf_mut[s_idx..e_idx].copy_from_slice(send_channel_buf.as_ref());
                s_idx += send_channel_buf.len();
            }

            for i in 0..n_speaker {
                let e_idx = s_idx + send_channel_buf.len();
                if resend_buf_readers[0].space() < sample_per_send_packet * 2 {
                    swap_buf_mut[s_idx..e_idx].copy_from_slice(zeroed_channel_buf.as_ref());
                } else {
                    let n_bytes = resend_buf_readers[i].read_buffer(send_channel_buf.as_mut());
                    assert_eq!(n_bytes, send_channel_buf.len());
                    swap_buf_mut[s_idx..e_idx].copy_from_slice(send_channel_buf.as_ref());
                }
                s_idx += send_channel_buf.len();
            }

            swap_buf = sender_buf.swap(swap_buf);
            notify_packet_ready.notify_waiters();
            
            pkt_id += 1;
            if pkt_id == std::i32::MAX {
                pkt_id = 0;
            }
        }
    });

    let _process_receiver_buf = tokio::spawn(async move {
        while let Ok(received_buf) = incoming_socket.recv() {
            assert_eq!(recv_pkt_len, received_buf.len());
            let _secs = u32::from_le_bytes(received_buf[2..6].try_into().unwrap());
            let _ms = i16::from_le_bytes(received_buf[6..8].try_into().unwrap());
            let _pkt_id = i32::from_le_bytes(received_buf[8..12].try_into().unwrap());
            // println!("{}--{}--{}", _pkt_id, _secs, _ms);

            for i in 0..n_speaker {
                let s_idx = recv_header_len + sample_per_recv_packet * 2 * i;
                let e_idx = s_idx + sample_per_recv_packet * 2;

                resend_buf_writers[i].write_buffer(&received_buf[s_idx..e_idx]);
                playback_buf_writers[i].write_buffer(&received_buf[s_idx..e_idx]);
            }
        }
    });

    let (listen_port, max_clients) = (cfg.tcp_sender.listen_port, cfg.tcp_sender.max_clients);
    let send_handler = tokio::spawn(async move {
        start_server(
            listen_port,
            max_clients,
            send_packet_buf,
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
            resend,
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
    jack_server.kill().await.expect("Kill jack server failed");
    alsa_out.kill().await.expect("Kill alsa_out failed");
    sleep(Duration::from_secs(1)).await;
}
