# Direct ALSA + UDP/TCP Transport Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace JACK with direct ALSA multi-device capture + cpal cross-platform playback, add UDP/TCP selectable transport, and optimize packet assembly for lower latency.

**Architecture:** Each capture device runs a dedicated OS thread with blocking ALSA reads. The first device drives system tempo via a crossbeam channel; additional devices write to lock-free ring buffers with drift compensation. A tokio task assembles packets using `BytesMut` and broadcasts via `bytes::Bytes`. Transport layer supports UDP (default) and TCP, selected by config. Playback uses cpal for cross-platform output.

**Tech Stack:** Rust 2021, alsa crate (Linux capture), cpal (cross-platform playback), ringbuf (lock-free SPSC), tokio (async networking), bytes (zero-copy buffers), crossbeam (channels), serde + toml (config)

**Spec:** `docs/superpowers/specs/2026-04-16-alsa-direct-udp-design.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Rewrite | `Cargo.toml` | Update dependencies: remove jack/arc-swap, add alsa/cpal/ringbuf |
| Rewrite | `src/config_file.rs` | New config structs matching spec TOML format |
| Create | `src/alsa_capture.rs` | ALSA multi-device capture threads |
| Create | `src/alsa_playback.rs` | cpal cross-platform playback |
| Create | `src/transport_server.rs` | UDP/TCP audio broadcast server |
| Create | `src/transport_client.rs` | UDP/TCP audio receiver client |
| Rewrite | `src/main.rs` | Orchestration, packet assembly/disassembly |
| Rewrite | `config.toml` | New format matching spec |
| Delete | `src/jack_client.rs` | Replaced by alsa_capture + alsa_playback |
| Delete | `src/tcp_server.rs` | Replaced by transport_server |
| Delete | `src/tcp_client.rs` | Replaced by transport_client |
| Delete | `src/system_call.rs` | No longer needed (no jackd) |
| Delete | `src/ring_buf.rs` | Unused, replaced by ringbuf crate |

---

### Task 1: Update Dependencies and Config

**Files:**
- Modify: `Cargo.toml`
- Rewrite: `src/config_file.rs`
- Rewrite: `config.toml`

- [ ] **Step 1: Update Cargo.toml**

```toml
[package]
name = "mic2sock"
version = "0.2.0"
edition = "2021"

[dependencies]
alsa = "0.9"
cpal = { version = "0.15", features = ["jack"] }
serde = { version = "1.0", features = ["derive", "std"] }
toml = "0.7"
tokio = { version = "1.28", features = ["full"] }
bytes = "1.4"
crossbeam = "0.8"
ringbuf = "0.4"
```

Note: `jack` and `arc-swap` removed. `alsa`, `cpal`, `ringbuf` added.

- [ ] **Step 2: Rewrite config_file.rs**

```rust
use serde::{Deserialize, Serialize};
use std::{fs, io::Write};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub const HEADER_LEN: usize = 12;

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub general: GeneralConfig,
    pub capture_device: Vec<CaptureDeviceConfig>,
    pub playback: PlaybackConfig,
    pub sender: SenderConfig,
    pub receiver: ReceiverConfig,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GeneralConfig {
    pub sample_rate: usize,
    pub period: usize,
    pub n_period: usize,
    pub sample_per_packet: usize,
    pub device_id: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CaptureDeviceConfig {
    pub device_name: String,
    pub n_channel: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PlaybackConfig {
    pub device_name: String,
    pub n_channel: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SenderConfig {
    pub protocol: String,
    pub listen_port: usize,
    pub max_clients: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ReceiverConfig {
    pub protocol: String,
    pub host: String,
    pub port: usize,
    pub n_channel: usize,
}

impl Config {
    pub fn new() -> Config {
        match Config::read_conf_file() {
            Ok(conf) => conf,
            Err(err) => {
                println!("failed reading config.toml! {}", err);
                println!("creating default conf.toml; please rename it to config.toml");
                let conf = Config::default();
                let toml_str = toml::to_string(&conf).unwrap();
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open("conf.toml")
                    .unwrap();
                f.write_all(toml_str.as_bytes()).unwrap();
                conf
            }
        }
    }

    fn read_conf_file() -> Result<Config, Error> {
        let contents = fs::read_to_string("config.toml")?;
        let conf: Config = toml::from_str(&contents)?;
        Ok(conf)
    }

    pub fn total_capture_channels(&self) -> usize {
        self.capture_device.iter().map(|d| d.n_channel).sum()
    }

    fn default() -> Config {
        Config {
            general: GeneralConfig {
                sample_rate: 16000,
                period: 32,
                n_period: 3,
                sample_per_packet: 32,
                device_id: 0,
            },
            capture_device: vec![CaptureDeviceConfig {
                device_name: "hw:RASPZX16ch".to_string(),
                n_channel: 16,
            }],
            playback: PlaybackConfig {
                device_name: "plughw:Device".to_string(),
                n_channel: 1,
            },
            sender: SenderConfig {
                protocol: "udp".to_string(),
                listen_port: 7998,
                max_clients: 100,
            },
            receiver: ReceiverConfig {
                protocol: "udp".to_string(),
                host: "none".to_string(),
                port: 4000,
                n_channel: 1,
            },
        }
    }
}
```

- [ ] **Step 3: Rewrite config.toml**

```toml
[general]
sample_rate = 16000
period = 32
n_period = 3
sample_per_packet = 32
device_id = 0

[[capture_device]]
device_name = "hw:RASPZX16ch"
n_channel = 16

[playback]
device_name = "plughw:Device"
n_channel = 1

[sender]
protocol = "udp"
listen_port = 7998
max_clients = 100

[receiver]
protocol = "udp"
host = "none"
port = 4000
n_channel = 1
```

- [ ] **Step 4: Verify it compiles**

Temporarily create a minimal `main.rs` that just loads config:

```rust
mod config_file;
use config_file::Config;

fn main() {
    let cfg = Config::new();
    println!("Loaded {} capture devices, {} total channels",
        cfg.capture_device.len(), cfg.total_capture_channels());
}
```

Comment out or delete the old module files temporarily to avoid compile errors.

Run: `cargo check`
Expected: compiles successfully

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/config_file.rs config.toml src/main.rs
git commit -m "feat: update deps and config structure for ALSA/UDP migration"
```

---

### Task 2: ALSA Capture Module

**Files:**
- Create: `src/alsa_capture.rs`

**Reference docs:** `alsa` crate — `PCM::open`, `HwParams`, `pcm::IO`, `readi`. The crate wraps libasound; the Rust API mirrors ALSA C API closely.

- [ ] **Step 1: Create alsa_capture.rs**

```rust
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use crossbeam::channel::Sender;
use ringbuf::traits::{Producer, Split};
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub type CaptureRingProducer = ringbuf::HeapProd<i16>;
pub type CaptureRingConsumer = ringbuf::HeapCons<i16>;

pub struct CaptureDevice {
    pub device_name: String,
    pub n_channel: usize,
}

/// Open and configure an ALSA PCM device for capture.
fn open_capture(
    device_name: &str,
    sample_rate: u32,
    n_channel: u32,
    period: u64,
    n_period: u64,
) -> Result<PCM, Box<dyn std::error::Error>> {
    let pcm = PCM::open(
        &*alsa::CString::new(device_name)?,
        Direction::Capture,
        false, // blocking
    )?;

    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        hwp.set_rate(sample_rate, ValueOr::Nearest)?;
        hwp.set_channels(n_channel)?;
        hwp.set_period_size(period as alsa::pcm::Frames, ValueOr::Nearest)?;
        hwp.set_buffer_size((period * n_period) as alsa::pcm::Frames)?;
        pcm.hw_params(&hwp)?;
    }

    pcm.prepare()?;
    Ok(pcm)
}

/// Start the primary capture device thread.
/// Sends each period of interleaved i16 samples via crossbeam channel.
pub fn start_primary_capture(
    device: CaptureDevice,
    sample_rate: usize,
    period: usize,
    n_period: usize,
    sender: Sender<Vec<i16>>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pcm = match open_capture(
            &device.device_name,
            sample_rate as u32,
            device.n_channel as u32,
            period as u64,
            n_period as u64,
        ) {
            Ok(pcm) => pcm,
            Err(e) => {
                eprintln!("Failed to open primary capture {}: {}", device.device_name, e);
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
        };

        let io = pcm.io_i16().unwrap();
        let frame_size = period * device.n_channel;
        let mut buf = vec![0i16; frame_size];

        println!("Primary capture started: {} ({} ch)", device.device_name, device.n_channel);

        while !shutdown.load(Ordering::Relaxed) {
            match io.readi(&mut buf) {
                Ok(n) => {
                    if n != period {
                        eprintln!("Primary capture: short read {} < {}", n, period);
                    }
                    if sender.send(buf.clone()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Primary capture error: {}", e);
                    // Try to recover from xrun
                    if pcm.prepare().is_err() {
                        shutdown.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        println!("Primary capture stopped: {}", device.device_name);
    })
}

/// Start a secondary capture device thread.
/// Writes interleaved i16 samples into a lock-free ring buffer.
pub fn start_secondary_capture(
    device: CaptureDevice,
    sample_rate: usize,
    period: usize,
    n_period: usize,
    mut producer: CaptureRingProducer,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pcm = match open_capture(
            &device.device_name,
            sample_rate as u32,
            device.n_channel as u32,
            period as u64,
            n_period as u64,
        ) {
            Ok(pcm) => pcm,
            Err(e) => {
                eprintln!("Failed to open secondary capture {}: {}", device.device_name, e);
                return;
            }
        };

        let io = pcm.io_i16().unwrap();
        let frame_size = period * device.n_channel;
        let mut buf = vec![0i16; frame_size];

        println!("Secondary capture started: {} ({} ch)", device.device_name, device.n_channel);

        while !shutdown.load(Ordering::Relaxed) {
            match io.readi(&mut buf) {
                Ok(_) => {
                    producer.push_slice(&buf);
                }
                Err(e) => {
                    eprintln!("Secondary capture {} error: {}", device.device_name, e);
                    if pcm.prepare().is_err() {
                        break;
                    }
                }
            }
        }
        println!("Secondary capture stopped: {}", device.device_name);
    })
}

/// Create a ring buffer for a secondary device.
/// Capacity: 4 × period × n_channel (enough for drift compensation).
pub fn create_ring_buffer(period: usize, n_channel: usize) -> (CaptureRingProducer, CaptureRingConsumer) {
    let capacity = period * n_channel * 4;
    let rb = HeapRb::<i16>::new(capacity);
    rb.split()
}
```

- [ ] **Step 2: Add module to main.rs and verify compile**

Add `mod alsa_capture;` to the temporary `main.rs`. Run: `cargo check`

Expected: compiles (the alsa crate won't link without libasound-dev, but `cargo check` should pass type-checking if on a system with headers, or fail gracefully). If on a dev machine without ALSA headers, install: `apt-get install libasound2-dev`.

- [ ] **Step 3: Commit**

```bash
git add src/alsa_capture.rs src/main.rs
git commit -m "feat: add ALSA multi-device capture module"
```

---

### Task 3: Playback Module (cpal)

**Files:**
- Create: `src/alsa_playback.rs`

**Reference docs:** `cpal` crate — `default_host()`, `Device::build_output_stream()`, `StreamConfig`. cpal uses a callback model similar to JACK's process callback.

- [ ] **Step 1: Create alsa_playback.rs**

```rust
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig, BufferSize};
use ringbuf::traits::Consumer;
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub type PlaybackRingProducer = ringbuf::HeapProd<i16>;
pub type PlaybackRingConsumer = ringbuf::HeapCons<i16>;

/// Create a ring buffer for playback.
/// Capacity: 4 × sample_per_packet × n_channel.
pub fn create_playback_ring(sample_per_packet: usize, n_channel: usize) -> (PlaybackRingProducer, PlaybackRingConsumer) {
    let capacity = sample_per_packet * n_channel * 4;
    let rb = HeapRb::<i16>::new(capacity);
    ringbuf::traits::Split::split(rb)
}

/// Start cpal playback.
/// Returns the stream handle (must be kept alive).
pub fn start_playback(
    device_name: &str,
    sample_rate: usize,
    n_channel: usize,
    period: usize,
    mut consumer: PlaybackRingConsumer,
    shutdown: Arc<AtomicBool>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let host = cpal::default_host();

    let device = if device_name.is_empty() || device_name == "default" {
        host.default_output_device()
            .ok_or("no default output device")?
    } else {
        host.output_devices()?
            .find(|d| d.name().map(|n| n.contains(device_name)).unwrap_or(false))
            .ok_or_else(|| format!("output device '{}' not found", device_name))?
    };

    println!("Playback device: {}", device.name().unwrap_or_default());

    let config = StreamConfig {
        channels: n_channel as u16,
        sample_rate: SampleRate(sample_rate as u32),
        buffer_size: BufferSize::Fixed(period as u32),
    };

    let stream = device.build_output_stream(
        &config,
        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
            let read = consumer.pop_slice(data);
            // Fill remaining with silence if ring buffer underrun
            for sample in &mut data[read..] {
                *sample = 0;
            }
        },
        move |err| {
            eprintln!("Playback stream error: {}", err);
        },
        None,
    )?;

    stream.play()?;
    println!("Playback started ({} ch, {} Hz)", n_channel, sample_rate);

    Ok(stream)
}
```

- [ ] **Step 2: Add module to main.rs and verify compile**

Add `mod alsa_playback;` to temporary `main.rs`. Run: `cargo check`

- [ ] **Step 3: Commit**

```bash
git add src/alsa_playback.rs src/main.rs
git commit -m "feat: add cpal cross-platform playback module"
```

---

### Task 4: Transport Server (UDP/TCP)

**Files:**
- Create: `src/transport_server.rs`
- Delete: `src/tcp_server.rs`

- [ ] **Step 1: Create transport_server.rs**

```rust
use bytes::Bytes;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, Semaphore};
use tokio::time::{self, Duration, Instant};

// ── UDP Server ──

pub async fn start_udp_server(
    port: usize,
    max_clients: usize,
    pkt_sender: broadcast::Sender<Bytes>,
    shutdown: impl Future,
) {
    let socket = Arc::new(
        UdpSocket::bind(format!("0.0.0.0:{}", port))
            .await
            .expect("Failed to bind UDP socket"),
    );
    println!("UDP server listening on port {}", port);

    let clients: Arc<tokio::sync::Mutex<HashMap<SocketAddr, Instant>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // Registration listener task
    let reg_socket = socket.clone();
    let reg_clients = clients.clone();
    let reg_handle = tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            match reg_socket.recv_from(&mut buf).await {
                Ok((_, addr)) => {
                    let mut map = reg_clients.lock().await;
                    if map.len() < max_clients || map.contains_key(&addr) {
                        map.insert(addr, Instant::now());
                    }
                }
                Err(e) => {
                    eprintln!("UDP registration error: {}", e);
                }
            }
        }
    });

    // Broadcast sender task
    let send_socket = socket.clone();
    let send_clients = clients.clone();
    let mut receiver = pkt_sender.subscribe();
    let send_handle = tokio::spawn(async move {
        loop {
            let packet = match receiver.recv().await {
                Ok(pkt) => pkt,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("UDP broadcast lagged by {} packets", n);
                    continue;
                }
                Err(_) => break,
            };

            let mut map = send_clients.lock().await;
            // Remove stale clients (no registration in 5 seconds)
            let now = Instant::now();
            map.retain(|_, last_seen| now.duration_since(*last_seen) < Duration::from_secs(5));

            for addr in map.keys() {
                let _ = send_socket.send_to(&packet, addr).await;
            }
        }
    });

    shutdown.await;
    println!("Shutting down UDP server");
    reg_handle.abort();
    send_handle.abort();
}

// ── TCP Server ──

pub async fn start_tcp_server(
    port: usize,
    max_clients: usize,
    pkt_sender: broadcast::Sender<Bytes>,
    shutdown: impl Future,
) {
    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("Failed to bind TCP listener");
    println!("TCP server listening on port {}", port);

    let semaphore = Arc::new(Semaphore::new(max_clients));
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    let accept_handle = tokio::spawn({
        let shutdown_tx = shutdown_tx.clone();
        let pkt_sender = pkt_sender.clone();
        let semaphore = semaphore.clone();
        async move {
            loop {
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let (socket, addr) = match listener.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("TCP accept error: {}", e);
                        continue;
                    }
                };
                println!("TCP connection from {}", addr);
                let _ = socket.set_nodelay(true);

                let mut receiver = pkt_sender.subscribe();
                let mut shutdown_rx = shutdown_tx.subscribe();

                tokio::spawn(async move {
                    let mut socket = socket;
                    loop {
                        tokio::select! {
                            result = receiver.recv() => {
                                match result {
                                    Ok(packet) => {
                                        if socket.write_all(&packet).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                    Err(_) => break,
                                }
                            }
                            _ = shutdown_rx.recv() => break,
                        }
                    }
                    println!("{} disconnected", addr);
                    drop(permit);
                });
            }
        }
    });

    shutdown.await;
    println!("Shutting down TCP server");
    let _ = shutdown_tx.send(());
    accept_handle.abort();
}

/// Start the appropriate server based on protocol config.
pub async fn start_server(
    protocol: &str,
    port: usize,
    max_clients: usize,
    pkt_sender: broadcast::Sender<Bytes>,
    shutdown: impl Future,
) {
    match protocol {
        "udp" => start_udp_server(port, max_clients, pkt_sender, shutdown).await,
        "tcp" => start_tcp_server(port, max_clients, pkt_sender, shutdown).await,
        other => panic!("Unknown sender protocol: {}", other),
    }
}
```

- [ ] **Step 2: Add module to main.rs and verify compile**

Add `mod transport_server;` to temporary `main.rs`. Run: `cargo check`

- [ ] **Step 3: Commit**

```bash
git add src/transport_server.rs src/main.rs
git rm src/tcp_server.rs
git commit -m "feat: add UDP/TCP transport server, remove old TCP-only server"
```

---

### Task 5: Transport Client (UDP/TCP)

**Files:**
- Create: `src/transport_client.rs`
- Delete: `src/tcp_client.rs`

- [ ] **Step 1: Create transport_client.rs**

```rust
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncReadExt;
use tokio::net::{self, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

// ── UDP Client ──

async fn udp_client_loop(
    host: &str,
    port: usize,
    pkt_size: usize,
    sender: mpsc::Sender<Vec<u8>>,
    shutdown: &AtomicBool,
) {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind UDP client socket");

    let server_addr = format!("{}:{}", host, port);
    // Connect to server so we can use recv() instead of recv_from()
    socket.connect(&server_addr).await.expect("Failed to connect UDP socket");
    // Send initial registration
    let _ = socket.send(b"register").await;

    let mut buf = vec![0u8; pkt_size];
    let mut registration_interval = time::interval(Duration::from_secs(2));

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            result = socket.recv(&mut buf) => {
                match result {
                    Ok(n) if n == pkt_size => {
                        let _ = sender.send(buf[..n].to_vec()).await;
                    }
                    Ok(n) => {
                        eprintln!("UDP client: unexpected packet size {} (expected {})", n, pkt_size);
                    }
                    Err(e) => {
                        eprintln!("UDP client recv error: {}", e);
                    }
                }
            }
            _ = registration_interval.tick() => {
                let _ = socket.send(b"register").await;
            }
        }
    }
}

// ── TCP Client ──

async fn tcp_client_loop(
    host: &str,
    port: usize,
    pkt_size: usize,
    sender: mpsc::Sender<Vec<u8>>,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let addr = format!("{}:{}", host, port);
        match TcpStream::connect(&addr).await {
            Ok(mut stream) => {
                println!("TCP client connected to {}", addr);
                let mut pkt_buf = Vec::<u8>::with_capacity(pkt_size * 2);
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    match stream.read_buf(&mut pkt_buf).await {
                        Ok(0) => break,
                        Ok(_) => {
                            // Process all complete packets in the buffer
                            while pkt_buf.len() >= pkt_size {
                                let _ = sender.send(pkt_buf[..pkt_size].to_vec()).await;
                                pkt_buf.drain(..pkt_size);
                            }
                        }
                        Err(e) => {
                            eprintln!("TCP client read error: {}", e);
                            break;
                        }
                    }
                }
                println!("TCP client disconnected from {}", addr);
            }
            Err(_) => {
                if net::lookup_host(&addr).await.is_err() {
                    return;
                }
                time::sleep(Duration::from_secs(2)).await;
                println!("TCP client reconnecting...");
            }
        }
    }
}

/// Start the appropriate client based on protocol config.
pub async fn start_client(
    protocol: &str,
    host: &str,
    port: usize,
    pkt_size: usize,
    sender: mpsc::Sender<Vec<u8>>,
    shutdown: impl Future,
) {
    let stop = AtomicBool::new(false);

    tokio::select! {
        _ = async {
            match protocol {
                "udp" => udp_client_loop(host, port, pkt_size, sender, &stop).await,
                "tcp" => tcp_client_loop(host, port, pkt_size, sender, &stop).await,
                other => panic!("Unknown receiver protocol: {}", other),
            }
        } => {}
        _ = shutdown => {
            stop.store(true, Ordering::Relaxed);
            println!("Transport client shutting down");
        }
    }
}
```

- [ ] **Step 2: Add module to main.rs and verify compile**

Add `mod transport_client;` to temporary `main.rs`. Run: `cargo check`

- [ ] **Step 3: Commit**

```bash
git add src/transport_client.rs src/main.rs
git rm src/tcp_client.rs
git commit -m "feat: add UDP/TCP transport client, remove old TCP-only client"
```

---

### Task 6: Main Orchestration

**Files:**
- Rewrite: `src/main.rs`
- Delete: `src/system_call.rs`, `src/jack_client.rs`, `src/ring_buf.rs`

This is the core integration task. `main.rs` wires up capture → packet assembly → transport, and transport client → playback.

- [ ] **Step 1: Rewrite main.rs**

```rust
type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

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

use bytes::{Bytes, BytesMut, BufMut};
use crossbeam::channel::bounded;
use ringbuf::traits::Consumer;
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

    let recv_pkt_len = HEADER_LEN + cfg.receiver.n_channel * sample_per_packet * 2;

    println!(
        "Capture: {} devices, {} total channels, packet size {}",
        cfg.capture_device.len(),
        total_capture_ch,
        send_pkt_len,
    );

    let shutdown = Arc::new(AtomicBool::new(false));

    // ── Start capture devices ──

    let (primary_tx, primary_rx) = bounded::<Vec<i16>>(2);

    let mut secondary_consumers = Vec::new();
    let mut capture_threads = Vec::new();

    for (i, dev_cfg) in cfg.capture_device.iter().enumerate() {
        let device = CaptureDevice {
            device_name: dev_cfg.device_name.clone(),
            n_channel: dev_cfg.n_channel,
        };

        if i == 0 {
            // Primary device
            capture_threads.push(start_primary_capture(
                device,
                sample_rate,
                period,
                n_period,
                primary_tx.clone(),
                shutdown.clone(),
            ));
        } else {
            // Secondary device
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
    drop(primary_tx); // Close our copy; capture thread holds the sender

    // ── Broadcast channel for transport ──

    let (pkt_broadcast_tx, _) = broadcast::channel::<Bytes>(4);

    // ── Start transport server ──

    let server_handle = {
        let tx = pkt_broadcast_tx.clone();
        let protocol = cfg.sender.protocol.clone();
        let port = cfg.sender.listen_port;
        let max_clients = cfg.sender.max_clients;
        tokio::spawn(async move {
            start_server(&protocol, port, max_clients, tx, tokio::signal::ctrl_c()).await;
        })
    };

    // ── Start transport client + playback ──

    let (recv_tx, mut recv_rx) = mpsc::channel::<Vec<u8>>(4);

    let client_handle = {
        let protocol = cfg.receiver.protocol.clone();
        let host = cfg.receiver.host.clone();
        let port = cfg.receiver.port;
        tokio::spawn(async move {
            start_client(&protocol, &host, port, recv_pkt_len, recv_tx, tokio::signal::ctrl_c()).await;
        })
    };

    // Start playback if receiver is configured
    let _playback_stream = if cfg.receiver.host != "none" {
        let (pb_producer, pb_consumer) = create_playback_ring(sample_per_packet, cfg.receiver.n_channel);

        // Playback ring writer task
        let recv_n_ch = cfg.receiver.n_channel;
        tokio::spawn(async move {
            let mut pb_producer = pb_producer;
            let mut last_pkt_id: Option<i32> = None;
            while let Some(pkt) = recv_rx.recv().await {
                if pkt.len() < HEADER_LEN {
                    continue;
                }
                let pkt_id = i32::from_le_bytes(pkt[8..12].try_into().unwrap());

                // Detect gaps for UDP loss — skip means silence (ring not written)
                if let Some(last) = last_pkt_id {
                    if pkt_id != last.wrapping_add(1) && pkt_id > last {
                        // Gap detected — silence is automatic (we just don't write)
                    } else if pkt_id <= last && pkt_id != 0 {
                        // Out of order — drop
                        continue;
                    }
                }
                last_pkt_id = Some(pkt_id);

                let audio_data = &pkt[HEADER_LEN..];
                // Convert u8 to i16 and push to ring
                let samples: &[i16] = unsafe {
                    std::slice::from_raw_parts(
                        audio_data.as_ptr() as *const i16,
                        audio_data.len() / 2,
                    )
                };
                use ringbuf::traits::Producer;
                pb_producer.push_slice(samples);
            }
        });

        match start_playback(
            &cfg.playback.device_name,
            sample_rate,
            cfg.receiver.n_channel,
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
        // Drain recv_rx even if not playing back
        tokio::spawn(async move {
            while recv_rx.recv().await.is_some() {}
        });
        None
    };

    // ── Packet assembly loop ──

    let assembly_shutdown = shutdown.clone();
    let pkt_tx = pkt_broadcast_tx.clone();

    let assembly_handle = tokio::task::spawn_blocking(move || {
        let mut pkt_id: i32 = 0;
        let primary_n_ch = cfg.capture_device[0].n_channel;

        // Accumulation buffers for when sample_per_packet != period
        // Each device accumulates interleaved samples until sample_per_packet frames are ready.
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
            // Wait for primary device (one period of data)
            let primary_data = match primary_rx.recv() {
                Ok(data) => data,
                Err(_) => break, // Channel closed — primary device stopped
            };

            // Accumulate primary data
            primary_accum.extend_from_slice(&primary_data);

            // Read one period from each secondary device into accum
            for (idx, (n_ch, consumer)) in secondary_consumers.iter_mut().enumerate() {
                let frame_count = period * *n_ch;
                let available = consumer.occupied_len();

                // Drift compensation
                if available > period * *n_ch * 2 {
                    // Too fast — skip 1 frame
                    let mut discard = vec![0i16; *n_ch];
                    consumer.pop_slice(&mut discard);
                }

                let read = consumer.pop_slice(&mut secondary_bufs[idx][..frame_count]);
                if read < frame_count {
                    // Not enough data — fill with last frame (duplicate)
                    for j in read..frame_count {
                        secondary_bufs[idx][j] = last_frames[idx][j % *n_ch];
                    }
                } else {
                    // Save last frame for potential duplication
                    let start = frame_count - *n_ch;
                    last_frames[idx].copy_from_slice(&secondary_bufs[idx][start..frame_count]);
                }
                secondary_accums[idx].extend_from_slice(&secondary_bufs[idx][..frame_count]);
            }

            // Check if we've accumulated enough for a packet
            if primary_accum.len() < sample_per_packet * primary_n_ch {
                continue; // Need more periods before sending
            }

            // Assemble packet
            let mut pkt = BytesMut::with_capacity(send_pkt_len);

            // Header
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

            // Primary device audio
            let primary_samples = sample_per_packet * primary_n_ch;
            let primary_bytes = unsafe {
                std::slice::from_raw_parts(
                    primary_accum.as_ptr() as *const u8,
                    primary_samples * 2,
                )
            };
            pkt.put_slice(primary_bytes);
            primary_accum.drain(..primary_samples);

            // Secondary devices
            for (idx, (n_ch, _)) in secondary_consumers.iter().enumerate() {
                let sec_samples = sample_per_packet * n_ch;
                let sec_bytes = unsafe {
                    std::slice::from_raw_parts(
                        secondary_accums[idx].as_ptr() as *const u8,
                        sec_samples * 2,
                    )
                };
                pkt.put_slice(sec_bytes);
                secondary_accums[idx].drain(..sec_samples);
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
    client_handle.abort();

    for handle in capture_threads {
        let _ = handle.join();
    }

    println!("Shutdown complete");
}
```

- [ ] **Step 2: Delete old files**

```bash
git rm src/jack_client.rs src/system_call.rs src/ring_buf.rs
```

- [ ] **Step 3: Verify full build compiles**

Run: `cargo check`
Expected: compiles with no errors (warnings are OK for now)

- [ ] **Step 4: Run cargo clippy**

Run: `cargo clippy -- -W clippy::all`
Fix any warnings.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: rewrite main orchestration for ALSA/cpal/UDP architecture

Replaces JACK with direct ALSA capture, cpal playback, and
UDP/TCP selectable transport. Implements multi-device clock
drift compensation and zero-copy packet assembly with Bytes."
```

---

### Task 7: Integration Verification

**Files:** None (testing only)

- [ ] **Step 1: Verify full build**

Run: `cargo build --release`
Expected: builds successfully

- [ ] **Step 2: Verify config loading**

Run: `cargo run --release` (will fail on audio device but should print config info first)
Expected: prints "Capture: 1 devices, 16 total channels, packet size ..."

- [ ] **Step 3: Verify no old code remains**

Check that no references to `jack`, `system_call`, `ring_buf`, `tcp_server`, `tcp_client` remain:
```bash
grep -r "jack\|system_call\|ring_buf\|tcp_server\|tcp_client" src/
```
Expected: no matches (except possibly comments or the cpal jack feature flag in Cargo.toml)

- [ ] **Step 4: Final commit with clean state**

```bash
git status
# If any remaining changes:
git add -A
git commit -m "chore: clean up migration to ALSA/UDP architecture"
```
