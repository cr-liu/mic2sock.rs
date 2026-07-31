/// How an arrival was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// Within `d_max_adaptive`: ordinary jitter. Feeds the depth statistic; the
    /// buffer is expected to absorb it.
    Jitter { above_min_ms: u64 },
    /// Beyond `d_max_adaptive`: an outage. **Excluded from the statistic.**
    ///
    /// Without this split, one 1-second stall would inflate the target depth to
    /// a full second and keep it there. No buffer can cover a 1-second event, so
    /// deepening for it trades permanent latency for nothing.
    Outage { above_min_ms: u64 },
}

/// Tracks the target buffer depth from observed arrival delays.
///
/// `d = arrival_time - header_timestamp` has no meaningful absolute value (the
/// two ends' clocks are offset), so only its variation is used.
pub struct DepthEstimator {
    d_max_adaptive_ms: u64,
    packet_ms: u64,
    window_ms: u64,
    /// (now_ms, d) of jitter-classified arrivals inside the window.
    samples: std::collections::VecDeque<(u64, i64)>,
    target_ms: u64,
    last_shrink_ms: u64,
}

/// Sliding window for the statistic. 30 s keeps clock-drift contamination
/// (50 ppm over 30 s = 1.5 ms) far below the jitter being measured (tens of ms).
pub const WINDOW_MS: u64 = 30_000;
/// Minimum interval between one-packet shrinks of the target.
pub const SHRINK_INTERVAL_MS: u64 = 10_000;

impl DepthEstimator {
    pub fn new(d_max_adaptive_ms: u64, packet_ms: u64) -> Self {
        let packet_ms = packet_ms.max(1);
        DepthEstimator {
            d_max_adaptive_ms,
            packet_ms,
            window_ms: WINDOW_MS,
            samples: std::collections::VecDeque::new(),
            target_ms: 2 * packet_ms,
            last_shrink_ms: 0,
        }
    }

    pub fn target_ms(&self) -> u64 {
        self.target_ms
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Observes one arrival.
    ///
    /// * `now_ms` — local monotonic arrival time.
    /// * `header_ms` — the packet header's timestamp, on the sender's clock.
    /// * `_reserved` — kept at 0; present so the signature does not churn when
    ///   per-source statistics are added.
    ///
    /// The absolute difference is meaningless because the clocks are offset, so
    /// only its excess over the windowed minimum is used.
    pub fn observe(&mut self, now_ms: u64, header_ms: u64, _reserved: u8) -> Arrival {
        let d = now_ms as i64 - header_ms as i64;

        // Evict stale samples first so d_min reflects the current window.
        while let Some(&(t, _)) = self.samples.front() {
            if now_ms.saturating_sub(t) > self.window_ms {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        let d_min = self.samples.iter().map(|&(_, d)| d).min().unwrap_or(d);
        let above = (d - d_min).max(0) as u64;

        if above > self.d_max_adaptive_ms {
            // An outage. Deliberately NOT recorded: no buffer can cover it, and
            // letting it into the statistic would pin the target at its length.
            return Arrival::Outage {
                above_min_ms: above,
            };
        }

        self.samples.push_back((now_ms, d));
        self.recompute(now_ms);
        Arrival::Jitter {
            above_min_ms: above,
        }
    }

    /// Having to synthesize audio is direct evidence the buffer was too shallow.
    pub fn on_conceal(&mut self, now_ms: u64) {
        self.grow_to(self.target_ms + self.packet_ms, now_ms);
    }

    fn recompute(&mut self, now_ms: u64) {
        let d_min = match self.samples.iter().map(|&(_, d)| d).min() {
            Some(m) => m,
            None => return,
        };
        let w = self
            .samples
            .iter()
            .map(|&(_, d)| (d - d_min).max(0) as u64)
            .max()
            .unwrap_or(0);
        let want = (w + 2 * self.packet_ms).min(self.d_max_adaptive_ms);

        if want > self.target_ms {
            self.grow_to(want, now_ms);
        } else if want < self.target_ms {
            // Shrink at most one packet, at most once per interval. Growth is
            // immediate and shrink is slow so the depth does not oscillate.
            if now_ms.saturating_sub(self.last_shrink_ms) >= SHRINK_INTERVAL_MS {
                self.target_ms = self.target_ms.saturating_sub(self.packet_ms).max(want);
                self.last_shrink_ms = now_ms;
            }
        }
    }

    fn grow_to(&mut self, want: u64, now_ms: u64) {
        let capped = want.min(self.d_max_adaptive_ms.max(2 * self.packet_ms));
        if capped > self.target_ms {
            self.target_ms = capped;
            // Growth resets the shrink timer, so a burst cannot be immediately
            // undone by a shrink that was already due.
            self.last_shrink_ms = now_ms;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn est() -> DepthEstimator {
        // d_max_adaptive 80 ms, 10 ms packets
        DepthEstimator::new(80, 10)
    }

    #[test]
    fn first_arrival_gives_the_floor_target() {
        let mut e = est();
        assert_eq!(
            e.observe(1000, 1000, 0),
            Arrival::Jitter { above_min_ms: 0 }
        );
        // w = 0, plus the 2-packet margin.
        assert_eq!(e.target_ms(), 20);
    }

    /// The statistic is relative: a large constant clock offset must not change
    /// the target at all.
    #[test]
    fn a_constant_clock_offset_does_not_change_the_target() {
        let mut a = est();
        let mut b = est();
        for k in 0..50u64 {
            a.observe(1000 + k * 10, 1000 + k * 10, 0);
            // b's header timestamps are offset by a fixed +500000 ms. The
            // direction is arbitrary -- what matters is that a constant offset
            // shifts every `d` equally, so `d - d_min` is untouched. Offsetting
            // forward keeps the expression inside u64.
            b.observe(1000 + k * 10, 1000 + k * 10 + 500_000, 0);
        }
        assert_eq!(a.target_ms(), b.target_ms());
    }

    #[test]
    fn jitter_within_threshold_raises_the_target() {
        let mut e = est();
        e.observe(1000, 1000, 0);
        // 40 ms late relative to the minimum.
        assert_eq!(
            e.observe(1050, 1010, 0),
            Arrival::Jitter { above_min_ms: 40 }
        );
        assert_eq!(e.target_ms(), 40 + 20);
    }

    /// The load-bearing behaviour: a one-second stall must NOT inflate the
    /// target, or the buffer would permanently carry a second of latency.
    #[test]
    fn an_outage_is_excluded_from_the_target() {
        let mut e = est();
        e.observe(1000, 1000, 0);
        let before = e.target_ms();
        assert_eq!(
            e.observe(2000, 1000, 0),
            Arrival::Outage { above_min_ms: 1000 }
        );
        assert_eq!(e.target_ms(), before, "an outage inflated the target");
    }

    #[test]
    fn target_is_capped_at_d_max_adaptive() {
        let mut e = est();
        e.observe(1000, 1000, 0);
        // 79 ms is still jitter, but 79 + 20 margin exceeds the 80 ms cap.
        e.observe(1079, 1000, 0);
        assert_eq!(e.target_ms(), 80);
    }

    /// Concealment is the most direct feedback there is: if we had to synthesize
    /// audio, the buffer was too shallow.
    #[test]
    fn a_conceal_event_deepens_the_target_immediately() {
        let mut e = est();
        e.observe(1000, 1000, 0);
        let before = e.target_ms();
        e.on_conceal(1001);
        assert!(e.target_ms() > before, "conceal did not deepen the target");
    }

    /// Growth is immediate, shrink is rate limited: reacting fast to degradation
    /// and slowly to improvement avoids oscillating the depth.
    #[test]
    fn shrink_is_rate_limited_but_growth_is_not() {
        let mut e = est();
        e.observe(0, 0, 0);
        e.observe(60, 0, 0); // w = 60 -> target 80 (capped)
        assert_eq!(e.target_ms(), 80);

        // The window slides past those samples, so w collapses to 0.
        let t = WINDOW_MS + 1000;
        e.observe(t, t, 0);
        // Only one packet may be shed, and only once per SHRINK_INTERVAL_MS.
        assert_eq!(e.target_ms(), 70, "shrank by more than one packet");
        e.observe(t + 1, t + 1, 0);
        assert_eq!(e.target_ms(), 70, "shrank twice inside the interval");
        e.observe(t + SHRINK_INTERVAL_MS, t + SHRINK_INTERVAL_MS, 0);
        assert_eq!(e.target_ms(), 60);
    }

    #[test]
    fn samples_older_than_the_window_are_dropped() {
        let mut e = est();
        e.observe(0, 0, 0);
        e.observe(50, 0, 0);
        assert_eq!(e.sample_count(), 2);
        e.observe(WINDOW_MS + 51, WINDOW_MS + 51, 0);
        assert_eq!(e.sample_count(), 1, "stale samples were not evicted");
    }
}
