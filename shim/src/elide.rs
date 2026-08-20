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

/// Packets observed before the gate trusts its floor estimate. A stream that
/// begins mid-speech seeds the floor at speech level, and until a quieter
/// moment recalibrates it the threshold is wrong — so the gate stays closed
/// for roughly half a second of packets rather than guessing.
const WARMUP_PACKETS: u64 = 250;

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
    /// Timestamp of the last packet that measured loud.
    last_loud_ms: u64,
    /// The pressure hysteresis state.
    armed: bool,
}

impl Default for EnergyGate {
    fn default() -> Self {
        EnergyGate::new()
    }
}

impl EnergyGate {
    pub fn new() -> Self {
        EnergyGate {
            floor: u64::MAX,
            seen: 0,
            last_loud_ms: 0,
            armed: false,
        }
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

        let quiet = mean < self.floor * MEAN_FACTOR + MEAN_OFFSET
            && peak < self.floor * PEAK_FACTOR + PEAK_OFFSET;
        if !quiet {
            self.last_loud_ms = now_ms;
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
            && self.seen >= WARMUP_PACKETS
            && now_ms.saturating_sub(self.last_loud_ms) >= ELIDE_HANGOVER_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: PacketLayout = PacketLayout::new(16, 32, 12);
    /// A depth that keeps the gate armed against the default target.
    const DEEP: u64 = 10_000;

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

    fn warmed_gate(noise: i16) -> (EnergyGate, u64) {
        let mut g = EnergyGate::new();
        let mut now = 0;
        for _ in 0..WARMUP_PACKETS {
            now += 2;
            g.should_elide(&packet(noise), &L, now, DEEP, 80);
        }
        (g, now)
    }

    #[test]
    fn quiet_elides_only_after_warmup_and_hangover() {
        let mut g = EnergyGate::new();
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
}
