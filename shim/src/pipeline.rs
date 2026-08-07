//! Orchestration: source → jitter buffer → reframer → sink.
//!
//! Lives in the library rather than in `main.rs` so an integration test can drive it
//! directly; a binary crate's modules cannot be reached from `tests/`.
//!
//! **The release rate is the consumer's read rate.** There is no timer in this loop:
//! one packet is released per accepted sink write. Windows' default timer granularity is
//! 15.6 ms against a 10 ms packet, so a timer-driven design would add more jitter than
//! the network does.
//!
//! Be precise about what that bounds, though. `send().await` returns when the sink task
//! *dequeues*, and its `write_all` returns when the local TCP stack accepts the bytes —
//! not when the consumer application reads them. Between the two-slot channel, the
//! packet in the sink's write, and both kernel buffers (Linux doubled a 16 KB
//! `SO_SNDBUF` request to 32 KB), a measured 11 packets — about 110 ms — crossed while a
//! consumer read nothing. So this bounds the lead to a few packets rather than the three
//! seconds a large queue would allow; it does not make a release equal a consumer read.
//!
//! It also assumes the consumer reads at its own audio rate. A consumer that reads flat
//! out *is* the clock, and will be served flat out — the code cannot tell "feeding a
//! bounded device queue" from "draining as fast as possible". That assumption holds for
//! an audio consumer and has to be confirmed against the real one.

use crate::config::Config;
use crate::depth::{Arrival, DepthEstimator};
use crate::jitter::{JitterBuffer, Released, State as JitterState};
use crate::metrics::Metrics;
use crate::reframe::Reframer;
use crate::sink::Sink;
use crate::source::{SourceEvent, TcpSource};
use bytes::Bytes;
use clocksync::DepthController;
use protocol::Header;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio::sync::mpsc;

const METRICS_INTERVAL_MS: u64 = 60_000;
/// Ramp used entering and leaving an outage.
const FADE_MS: u64 = 20;
/// Largest gap concealed by repeating; beyond this the timeline breaks and is counted.
const MAX_CONCEAL_PACKETS: usize = 8;
const REORDER_WINDOW: usize = 16;
/// Depth of the channel to the sink, in packets.
///
/// **Load-bearing, and small on purpose.** `send().await` on this channel is the
/// release clock, so it has to block almost immediately: two slots let the sink write
/// one packet while the loop prepares the next, and nothing more. Sizing it by the
/// catchup budget instead — three seconds of packets — would let the loop run three
/// seconds ahead of the consumer, and the pacing this whole design rests on would be
/// the loop's own speed rather than the consumer's read rate.
const SINK_QUEUE_PACKETS: usize = 2;
/// Floor on the source channel's depth, in packets. The actual depth is derived
/// from the config (see `run_with_sink`): this side must absorb a post-stall burst
/// — the Pi delivers its backlog as fast as the link allows — so it is sized by the
/// same catchup-plus-valve budget the jitter buffer retains, not by a constant that
/// silently stops matching its own rationale the day `catchup_max_ms` is raised.
const SOURCE_QUEUE_MIN_PACKETS: usize = 512;

/// Exit code used when the source proves the configured geometry cannot be right.
const EXIT_GEOMETRY: i32 = 4;
/// Exit code used when the sink port cannot be bound.
const EXIT_BIND: i32 = 3;

/// The binary's entry point: binds the sink port from the config, and exits the
/// process if it cannot.
pub async fn run(cfg: Config) {
    let out_layout = cfg.layout();
    let sink = match Sink::bind(
        &format!("127.0.0.1:{}", cfg.sink_port),
        out_layout.packet_len(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("shim: cannot bind localhost:{}: {}", cfg.sink_port, e);
            std::process::exit(EXIT_BIND);
        }
    };
    run_with_sink(cfg, sink).await
}

/// Handles one source event; returns whether it proved a new sender generation, so
/// the caller can reset the reframer's output timeline.
///
/// The generation reset is applied *within* the event that proved it: `jb.insert` is
/// what detects an id reset, and deferring the reset until the whole ready batch had
/// been drained meant a hundred packets of a restarted sender were first classified
/// against the old clock offset — and then thrown away by the reset that followed.
/// `generation_resets` is only ever incremented inside `jb.insert`, so a snapshot
/// around the call sees every transition; no cross-iteration mirror is needed.
fn on_event(
    ev: SourceEvent,
    t: u64,
    jb: &mut JitterBuffer,
    est: &mut DepthEstimator,
    m: &mut Metrics,
) -> bool {
    match ev {
        SourceEvent::Connected => {
            // Every connect, including the first. This is the only evidence that a
            // backward packet-id jump is a sender restart rather than a replay of audio
            // already emitted, and it has to arrive in order with the packets around it
            // — hence travelling through the same channel.
            jb.on_source_reconnect();
            false
        }
        SourceEvent::Packet(pkt) => {
            let Some(h) = Header::parse(&pkt) else {
                return false;
            };
            let header_ms = h.epoch_ms();
            let generations_before = jb.generation_resets;
            jb.insert(h.pkt_id, pkt, t);
            // A *proven* id reset — not a mere reconnect — means a new sender process
            // and so possibly a new clock offset, which is the only thing that
            // justifies discarding the delay statistic. An ordinary reconnect must
            // not: the first packet after a stall is the most delayed one, and with
            // no reference left to measure it against it would become its own
            // baseline. Checked before observing, so this packet is the first sample
            // of the new generation rather than the last of the old.
            let reset = jb.generation_resets != generations_before;
            if reset {
                est.on_generation_reset();
            }
            match est.observe(t, header_ms) {
                Arrival::Jitter { above_min_ms } => m.record_delay(above_min_ms),
                Arrival::Outage { .. } => m.outage_events += 1,
            }
            reset
        }
    }
}

/// Runs against an already-bound sink.
///
/// Split out so a test can bind port 0 itself, read back the assigned port, and pass
/// the listener in. Taking a port *number* instead would leave a window between finding
/// a free port and binding it — with several tests in one binary that is a real race,
/// not a theoretical one — and would also mean a test process could be killed by this
/// module's `exit` on a bind failure.
pub async fn run_with_sink(cfg: Config, sink: Sink) {
    // The input geometry matches the output for now: the Pi's sample_per_packet is
    // only reduced in a later phase, and the reframer does not require the two to
    // divide each other.
    let in_layout = cfg.layout();
    let out_layout = cfg.layout();

    let source_queue =
        (cfg.catchup_max_packets() + cfg.max_depth_packets()).max(SOURCE_QUEUE_MIN_PACKETS);
    let (src_tx, mut src_rx) = mpsc::channel::<SourceEvent>(source_queue);
    let (sink_tx, sink_rx) = mpsc::channel::<Bytes>(SINK_QUEUE_PACKETS);

    let src = TcpSource::new(
        cfg.source_host.clone(),
        cfg.source_port,
        in_layout.packet_len(),
    );
    tokio::spawn(async move {
        // A geometry mismatch is fatal and must take the process with it. Left as a
        // dropped `Result` inside a spawned task it would be invisible, and the shim
        // would sit there forwarding mis-framed audio for as long as nobody looked.
        if let Err(e) = src.run(src_tx).await {
            eprintln!("shim: {}", e);
            std::process::exit(EXIT_GEOMETRY);
        }
    });
    let connected = sink.connected();
    tokio::spawn(sink.run(sink_rx));

    let packet_ms = cfg.packet_ms();
    let mut est = DepthEstimator::new(cfg.d_max_adaptive_ms, packet_ms);
    let mut jb = JitterBuffer::new(
        cfg.outage_threshold_ms,
        MAX_CONCEAL_PACKETS,
        // Accepting far ahead of the release position is what lets a burst be
        // buffered at all; it is unrelated to the conceal horizon.
        (cfg.catchup_max_packets() + cfg.max_depth_packets()).max(64),
        REORDER_WINDOW,
        cfg.retain_cap_packets(),
    );
    let mut ctl = DepthController::new(1.0, cfg.catchup_clamp, cfg.catchup_slew_per_sec);
    let mut refr = Reframer::new(in_layout, out_layout, 0, cfg.sample_rate, FADE_MS);
    let mut m = Metrics::new();
    let max_depth_packets = cfg.max_depth_packets();

    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;
    let mut last_ctl_ms = 0u64;
    let mut pending: Vec<Bytes> = Vec::new();
    let mut timeline_reset_due = false;

    loop {
        let target_packets = (est.target_ms() / packet_ms) as usize;

        // With no consumer there is no release clock, so releasing would only feed the
        // sink's discard — and arrive at the moment the consumer connects with an empty
        // buffer, which is the opposite of the point. Hold a rolling window of the
        // current target instead (spec §6.5) and wait for the next arrival. Waiting on
        // the channel rather than polling is also what keeps this off a timer.
        if !connected.load(Ordering::Relaxed) {
            jb.retain_window(target_packets.max(1));
            match src_rx.recv().await {
                Some(ev) => {
                    timeline_reset_due |= on_event(ev, now_ms(), &mut jb, &mut est, &mut m);
                }
                None => return,
            }
            continue;
        }

        // Take everything that has arrived without blocking, so the jitter buffer
        // sees arrivals promptly and in order.
        while let Ok(event) = src_rx.try_recv() {
            timeline_reset_due |= on_event(event, now_ms(), &mut jb, &mut est, &mut m);
        }

        if timeline_reset_due {
            // A proven sender restart is the one case where the output timestamps
            // should jump rather than stay monotonic: the new process may be on a
            // different clock.
            timeline_reset_due = false;
            refr.reset_timeline();
        }

        let dropped = jb.enforce_max_depth(max_depth_packets);
        if dropped > 0 {
            m.max_depth_hit += 1;
            eprintln!("shim: max_depth exceeded, discarded {} packets", dropped);
        }

        // The depth controller runs on wall time, not per packet — and only while the
        // buffer is actually releasing real audio. During an outage or while priming,
        // zero occupancy does not mean "play slower": the real timeline is frozen and
        // what is going out is synthetic. Taking the error from it slewed `step` all
        // the way down to the lower clamp over a 12.5 s outage, so recovery began by
        // running *backwards* — occupancy then grew by another 150 ms and a backlog
        // that was exactly within the lossless budget tripped the overflow trim.
        let t = now_ms();
        if jb.state() != JitterState::Normal {
            last_ctl_ms = t;
        }
        if jb.state() == JitterState::Normal && t.saturating_sub(last_ctl_ms) >= packet_ms {
            let dt = (t - last_ctl_ms) as f64 / 1000.0;
            last_ctl_ms = t;
            let measured_ms = jb.buffered() as u64 * packet_ms;
            let mut error = (measured_ms as f64 - est.target_ms() as f64) / 1000.0;
            // Beyond catchup_max the excess is discarded rather than absorbed, because
            // absorbing it would take N/clamp seconds of elevated latency. Non-zero
            // here means the lossless promise is degrading.
            let excess_ms = measured_ms.saturating_sub(est.target_ms());
            if excess_ms > cfg.catchup_max_ms {
                let over_packets = ((excess_ms - cfg.catchup_max_ms) / packet_ms) as usize;
                let cap = jb.buffered().saturating_sub(over_packets);
                if jb.enforce_max_depth(cap) > 0 {
                    m.catchup_overflow += 1;
                }
                error = cfg.catchup_max_ms as f64 / 1000.0;
            }
            ctl.update(error, dt.max(1e-3));
        }

        // One release per accepted write: the consumer paces this.
        let now = now_ms();
        match jb.release_with_target(now, target_packets) {
            Released::Real(p) => refr.push_audible(&p),
            Released::Repeat(p) => {
                m.conceal_events += 1;
                m.conceal_samples += in_layout.spp as u64;
                est.on_conceal(now);
                // Not `push_packet`: the audio is a repeat, so its header names a time
                // that has already been emitted and must not anchor the timeline.
                refr.push_repeat(&p);
            }
            Released::Silence => refr.push_silence(),
            Released::Nothing => {
                // Nothing buffered and not yet an outage. This is the one wait in the
                // loop, and it is a starvation guard rather than a clock: without a
                // consumer-driven write to block on there is nothing to pace against,
                // and spinning would burn a core for up to outage_threshold_ms.
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                continue;
            }
        }

        refr.drain(ctl.step(), &mut pending);
        for p in pending.drain(..) {
            // This await is the release clock: with a two-packet queue it returns only
            // once the sink has taken the previous packet, and the sink's own write
            // returns only once the consumer has.
            if sink_tx.send(p).await.is_err() {
                return;
            }
        }

        let t = now_ms();
        if m.flush_due(t, METRICS_INTERVAL_MS) {
            // These three counters are the jitter buffer's, mirrored at flush time so
            // each has one owner; the JSONL once reported zeroes for all of them while
            // the buffer had been counting all along. Only these three: the metrics
            // fields named conceal_events and outage_events are *pipeline* definitions
            // (a Repeat release; the estimator's delay classification) and genuinely
            // differ from the buffer's same-named internal counters.
            m.late_discards = jb.late_discards;
            m.duplicate_discards = jb.duplicate_discards;
            m.resync_events = jb.resync_events;
            m.set_last_flush(t);
            let line = m.to_json_line(t, est.target_ms(), ctl.step());
            eprintln!("{}", line);
            if let Some(path) = &cfg.metrics_path {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "{}", line);
                }
            }
        }
    }
}
