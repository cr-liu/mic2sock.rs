mod config_file;
use config_file::{Config, HEADER_LEN};
mod alsa_capture;
use alsa_capture::{CaptureDevice, create_ring_buffer, start_primary_capture, start_secondary_capture};
mod alsa_playback;
use alsa_playback::{create_playback_ring, start_playback};
mod transport_server;
use transport_server::start_server;
mod transport_client;
use transport_client::start_client;
mod gui;

use bytes::{Bytes, BytesMut, BufMut};
use crossbeam::channel::bounded;
use ringbuf::traits::{Consumer, Observer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};

#[tokio::main]
async fn main() {
    let cfg = Config::new();
    let sample_rate = cfg.general.sample_rate;
    let period = cfg.general.period;
    let n_period = cfg.general.n_period;
    let sample_per_packet = cfg.general.sample_per_packet;
    let device_id = cfg.general.device_id as u16;
    let total_capture_ch = cfg.total_capture_channels();
    let packet_time_len = (sample_per_packet * 1000 / sample_rate) as i16;

    let pkt_payload_size = total_capture_ch * sample_per_packet * 2;
    let send_pkt_len = HEADER_LEN + pkt_payload_size;

    let recv_pkt_len = cfg.receiver.pkt_len
        .unwrap_or(HEADER_LEN + cfg.receiver.n_channel * sample_per_packet * 2);

    println!(
        "Capture: {} devices, {} total channels, packet size {}",
        cfg.capture_device.len(),
        total_capture_ch,
        send_pkt_len,
    );

    let shutdown = Arc::new(AtomicBool::new(false));

    // ── Start capture devices ──

    let (primary_tx, primary_rx) = bounded::<Vec<i16>>(2);

    let (wave_tap_tx, _) = tokio::sync::broadcast::channel::<Vec<i16>>(4);
    let wave_tap_for_gui = wave_tap_tx.clone();

    let mut secondary_consumers = Vec::new();
    let mut capture_threads = Vec::new();

    for (i, dev_cfg) in cfg.capture_device.iter().enumerate() {
        let device = CaptureDevice {
            device_name: dev_cfg.device_name.clone(),
            n_channel: dev_cfg.n_channel,
        };

        if i == 0 {
            capture_threads.push(start_primary_capture(
                device,
                sample_rate,
                period,
                n_period,
                primary_tx.clone(),
                wave_tap_tx.clone(),
                shutdown.clone(),
            ));
        } else {
            let (producer, consumer) = create_ring_buffer(period, dev_cfg.n_channel);
            secondary_consumers.push((dev_cfg.n_channel, consumer));
            capture_threads.push(start_secondary_capture(
                device,
                sample_rate,
                period,
                n_period,
                producer,
                shutdown.clone(),
            ));
        }
    }
    drop(primary_tx);

    // ── Broadcast channel for transport ──

    let (pkt_broadcast_tx, _) = broadcast::channel::<Bytes>(4);

    // ── Start transport server ──

    use std::collections::HashSet;

    // Parse static_receivers: skip invalid entries with a log.
    let static_receivers_parsed: Vec<std::net::SocketAddr> = cfg
        .sender
        .static_receivers
        .iter()
        .filter_map(|s| match s.parse() {
            Ok(addr) => Some(addr),
            Err(e) => {
                eprintln!("invalid static_receiver '{}': {}", s, e);
                None
            }
        })
        .collect();

    let static_set = Arc::new(arc_swap::ArcSwap::from_pointee(
        static_receivers_parsed.iter().copied().collect::<HashSet<std::net::SocketAddr>>()
    ));
    let static_set_for_gui = static_set.clone();

    let server_handle = {
        let tx = pkt_broadcast_tx.clone();
        let protocol = cfg.sender.protocol.clone();
        let port = cfg.sender.listen_port;
        let max_clients = cfg.sender.max_clients;
        let static_set_for_server = static_set.clone();
        tokio::spawn(async move {
            start_server(
                &protocol,
                port,
                max_clients,
                static_set_for_server,
                tx,
                tokio::signal::ctrl_c(),
            )
            .await;
        })
    };

    let gui_handle = if cfg.gui.enabled {
        let gui_cfg = cfg.gui.clone();
        let handles = Arc::new(gui::GuiHandles {
            static_set: static_set_for_gui.clone(),
            waveform_tap: wave_tap_for_gui.clone(),
            config_path: std::path::PathBuf::from("config.toml"),
        });
        Some(tokio::spawn(async move {
            if let Err(e) = gui::start_gui(gui_cfg, handles).await {
                eprintln!("GUI error: {}", e);
            }
        }))
    } else {
        None
    };

    // ── Start transport client + playback ──
    // Only start the client if a receiver host is configured.
    // host = "none" (or empty) means "sender only, no incoming stream".

    let (recv_tx, mut recv_rx) = mpsc::channel::<Vec<u8>>(4);
    let receiver_enabled = cfg.receiver.host != "none" && !cfg.receiver.host.is_empty();

    let client_handle = if receiver_enabled {
        let protocol = cfg.receiver.protocol.clone();
        let host = cfg.receiver.host.clone();
        let port = cfg.receiver.port;
        Some(tokio::spawn(async move {
            start_client(&protocol, &host, port, recv_pkt_len, recv_tx, tokio::signal::ctrl_c()).await;
        }))
    } else {
        drop(recv_tx); // Close the sender so recv_rx.recv() returns None cleanly
        None
    };

    // Start playback if receiver is configured (always mono, plays ch0 only)
    let _playback_stream = if receiver_enabled {
        let (pb_producer, pb_consumer) = create_playback_ring(sample_per_packet, 1);
        let ch0_bytes_len = sample_per_packet * 2;

        tokio::spawn(async move {
            let mut pb_producer = pb_producer;
            let mut last_pkt_id: Option<i32> = None;
            while let Some(pkt) = recv_rx.recv().await {
                if pkt.len() < HEADER_LEN + ch0_bytes_len {
                    continue;
                }
                let pkt_id = i32::from_le_bytes(pkt[8..12].try_into().unwrap());

                if let Some(last) = last_pkt_id {
                    let diff = pkt_id.wrapping_sub(last);
                    if diff == 0 {
                        continue; // duplicate
                    } else if diff <= 0 || diff > 1000 {
                        // Out of order or too far behind — drop
                        continue;
                    }
                }
                last_pkt_id = Some(pkt_id);

                // Channel-major format: ch0 is the first block after header
                let ch0_bytes = &pkt[HEADER_LEN..HEADER_LEN + ch0_bytes_len];
                let samples: Vec<i16> = ch0_bytes
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
                use ringbuf::traits::Producer;
                pb_producer.push_slice(&samples);
            }
        });

        match start_playback(
            &cfg.playback.device_name,
            sample_rate,
            1,
            period,
            pb_consumer,
            shutdown.clone(),
        ) {
            Ok(stream) => Some(stream),
            Err(e) => {
                eprintln!("Failed to start playback: {}", e);
                None
            }
        }
    } else {
        drop(recv_rx);
        None
    };

    // ── Packet assembly loop ──

    let assembly_shutdown = shutdown.clone();
    let pkt_tx = pkt_broadcast_tx.clone();

    let assembly_handle = tokio::task::spawn_blocking(move || {
        let mut pkt_id: i32 = 0;
        let primary_n_ch = cfg.capture_device[0].n_channel;

        // Accumulation buffers for when sample_per_packet != period
        let mut primary_accum = Vec::<i16>::with_capacity(sample_per_packet * primary_n_ch);
        let mut secondary_accums: Vec<Vec<i16>> = secondary_consumers
            .iter()
            .map(|(n_ch, _)| Vec::with_capacity(sample_per_packet * n_ch))
            .collect();

        // Buffers for secondary device reads (one period at a time)
        let mut secondary_bufs: Vec<Vec<i16>> = secondary_consumers
            .iter()
            .map(|(n_ch, _)| vec![0i16; period * n_ch])
            .collect();
        let mut last_frames: Vec<Vec<i16>> = secondary_consumers
            .iter()
            .map(|(n_ch, _)| vec![0i16; *n_ch])
            .collect();

        while !assembly_shutdown.load(Ordering::Relaxed) {
            let primary_data = match primary_rx.recv() {
                Ok(data) => data,
                Err(_) => break,
            };

            primary_accum.extend_from_slice(&primary_data);

            for (idx, (n_ch, consumer)) in secondary_consumers.iter_mut().enumerate() {
                let frame_count = period * *n_ch;
                let available = consumer.occupied_len();

                if available > period * *n_ch * 2 {
                    let mut discard = vec![0i16; *n_ch];
                    consumer.pop_slice(&mut discard);
                }

                let read = consumer.pop_slice(&mut secondary_bufs[idx][..frame_count]);
                if read < frame_count {
                    for j in read..frame_count {
                        secondary_bufs[idx][j] = last_frames[idx][j % *n_ch];
                    }
                } else {
                    let start = frame_count - *n_ch;
                    last_frames[idx].copy_from_slice(&secondary_bufs[idx][start..frame_count]);
                }
                secondary_accums[idx].extend_from_slice(&secondary_bufs[idx][..frame_count]);
            }

            if primary_accum.len() < sample_per_packet * primary_n_ch {
                continue;
            }

            let mut pkt = BytesMut::with_capacity(send_pkt_len);

            let unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            let mut secs = (unix_ms / 1000) as u32;
            let mut ms = (unix_ms % 1000) as i16 - packet_time_len;
            if ms < 0 {
                secs -= 1;
                ms += 1000;
            }

            pkt.put_u16_le(device_id);
            pkt.put_u32_le(secs);
            pkt.put_i16_le(ms);
            pkt.put_i32_le(pkt_id);

            // De-interleave: write channel-major (per-channel blocks) across all devices
            for ch in 0..primary_n_ch {
                for frame_idx in 0..sample_per_packet {
                    pkt.put_i16_le(primary_accum[frame_idx * primary_n_ch + ch]);
                }
            }
            primary_accum.drain(..sample_per_packet * primary_n_ch);

            for (idx, (n_ch, _)) in secondary_consumers.iter().enumerate() {
                for ch in 0..*n_ch {
                    for frame_idx in 0..sample_per_packet {
                        pkt.put_i16_le(secondary_accums[idx][frame_idx * n_ch + ch]);
                    }
                }
                secondary_accums[idx].drain(..sample_per_packet * n_ch);
            }

            let packet = pkt.freeze();
            let _ = pkt_tx.send(packet);

            pkt_id = pkt_id.wrapping_add(1);
        }
        println!("Packet assembly stopped");
    });

    // ── Wait for shutdown ──

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("Ctrl+C received, shutting down...");
            shutdown.store(true, Ordering::SeqCst);
        }
        _ = assembly_handle => {
            println!("Assembly loop ended");
            shutdown.store(true, Ordering::SeqCst);
        }
    }

    server_handle.abort();
    if let Some(h) = client_handle {
        h.abort();
    }
    if let Some(h) = gui_handle {
        h.abort();
    }

    for handle in capture_threads {
        let _ = handle.join();
    }

    println!("Shutdown complete");
}
