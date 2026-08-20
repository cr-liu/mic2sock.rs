//! Silence elision: an energy gate that decides whether a released source
//! packet may be dropped outright instead of played, so a backlog drains at
//! many times real time instead of the resampler's 2.5%.
//!
//! Only ever consulted while the buffer is deeper than the adaptive target
//! plus a margin — at target depth nothing is elided and the stream is a
//! faithful copy. The 2 ms source packets are the elision granularity, which
//! is what keeps a dropped packet from clipping into a word: speech onsets
//! cross the threshold within a packet or two, and a hangover keeps a tail of
//! room tone after every loud packet.
//!
//! **Incompatible with a future AEC reference channel**: elision warps the
//! timeline non-uniformly, and spec §7.2's mic-vs-reference alignment cannot
//! survive that. Disable it before ch17 returns.

use protocol::PacketLayout;

/// Depth above the adaptive target before elision may engage, in ms. Below
/// this the buffer is near its normal operating point and draining faster
/// would only cause conceals later.
pub const ELIDE_MARGIN_MS: u64 = 40;

/// Room tone kept after the last loud packet, in ms. Protects word tails and
/// keeps breathing audible; onsets protect themselves by crossing the
/// threshold.
pub const ELIDE_HANGOVER_MS: u64 = 50;

/// Ceiling on releases per tick (the one pushed packet plus elided ones), so
/// one tick never stalls the loop even against a deep all-silent backlog.
/// 16 packets at 2 ms is 32 ms of audio examined per tick — a drain rate of
/// up to 16x real time.
pub const ELIDE_BUDGET_PER_TICK: usize = 16;

/// Packets observed before the gate trusts its floor estimate. A stream that
/// begins mid-speech seeds the floor at speech level, and until a quieter
/// moment recalibrates it the threshold is wrong — so the gate stays closed
/// for roughly half a second of packets rather than guessing.
const WARMUP_PACKETS: u64 = 250;

/// Threshold = floor * FACTOR + OFFSET, in mean-abs sample units. The factor
/// separates "room tone" from "someone speaking"; the offset keeps a
/// digital-zero floor from making the threshold zero too.
const THRESHOLD_FACTOR: u64 = 4;
const THRESHOLD_OFFSET: u64 = 8;

pub struct EnergyGate {
    /// Noise-floor estimate in mean-abs units: falls instantly to any quieter
    /// packet, rises with a ~30 s time constant so a stretch of speech cannot
    /// lift it to speech level.
    floor: u64,
    seen: u64,
    /// Timestamp of the last packet that measured loud.
    last_loud_ms: u64,
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
        }
    }

    /// Loudest channel's mean absolute sample value. Per channel and then max,
    /// not a global mean: one speaker near one microphone of sixteen must
    /// count as loud, and a global mean would divide them away.
    fn energy(payload: &[u8], l: &PacketLayout) -> u64 {
        let mut loudest = 0u64;
        for ch in 0..l.n_ch {
            let base = l.header_len + ch * l.spp * 2;
            let mut sum = 0u64;
            for s in 0..l.spp {
                let v = i16::from_le_bytes([payload[base + s * 2], payload[base + s * 2 + 1]]);
                sum += v.unsigned_abs() as u64;
            }
            loudest = loudest.max(sum / l.spp as u64);
        }
        loudest
    }

    /// Observes one released packet and answers whether it may be elided.
    /// Must be called for every real release while elision is enabled, elided
    /// or not — the floor and the hangover only stay honest if they see the
    /// whole stream.
    pub fn may_elide(&mut self, payload: &[u8], l: &PacketLayout, now_ms: u64) -> bool {
        let e = Self::energy(payload, l);
        self.seen += 1;

        if e < self.floor {
            self.floor = e;
        } else {
            // Slow rise: at 500 packets/s this is a ~30 s time constant, so
            // the floor tracks a drifting noise level (fans, motors) without
            // following speech up.
            self.floor += ((e - self.floor) / 16_384).max(1).min(e - self.floor);
        }

        let threshold = self.floor * THRESHOLD_FACTOR + THRESHOLD_OFFSET;
        let quiet = e < threshold;
        if !quiet {
            self.last_loud_ms = now_ms;
        }

        quiet
            && self.seen >= WARMUP_PACKETS
            && now_ms.saturating_sub(self.last_loud_ms) >= ELIDE_HANGOVER_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: PacketLayout = PacketLayout::new(16, 32, 12);

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
        assert_eq!(EnergyGate::energy(&buf, &L), 3000);
    }

    fn warmed_gate(noise: i16) -> (EnergyGate, u64) {
        let mut g = EnergyGate::new();
        let mut now = 0;
        for _ in 0..WARMUP_PACKETS {
            now += 2;
            g.may_elide(&packet(noise), &L, now);
        }
        (g, now)
    }

    #[test]
    fn quiet_elides_only_after_warmup_and_hangover() {
        let mut g = EnergyGate::new();
        // Before warmup nothing may be elided, however quiet.
        assert!(!g.may_elide(&packet(0), &L, 2));

        let (mut g, mut now) = warmed_gate(2);
        now += ELIDE_HANGOVER_MS + 2;
        assert!(
            g.may_elide(&packet(2), &L, now),
            "room tone after warmup and hangover must be elidable"
        );

        // A loud packet is never elided and re-arms the hangover.
        now += 2;
        assert!(!g.may_elide(&packet(2000), &L, now));
        now += 2;
        assert!(
            !g.may_elide(&packet(2), &L, now),
            "quiet right after speech is the word tail; hangover must keep it"
        );
        now += ELIDE_HANGOVER_MS;
        assert!(g.may_elide(&packet(2), &L, now));
    }

    /// A stretch of speech must not lift the floor to speech level: after it
    /// ends, room tone has to still classify as quiet.
    #[test]
    fn speech_does_not_become_the_floor() {
        let (mut g, mut now) = warmed_gate(2);
        for _ in 0..2500 {
            // 5 s of speech
            now += 2;
            g.may_elide(&packet(2000), &L, now);
        }
        now += ELIDE_HANGOVER_MS + 2;
        assert!(
            g.may_elide(&packet(2), &L, now),
            "floor rose to speech level during a 5 s utterance"
        );
    }
}
