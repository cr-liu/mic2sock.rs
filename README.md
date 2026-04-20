# mic2sock

Capture multi-channel audio from ALSA devices and stream it to network clients over UDP (default) or TCP.

Designed for Raspberry Pi with microphone arrays. Receivers can run on any platform with a UDP socket.

## Build

Linux, requires ALSA headers:

```bash
sudo apt-get install libasound2-dev pkg-config
cargo build --release
```

## Configure

Edit `config.toml`. Minimal example:

```toml
[general]
sample_rate = 16000
period = 32
n_period = 3
sample_per_packet = 32
device_id = 0

[[capture_device]]
device_name = "hw:Device"   # check with: arecord -l
n_channel = 1

[playback]
device_name = "plughw:Device"
n_channel = 1

[sender]
protocol = "udp"            # or "tcp"
listen_port = 7998
max_clients = 100

[receiver]
protocol = "udp"
host = "none"               # "none" disables local playback
port = 4000
n_channel = 1
```

Multiple capture devices are supported — just add more `[[capture_device]]` blocks. Channels from all devices are concatenated.

### Static receivers (optional)

Pre-configure destinations that don't need to register:

```toml
[sender]
static_receivers = ["192.168.1.100:7999"]
```

## Run

```bash
./target/release/mic2sock
```

Config is read from `./config.toml`. Ctrl+C to stop.

## Web GUI

On startup the binary serves a web GUI at `http://<bind_addr>:<port>` (default `http://0.0.0.0:8080`). Log in with the password from `[gui] password` (default: `test`).

Features:
- Edit `config.toml` in the browser. `static_receivers` hot-reloads; other fields prompt "restart required"
- Live waveform of the primary capture device's channel 0
- Restart button (requires systemd/supervisor to auto-relaunch)

```toml
[gui]
enabled = true           # set false to disable
bind_addr = "0.0.0.0"    # use "127.0.0.1" to restrict to local
port = 8080
password = "test"        # empty = no auth
```

## Packet Format

Header (12 bytes, LE) + channel-major payload:

```
[device_id:u16][ts_s:u32][ts_ms:i16][pkt_id:i32]
[ch0: sample_per_packet × i16 LE]
[ch1: sample_per_packet × i16 LE]
...
```

Total size: `12 + total_channels × sample_per_packet × 2`

## Companion Receiver

See [stream-playback](https://github.com/cr-liu/stream-playback) for a minimal UDP receiver that plays ch0 through the system default audio output (macOS/Windows/Linux).
