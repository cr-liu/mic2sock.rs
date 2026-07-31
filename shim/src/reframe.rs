use bytes::Bytes;
use clocksync::Resampler;
use protocol::block::{deblock_channel, reblock_channel};
use protocol::{Header, PacketLayout};

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
pub struct Reframer {
    in_layout: PacketLayout,
    out_layout: PacketLayout,
    resampler: Resampler,
    /// Per-channel output accumulators, drained when a full packet is available.
    acc: Vec<Vec<i16>>,
    scratch: Vec<i16>,
    device_id: u16,
    out_pkt_id: i32,
    /// Header of the first input packet contributing to the packet being built.
    pending_header: Option<Header>,
    /// Timestamp for the next output packet, in milliseconds since the epoch, used
    /// when no input header is available to anchor it. See `drain`.
    next_out_ts_ms: u64,
    /// Duration of one output packet in milliseconds, the step of that timeline.
    out_packet_ms: u64,
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
            scratch: vec![0i16; 0],
            device_id,
            out_pkt_id: 0,
            pending_header: None,
            next_out_ts_ms: 0,
            out_packet_ms: (out_layout.spp as u64 * 1000 / sample_rate as u64).max(1),
            gain: 1.0,
            gain_target: 1.0,
            gain_step: 1.0 / fade_samples as f64,
        }
    }

    /// Milliseconds since the epoch named by a header. `ms` is signed on the wire,
    /// so this is computed in `i64` before being brought back.
    fn header_ms(h: &Header) -> u64 {
        (h.secs as i64 * 1000 + h.ms as i64).max(0) as u64
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
        }
        if self.pending_header.is_none() {
            self.pending_header = Header::parse(packet);
        }
        self.push_samples(packet);
    }

    fn push_samples(&mut self, packet: &[u8]) {
        if self.scratch.len() != self.in_layout.spp {
            self.scratch = vec![0i16; self.in_layout.spp];
        }
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
        self.next_out_ts_ms = 0;
        self.pending_header = None;
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
        let mut pulled: Vec<Vec<i16>> = (0..self.in_layout.n_ch).map(|_| Vec::new()).collect();
        self.resampler.pull(step, want, &mut pulled);
        for (c, s) in pulled.iter().enumerate() {
            self.acc[c].extend_from_slice(s);
        }

        while self.acc.iter().all(|a| a.len() >= self.out_layout.spp) {
            let mut buf = vec![0u8; self.out_layout.packet_len()];
            // Header timestamps name the *start* of a packet's audio, so an output
            // packet is stamped with the first input packet that contributed to it.
            // When there is none — during concealment, or when one drain frames two
            // packets because `step` is below 1.0 and the accumulator has run ahead —
            // the timeline is *extrapolated* rather than left at zero. Borrowing a
            // zeroed header there put 1970 on every silence packet and on roughly
            // every fortieth packet at the ordinary drift-correcting step.
            // Monotonic: an anchor is only taken if it does not move the timeline
            // backwards. Extrapolation can outrun the input — during an outage, or
            // whenever the consumer reads faster than the sender sends — and snapping
            // back to a fresher-but-older input stamp made the sequence go backwards by
            // as much as it had run ahead. A genuine clock change comes with a proven
            // sender restart, and `reset_timeline` is how that one is expressed.
            let anchor = self.pending_header.take().map(|h| Self::header_ms(&h));
            let ts_ms = match anchor {
                Some(ts) if ts >= self.next_out_ts_ms => ts,
                _ => self.next_out_ts_ms,
            };
            self.next_out_ts_ms = ts_ms + self.out_packet_ms;
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
                let taken: Vec<i16> = self.acc[c].drain(..self.out_layout.spp).collect();
                let faded: Vec<i16> = if ramp.is_empty() {
                    taken
                } else {
                    taken
                        .iter()
                        .zip(&ramp)
                        .map(|(&s, &g)| (s as f64 * g).round() as i16)
                        .collect()
                };
                reblock_channel(&mut buf, &self.out_layout, c, &faded);
            }
            self.gain = gain_after;

            self.out_pkt_id = if self.out_pkt_id == i32::MAX - 1 {
                0
            } else {
                self.out_pkt_id + 1
            };
            out.push(Bytes::from(buf));
        }
    }

    /// Output samples per channel currently accumulated but not yet framed.
    pub fn accumulated(&self) -> usize {
        self.acc.first().map_or(0, Vec::len)
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
            let mut buf = vec![0u8; li.packet_len()];
            Header {
                device_id: 5,
                secs: 100,
                ms: (k * 10) as i16,
                pkt_id: k,
            }
            .write_to(&mut buf);
            r.push_packet(&Bytes::from(buf));
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
        let stamped = |ms: i16, id: i32| {
            let mut buf = vec![0u8; li.packet_len()];
            Header {
                device_id: 5,
                secs: 100,
                ms,
                pkt_id: id,
            }
            .write_to(&mut buf);
            Bytes::from(buf)
        };

        for k in 0..4i32 {
            r.push_packet(&stamped((k * 10) as i16, k));
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
        r.push_packet(&stamped(40, 4));
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
            let mut buf = vec![0u8; li.packet_len()];
            Header {
                device_id: 5,
                secs: 100,
                ms: (k * 10) as i16,
                pkt_id: k,
            }
            .write_to(&mut buf);
            let buf = Bytes::from(buf);
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
}
