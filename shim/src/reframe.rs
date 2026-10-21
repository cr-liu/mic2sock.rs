use bytes::Bytes;
use clocksync::Resampler;
use protocol::block::{deblock_channel, reblock_channel};
use protocol::{next_pkt_id, Header, PacketLayout};

/// Converts between the packet domain and the sample domain, with resampling in
/// between.
///
/// Resampling must happen **before** reblocking: it changes the sample count, so
/// it cannot sit after a fixed-size framing step. A useful consequence is that
/// the input and output samples-per-packet need not divide each other — the
/// reblocker simply accumulates.
///
/// The resampler is a 4-point Catmull-Rom interpolator, so it always holds exactly three
/// samples of history in its taps — a fixed, one-time 0.19 ms warm-up at 16 kHz rather
/// than a per-packet loss. It is identical across channels, so it cannot disturb
/// inter-channel phase, which is the one property this module has to preserve.
///
/// One caveat on "warm-up rather than loss": the kernel starts between taps 1 and 2, so
/// the very first input sample of a stream is never emitted — output begins at input
/// sample 1. Every channel is shifted alike and every later boundary is contiguous, so
/// nothing drifts and nothing further is lost; it is a single sample at the head of the
/// stream, 62 µs, and priming the taps to recover it would mean interpolating real audio
/// against invented zeros.
///
/// Output timestamps are the shim's own continuous timeline: anchored to the
/// earliest input packet that has contributed since the last frame, and extrapolated
/// by one output packet duration when there is none, so that concealment and the
/// drift-correcting `step` below 1.0 cannot put 1970 on the wire. Before the first
/// real input packet there is nothing to anchor to and the timeline starts at zero —
/// the pipeline should not be emitting audio it has never received.
///
/// The anchor is accurate **to within one packet**: output boundaries do not line up
/// with input boundaries — that is the point of reframing — so the input packet whose
/// audio actually begins an output packet may be the next one along. Sample-accurate
/// timestamps would mean tracking the resampler's consumption back to a per-sample
/// input position, and 10 ms is already the granularity the sender itself works at
/// (its own timestamps carry a 10 ms fudge). What must not drift is *inter-channel*
/// alignment, and that is unaffected: every channel is framed from the same
/// boundary.
/// Largest correction the output timeline may absorb per output packet when it
/// finds itself ahead of the input stamps, in nanoseconds. 50 us per 10 ms
/// packet is 5 ms per second: a sender packet stamped 30 ms late is worked off
/// in six seconds, and a 50 ppm sample-clock-vs-wall-clock mismatch (50 us/s)
/// is tracked with a hundredfold margin -- while a real gap still jumps at once.
const SERVO_MAX_NS: u64 = 50_000;

pub struct Reframer {
    in_layout: PacketLayout,
    out_layout: PacketLayout,
    resampler: Resampler,
    /// Per-channel output accumulators, drained when a full packet is available.
    acc: Vec<Vec<i16>>,
    scratch: Vec<i16>,
    device_id: u16,
    out_pkt_id: i32,
    /// Timestamp of the first input packet contributing to the packet being built —
    /// only the milliseconds, because that is all `drain` ever read from the header
    /// it used to keep, and a whole retained `Header` implied interactions with the
    /// timeline that never existed.
    pending_anchor_ms: Option<u64>,
    /// Timestamp for the next output packet, in integer nanoseconds since the
    /// epoch, used when no input header is available to anchor it. See `drain`.
    /// Sub-millisecond because the per-packet advance is `10 ms * step`; an
    /// integer because an f64 at epoch magnitude has a 0.25 us ulp, which
    /// rounded a 9750.1 us advance to 9750.0 and lost 864 ms over a day of
    /// anchorless concealment (review round 15). At 1 ns the per-packet
    /// rounding is 0.5 ns, ~40 us/day, and u64 holds the epoch in ns until 2554.
    next_out_ts_ns: u64,
    /// Input samples per output packet at step 1.0, i.e. `out_layout.spp`, kept
    /// with the sample rate to convert a step into an advance in time.
    out_spp: usize,
    sample_rate: usize,
    /// Duration of one INPUT packet in nanoseconds: an anchor that leads the
    /// extrapolation by at least this much is a genuine gap in the input; less
    /// than that is framing slack (the anchor is the first input packet pushed
    /// after the previous output, up to one input packet late).
    in_packet_ns: u64,
    /// Linear fade multiplier, 0.0..=1.0, and where it is heading.
    gain: f64,
    gain_target: f64,
    /// Change in gain per *sample*: the ramp is applied inside the packet, not to
    /// the packet as a whole.
    gain_step: f64,
}

impl Reframer {
    /// `fade_ms` is the ramp used entering and leaving an outage. `device_id` is only
    /// the value used before any input packet has been seen — after that the sender's
    /// own is carried through.
    pub fn new(
        in_layout: PacketLayout,
        out_layout: PacketLayout,
        device_id: u16,
        sample_rate: usize,
        fade_ms: u64,
    ) -> Self {
        let n_ch = in_layout.n_ch;
        let fade_samples = (sample_rate as u64 * fade_ms / 1000).max(1);
        Reframer {
            in_layout,
            out_layout,
            resampler: Resampler::new(n_ch),
            acc: (0..n_ch).map(|_| Vec::new()).collect(),
            scratch: vec![0i16; in_layout.spp],
            device_id,
            out_pkt_id: 0,
            pending_anchor_ms: None,
            next_out_ts_ns: 0,
            out_spp: out_layout.spp,
            sample_rate,
            in_packet_ns: in_layout.spp as u64 * 1_000_000_000 / sample_rate as u64,
            gain: 1.0,
            gain_target: 1.0,
            gain_step: 1.0 / fade_samples as f64,
        }
    }

    /// Feeds one input packet's audio, without letting it anchor the output timeline.
    ///
    /// For concealment: the audio is a *repeat* of a packet already emitted, so its
    /// header names a time that has passed and must not anchor the output timeline.
    ///
    /// Strictly this is belt-and-braces — a repeat's stamp is always behind where output
    /// has reached, so `drain`'s monotonic rule would reject it anyway, and no test can
    /// tell the two apart. It stays because the intent belongs at the call site rather
    /// than resting on arithmetic three functions away.
    pub fn push_repeat(&mut self, packet: &[u8]) {
        self.push_samples(packet);
    }

    /// Feeds one input packet's audio into the resampler.
    pub fn push_packet(&mut self, packet: &[u8]) {
        if let Some(h) = Header::parse(packet) {
            // Carry the sender's device id through rather than stamping our own. The
            // consumer was connected straight to the array before the shim existed
            // and may key on this field, so the shim has to be transparent in it.
            // Kept across silence too, so an outage does not renumber the device.
            self.device_id = h.device_id;
            self.pending_anchor_ms.get_or_insert(h.epoch_ms());
        }
        self.push_samples(packet);
    }

    fn push_samples(&mut self, packet: &[u8]) {
        for c in 0..self.in_layout.n_ch {
            deblock_channel(packet, &self.in_layout, c, &mut self.scratch);
            self.resampler.push(c, &self.scratch);
        }
    }

    /// Forgets the output timeline, so the next real packet re-anchors it.
    ///
    /// Called on a proven sender restart: a new sender process may have a different
    /// clock, and that is the one case where the output timestamps *should* jump rather
    /// than stay monotonic.
    pub fn reset_timeline(&mut self) {
        self.next_out_ts_ns = 0;
        self.pending_anchor_ms = None;
    }

    /// Feeds one input packet's worth of silence, and aims the fade at zero.
    pub fn push_silence(&mut self) {
        let zeros = vec![0i16; self.in_layout.spp];
        for c in 0..self.in_layout.n_ch {
            self.resampler.push(c, &zeros);
        }
        self.gain_target = 0.0;
    }

    /// Feeds real audio and aims the fade back at unity.
    pub fn push_audible(&mut self, packet: &[u8]) {
        self.push_packet(packet);
        self.gain_target = 1.0;
    }

    /// The gain trajectory for the next `n` output samples, and where it ends.
    ///
    /// Computed once per output packet and applied to **every channel**, so the fade
    /// cannot scale one channel differently from another. It also has to be
    /// per-sample: a single multiplier for a whole packet turns a 20 ms fade into two
    /// 10 ms steps of −6 dB, and a step in the waveform is the click the fade exists
    /// to avoid.
    fn ramp(&self, n: usize) -> (Vec<f64>, f64) {
        let mut g = self.gain;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            if self.gain_target > g {
                g = (g + self.gain_step).min(self.gain_target);
            } else if self.gain_target < g {
                g = (g - self.gain_step).max(self.gain_target);
            }
            out.push(g);
        }
        (out, g)
    }

    /// Pulls resampled audio at `step` and appends every complete output packet
    /// to `out`.
    pub fn drain(&mut self, step: f64, out: &mut Vec<Bytes>) {
        let want = self.out_layout.spp * 2;
        // Straight into the accumulators: `pull` appends per channel, and `acc` is
        // exactly where the samples were going. A per-call staging Vec here cost
        // ~12,000 allocations and a megabyte of copying per second, all throwaway.
        self.resampler.pull(step, want, &mut self.acc);

        while self.acc.iter().all(|a| a.len() >= self.out_layout.spp) {
            let mut buf = vec![0u8; self.out_layout.packet_len()];
            // Header timestamps name the *start* of a packet's audio.
            //
            // The timeline advances by the INPUT time this packet actually contains:
            // the resampler consumes exactly `step` input samples per output sample
            // (a shared Q32 phase, see clocksync), so one output packet spans
            // `out_spp * step` input samples. Advancing by a fixed output duration
            // instead -- the previous rule -- was only right at step 1.0: at the
            // lower clamp the timeline gained 25 ms per second over the audio it
            // described, every real anchor then measured as "behind" and was
            // refused by the monotonic rule, and the stamps ran ahead of real time
            // without bound. A consumer computing latency from them saw it go
            // negative.
            //
            // Anchors (the first input header pushed since the previous output)
            // are read three ways. Leading the extrapolation by at least one input
            // packet: a genuine gap in the input, jump forward to it. Behind it:
            // we have run ahead -- a late-stamped sender packet once pulled the
            // timeline forward, or the sender's wall clock runs slow against its
            // sample clock -- so the next advance is shortened to servo back
            // without ever stepping backwards (the stamps stay monotonic; they
            // simply advance a little slower than the content for a while).
            // Within one input packet of the extrapolation: framing slack, ignore.
            let advance_ns =
                (self.out_spp as f64 * step * 1e9 / self.sample_rate as f64).round() as u64;
            let mut servo_ns = 0u64;
            let ts_ns = match self.pending_anchor_ms.take() {
                Some(anchor_ms) => {
                    let anchor_ns = anchor_ms * 1_000_000;
                    if anchor_ns >= self.next_out_ts_ns + self.in_packet_ns {
                        anchor_ns
                    } else {
                        if anchor_ns < self.next_out_ts_ns {
                            servo_ns = (self.next_out_ts_ns - anchor_ns).min(SERVO_MAX_NS);
                        }
                        self.next_out_ts_ns
                    }
                }
                None => self.next_out_ts_ns,
            };
            // The servo never takes more than half an advance, so the timeline
            // keeps moving forward through any correction.
            self.next_out_ts_ns = ts_ns + (advance_ns - servo_ns).max(advance_ns / 2);
            let ts_ms = ts_ns / 1_000_000;
            Header {
                device_id: self.device_id,
                secs: (ts_ms / 1000) as u32,
                ms: (ts_ms % 1000) as i16,
                pkt_id: self.out_pkt_id,
            }
            .write_to(&mut buf);

            let (ramp, gain_after) = if self.gain >= 1.0 && self.gain_target >= 1.0 {
                (Vec::new(), self.gain)
            } else {
                self.ramp(self.out_layout.spp)
            };
            for c in 0..self.out_layout.n_ch {
                // The frame is already contiguous at the front of the accumulator, so
                // in steady state (no fade running) it is reblocked in place — the
                // per-channel staging Vec this replaces was 1,700 allocations/s of
                // pure copying. Only a running fade materialises a scaled copy.
                if ramp.is_empty() {
                    reblock_channel(
                        &mut buf,
                        &self.out_layout,
                        c,
                        &self.acc[c][..self.out_layout.spp],
                    );
                } else {
                    let faded: Vec<i16> = self.acc[c][..self.out_layout.spp]
                        .iter()
                        .zip(&ramp)
                        .map(|(&s, &g)| (s as f64 * g).round() as i16)
                        .collect();
                    reblock_channel(&mut buf, &self.out_layout, c, &faded);
                }
                self.acc[c].drain(..self.out_layout.spp);
            }
            self.gain = gain_after;

            self.out_pkt_id = next_pkt_id(self.out_pkt_id);
            out.push(Bytes::from(buf));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layouts(spp_in: usize, spp_out: usize) -> (PacketLayout, PacketLayout) {
        (
            PacketLayout::new(2, spp_in, 12),
            PacketLayout::new(2, spp_out, 12),
        )
    }

    /// A zero-payload packet stamped at `secs = 100` plus `ms` — the header-only
    /// fixture the timeline tests share.
    fn stamped(l: &PacketLayout, ms: i16, pkt_id: i32) -> Bytes {
        let mut buf = vec![0u8; l.packet_len()];
        Header {
            device_id: 5,
            secs: 100,
            ms,
            pkt_id,
        }
        .write_to(&mut buf);
        Bytes::from(buf)
    }

    /// Like `stamped`, but at an absolute time in ms since the epoch -- for the
    /// long-running timeline tests that span whole seconds.
    fn stamped_abs(l: &PacketLayout, t_ms: u64, pkt_id: i32) -> Bytes {
        let mut buf = vec![0u8; l.packet_len()];
        Header {
            device_id: 5,
            secs: (t_ms / 1000) as u32,
            ms: (t_ms % 1000) as i16,
            pkt_id,
        }
        .write_to(&mut buf);
        Bytes::from(buf)
    }

    /// Builds an input packet whose channel `c` carries `base + c*1000 + i`.
    fn make_input(l: &PacketLayout, pkt_id: i32, base: i16) -> Bytes {
        let mut buf = vec![0u8; l.packet_len()];
        Header {
            device_id: 5,
            secs: 100,
            ms: 0,
            pkt_id,
        }
        .write_to(&mut buf);
        for c in 0..l.n_ch {
            let s: Vec<i16> = (0..l.spp)
                .map(|i| base.wrapping_add((c * 1000 + i) as i16))
                .collect();
            reblock_channel(&mut buf, l, c, &s);
        }
        Bytes::from(buf)
    }

    #[test]
    fn accumulates_small_inputs_into_one_large_output() {
        let (li, lo) = layouts(32, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        // Six 32-sample inputs make one 160-sample output: five would be exactly
        // 160 pushed, but three samples are always held in the interpolator's taps.
        for k in 0..6 {
            r.push_packet(&make_input(&li, k, 0));
            r.drain(1.0, &mut out);
        }
        assert_eq!(out.len(), 1, "expected exactly one output packet");
        assert_eq!(out[0].len(), lo.packet_len());
    }

    /// spp_in need not divide spp_out; the accumulator absorbs the remainder.
    #[test]
    fn input_size_need_not_divide_output_size() {
        let (li, lo) = layouts(50, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        for k in 0..8 {
            r.push_packet(&make_input(&li, k, 0));
            r.drain(1.0, &mut out);
        }
        assert!(!out.is_empty());
        for p in &out {
            assert_eq!(p.len(), lo.packet_len());
        }
    }

    #[test]
    fn output_packet_ids_are_sequential_from_zero() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        // Five inputs for four outputs: the interpolator holds three samples back.
        for k in 0..5 {
            r.push_packet(&make_input(&li, k * 7, 0));
            r.drain(1.0, &mut out);
        }
        let ids: Vec<i32> = out
            .iter()
            .map(|p| Header::parse(p).unwrap().pkt_id)
            .collect();
        assert_eq!(
            ids,
            vec![0, 1, 2, 3],
            "shim must renumber, ignoring input ids"
        );
    }

    /// The sender's device id is carried through, not replaced: the consumer was
    /// wired straight to the array before the shim existed and may key on it. The
    /// constructor's value is only the pre-first-packet fallback.
    #[test]
    fn output_header_carries_the_senders_device_id() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 42, 16000, 20);
        let mut out = Vec::new();
        // Two inputs, so that a whole output packet exists past the taps.
        // `make_input` stamps device_id 5, which must win over the constructor's 42.
        for k in 0..2 {
            r.push_packet(&make_input(&li, k, 0));
            r.drain(1.0, &mut out);
        }
        assert_eq!(Header::parse(&out[0]).unwrap().device_id, 5);

        // And it survives an outage, so silence does not renumber the device.
        let before = out.len();
        for _ in 0..3 {
            r.push_silence();
            r.drain(1.0, &mut out);
        }
        assert!(out.len() > before);
        assert_eq!(Header::parse(out.last().unwrap()).unwrap().device_id, 5);
    }

    /// At unity step the audio must pass through unchanged, or the whole
    /// pipeline is lossy for no reason.
    #[test]
    fn unity_step_preserves_channel_content() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        // Several packets, because the resampler needs 4 taps of history.
        for k in 0..6 {
            r.push_packet(&make_input(&li, k, 0));
            r.drain(1.0, &mut out);
        }
        assert!(out.len() >= 3);
        // Channel 1 is channel 0 shifted by exactly 1000, by construction.
        let mut c0 = vec![0i16; lo.spp];
        let mut c1 = vec![0i16; lo.spp];
        deblock_channel(&out[2], &lo, 0, &mut c0);
        deblock_channel(&out[2], &lo, 1, &mut c1);
        for i in 0..lo.spp {
            assert_eq!(
                c1[i].wrapping_sub(c0[i]),
                1000,
                "inter-channel offset broken at {}",
                i
            );
        }
    }

    #[test]
    fn silence_produces_a_full_zeroed_packet() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        for _ in 0..6 {
            r.push_silence();
            r.drain(1.0, &mut out);
        }
        assert!(!out.is_empty());
        let mut c = vec![0i16; lo.spp];
        deblock_channel(out.last().unwrap(), &lo, 0, &mut c);
        assert!(c.iter().all(|&s| s == 0), "silence packet was not silent");
    }

    /// Concealment must continue the timeline, not restart it at the epoch. Borrowing
    /// a zeroed header put 1970 on every silence packet, and the consumer parses that
    /// field.
    #[test]
    fn silence_packets_continue_the_timeline() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();

        // Input packets 10 ms apart, as the sender actually stamps them.
        for k in 0..6i32 {
            r.push_packet(&stamped(&li, (k * 10) as i16, k));
            r.drain(1.0, &mut out);
        }
        let real = out.len();
        assert!(real >= 4);
        for _ in 0..4 {
            r.push_silence();
            r.drain(1.0, &mut out);
        }
        assert!(out.len() > real, "no silence packet was produced");

        let stamps: Vec<u64> = out
            .iter()
            .map(|p| {
                let h = Header::parse(p).unwrap();
                h.secs as u64 * 1000 + h.ms as u64
            })
            .collect();
        // Nothing at the epoch, and the timeline never goes backwards.
        for (i, w) in stamps.windows(2).enumerate() {
            assert!(w[0] >= 100_000, "packet {} fell back to the epoch", i);
            assert!(w[1] >= w[0], "timeline went backwards at packet {}", i + 1);
        }
        // The concealed tail has no input header to anchor to, so it must advance by
        // exactly one output packet duration. That is the extrapolation under test.
        for i in real..stamps.len() {
            assert_eq!(
                stamps[i],
                stamps[i - 1] + 10,
                "silence packet {} did not continue the timeline",
                i
            );
        }
    }

    /// Extrapolation can outrun the input — during an outage, or whenever the consumer
    /// reads faster than the sender sends — and a fresher-but-older input stamp must not
    /// snap the sequence back. Measured at 1000 concealed packets ahead, where an
    /// unguarded anchor rolled the timeline back by nearly ten seconds.
    #[test]
    fn a_real_header_never_moves_the_timeline_backwards() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        for k in 0..4i32 {
            r.push_packet(&stamped(&li, (k * 10) as i16, k));
            r.drain(1.0, &mut out);
        }
        // Run the timeline a long way ahead on concealment alone.
        for _ in 0..200 {
            r.push_silence();
            r.drain(1.0, &mut out);
        }
        let ahead = Header::parse(out.last().unwrap()).unwrap();
        let ahead_ms = ahead.secs as u64 * 1000 + ahead.ms as u64;

        // Now a real packet whose own timestamp is far behind where output has reached.
        r.push_packet(&stamped(&li, 40, 4));
        r.drain(1.0, &mut out);
        let after = Header::parse(out.last().unwrap()).unwrap();
        let after_ms = after.secs as u64 * 1000 + after.ms as u64;
        assert!(
            after_ms > ahead_ms,
            "the timeline snapped backwards: {} then {}",
            ahead_ms,
            after_ms
        );
    }

    /// Concealment replays audio that has already been emitted, so its header names a
    /// time that has passed. Letting it anchor the timeline stamped the concealed packet
    /// with that old time — a duplicate of an earlier stamp, and a step backwards once
    /// extrapolation had moved on. The consumer parses this field.
    #[test]
    fn a_repeat_does_not_anchor_the_timeline_to_replayed_audio() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();

        let mut inputs: Vec<Bytes> = Vec::new();
        for k in 0..6i32 {
            let buf = stamped(&li, (k * 10) as i16, k);
            inputs.push(buf.clone());
            r.push_packet(&buf);
            r.drain(1.0, &mut out);
        }
        let real = out.len();
        assert!(real >= 2);

        // Conceal by replaying the first input, whose header is far in the past.
        for _ in 0..3 {
            r.push_repeat(&inputs[0]);
            r.drain(1.0, &mut out);
        }
        assert!(out.len() > real, "no concealed packet was produced");

        let stamps: Vec<u64> = out
            .iter()
            .map(|p| {
                let h = Header::parse(p).unwrap();
                h.secs as u64 * 1000 + h.ms as u64
            })
            .collect();
        for i in 1..stamps.len() {
            assert!(
                stamps[i] > stamps[i - 1],
                "timeline stalled or went backwards at {}: {:?}",
                i,
                stamps
            );
        }
    }

    /// At a step below 1.0 — the ordinary drift correction, not an exceptional case —
    /// the accumulator runs ahead and one drain eventually frames two packets. The
    /// second has no input header to borrow, and used to be stamped 1970.
    #[test]
    fn two_packets_from_one_drain_both_get_real_timestamps() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        for k in 0..80 {
            r.push_packet(&make_input(&li, k, 0));
            r.drain(0.975, &mut out);
        }
        assert!(out.len() > 80, "the accumulator did not run ahead");
        for (i, p) in out.iter().enumerate() {
            let h = Header::parse(p).unwrap();
            assert!(
                h.secs >= 100,
                "packet {} fell back to the epoch: secs {}",
                i,
                h.secs
            );
        }
    }

    /// A constant, non-zero signal on every channel, so that a change in a sample can
    /// only have come from the gain.
    fn flat_input(l: &PacketLayout, level: i16) -> Bytes {
        let mut buf = vec![0u8; l.packet_len()];
        Header {
            device_id: 5,
            secs: 100,
            ms: 0,
            pkt_id: 0,
        }
        .write_to(&mut buf);
        for c in 0..l.n_ch {
            reblock_channel(&mut buf, l, c, &vec![level; l.spp]);
        }
        Bytes::from(buf)
    }

    /// The fade has to be a ramp *within* the packet. A single multiplier per packet
    /// makes a 20 ms fade two 10 ms steps of −6 dB, and a step in the waveform is the
    /// click the fade exists to prevent.
    ///
    /// Measured on the fade *in*, and one packet after the audio resumes. On the way
    /// out the signal is heading for zero anyway, so a flat multiplier and a ramp are
    /// indistinguishable there — the first version of this test passed on three
    /// trailing zeros rather than on the gain.
    #[test]
    fn the_fade_ramps_within_a_packet_and_across_channels_alike() {
        let (li, lo) = layouts(160, 160);
        // A 200 ms fade, so the ramp spans twenty packets: at the configured 20 ms it
        // is over in two, which leaves no packet where the audio has fully resumed and
        // the gain is still moving.
        let mut r = Reframer::new(li, lo, 5, 16000, 200);
        let mut out = Vec::new();
        let flat = flat_input(&li, 10_000);

        // All the way down first, so the ramp back up runs over real audio.
        for _ in 0..25 {
            r.push_silence();
            r.drain(1.0, &mut out);
        }
        for _ in 0..2 {
            r.push_audible(&flat);
            r.drain(1.0, &mut out);
        }
        let before = out.len();
        r.push_audible(&flat);
        r.drain(1.0, &mut out);
        assert!(out.len() > before, "no packet was produced for the fade");

        let mut c0 = vec![0i16; lo.spp];
        let mut c1 = vec![0i16; lo.spp];
        deblock_channel(&out[before], &lo, 0, &mut c0);
        deblock_channel(&out[before], &lo, 1, &mut c1);
        assert!(
            c0.iter().all(|&s| s > 0),
            "the audio had not resumed in this packet: {:?}",
            &c0[..8]
        );
        assert!(
            c0[lo.spp - 1] > c0[0],
            "the gain did not move within the packet: {} .. {}",
            c0[0],
            c0[lo.spp - 1]
        );
        assert!(
            c0.windows(2).all(|w| w[1] >= w[0]),
            "the ramp is not monotonic"
        );
        // And identical on every channel: a fade that scaled channels differently
        // would break exactly what the reframer exists to preserve.
        assert_eq!(c0, c1, "the fade scaled two channels differently");
    }

    /// Entering an outage reaches true silence, and does not get there in one step.
    #[test]
    fn a_fade_out_reaches_silence_without_jumping_there() {
        let (li, lo) = layouts(160, 160);
        // 20 ms at 16 kHz is 320 samples: two 160-sample packets.
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        let flat = flat_input(&li, 10_000);
        for _ in 0..6 {
            r.push_audible(&flat);
            r.drain(1.0, &mut out);
        }
        let before = out.len();
        for _ in 0..4 {
            r.push_silence();
            r.drain(1.0, &mut out);
        }
        let mut c = vec![0i16; lo.spp];
        // Still audible where the fade begins, fully out a couple of packets later.
        deblock_channel(&out[before], &lo, 0, &mut c);
        assert!(c[0] != 0, "the fade was instant");
        deblock_channel(&out[before + 2], &lo, 0, &mut c);
        assert!(
            c.iter().all(|&s| s == 0),
            "silence was never reached: {:?}",
            &c[..8]
        );
    }

    /// A step above 1 consumes input faster than it produces output, which is
    /// how a backlog is worked off.
    #[test]
    fn a_step_above_one_yields_fewer_output_packets() {
        let (li, lo) = layouts(160, 160);
        let mut fast = Reframer::new(li, lo, 5, 16000, 20);
        let mut slow = Reframer::new(li, lo, 5, 16000, 20);
        let mut of = Vec::new();
        let mut os = Vec::new();
        // A hundred packets, not forty: at forty, 2% is 125 samples, which does not
        // cross a 160-sample packet boundary, so both sides emit 39 and the test
        // cannot tell them apart.
        for k in 0..100 {
            fast.push_packet(&make_input(&li, k, 0));
            fast.drain(1.02, &mut of);
            slow.push_packet(&make_input(&li, k, 0));
            slow.drain(1.0, &mut os);
        }
        assert!(of.len() < os.len(), "fast={} slow={}", of.len(), os.len());
    }
    /// The timeline must advance by the input time an output packet contains --
    /// `10 ms * step` -- not by a fixed 10 ms. At the lower clamp the old rule
    /// gained 25 ms per second over the audio it described, refused every real
    /// anchor as "behind", and ran ahead of real time without bound; a consumer
    /// computing latency from the stamps saw it go negative. Here 2 ms input
    /// packets are stamped at their true times for 20 s at step 0.975, and the
    /// output stamps must stay at or behind the latest input stamp throughout.
    #[test]
    fn stamps_never_run_ahead_of_the_input_at_a_slow_step() {
        let (li, lo) = layouts(32, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        let mut max_lead_ms: i64 = i64::MIN;
        let mut last_out_ms = 0u64;
        for k in 0..10_000i32 {
            // True capture time of packet k: base 100 s + 2 ms * k.
            let t_ms = 100_000 + 2 * k as u64;
            r.push_packet(&stamped_abs(&li, t_ms, k));
            r.drain(0.975, &mut out);
            if let Some(p) = out.last() {
                let h = Header::parse(p).unwrap();
                let o = h.secs as u64 * 1000 + h.ms as u64;
                assert!(o >= last_out_ms, "timeline went backwards");
                last_out_ms = o;
                max_lead_ms = max_lead_ms.max(o as i64 - t_ms as i64);
            }
        }
        // Framing slack is at most one input packet (2 ms); anything beyond it
        // is the runaway this test exists to catch (the old rule reached ~500 ms).
        assert!(
            max_lead_ms <= 2,
            "output stamps led the input by {} ms at step 0.975",
            max_lead_ms
        );
        // ...and not stuck behind either: with 10 s of input at 0.975 the output
        // timeline covers the same input span.
        let behind_ms = (100_000 + 2 * 9_999) as i64 - last_out_ms as i64;
        assert!(
            behind_ms < 50,
            "timeline fell {} ms behind the input",
            behind_ms
        );
    }

    /// One late-stamped sender packet pulls the timeline forward (it looks like
    /// a gap); the servo must then work the lead off within seconds without the
    /// stamps ever stepping backwards.
    #[test]
    fn a_late_stamp_is_worked_off_by_the_servo() {
        let (li, lo) = layouts(32, 160);
        let mut r = Reframer::new(li, lo, 5, 16000, 20);
        let mut out = Vec::new();
        let mut lead_after = Vec::new();
        let mut last_out_ms = 0u64;
        for k in 0..6_000i32 {
            let true_ms = 100_000 + 2 * k as u64;
            // A scheduler hiccup stamps a burst of five packets 30 ms late: five,
            // so that whichever of them lands as the anchor of its output packet
            // carries the late stamp (anchors are the FIRST input since the
            // previous output, and the framing offset depends on the
            // resampler's tap priming).
            let stamp = if (500..505).contains(&k) {
                true_ms + 30
            } else {
                true_ms
            };
            r.push_packet(&stamped_abs(&li, stamp, k));
            r.drain(1.0, &mut out);
            if let Some(p) = out.last() {
                let h = Header::parse(p).unwrap();
                let o = h.secs as u64 * 1000 + h.ms as u64;
                assert!(o >= last_out_ms, "timeline went backwards");
                last_out_ms = o;
                if k > 500 {
                    lead_after.push(o as i64 - true_ms as i64);
                }
            }
        }
        let peak = *lead_after.iter().max().unwrap();
        let settled = *lead_after.last().unwrap();
        assert!(
            peak >= 20,
            "the late stamp was not even taken up: peak lead {}",
            peak
        );
        assert!(
            settled <= 2,
            "servo failed to work off the lead: still {} ms ahead after 11 s",
            settled
        );
    }
}
