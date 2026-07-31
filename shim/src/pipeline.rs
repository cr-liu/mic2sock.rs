//! Orchestration: source → jitter buffer → reframer → sink.
//!
//! Lives in the library rather than in `main.rs` so an integration test can drive it
//! directly; a binary crate's modules cannot be reached from `tests/`.
//!
//! **The release rate is the consumer's read rate.** There is no timer in this loop.
//! One packet is released per accepted sink write, and every channel between here and
//! the socket is deliberately tiny, so `send().await` returns only once the consumer
//! has actually taken the previous packet. Windows' default timer granularity is
//! 15.6 ms against a 10 ms packet, so a timer-driven design would add more jitter than
//! the network does.

use crate::config::Config;
use crate::depth::{Arrival, DepthEstimator};
use crate::jitter::{JitterBuffer, Released};
use crate::metrics::Metrics;
use crate::reframe::Reframer;
use crate::sink::Sink;
use crate::source::{SourceEvent, TcpSource};
use bytes::Bytes;
use clocksync::DepthController;
use protocol::Header;
use std::io::Write;
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
/// Depth of the channel from the source. Larger, because this side must absorb a
/// burst: after a stall the Pi delivers its backlog as fast as the link allows, and
/// dropping it here would defeat the catchup machinery downstream.
const SOURCE_QUEUE_PACKETS: usize = 512;

/// Exit code used when the source proves the configured geometry cannot be right.
pub const EXIT_GEOMETRY: i32 = 4;
/// Exit code used when the sink port cannot be bound.
pub const EXIT_BIND: i32 = 3;

pub async fn run(cfg: Config) {
    // The input geometry matches the output for now: the Pi's sample_per_packet is
    // only reduced in a later phase, and the reframer does not require the two to
    // divide each other.
    let in_layout = cfg.layout();
    let out_layout = cfg.layout();

    let (src_tx, mut src_rx) = mpsc::channel::<SourceEvent>(SOURCE_QUEUE_PACKETS);
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

    let sink = match Sink::bind(
        &format!("127.0.0.1:{}", cfg.sink_port),
        3,
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
    let mut last_ctl_ms = 0u64;
    let mut pending: Vec<Bytes> = Vec::new();
    let mut generation_resets_seen = 0u64;

    loop {
        // Take everything that has arrived without blocking, so the jitter buffer
        // sees arrivals promptly and in order.
        while let Ok(event) = src_rx.try_recv() {
            let t = start.elapsed().as_millis() as u64;
            match event {
                SourceEvent::Connected => {
                    // Every connect, including the first. This is the only evidence
                    // that a backward packet-id jump is a sender restart rather than a
                    // replay of audio already emitted, and it has to arrive in order
                    // with the packets — hence travelling through this channel.
                    jb.on_source_reconnect();
                }
                SourceEvent::Packet(pkt) => {
                    if let Some(h) = Header::parse(&pkt) {
                        let header_ms = h.secs as u64 * 1000 + h.ms.max(0) as u64;
                        match est.observe(t, header_ms, 0) {
                            Arrival::Jitter { above_min_ms } => m.record_delay(above_min_ms),
                            Arrival::Outage { .. } => m.outage_events += 1,
                        }
                        jb.insert(h.pkt_id, pkt, t);
                    }
                }
            }
        }

        // A *proven* id reset — not a mere reconnect — means a new sender process and
        // so possibly a new clock offset, which is the only thing that justifies
        // throwing away the delay statistic. An ordinary reconnect must not: the first
        // packet after a stall is the most delayed one, and with no reference left to
        // measure it against it would become its own baseline.
        if jb.generation_resets != generation_resets_seen {
            generation_resets_seen = jb.generation_resets;
            est.on_generation_reset();
        }

        let dropped = jb.enforce_max_depth(max_depth_packets);
        if dropped > 0 {
            m.max_depth_hit += 1;
            eprintln!("shim: max_depth exceeded, discarded {} packets", dropped);
        }

        // The depth controller runs on wall time, not per packet.
        let t = start.elapsed().as_millis() as u64;
        if t.saturating_sub(last_ctl_ms) >= packet_ms {
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
        let target_packets = (est.target_ms() / packet_ms) as usize;
        let now = start.elapsed().as_millis() as u64;
        match jb.release_with_target(now, target_packets) {
            Released::Real(p) => refr.push_audible(&p),
            Released::Repeat(p) => {
                m.conceal_events += 1;
                m.conceal_samples += in_layout.spp as u64;
                est.on_conceal(now);
                refr.push_packet(&p);
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

        // Counters the jitter buffer owns. Copied rather than incremented here so
        // there is one source of truth for each; the JSONL reported zeroes for all
        // four while the buffer had been counting them all along.
        m.late_discards = jb.late_discards;
        m.duplicate_discards = jb.duplicate_discards;
        m.resync_events = jb.resync_events;

        let t = start.elapsed().as_millis() as u64;
        if m.flush_due(t, METRICS_INTERVAL_MS) {
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
