# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`mic2sock` — a Rust daemon that captures multi-channel microphone audio through JACK, packs it into
timestamped binary packets, and broadcasts those packets to any number of TCP clients. Optionally it
also runs as a TCP *client*: it pulls a remote mono/multi-channel audio stream, plays it out through
JACK, and re-embeds ("resend") that same audio as extra trailing channels in its own outgoing packets
so downstream consumers get a time-aligned mic + far-end recording.

Targets Linux/ALSA (Raspberry Pi + ReSpeaker/16-ch arrays) and macOS/CoreAudio; the git history shows
partial Windows work (`start_jackd = false` path).

## Build / run

Requires the JACK **development** package at build time (`jack-sys` resolves `jack` via pkg-config —
without `jack.pc` the build script panics), and `jackd` on `PATH` at runtime.

```bash
sudo apt install libjack-jackd2-dev jackd2   # Debian/Ubuntu; brew install jack on macOS

# 纯逻辑 crate（protocol / clocksync）—— 不需要 libjack，任何机器/CI 都能跑
cargo test
cargo clippy -p protocol -p clocksync --all-targets

# Pi 上的守护进程 —— 需要 libjack
cargo build -p mic2sock --release
cargo run -p mic2sock
cargo clippy -p mic2sock --all-targets

cargo fmt
```

这是一个 cargo workspace，`default-members = ["protocol", "clocksync"]` **刻意排除了
`mic2sock`** —— 后者需要 libjack，在没有 `jack.pc` 的机器上 `jack-sys` 的 build script
会 panic。所以裸 `cargo build` / `cargo test` 只处理纯逻辑 crate（而且会静默地**不**构建
守护进程），构建或运行 daemon 必须显式写 `-p mic2sock`。

Run from a directory containing `config.toml` — the path is relative to the CWD, so `cargo run` from
the repo root works. If `config.toml` is missing or fails to parse, the program does **not** fail: it
writes defaults to `conf.toml`, prints "please rename it to config.toml", and keeps running on those
defaults. A confusing config bug is usually this path being hit silently.

There are no tests (`#[cfg(test)]` appears nowhere). Verification is empirical: run the daemon and
attach a client. `asio_client.cpp` is a standalone reference consumer that prints packet headers; it
is in no build system — compile it by hand against standalone Asio, and note its `pkt_len = 5452` is
hardcoded for 16 mic + 1 resend channel at 160 samples/packet.

## Wire format

Little-endian throughout. Sender packet = `tcp_sender.header_len` + `n_ch * sample_per_packet * 2`
bytes, where `n_ch = mic.n_channel + speaker.n_channel`.

| offset | type  | field                                       |
|--------|-------|---------------------------------------------|
| 0..2   | u16   | `mic.device_id`                             |
| 2..6   | u32   | Unix seconds                                |
| 6..8   | i16   | milliseconds                                |
| 8..12  | i32   | monotonically increasing packet id (wraps at `i32::MAX`) |

Audio is **channel-blocked, not sample-interleaved**: all samples of channel 0, then all of channel 1,
etc. Mic channels come first, resend (far-end) channels last. Timestamps are back-dated — 10 ms of
fudge minus one packet duration — so they name the *start* of the packet's audio.

The header offsets are hardcoded in `process_send_buf`, so `tcp_sender.header_len` must stay 12.
`tcp_receiver.header_len` is genuinely variable (default 16): the parser reads the same 2..12 fields
but payload slicing starts at `recv_header_len`.

## Architecture

Startup order in `main.rs` matters and is load-bearing:

1. `system_call::start_jackd` spawns `jackd` as a child (`kill_on_drop(true)`, `-R` realtime), with
   driver/device/period/rate from config. A fixed `sleep(1500ms)` is the only barrier before the
   client connects — a slow-starting server shows up as a `jack::Client::new().unwrap()` panic.
2. `jack_client::inspect_device` counts physical capture ports and playback input ports, then main
   **clamps** `mic.n_channel` / `speaker.n_channel` down to what the hardware actually has, mutating
   the config in place via `Arc::get_mut`. This only works while the `Arc` has exactly one strong
   reference. Adding a `cfg.clone()` that outlives this point turns the clamp into a silent no-op and
   the port-connect loops then index past the end of `in_ports_name` and panic.
3. Three families of lock-free SPSC `jack::RingBuffer` pairs are created, one buffer per channel:
   `capture_buf` (RT thread → sender task), `resend_buf` and `playback_buf` (receiver task → sender
   task / RT thread respectively).
4. The JACK client runs on a **plain `std::thread`, deliberately outside the tokio runtime** (see
   commit `ad53afb`) so the realtime process callback never contends with the async scheduler. Nothing
   in that callback may allocate, lock, or await.

Concurrency map — four tokio tasks joined in `main`, plus one OS thread:

- **RT audio thread** (`jack_client::start_jack_client`) — the process callback converts f32↔i16,
  writes each mic channel into `capture_buf`, drains `playback_buf` into the out ports (silence when
  short), and `Notify::notify_one()`s once every `sample_per_packet` frames. Then blocks on a
  crossbeam rendezvous channel until shutdown.
- **`process_send_buf`** — woken by that `Notify`, builds one packet per wakeup and publishes it on a
  `tokio::sync::broadcast` channel.
- **`tcp_server::start_server`** — `TcpListener` on `tcp_sender.listen_port`, connections capped by a
  `Semaphore` (`max_clients`), `set_nodelay(true)`. Each connection is a `SocketHandler` that
  subscribes to the broadcast and `write_all`s every packet; a write error marks it shut down.
- **`tcp_client::start_tcp_client`** — no-op unless `tcp_receiver.host` is set to something other than
  `"none"`. Reconnects every 2 s (giving up only if DNS resolution fails), accumulates `read_buf`
  until a whole packet is present, forwards it over an mpsc channel.
- **`process_recv_buf`** — writes each received channel into both `resend_buf` and `playback_buf`,
  dropping packets when playback space runs low.
- **JACK watchdog** — `Notifications::shutdown` (JACK server died) sets an `AtomicBool`; a task polls
  it every 100 ms, signals the audio thread, and panics the process. Intentional: the daemon is meant
  to be restarted by a supervisor rather than limp on without audio (commit `5f56e47`).

Timing invariant: `sample_per_packet` must be an exact multiple of `mic.period` (default 160 / 32 = 5).
Otherwise the notify cadence drifts away from the data actually in the ring buffers and
`process_send_buf`'s `assert_eq!(n_bytes, ...)` on the read length fires.

`src/ring_buf.rs` is dead code, kept behind `#![allow(dead_code)]` — a hand-rolled ring buffer
superseded by `jack::RingBuffer` (commit `4711c30`). Don't extend it; don't wire it back in.

Error handling convention: `crate::Result<T> = Result<T, Box<dyn Error + Send + Sync>>`, and the
codebase leans hard on `unwrap()`/`panic!` for setup and invariant failures by design — fail loudly at
startup rather than stream corrupt audio.
