# Direct ALSA Multi-Device Capture + UDP/TCP Transport

## Summary

Replace JACK with direct ALSA capture and cpal playback. Add UDP as default transport with TCP fallback. Support multiple capture devices with clock drift compensation.

## Motivation

- JACK adds an unnecessary buffer layer (~2-4ms latency) for a use case that doesn't need its routing/mixing capabilities
- `alsa_in` bridge for multi-device is unstable at small period sizes
- TCP introduces head-of-line blocking on packet loss; UDP is better suited for real-time audio
- Removing jackd simplifies deployment (one fewer external process)

## Configuration

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

[[capture_device]]
device_name = "hw:USBMic"
n_channel = 2

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

Key changes from current config:
- `sample_rate`, `period`, `n_period` moved to `[general]`, shared globally
- `[[capture_device]]` is a TOML array, supports arbitrary number of devices
- `header_len` removed from config, defined as constant in code (fixed 12 bytes)
- `sample_per_packet` default lowered to 32 (= period) to minimize accumulation latency
- New `protocol` field in `[sender]` and `[receiver]`: `"udp"` or `"tcp"`

**Migration**: Old `config.toml` files (with `[mic]`, `[tcp_sender]`, etc.) will fail to parse with a clear serde error. No automatic migration — users must update their config to the new format. The default config generated on parse failure will use the new structure.

## Architecture

### Module Structure

| File | Role |
|------|------|
| `alsa_capture.rs` | ALSA multi-device capture, one thread per device |
| `alsa_playback.rs` | cpal cross-platform playback (ALSA on Linux, WASAPI on Windows) |
| `transport_server.rs` | UDP/TCP server, broadcasts audio to connected clients |
| `transport_client.rs` | UDP/TCP client, receives audio from remote server |
| `config_file.rs` | New config structure with serde |
| `main.rs` | Orchestration: device→packet assembly→transport, recv→playback |

Deleted files: `jack_client.rs`, `tcp_server.rs`, `tcp_client.rs`, `system_call.rs`, `ring_buf.rs`

### Data Flow

```
Capture thread 0 (primary) ──channel──→ ┐
Capture thread 1 (secondary) ──ringbuf──→ ├─ packet assembly ──→ broadcast channel ──→ transport_server
Capture thread N (secondary) ──ringbuf──→ ┘

transport_client ──→ unpack to ring buffer ──→ cpal playback callback reads
```

### Audio Capture (alsa_capture.rs)

Each `[[capture_device]]` gets a dedicated OS thread with blocking `readi`:

- **Primary device** (first in config): sends frames via `crossbeam::channel` to the packet assembly task. This channel recv is the system's timing heartbeat.
- **Secondary devices**: write frames into per-device lock-free ring buffers (`ringbuf` crate).

ALSA configuration per device:
- Format: `S16_LE` (native i16, no f32 conversion needed)
- Access: `RW_INTERLEAVED`
- Period size, buffer size, sample rate from `[general]`

### Clock Drift Compensation

Secondary devices have independent clocks. Compensation happens during packet assembly.

Ring buffer water level is measured in **frames** (1 frame = 1 sample per channel = `n_channel` i16 values = `n_channel × 2` bytes).

1. Primary device channel recv blocks until a period of audio arrives (drives system tempo)
2. For each secondary device, read `period` frames from its ring buffer
3. Monitor ring buffer water level (in frames):
   - Level > 2×period frames → secondary is faster → skip 1 frame to catch up
   - Level < period/2 frames → secondary is slower → duplicate last frame
   - Normal range → read normally

At 16kHz, drift between USB devices is typically a few ppm. Compensation triggers roughly once every few seconds to tens of seconds — inaudible.

### Device Hot-Unplug

- If a secondary device disconnects, its capture thread exits. The packet assembly loop detects insufficient data in the ring buffer and fills that device's channels with silence. The system continues streaming with remaining devices.
- If the primary device disconnects, the system shuts down gracefully (the primary drives the heartbeat; without it there is no timing source).

### Audio Playback (alsa_playback.rs)

Uses `cpal` for cross-platform support (Linux ALSA, Windows WASAPI, macOS CoreAudio):

- One dedicated thread/callback
- Reads from a ring buffer (3-4 packets capacity, ~6-8ms)
- `transport_client` unpacks received data and writes into this ring buffer
- If ring buffer is empty (network dropout), output silence to avoid underrun

### Transport Server (transport_server.rs)

Shared broadcast channel feeds all client handlers. Protocol selected by config.

**UDP mode:**
- Single `UdpSocket` bound to `listen_port`
- Clients send a registration datagram (any content) to "connect"
- Server records client address, sends audio datagrams via `sendto`
- Client auto-removed after 5 seconds without re-registration
- Each packet = one UDP datagram (header + audio), no reassembly needed

**TCP mode:**
- Preserves current `tcp_server.rs` logic: TcpListener, per-client spawn, `set_nodelay(true)`
- Semaphore-based connection limit

### Transport Client (transport_client.rs)

**UDP mode:**
- Bind local port, send registration datagram to server
- Re-send registration every 2 seconds to stay "alive"
- `recv` returns complete packets directly

**TCP mode:**
- Preserves current connection + reconnection logic

### Packet Format

Header is unchanged. Payload layout changes to accommodate multi-device (previously single device only):

```
Header (12 bytes):
  device_id:    u16 (LE)
  timestamp_s:  u32 (LE)
  timestamp_ms: u16 (LE) — adjusted by packet_time_len
  packet_id:    u32 (LE) — wrapping i32

Payload (channels packed sequentially across all devices):
  Device 0 ch0: [sample_per_packet × i16 LE]
  Device 0 ch1: [sample_per_packet × i16 LE]
  ...
  Device 1 ch0: [sample_per_packet × i16 LE]
  Device 1 ch1: [sample_per_packet × i16 LE]
  ...
```

Total packet size = 12 + total_channels × sample_per_packet × 2

The receiver sees a flat list of channels — it does not need to know the device topology. This is backward compatible with single-device setups.

**UDP packet loss handling**: The receiver uses `packet_id` to detect gaps. On a missing packet, the playback ring buffer is not written to — the playback side outputs silence for the gap duration. No reordering buffer; out-of-order packets are dropped (real-time audio cannot wait for retransmission).

### Broadcast Channel Optimization

- Capacity reduced from 16 to 4
- Payload type changed from `Vec<u8>` to `bytes::Bytes`
- Packet assembled using `BytesMut`, frozen to `Bytes` — reference-counted, zero per-subscriber clone
- No self-consumer `packet_receiver.recv()` in the send loop

## Dependency Changes

| Remove | Add |
|--------|-----|
| `jack` | `alsa` (Linux capture) |
| `arc-swap` | `cpal` (cross-platform playback) |
| | `ringbuf` (lock-free SPSC ring buffer) |

Retain: `tokio`, `bytes`, `serde`, `toml`, `crossbeam`

## Latency Estimate

| Stage | Before | After |
|-------|--------|-------|
| Capture buffer (JACK/ALSA) | 8ms (32×4) | 6ms (32×3) |
| Packet accumulation | 10ms (160 samples) | 0ms (packet = period) |
| Scheduling + assembly | ~1ms (Vec clone) | ~0.1ms (Bytes) |
| Network | TCP ~1ms | UDP ~0.5ms |
| Playback ring buffer | 2-10ms | 6-8ms (stable) |
| Playback ALSA/cpal buffer | 8ms | 6ms |
| **Total** | **~30-40ms** | **~19-21ms** |

All timing parameters (`period`, `n_period`) are configurable. On capable hardware, `period=16, n_period=2` can push total latency to ~15ms.

## Platform Support

| Component | Linux | Windows | macOS |
|-----------|-------|---------|-------|
| Capture (`alsa` crate) | Yes | No | No |
| Playback (`cpal` crate) | Yes | Yes | Yes |
| Transport (tokio UDP/TCP) | Yes | Yes | Yes |

Capture is Linux-only (target: Raspberry Pi with mic arrays). The receiver/playback side compiles and runs on all platforms.
