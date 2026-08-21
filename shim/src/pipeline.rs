//! Orchestration: source → jitter buffer → reframer → sink.
//!
//! Lives in the library rather than in `main.rs` so an integration test can drive it
//! directly; a binary crate's modules cannot be reached from `tests/`.
//!
//! **The release clock is the shim's own pacer** — one tick per packet duration on a
//! dedicated OS thread (see `spawn_pacer`). The consumer's read behaviour is deliberately
//! NOT trusted: the black box is closed-source, and an earlier consumer-paced design
//! (release per accepted sink write) put the whole depth-control loop at the mercy of an
//! unverifiable assumption — a consumer that reads flat out would pin the controller at
//! its lower clamp and hold depth at zero, which the bench reproduced. Self-pacing keeps
//! every property we can control on our side: a greedy reader is simply served in real
//! time, a paced reader waits ~0 per read, and a stalled reader backs the sink queue up,
//! which stalls releases and lets depth (and then the valve) absorb it.
//!
//! The consumer-vs-pacer clock mismatch this reintroduces is the one the legacy direct
//! connection always had (the consumer was paced by the robot's crystal then), and the
//! depth controller steers the long-run release rate to the arrival rate anyway.

use crate::config::Config;
use crate::depth::{Arrival, DepthEstimator};
use crate::elide::{EnergyGate, ELIDE_BUDGET_PER_TICK};
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
/// Largest gap concealed by repeating, in milliseconds of audio; beyond this the
/// timeline breaks and is counted. In ms rather than packets: the source packet
/// size is configurable now, and a packet-count horizon would silently shrink
/// five-fold the day the source moved to 2 ms packets.
const CONCEAL_HORIZON_MS: u64 = 80;
/// How far out-of-order an arrival may be and still be reordered, in ms.
const REORDER_WINDOW_MS: u64 = 160;
/// Depth of the channel to the sink, in packets.
///
/// Small on purpose, though it is no longer the release clock (the pacer is): it
/// bounds how far a stalled consumer can back audio up outside the jitter buffer.
/// Everything past these two slots and the small SO_SNDBUF stays in the buffer,
/// where depth accounting and the safety valve can see it.
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

/// The release clock: one tick per packet duration on an absolute schedule
/// (t0 + n*period), so per-tick error never accumulates. A dedicated OS thread,
/// not a tokio timer: tokio's timer on Windows is quantized to the ~15.6 ms
/// system tick (tokio #5021), while `std::thread::sleep` has used a
/// high-resolution waitable timer since Rust 1.75 (~0.5 ms on Windows 10
/// 1803+). Build the Windows binary with a toolchain >= 1.75 or pacing
/// degrades to the system tick.
///
/// The channel is shallow on purpose: if the pipeline falls behind, at most the
/// channel's capacity in ticks is owed and replayed back-to-back; beyond that
/// the pacer blocks, and the absolute schedule folds the excess away instead of
/// letting lateness accumulate.
fn spawn_pacer(period_ms: u64) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(4);
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut n: u64 = 0;
        loop {
            n += 1;
            let deadline = t0 + std::time::Duration::from_millis(n * period_ms);
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }
            if tx.blocking_send(()).is_err() {
                // The pipeline is gone; the thread must not outlive it.
                return;
            }
        }
    });
    rx
}

/// Runs against an already-bound sink.
///
/// Split out so a test can bind port 0 itself, read back the assigned port, and pass
/// the listener in. Taking a port *number* instead would leave a window between finding
/// a free port and binding it — with several tests in one binary that is a real race,
/// not a theoretical one — and would also mean a test process could be killed by this
/// module's `exit` on a bind failure.
pub async fn run_with_sink(cfg: Config, sink: Sink) {
    // The source may send smaller packets than the consumer receives (spp_in <=
    // spp_out); the reframer accumulates, and does not require the two to divide
    // each other. Everything upstream of the reframer -- jitter buffer, depth
    // estimator, pacer -- runs in units of SOURCE packets.
    let in_layout = cfg.in_layout();
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

    let packet_ms = cfg.packet_ms_in();
    let mut gate = EnergyGate::new(packet_ms);
    // Splice continuity: the final samples of the last pushed real packet, and
    // whether any packet was elided since. Cleared on non-real releases -- the
    // reframer's own fades govern those transitions.
    let mut last_tail: Option<Vec<i16>> = None;
    let mut splice_pending = false;
    let mut tick_rx = spawn_pacer(packet_ms);
    let mut est = DepthEstimator::new(cfg.d_max_adaptive_ms, packet_ms);
    let mut jb = JitterBuffer::new(
        cfg.outage_threshold_ms,
        (CONCEAL_HORIZON_MS / packet_ms).max(1) as usize,
        // Accepting far ahead of the release position is what lets a burst be
        // buffered at all; it is unrelated to the conceal horizon.
        (cfg.catchup_max_packets() + cfg.max_depth_packets()).max(64),
        (REORDER_WINDOW_MS / packet_ms).max(1) as usize,
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
            // Ticks that piled up while nobody was listening are stale; letting
            // them queue would burst-release the moment a consumer appears.
            while tick_rx.try_recv().is_ok() {}
            match src_rx.recv().await {
                Some(ev) => {
                    timeline_reset_due |= on_event(ev, now_ms(), &mut jb, &mut est, &mut m);
                }
                None => return,
            }
            continue;
        }

        // Wait for the release tick, but keep stamping arrivals while waiting --
        // parking them in the channel for up to a full packet period would skew
        // the delay statistic by that much. The tick arm comes FIRST under
        // `biased`: with arrivals-first, a sustained post-stall flood kept the
        // select on the arrival arm indefinitely and releases stalled for the
        // whole burst.
        loop {
            tokio::select! {
                biased;
                tick = tick_rx.recv() => {
                    if tick.is_none() {
                        return;
                    }
                    break;
                }
                ev = src_rx.recv() => match ev {
                    Some(ev) => {
                        timeline_reset_due |= on_event(ev, now_ms(), &mut jb, &mut est, &mut m);
                    }
                    None => return,
                },
            }
        }
        // Take everything else that has arrived without blocking, so the jitter
        // buffer sees arrivals promptly and in order -- bounded, so a flood
        // cannot postpone the release this tick already earned.
        let mut drained = 0;
        while let Ok(event) = src_rx.try_recv() {
            timeline_reset_due |= on_event(event, now_ms(), &mut jb, &mut est, &mut m);
            drained += 1;
            if drained >= 2048 {
                break;
            }
        }

        if timeline_reset_due {
            // A proven sender restart is the one case where the output timestamps
            // should jump rather than stay monotonic: the new process may be on a
            // different clock.
            timeline_reset_due = false;
            refr.reset_timeline();
            // A proven restart is a possibly different device, gain and room:
            // the gate's calibration must not cross the generation boundary --
            // and neither may the splice state, or the new source's first
            // packet gets ramped from the old source's final samples.
            gate.reset();
            last_tail = None;
            splice_pending = false;
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

        // One packet reaches the reframer per tick. While the buffer is deeper
        // than target + margin, released packets that measure as room tone are
        // elided -- dropped without reaching the reframer -- and the release
        // repeats within a per-tick budget, so a backlog drains at up to
        // ELIDE_BUDGET_PER_TICK times real time without touching speech.
        let now = now_ms();
        let mut budget = ELIDE_BUDGET_PER_TICK;
        // A resync inside release_with_target skips the release position over
        // a gap too wide to conceal; the packet it lands on was never adjacent
        // to what preceded it, so it needs the same joint ramp as an elision.
        let resyncs_before = jb.resync_events;
        loop {
            budget -= 1;
            match jb.release_with_target(now, target_packets) {
                Released::Real(p) => {
                    if cfg.silence_elision {
                        // The gate observes every real release, elided or not:
                        // its floor, hangover and pressure hysteresis only stay
                        // honest if they see the whole stream. It arms only
                        // when the buffer is genuinely backed up and drops only
                        // what measures as room tone by both mean and peak.
                        // Pre-release depth: the candidate packet has already
                        // been taken out of the buffer, so add it back — the
                        // engage test is about the backlog that produced this
                        // release, not the residue after it.
                        let depth_ms = (jb.buffered() as u64 + 1) * packet_ms;
                        if gate.should_elide(&p, &in_layout, now, depth_ms, est.target_ms())
                            && budget > 0
                        {
                            m.silence_elided += 1;
                            splice_pending = true;
                            continue;
                        }
                    }
                    // An elision cut joins two packets that were never
                    // adjacent; ramp this packet's first millisecond from the
                    // previous packet's final samples so the joint carries no
                    // step at all (a step repeating at the packet rate during
                    // a sustained drain is a click train, not masked noise).
                    if jb.resync_events != resyncs_before {
                        splice_pending = true;
                    }
                    if splice_pending {
                        if let Some(tail) = &last_tail {
                            let mut spliced = p.to_vec();
                            EnergyGate::splice_ramp(&mut spliced, &in_layout, tail);
                            last_tail = Some(EnergyGate::tail_samples(&spliced, &in_layout));
                            splice_pending = false;
                            refr.push_audible(&spliced);
                            break;
                        }
                        splice_pending = false;
                    }
                    last_tail = Some(EnergyGate::tail_samples(&p, &in_layout));
                    refr.push_audible(&p);
                }
                Released::Repeat(p) => {
                    m.conceal_events += 1;
                    m.conceal_samples += in_layout.spp as u64;
                    est.on_conceal(now);
                    // A repeat is spliced at both ends: its head is ramped from
                    // the previous packet's tail here, and splice_pending ramps
                    // the next real packet from the repeat's tail -- otherwise a
                    // conceal on periodic content clicks at both joints (the
                    // outage fade only covers Silence transitions).
                    // Not `push_packet`: the audio is a repeat, so its header
                    // names a time that has already been emitted and must not
                    // anchor the timeline.
                    if let Some(tail) = &last_tail {
                        let mut r = p.to_vec();
                        EnergyGate::splice_ramp(&mut r, &in_layout, tail);
                        last_tail = Some(EnergyGate::tail_samples(&r, &in_layout));
                        refr.push_repeat(&r);
                    } else {
                        last_tail = Some(EnergyGate::tail_samples(&p, &in_layout));
                        refr.push_repeat(&p);
                    }
                    splice_pending = true;
                }
                Released::Silence => {
                    m.silence_packets += 1;
                    last_tail = None;
                    splice_pending = false;
                    refr.push_silence()
                }
                Released::Nothing => {}
            }
            break;
        }

        refr.drain(ctl.step(), &mut pending);
        for p in pending.drain(..) {
            // Usually instant (the queue is two deep and the consumer keeps up).
            // When the consumer stalls this blocks, ticks coalesce in their shallow
            // channel, and releases stop -- depth then grows where the accounting
            // and the safety valve can see it.
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
