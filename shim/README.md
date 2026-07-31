# shim

A relay that sits between the microphone array's Pi and the closed-source consumer on
Windows, absorbing network jitter so the consumer never sees it.

```
Pi (mic2sock) ──TCP over Wi-Fi──▶ shim.exe ──TCP over localhost──▶ black box
```

The consumer is pointed at `127.0.0.1` instead of at the Pi, and gets a stream in
exactly the format it already parses: same 12-byte header, same 17 channels, same 160
samples per packet, same 5452 bytes. Nothing about it has to change.

## What it is for

Wi-Fi delivers packets in bursts. A consumer reading straight from the network sees
that burstiness directly, and it has no way to absorb it — it has to play what it is
given, when it is given it. The shim takes the burstiness on itself:

- **A jitter buffer** holds a small, adaptive amount of audio (target depth is
  measured from the link, capped at `d_max_adaptive_ms`) and releases it evenly.
- **Gaps are concealed in place**, by repeating the previous packet, *before* the
  packet that follows the gap. Concealing late would shift the far-end reference
  channel against the microphone channels, and 10 ms is 160 samples — far outside the
  region an echo canceller's filter has converged over.
- **A backlog is absorbed rather than dropped.** After a stall, the pile-up is worked
  off by playing very slightly fast (`catchup_clamp`, default 2.5%) instead of being
  discarded. Three seconds of backlog takes about two minutes to absorb; that is the
  price of losing nothing, and it was chosen deliberately.
- **Output ids are the shim's own gapless sequence.** A concealed packet still carries
  a valid, consecutive id, because what the consumer does when an id jumps is unknown.

## Build and run

Built natively on the Windows machine that will run it — there is no cross-compiler in
this repo's environment:

```
cargo build -p shim --release
```

Then put `shim.toml` next to `shim.exe`:

```
copy shim.toml.example shim.toml    :: then edit source_host
shim.exe
```

The config is read from **beside the executable**, not from the working directory,
because on Windows the exe is routinely started from somewhere else. `--config <path>`
overrides it.

`cargo build -p shim` also works on Linux and macOS, which is how it is tested — the
crate has no libjack dependency, unlike `mic2sock` in the same workspace.

## Exit codes

The shim fails at startup rather than running in a degraded state, because a
misconfiguration that keeps running looks like an intermittent audio fault later.

| Code | Meaning |
|------|---------|
| 2 | The config is missing, malformed, or invalid — including a misspelled key. The message names the problem. |
| 3 | `sink_port` could not be bound. Usually a stale instance still running. |
| 4 | The stream does not match the configured geometry. Check `n_ch` and `spp_out`. |

Code 4 deserves a note. A wrong `n_ch` cannot be detected from the config — the
consumer is closed-source and cannot be asked — but it *is* detectable against the
stream: with the wrong packet length the shim slices the byte stream at the wrong
boundaries, so the "headers" it reads are garbage. The first eight packets of every
connection are checked for that (ids advancing by one, milliseconds in range), and a
mismatch is fatal rather than an hour of silently mis-framed audio.

## Reading the metrics

With `metrics_path` set, one JSON object is appended per minute. Three fields answer
"is it working", and they should all be zero:

- `catchup_overflow` — a backlog exceeded `catchup_max_ms` and the excess was
  discarded. The lossless promise is degrading; raise `catchup_max_ms` **on both ends**
  or find out why the link stalls for that long.
- `max_depth_hit` — the safety valve fired, so the consumer stopped reading.
- `resync_events` — the timeline was broken deliberately. A few after a sender restart
  are expected; a steady trickle is not.

And one answers "how good is this link, really": **`p99_9_ms`**, the 99.9th percentile
of arrival delay above the running minimum. That number is what a buffer has to cover,
so it is also the answer to "how low could the latency go here" — and the instrument
for judging whether a change to the network, or the later move off JACK, actually
helped. `conceal_events` alongside it says how often the buffer was too shallow to
cover it.

## Configuration that has to agree with something else

Most settings are independent. Two are not:

- **`catchup_max_ms` must equal the Pi's send-side backlog.** Whichever end has the
  smaller value discards content, and the other end's buffering is then wasted.
- **`n_ch`, `spp_out`, `header_len`, `sample_rate` must equal what the consumer was
  built for.** The shim refuses to start on any other value.

`max_depth_ms` is validated to be at least `d_max_adaptive_ms + catchup_max_ms`, so
raising the catchup budget without raising the valve is refused rather than silently
turning the valve into the thing that discards the backlog.

## Firewall

The listening socket is on `127.0.0.1`, so no inbound rule is needed. The outbound
connection to the Pi is ordinary TCP. If a future version uses UDP, an inbound rule
becomes necessary — and that decision should be made from the `p99_9_ms` data this
version collects, not assumed.
