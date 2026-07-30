/// What to do with an observed `pkt_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapAction {
    /// Sequence is contiguous; emit the packet as-is.
    Pass,
    /// A gap was detected. Emit `samples_per_channel` samples of concealment
    /// **in place, to every channel equally**, and only then emit the packet.
    ///
    /// In place rather than later is a hard requirement: concealing late shifts
    /// the far-end reference channel relative to the mic channels, and at 16 kHz
    /// one packet is 160 samples — well outside an AEC filter's converged region,
    /// costing 0.5-2 s of re-convergence.
    Fill { samples_per_channel: usize },
    /// A late or duplicate packet; drop it.
    Drop,
    /// Sequence discontinuity beyond what can be concealed (source restart, very
    /// long outage, or a gap larger than `max_fill_packets`). Re-anchor and emit
    /// the packet without concealment. The timeline breaks here deliberately, so
    /// callers should count these.
    Resync,
}

/// The sender resets `pkt_id` to 0 on reaching `i32::MAX`, so the sequence runs
/// `..., MAX-2, MAX-1, 0, 1, ...` and `i32::MAX` itself never appears.
fn advance(id: i32) -> i32 {
    let n = id.wrapping_add(1);
    if n == i32::MAX {
        0
    } else {
        n
    }
}

fn retreat(id: i32) -> i32 {
    if id == 0 {
        i32::MAX - 1
    } else {
        id - 1
    }
}

/// Tracks contiguity of a `pkt_id` sequence.
pub struct GapTracker {
    next_id: Option<i32>,
    spp: usize,
    max_fill_packets: usize,
    reorder_window: usize,
}

impl GapTracker {
    /// * `spp` — samples per channel per packet, used to convert "how many
    ///   packets are missing" into "how many samples to conceal".
    /// * `max_fill_packets` — the largest gap, in packets, that will be
    ///   concealed; anything larger becomes `Resync`. The practical ceiling is
    ///   set by how much the downstream ring buffer can absorb.
    /// * `reorder_window` — how far back an id may be and still count as a late
    ///   packet (`Drop`) rather than a source restart.
    pub fn new(spp: usize, max_fill_packets: usize, reorder_window: usize) -> Self {
        GapTracker {
            next_id: None,
            spp,
            max_fill_packets,
            reorder_window,
        }
    }

    /// Discards accumulated state; the next packet is treated as the first.
    pub fn reset(&mut self) {
        self.next_id = None;
    }

    pub fn observe(&mut self, pkt_id: i32) -> GapAction {
        let Some(next) = self.next_id else {
            self.next_id = Some(advance(pkt_id));
            return GapAction::Pass;
        };

        // Walk forward from the expected id, at most max_fill_packets steps,
        // looking for a hit. Stepping with `advance` rather than subtracting is
        // what makes the discontinuous id space (i32::MAX resets to 0) come out
        // right without a special case.
        let mut probe = next;
        for missing in 0..=self.max_fill_packets {
            if probe == pkt_id {
                self.next_id = Some(advance(pkt_id));
                return if missing == 0 {
                    GapAction::Pass
                } else {
                    GapAction::Fill {
                        samples_per_channel: missing * self.spp,
                    }
                };
            }
            probe = advance(probe);
        }

        // Walk backward to decide whether this is a late or duplicate packet.
        let mut probe = next;
        for _ in 0..self.reorder_window {
            probe = retreat(probe);
            if probe == pkt_id {
                return GapAction::Drop;
            }
        }

        // Neither within the forward concealable range nor the backward reorder
        // window.
        self.next_id = Some(advance(pkt_id));
        GapAction::Resync
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPP: usize = 160;

    fn tracker() -> GapTracker {
        GapTracker::new(SPP, 8, 16)
    }

    #[test]
    fn first_packet_passes() {
        assert_eq!(tracker().observe(1000), GapAction::Pass);
    }

    #[test]
    fn consecutive_packets_pass() {
        let mut t = tracker();
        for id in 1000..1010 {
            assert_eq!(t.observe(id), GapAction::Pass, "id {}", id);
        }
    }

    #[test]
    fn one_missing_packet_fills_one_packet_of_samples() {
        let mut t = tracker();
        assert_eq!(t.observe(1000), GapAction::Pass);
        assert_eq!(
            t.observe(1002),
            GapAction::Fill {
                samples_per_channel: SPP
            }
        );
        // The sequence is contiguous again after concealment.
        assert_eq!(t.observe(1003), GapAction::Pass);
    }

    #[test]
    fn three_missing_packets_fill_three_packets_of_samples() {
        let mut t = tracker();
        t.observe(1000);
        assert_eq!(
            t.observe(1004),
            GapAction::Fill {
                samples_per_channel: 3 * SPP
            }
        );
    }

    #[test]
    fn duplicate_is_dropped() {
        let mut t = tracker();
        t.observe(1000);
        t.observe(1001);
        assert_eq!(t.observe(1001), GapAction::Drop);
        assert_eq!(t.observe(1000), GapAction::Drop);
    }

    #[test]
    fn late_packet_within_reorder_window_is_dropped() {
        let mut t = tracker();
        for id in 1000..1020 {
            t.observe(id);
        }
        // next_id == 1020; 1010 is inside the 16-wide reorder window.
        assert_eq!(t.observe(1010), GapAction::Drop);
    }

    #[test]
    fn gap_larger_than_max_fill_resyncs() {
        let mut t = tracker();
        t.observe(1000);
        // max_fill_packets = 8, this gap is 20 packets, so give up concealing.
        assert_eq!(t.observe(1021), GapAction::Resync);
        // Re-anchored, so the sequence continues.
        assert_eq!(t.observe(1022), GapAction::Pass);
    }

    #[test]
    fn source_restart_resyncs() {
        let mut t = tracker();
        for id in 1000..1010 {
            t.observe(id);
        }
        // The Pi restarted and pkt_id went back to 0: a backward jump far outside
        // the reorder window.
        assert_eq!(t.observe(0), GapAction::Resync);
        assert_eq!(t.observe(1), GapAction::Pass);
    }

    /// Happens once every 2^31 packets, i.e. roughly every 248 days at 100 pps.
    /// Tested here because a bug in it takes a minute to catch now and is
    /// undebuggable 248 days from now.
    #[test]
    fn i32_max_wrap_is_continuous() {
        let mut t = tracker();
        assert_eq!(t.observe(i32::MAX - 2), GapAction::Pass);
        assert_eq!(t.observe(i32::MAX - 1), GapAction::Pass);
        assert_eq!(t.observe(0), GapAction::Pass, "wrap misclassified");
        assert_eq!(t.observe(1), GapAction::Pass);
    }

    /// A gap straddling the wrap point must still be concealed correctly.
    #[test]
    fn gap_across_wrap_fills_correctly() {
        let mut t = tracker();
        assert_eq!(t.observe(i32::MAX - 2), GapAction::Pass);
        // Skips MAX-1 and 0, landing on 1: two packets missing.
        assert_eq!(
            t.observe(1),
            GapAction::Fill {
                samples_per_channel: 2 * SPP
            }
        );
    }

    #[test]
    fn fill_boundary_exactly_at_max_fill_packets() {
        let mut t = tracker();
        t.observe(1000);
        // Exactly 8 missing, equal to max_fill_packets, so still concealed.
        assert_eq!(
            t.observe(1009),
            GapAction::Fill {
                samples_per_channel: 8 * SPP
            }
        );

        let mut t2 = tracker();
        t2.observe(1000);
        // 9 missing exceeds the limit, so Resync.
        assert_eq!(t2.observe(1010), GapAction::Resync);
    }
}
