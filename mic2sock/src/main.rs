type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

mod alsa_capture;
mod config_file;
mod tcp_server;

use alsa_capture::{open_capture, run_capture};
use config_file::Config;
use tcp_server::start_server;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() {
    let mut cfg = Config::new();

    // This build captures and sends the mic channels only. The far-end
    // resend/playback path (speaker.n_channel > 0 with tcp_receiver enabled)
    // was a JACK-era feature and is not wired up under direct ALSA yet --
    // spec §7.2 describes how it must come back (tap at the device boundary,
    // never resample the network stream twice).
    if cfg.speaker.n_channel > 0 || cfg.tcp_receiver.host != "none" {
        println!(
            "warning: resend/playback is not supported in the ALSA build; \
             sending mic channels only"
        );
        cfg.speaker.n_channel = 0;
    }
    if cfg.mic.start_jackd {
        println!("note: jackd is no longer used; capturing directly via ALSA");
    }

    let (capture_dev, n_mic) = open_capture(&cfg);
    cfg.mic.n_channel = n_mic;
    let cfg = Arc::new(cfg);

    let pkt_len = cfg.tcp_sender.header_len + n_mic * cfg.tcp_sender.sample_per_packet * 2;
    println!("Send {} channels with packet length {}", n_mic, pkt_len);

    // Per-client send backlog. The shim absorbs up to catchup_max = 3 s of
    // backlog after a stall, and content is dropped at whichever end holds
    // less (spec §6.2) -- so the sender must retain the same 3 s. At 10 ms a
    // packet that is 300 packets, ~1.5 MB.
    let backlog_packets = 3000 / (cfg.tcp_sender.sample_per_packet * 1000 / cfg.mic.sample_rate);
    let (packet_sender, _keep_channel_open) = broadcast::channel(backlog_packets);

    let shutdown = Arc::new(AtomicBool::new(false));
    let panic_flag = Arc::new(AtomicBool::new(false));

    let cfg_cp = cfg.clone();
    let sender_cp = packet_sender.clone();
    let shutdown_cp = shutdown.clone();
    let panic_cp = panic_flag.clone();
    // A plain std::thread, deliberately outside the tokio runtime, so the
    // blocking readi loop never contends with the async scheduler -- the same
    // isolation the JACK process callback had.
    let audio_thread = std::thread::spawn(move || {
        run_capture(capture_dev, cfg_cp, sender_cp, shutdown_cp, panic_cp);
    });

    // Mirror of the old JACK watchdog: a fatal capture error raises the flag
    // and this task fails the whole process loudly. The daemon is meant to be
    // restarted by a supervisor rather than limp on without audio.
    let watchdog = {
        let panic_flag = panic_flag.clone();
        async move {
            loop {
                if panic_flag.load(Ordering::SeqCst) {
                    panic!("audio capture failure");
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
    };

    let send_handler = start_server(
        cfg.tcp_sender.listen_port,
        cfg.tcp_sender.max_clients,
        packet_sender,
        tokio::signal::ctrl_c(),
    );

    tokio::select! {
        _ = send_handler => {}
        _ = watchdog => {}
    }

    shutdown.store(true, Ordering::SeqCst);
    audio_thread.join().unwrap();
}
