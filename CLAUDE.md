# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

mic2sock.rs is a Rust application that bridges JACK audio server with TCP sockets for multi-channel audio streaming. It captures audio from JACK input ports (microphones), assembles packets with headers (device ID, timestamp, packet ID), and broadcasts them over TCP to connected clients. It can also receive audio from a remote TCP server and play it through JACK output ports (speakers). Primary target is Linux/Raspberry Pi with microphone arrays.

## Build & Run

```bash
cargo build --release       # Release build
cargo run --release         # Run (reads config.toml from working directory)
cargo check                 # Type-check without building
```

No test suite exists. No linter or formatter is configured beyond standard `cargo fmt` / `cargo clippy`.

## Runtime Dependencies

- **jackd** — JACK audio server (app can spawn it via config `start_jackd = true`)
- **alsa_out** — optional ALSA output bridge (controlled by `speaker.use_alsa_out`)
- **config.toml** — required at runtime in the working directory

## Architecture

**Threading model**: Tokio async runtime for all TCP networking; JACK real-time audio callbacks run on a dedicated OS thread managed by the JACK library. The two worlds communicate through `jack::RingBuffer` (lock-free SPSC) and `tokio::sync::broadcast` channels.

**Data flow**:
```
JACK mic inputs → RingBuffers → packet assembly (main.rs) → broadcast channel → TCP server → clients
TCP client → receive buffer → RingBuffers → JACK speaker outputs
```

**Modules** (all flat in `src/`):

| Module | Role |
|--------|------|
| `main.rs` | Orchestrator — config loading, JACK↔TCP bridging, packet assembly/disassembly, async task spawning |
| `jack_client.rs` | JACK port registration, process callback (f32↔i16 PCM conversion), port auto-connection |
| `tcp_server.rs` | Tokio TCP listener on configurable port; broadcasts audio packets to all connected clients |
| `tcp_client.rs` | Tokio TCP client with reconnection; receives packets from remote server |
| `config_file.rs` | TOML config parsing with serde; provides defaults when `config.toml` is missing |
| `system_call.rs` | Spawns `jackd` and `alsa_out` as child processes |
| `ring_buf.rs` | Generic ring buffer (unused — superseded by `jack::RingBuffer`) |

**Packet format** (TCP sender): 12-byte header (`device_id: u16`, `timestamp_s: u32`, `timestamp_ms: u16`, `packet_id: u32`) + interleaved i16 PCM samples across all channels.

## Key Patterns

- Global error type: `type Error = Box<dyn std::error::Error + Send + Sync>`
- JACK shutdown sets an `AtomicBool` flag → controlled panic on next check
- Config is wrapped in `Arc<Config>` and mutated via `Arc::get_mut` before sharing
- Ctrl+C triggers graceful shutdown across all async tasks
- Platform support: Linux (ALSA driver) primary, macOS (CoreAudio) secondary
