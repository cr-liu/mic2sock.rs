//! Silence elision: an energy gate that decides whether a released source
//! packet may be dropped outright instead of played, so a genuine backlog
//! drains at many times real time instead of the resampler's 2.5%.
//!
//! **Armed only under pressure, with hysteresis.** The gate arms when the
//! buffer is more than [`ELIDE_ENGAGE_OVER_TARGET_MS`] above the adaptive
//! target — ordinary jitter swings never arm it — and stays armed until the
//! drain brings depth within [`ELIDE_DISENGAGE_OVER_TARGET_MS`] of the
//! target. Between episodes the stream is a faithful, sample-exact copy; the
//! only always-on work is passive floor tracking, so the gate is already
//! calibrated the moment pressure arrives.
//!
//! **Conservative by construction.** A packet is quiet only if the loudest
//! channel's *mean* sits within [`MEAN_FACTOR`]x of the noise floor AND its
//! *peak* within [`PEAK_FACTOR`]x — a brief consonant or click in an
//! otherwise quiet packet keeps the packet. A loud packet re-arms a
//! [`ELIDE_HANGOVER_MS`] hangover that keeps word tails, and speech onsets
//! keep themselves by crossing the threshold. When in doubt the gate says
//! "keep": a kept silence costs drain speed, an elided phoneme costs speech.
//!
//! **Incompatible with a future AEC reference channel**: elision warps the
//! timeline non-uniformly, and spec §7.2's mic-vs-reference alignment cannot
//! survive that. Disable it before ch17 returns.

use protocol::PacketLayout;

/// Depth above the adaptive target that ARMS the gate, in ms. Half a second
/// of excess is unambiguously a backlog episode, not jitter.
pub const ELIDE_ENGAGE_OVER_TARGET_MS: u64 = 500;

/// Depth above the adaptive target at which an armed gate DISARMS, in ms.
/// Well below the engage level so the gate does not chatter at the boundary,
/// and slightly above zero so the drain does not overshoot into conceals.
pub const ELIDE_DISENGAGE_OVER_TARGET_MS: u64 = 100;

/// Room tone kept after the last loud packet, in ms. Protects word tails and
/// keeps breathing audible.
pub const ELIDE_HANGOVER_MS: u64 = 100;

/// Ceiling on releases per tick (the one pushed packet plus elided ones), so
/// one tick never stalls the loop even against a deep all-silent backlog.
pub const ELIDE_BUDGET_PER_TICK: usize = 16;

/// Time observed before the gate trusts its floor estimate, converted to a
/// packet count from the source packet duration — a raw packet count silently
/// changed fivefold between 2 ms and legacy 10 ms framing.
const WARMUP_MS: u64 = 500;

/// Absolute ceilings on what may ever classify as quiet, in i16 amplitude
/// units. These are what make the relative thresholds safe: a stream that
/// begins mid-speech seeds the floor at speech level and then satisfies
/// mean < 2*floor forever — as does DC-offset or clipped content, where
/// mean == peak == floor. No relative test can catch a floor poisoned by its
/// own calibration signal, so nothing louder than unambiguous room tone
/// (-44 dBFS mean, -38 dBFS peak against i16 full scale) is elidable, ever.
///
/// The peak ceiling doubles as the splice bound: an elision cut joins two
/// kept packets without a fade, and the worst-case waveform step is
/// 2 * ABS_PEAK_CEIL ~= -38 dBFS — at the level of the room tone being
/// spliced, masked by it, and far below any speech that matters.
const ABS_MEAN_CEIL: u64 = 100;
const ABS_PEAK_CEIL: u64 = 200;

/// Time of unambiguously loud content (mean above the ceiling) that must have
/// been observed since (re)calibration before anything may be elided. The
/// ceilings bound amplitude but cannot tell low-gain speech from room tone:
/// a stream whose speech never rises above the ceiling seeds the floor AT
/// speech level and every relative test passes forever. Demanding observed
/// contrast closes that: a poisoned floor implies no contrast was ever seen,
/// so the gate simply never opens on such a stream. A genuinely silent
/// stream also never elides -- conservative, and elision is an optimisation.
const CONTRAST_MIN_MS: u64 = 100;
/// A contrast packet must exceed CONTRAST_FACTOR x the floor it was measured
/// against (as well as the absolute ceiling).
const CONTRAST_FACTOR: u64 = 8;

/// Quiet requires the loudest channel's MEAN within MEAN_FACTOR x floor and
/// its PEAK within PEAK_FACTOR x floor (each plus a small absolute offset so
/// a digital-zero floor does not make the thresholds zero). The mean factor
/// is deliberately tight — only signal close to the measured room tone
/// qualifies — and the peak bound is what keeps a packet containing one
/// brief transient out of the elidable class.
const MEAN_FACTOR: u64 = 2;
const MEAN_OFFSET: u64 = 8;
const PEAK_FACTOR: u64 = 4;
const PEAK_OFFSET: u64 = 16;

pub struct EnergyGate {
    /// Noise-floor estimate in mean-abs units: falls instantly to any quieter
    /// packet, rises with a ~30 s time constant so a stretch of speech cannot
    /// lift it to speech level.
    floor: u64,
    seen: u64,
    warmup_packets: u64,
    /// Packets seen whose mean exceeded the absolute ceiling -- evidence that
    /// this stream's loud content rides above it, i.e. the floor is honest.
    loud_seen: u64,
    contrast_packets: u64,
    /// Lowest floor seen since (re)calibration. A floor that has risen far
    /// above it is tracking content, not noise -- speech whose gain dropped
    /// until it hugs its own level -- and the calibration is no longer
    /// trustworthy: the gate closes rather than guess. (Round 9, High.)
    min_floor: u64,
    /// Timestamp of the last packet that measured loud.
    last_loud_ms: u64,
    /// The pressure hysteresis state.
    armed: bool,
}

impl EnergyGate {
    pub fn new(packet_ms: u64) -> Self {
        EnergyGate {
            floor: u64::MAX,
            seen: 0,
            warmup_packets: (WARMUP_MS / packet_ms.max(1)).max(1),
            loud_seen: 0,
            contrast_packets: (CONTRAST_MIN_MS / packet_ms.max(1)).max(1),
            min_floor: u64::MAX,
            last_loud_ms: 0,
            armed: false,
        }
    }

    /// Forgets everything but the packet duration. A proven sender restart
    /// means a possibly different device, gain and room: calibration taken
    /// from the old generation must not judge the new one — the floor of a
    /// quiet old source could classify a quieter-voiced new speaker as room
    /// tone from the very first packet.
    pub fn reset(&mut self) {
        *self = EnergyGate::new(WARMUP_MS / self.warmup_packets.max(1));
    }

    /// Mean-abs of the loudest channel, and the peak-abs across all channels.
    /// Per channel and then max, not a global mean: one speaker near one
    /// microphone of sixteen must count as loud, and a global mean would
    /// divide them away.
    fn energy(payload: &[u8], l: &PacketLayout) -> (u64, u64) {
        let mut loudest_mean = 0u64;
        let mut peak = 0u64;
        for ch in 0..l.n_ch {
            let base = l.header_len + ch * l.spp * 2;
            let mut sum = 0u64;
            for s in 0..l.spp {
                let v = i16::from_le_bytes([payload[base + s * 2], payload[base + s * 2 + 1]]);
                let a = v.unsigned_abs() as u64;
                sum += a;
                peak = peak.max(a);
            }
            loudest_mean = loudest_mean.max(sum / l.spp as u64);
        }
        (loudest_mean, peak)
    }

    /// Observes one released packet and answers whether it may be elided.
    /// Must be called for every real release while elision is enabled, elided
    /// or not — the floor, the hangover and the hysteresis only stay honest
    /// if they see the whole stream.
    pub fn should_elide(
        &mut self,
        payload: &[u8],
        l: &PacketLayout,
        now_ms: u64,
        depth_ms: u64,
        target_ms: u64,
    ) -> bool {
        // Passive calibration happens regardless of pressure.
        let (mean, peak) = Self::energy(payload, l);
        self.seen += 1;

        if mean < self.floor {
            self.floor = mean;
        } else {
            // Slow rise: at 500 packets/s this is a ~30 s time constant, so
            // the floor tracks a drifting noise level (fans, motors) without
            // following speech up.
            // max-then-min, not clamp: when mean == floor the bounds invert
            // (1 > 0) and clamp panics on exactly the most common packet.
            self.floor += ((mean - self.floor) / 16_384).max(1).min(mean - self.floor);
        }

        self.min_floor = self.min_floor.min(self.floor);
        // Real ambient drift is gentle; a floor several times above the
        // quietest level ever measured means the "floor" has climbed onto
        // content, and nothing may be elided against it.
        let floor_trusted = self.floor <= self.min_floor.saturating_mul(4) + MEAN_OFFSET;
        let quiet = mean < self.floor * MEAN_FACTOR + MEAN_OFFSET
            && peak < self.floor * PEAK_FACTOR + PEAK_OFFSET
            && mean <= ABS_MEAN_CEIL
            && peak <= ABS_PEAK_CEIL;
        if !quiet {
            self.last_loud_ms = now_ms;
        }
        // Contrast must clear the floor by a wide factor, not merely the
        // ceiling: content at mean 101 over a floor of 101 is not evidence of
        // dynamic range, and counting it let historical near-floor 'loudness'
        // authorise eliding low-gain speech after a later gain change. A
        // packet only counts if it towers over the floor it was measured
        // against; a floor that falls later only makes old evidence stricter.
        if mean > ABS_MEAN_CEIL.max(self.floor * CONTRAST_FACTOR) {
            self.loud_seen += 1;
        }

        // Pressure hysteresis: arm on a genuine backlog, disarm once the
        // drain has brought depth back near the target.
        if depth_ms > target_ms + ELIDE_ENGAGE_OVER_TARGET_MS {
            self.armed = true;
        } else if depth_ms <= target_ms + ELIDE_DISENGAGE_OVER_TARGET_MS {
            self.armed = false;
        }

        self.armed
            && quiet
            && floor_trusted
            && self.seen >= self.warmup_packets
            && self.loud_seen >= self.contrast_packets
            && now_ms.saturating_sub(self.last_loud_ms) >= ELIDE_HANGOVER_MS
    }

    /// Rewrites the first ~1 ms of `payload` so each channel ramps linearly
    /// from `last_tail` (the final sample of the previously pushed packet)
    /// into this packet's own waveform. An elision cut joins two packets that
    /// were never adjacent; without this the joint is a step, and steps
    /// repeating at the packet rate during a sustained drain form a click
    /// train. With it the joint is exact: no discontinuity survives at all.
    pub fn splice_ramp(payload: &mut [u8], l: &PacketLayout, last_tail: &[i16]) {
        let ramp = l.spp.min(16);
        for (ch, &from) in last_tail.iter().enumerate().take(l.n_ch) {
            let base = l.header_len + ch * l.spp * 2;
            let idx = |s: usize| base + s * 2;
            let target =
                i16::from_le_bytes([payload[idx(ramp - 1)], payload[idx(ramp - 1) + 1]]) as i64;
            let from = from as i64;
            for s in 0..ramp.saturating_sub(1) {
                let v = from + (target - from) * (s as i64 + 1) / ramp as i64;
                payload[idx(s)..idx(s) + 2].copy_from_slice(&(v as i16).to_le_bytes());
            }
        }
    }

    /// The final sample of each channel, for splice continuity tracking.
    pub fn tail_samples(payload: &[u8], l: &PacketLayout) -> Vec<i16> {
        (0..l.n_ch)
            .map(|ch| {
                let i = l.header_len + ch * l.spp * 2 + (l.spp - 1) * 2;
                i16::from_le_bytes([payload[i], payload[i + 1]])
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: PacketLayout = PacketLayout::new(16, 32, 12);
    /// A depth that keeps the gate armed against the default target.
    const DEEP: u64 = 10_000;
    /// Warmup length at the 2 ms test packet duration.
    const WARMUP_N: u64 = WARMUP_MS / 2;

    fn packet(amplitude: i16) -> Vec<u8> {
        let mut buf = vec![0u8; L.packet_len()];
        for ch in 0..L.n_ch {
            let base = L.header_len + ch * L.spp * 2;
            for s in 0..L.spp {
                buf[base + s * 2..base + s * 2 + 2].copy_from_slice(&amplitude.to_le_bytes());
            }
        }
        buf
    }

    /// One loud channel among sixteen quiet ones must count as loud: a global
    /// mean would divide a single near-field speaker away.
    #[test]
    fn a_single_loud_channel_is_loud() {
        let mut buf = packet(2);
        let base = L.header_len + 5 * L.spp * 2;
        for s in 0..L.spp {
            buf[base + s * 2..base + s * 2 + 2].copy_from_slice(&3000i16.to_le_bytes());
        }
        assert_eq!(EnergyGate::energy(&buf, &L), (3000, 3000));
    }

    /// Warms the floor on `noise` and supplies contrast (a stretch of loud
    /// packets): a gate that has never seen unambiguous loudness never opens.
    fn warmed_gate(noise: i16) -> (EnergyGate, u64) {
        let mut g = EnergyGate::new(2);
        let mut now = 0;
        for _ in 0..WARMUP_N {
            now += 2;
            g.should_elide(&packet(noise), &L, now, DEEP, 80);
        }
        for _ in 0..(CONTRAST_MIN_MS / 2) {
            now += 2;
            g.should_elide(&packet(2000), &L, now, DEEP, 80);
        }
        (g, now)
    }

    #[test]
    fn quiet_elides_only_after_warmup_and_hangover() {
        let mut g = EnergyGate::new(2);
        // Before warmup nothing may be elided, however quiet and however deep.
        assert!(!g.should_elide(&packet(0), &L, 2, DEEP, 80));

        let (mut g, mut now) = warmed_gate(2);
        now += ELIDE_HANGOVER_MS + 2;
        assert!(
            g.should_elide(&packet(2), &L, now, DEEP, 80),
            "room tone after warmup and hangover must be elidable under pressure"
        );

        // A loud packet is never elided and re-arms the hangover.
        now += 2;
        assert!(!g.should_elide(&packet(2000), &L, now, DEEP, 80));
        now += 2;
        assert!(
            !g.should_elide(&packet(2), &L, now, DEEP, 80),
            "quiet right after speech is the word tail; hangover must keep it"
        );
        now += ELIDE_HANGOVER_MS;
        assert!(g.should_elide(&packet(2), &L, now, DEEP, 80));
    }

    /// Without pressure nothing is elided, however quiet — and the armed state
    /// must not chatter: it holds until depth returns near the target.
    #[test]
    fn elides_only_under_pressure_with_hysteresis() {
        let (mut g, mut now) = warmed_gate(2);
        now += ELIDE_HANGOVER_MS + 2;

        // At normal depth the gate stays cold.
        assert!(!g.should_elide(&packet(2), &L, now, 80, 80));
        // Below the engage level it must not arm...
        now += 2;
        assert!(!g.should_elide(&packet(2), &L, now, 80 + ELIDE_ENGAGE_OVER_TARGET_MS, 80));
        // ...one ms above it, it arms.
        now += 2;
        assert!(g.should_elide(&packet(2), &L, now, 81 + ELIDE_ENGAGE_OVER_TARGET_MS, 80));
        // Once armed it keeps eliding between the two watermarks...
        now += 2;
        assert!(g.should_elide(&packet(2), &L, now, 300, 80));
        // ...and disarms only at the low watermark.
        now += 2;
        assert!(!g.should_elide(&packet(2), &L, now, 80 + ELIDE_DISENGAGE_OVER_TARGET_MS, 80));
        // Back between the watermarks it must STAY disarmed.
        now += 2;
        assert!(!g.should_elide(&packet(2), &L, now, 300, 80));
    }

    /// A packet whose mean is near the floor but which contains one brief
    /// transient — a consonant, a click — must be kept: peak bounds the class.
    #[test]
    fn a_transient_in_a_quiet_packet_is_kept() {
        let (mut g, mut now) = warmed_gate(2);
        now += ELIDE_HANGOVER_MS + 2;

        let mut buf = packet(2);
        // One 800-amplitude spike: mean over 32 samples ~= 27, well under the
        // mean threshold, but the peak gives it away.
        let base = L.header_len; // channel 0, sample 0
        buf[base..base + 2].copy_from_slice(&800i16.to_le_bytes());
        assert!(
            !g.should_elide(&buf, &L, now, DEEP, 80),
            "a transient must classify as loud however low the packet mean is"
        );
    }

    /// A stretch of speech must not lift the floor to speech level: after it
    /// ends, room tone has to still classify as quiet.
    #[test]
    fn speech_does_not_become_the_floor() {
        let (mut g, mut now) = warmed_gate(2);
        for _ in 0..2500 {
            // 5 s of speech
            now += 2;
            g.should_elide(&packet(2000), &L, now, DEEP, 80);
        }
        now += ELIDE_HANGOVER_MS + 2;
        assert!(
            g.should_elide(&packet(2), &L, now, DEEP, 80),
            "floor rose to speech level during a 5 s utterance"
        );
    }
    /// A stream that begins mid-speech seeds the floor at speech level, and
    /// every relative test then classifies the speech as quiet forever — only
    /// the absolute ceilings stand between that poisoned floor and deletion.
    /// DC-offset and clipped content are the same failure (mean == peak ==
    /// floor) and must never be quiet either.
    #[test]
    fn a_poisoned_floor_cannot_elide_speech() {
        // Cold start straight into constant speech, deep backlog, forever.
        let mut g = EnergyGate::new(2);
        let mut now = 0;
        for _ in 0..(WARMUP_N * 4) {
            now += 2;
            assert!(
                !g.should_elide(&packet(2000), &L, now, DEEP, 80),
                "mid-speech calibration elided speech at t={}",
                now
            );
        }
        // Clipped/DC content: as loud as it gets, mean == peak == floor.
        let mut g = EnergyGate::new(2);
        let mut now = 0;
        for _ in 0..(WARMUP_N * 4) {
            now += 2;
            assert!(
                !g.should_elide(&packet(30000), &L, now, DEEP, 80),
                "clipped/DC content elided at t={}",
                now
            );
        }
    }

    /// The warmup is a duration, not a packet count: at 10 ms legacy packets
    /// it must complete in the same half second (50 packets), not 2.5 s.
    #[test]
    fn warmup_scales_with_packet_duration() {
        let l10 = PacketLayout::new(16, 160, 12);
        let mut buf = vec![0u8; l10.packet_len()];
        for ch in 0..l10.n_ch {
            let base = l10.header_len + ch * l10.spp * 2;
            for s in 0..l10.spp {
                buf[base + s * 2..base + s * 2 + 2].copy_from_slice(&2i16.to_le_bytes());
            }
        }
        let mut loud = vec![0u8; l10.packet_len()];
        for ch in 0..l10.n_ch {
            let base = l10.header_len + ch * l10.spp * 2;
            for s in 0..l10.spp {
                loud[base + s * 2..base + s * 2 + 2].copy_from_slice(&2000i16.to_le_bytes());
            }
        }
        let mut g = EnergyGate::new(10);
        let mut now = 0;
        for _ in 0..(WARMUP_MS / 10) {
            now += 10;
            g.should_elide(&buf, &l10, now, DEEP, 80);
        }
        // Contrast at this duration too: 100 ms is ten 10 ms packets.
        for _ in 0..(CONTRAST_MIN_MS / 10) {
            now += 10;
            g.should_elide(&loud, &l10, now, DEEP, 80);
        }
        now += ELIDE_HANGOVER_MS + 10;
        assert!(
            g.should_elide(&buf, &l10, now, DEEP, 80),
            "50 x 10 ms packets are half a second; warmup must be over"
        );
    }

    /// A proven sender restart resets the gate: old-generation calibration
    /// must not judge the new speaker, and the warmup starts over.
    #[test]
    fn reset_forgets_the_old_generation() {
        let (mut g, mut now) = warmed_gate(2);
        now += ELIDE_HANGOVER_MS + 2;
        assert!(g.should_elide(&packet(2), &L, now, DEEP, 80));

        g.reset();
        now += 2;
        assert!(
            !g.should_elide(&packet(2), &L, now, DEEP, 80),
            "a reset gate must re-warm before eliding anything"
        );
    }
    /// Low-gain speech that never rises above the absolute ceiling seeds the
    /// floor at its own level and passes every relative and absolute test --
    /// the contrast requirement is what keeps it: no packet above the ceiling
    /// means no evidence the floor is honest, so the gate never opens.
    /// (Review round 7, High: mean 80 / peak 160 speech was deletable.)
    #[test]
    fn low_gain_speech_inside_the_ceilings_is_never_elided() {
        let mut g = EnergyGate::new(2);
        let mut now = 0;
        for _ in 0..(WARMUP_N * 8) {
            now += 2;
            let mut buf = packet(80);
            // peak 160 on one sample, mean stays ~82: inside both ceilings.
            let base = L.header_len;
            buf[base..base + 2].copy_from_slice(&160i16.to_le_bytes());
            assert!(
                !g.should_elide(&buf, &L, now, DEEP, 80),
                "low-gain speech elided at t={}",
                now
            );
        }
    }

    /// The splice ramp erases the joint: whatever the previous packet ended
    /// at, the rewritten first millisecond steps by less than the old jump.
    #[test]
    fn splice_ramp_bounds_the_joint_step() {
        let mut buf = packet(-200);
        let tail = vec![200i16; L.n_ch];
        EnergyGate::splice_ramp(&mut buf, &L, &tail);
        for ch in 0..L.n_ch {
            let base = L.header_len + ch * L.spp * 2;
            let s0 = i16::from_le_bytes([buf[base], buf[base + 1]]) as i32;
            assert!(
                (s0 - 200).abs() <= 400 / 16 + 1,
                "first sample after splice jumped {} from the previous tail",
                (s0 - 200).abs()
            );
            let mut prev = 200i32;
            for s in 0..16 {
                let i = base + s * 2;
                let v = i16::from_le_bytes([buf[i], buf[i + 1]]) as i32;
                assert!(
                    (v - prev).abs() <= 400 / 16 + 1,
                    "step {} inside ramp",
                    (v - prev).abs()
                );
                prev = v;
            }
        }
    }
    /// Historical near-floor "loudness" is not contrast: 200 packets at mean
    /// 101 over a floor of 101 must not authorise eliding low-gain speech
    /// after a later gain drop. (Review round 8, High.)
    #[test]
    fn near_floor_loudness_is_not_contrast() {
        let mut g = EnergyGate::new(2);
        let mut now = 0;
        for _ in 0..200 {
            now += 2;
            g.should_elide(&packet(101), &L, now, DEEP, 80);
        }
        for _ in 0..(WARMUP_N * 4) {
            now += 2;
            let mut buf = packet(80);
            let base = L.header_len;
            buf[base..base + 2].copy_from_slice(&160i16.to_le_bytes());
            assert!(
                !g.should_elide(&buf, &L, now, DEEP, 80),
                "stale near-floor contrast authorised elision at t={}",
                now
            );
        }
    }
    /// Genuine contrast earned early must not authorise deletion after a gain
    /// drop leaves speech hugging a risen floor: floor 1 -> 80 is an 80x
    /// climb, which no ambient drift produces. (Review round 9, High.)
    #[test]
    fn contrast_expires_when_the_floor_climbs_onto_content() {
        let mut g = EnergyGate::new(2);
        let mut now = 0;
        for _ in 0..50 {
            now += 2;
            g.should_elide(&packet(1), &L, now, DEEP, 80);
        }
        for _ in 0..50 {
            now += 2;
            g.should_elide(&packet(30000), &L, now, DEEP, 80);
        }
        for _ in 0..2000 {
            now += 2;
            let mut buf = packet(80);
            let base = L.header_len;
            buf[base..base + 2].copy_from_slice(&160i16.to_le_bytes());
            assert!(
                !g.should_elide(&buf, &L, now, DEEP, 80),
                "gain-dropped speech elided at t={}",
                now
            );
        }
    }
}
