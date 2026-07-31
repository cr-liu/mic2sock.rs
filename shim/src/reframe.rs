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
/// The resampler is a 4-point Catmull-Rom interpolator, so it always holds
/// exactly three samples of history in its taps — a fixed, one-time 0.19 ms
/// warm-up at 16 kHz, not a per-packet loss: those samples come out as soon as
/// more input arrives. It is identical across channels, so it cannot disturb
/// inter-channel phase, which is the one property this module has to preserve.
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
    /// Linear fade multiplier applied on the way out and back in, 0.0..=1.0.
    gain: f64,
    gain_step: f64,
}

impl Reframer {
    /// `fade_ms` is the ramp used entering and leaving an outage.
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
            gain: 1.0,
            gain_step: 1.0 / fade_samples as f64,
        }
    }

    /// Feeds one input packet's audio into the resampler.
    pub fn push_packet(&mut self, packet: &[u8]) {
        if self.pending_header.is_none() {
            self.pending_header = Header::parse(packet);
        }
        if self.scratch.len() != self.in_layout.spp {
            self.scratch = vec![0i16; self.in_layout.spp];
        }
        for c in 0..self.in_layout.n_ch {
            deblock_channel(packet, &self.in_layout, c, &mut self.scratch);
            self.resampler.push(c, &self.scratch);
        }
    }

    /// Feeds one input packet's worth of silence, and ramps the fade down.
    pub fn push_silence(&mut self) {
        let zeros = vec![0i16; self.in_layout.spp];
        for c in 0..self.in_layout.n_ch {
            self.resampler.push(c, &zeros);
        }
        self.fade_towards(0.0);
    }

    /// Feeds real audio and ramps the fade back up.
    pub fn push_audible(&mut self, packet: &[u8]) {
        self.push_packet(packet);
        self.fade_towards(1.0);
    }

    fn fade_towards(&mut self, goal: f64) {
        let span = self.in_layout.spp as f64 * self.gain_step;
        if goal > self.gain {
            self.gain = (self.gain + span).min(1.0);
        } else if goal < self.gain {
            self.gain = (self.gain - span).max(0.0);
        }
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
            let h = self.pending_header.take().unwrap_or(Header {
                device_id: self.device_id,
                secs: 0,
                ms: 0,
                pkt_id: 0,
            });
            // The timestamp of the first contributing input packet: header
            // timestamps name the *start* of a packet's audio. The id is the
            // shim's own sequence, so the consumer never sees a jump.
            Header {
                device_id: self.device_id,
                secs: h.secs,
                ms: h.ms,
                pkt_id: self.out_pkt_id,
            }
            .write_to(&mut buf);

            for c in 0..self.out_layout.n_ch {
                let taken: Vec<i16> = self.acc[c].drain(..self.out_layout.spp).collect();
                let faded: Vec<i16> = if self.gain >= 1.0 {
                    taken
                } else {
                    taken
                        .iter()
                        .map(|&s| (s as f64 * self.gain).round() as i16)
                        .collect()
                };
                reblock_channel(&mut buf, &self.out_layout, c, &faded);
            }

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

    #[test]
    fn output_header_carries_the_configured_device_id() {
        let (li, lo) = layouts(160, 160);
        let mut r = Reframer::new(li, lo, 42, 16000, 20);
        let mut out = Vec::new();
        // Two inputs, so that a whole output packet exists past the taps.
        for k in 0..2 {
            r.push_packet(&make_input(&li, k, 0));
            r.drain(1.0, &mut out);
        }
        assert_eq!(Header::parse(&out[0]).unwrap().device_id, 42);
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
