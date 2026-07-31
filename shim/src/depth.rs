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
    /// Delay an on-time packet would have. See [`DepthEstimator::reference`].
    ref_ms: Option<i64>,
    ref_updated_ms: u64,
    target_ms: u64,
    last_shrink_ms: u64,
}

/// Sliding window for the statistic. 30 s keeps clock-drift contamination
/// (50 ppm over 30 s = 1.5 ms) far below the jitter being measured (tens of ms).
pub const WINDOW_MS: u64 = 30_000;
/// Minimum interval between one-packet shrinks of the target.
pub const SHRINK_INTERVAL_MS: u64 = 10_000;
/// How long the classification reference survives with no fresh samples.
///
/// Long enough that a *run* of late packets cannot re-baseline on itself, which
/// is how a one-second outage reached the statistic. Short enough that drift over
/// the interval (50 ppm × 300 s = 15 ms) stays well below the classification
/// threshold, so a surviving reference cannot by itself flip an on-time packet
/// into an outage.
pub const REF_EXPIRY_MS: u64 = 10 * WINDOW_MS;

impl DepthEstimator {
    /// # Panics
    ///
    /// If `d_max_adaptive_ms` is below two packets. Two packets is the buffer's
    /// structural floor, so a smaller ceiling would not be a ceiling: the
    /// estimator would start above it. `Config::validate` refuses such a pair.
    pub fn new(d_max_adaptive_ms: u64, packet_ms: u64) -> Self {
        let packet_ms = packet_ms.max(1);
        assert!(
            d_max_adaptive_ms >= 2 * packet_ms,
            "d_max_adaptive_ms {} is below the two-packet floor {}",
            d_max_adaptive_ms,
            2 * packet_ms
        );
        DepthEstimator {
            d_max_adaptive_ms,
            packet_ms,
            window_ms: WINDOW_MS,
            samples: std::collections::VecDeque::new(),
            ref_ms: None,
            ref_updated_ms: 0,
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

        // Monotonic time is a precondition. If it is violated the ordering the
        // eviction loop depends on is gone, so discard the window rather than
        // let un-evictable future-dated samples accumulate behind the front. The
        // shrink timestamp has to come back too: one left in the future blocks
        // every shrink until real time catches up with it.
        if matches!(self.samples.back(), Some(&(t, _)) if t > now_ms) {
            self.samples.clear();
            self.last_shrink_ms = now_ms;
        }

        // Evict stale samples so the statistic holds only fresh ones. The
        // reference deliberately survives this -- see `reference` -- but a stale
        // sample must not contribute to the spread: over a long arrival gap it has
        // drifted, and letting a drifted sample set `w` would inflate the target
        // with no jitter having occurred.
        while let Some(&(t, _)) = self.samples.front() {
            if now_ms.saturating_sub(t) > self.window_ms {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        let d_ref = self.reference(d, now_ms);
        let above = (d - d_ref).max(0) as u64;

        if above > self.d_max_adaptive_ms {
            // An outage. Deliberately NOT recorded: no buffer can cover it, and
            // letting it into the statistic would pin the target at its length.
            return Arrival::Outage {
                above_min_ms: above,
            };
        }

        // A sample far *below* the reference is not jitter — nothing arrives half a
        // second early. It is a step change in the clock offset, after which the
        // samples taken on the old offset are no longer comparable: keeping them
        // reports the step itself as the spread and pins the target at the cap for a
        // minute. Retiring them is also what stops a re-baselined outage run from
        // contaminating the window once delays return to normal.
        if d < d_ref - self.d_max_adaptive_ms as i64 {
            self.samples.clear();
        }

        self.samples.push_back((now_ms, d));
        // The accepted sample may itself be the new minimum, and the reference has
        // to include it: classifying against a pre-insertion minimum left the
        // durable value one sample stale, so a late packet arriving after the window
        // had emptied was measured against too high a reference and filed as jitter.
        // This is also the only place the expiry clock is wound, so that an outage
        // run cannot extend the reference's life by finding old samples still in the
        // window.
        self.ref_ms = Some(self.ref_ms.map_or(d, |r| r.min(d)));
        self.ref_updated_ms = now_ms;
        self.recompute(now_ms);
        Arrival::Jitter {
            above_min_ms: above,
        }
    }

    /// The delay an on-time packet would have, on the current clock offset.
    ///
    /// While the window has samples this is their minimum — that is what
    /// `above_min_ms` is measured against, and it tracks drift as the window
    /// slides. The value **persists across an arrival gap that empties the
    /// window**, because a reference that vanishes exactly when a late packet
    /// arrives lets that packet become its own minimum and be filed as ordinary
    /// jitter. That is how a one-second outage used to reach the statistic, and a
    /// *run* of late packets needs the same protection — so this is durable state
    /// and not a value re-derived from an empty window on each call.
    ///
    /// The minimum, specifically, and not the newest sample: with samples at 0 and
    /// 20 ms, a packet 90 ms above the minimum is only 70 above the newest, which
    /// is inside the threshold. Taking the newest hides exactly the arrivals this
    /// classification exists to catch.
    ///
    /// It expires after [`REF_EXPIRY_MS`], which is what stops a permanent offset
    /// change — a sender restart with a different clock offset — from classifying
    /// every arrival as an outage forever and freezing the target.
    fn reference(&mut self, d: i64, now_ms: u64) -> i64 {
        if let Some(min) = self.samples.iter().map(|&(_, d)| d).min() {
            self.ref_ms = Some(min);
            return min;
        }
        match self.ref_ms {
            Some(r) if now_ms.saturating_sub(self.ref_updated_ms) <= REF_EXPIRY_MS => r,
            _ => {
                self.ref_ms = Some(d);
                self.ref_updated_ms = now_ms;
                d
            }
        }
    }

    /// The sender's packet id sequence restarted, which proves a new process and so a
    /// possibly new clock offset. The old statistic describes the old offset, so it
    /// goes.
    ///
    /// **Not** a transport reconnect. `TcpSource` reconnects after any EOF, read
    /// timeout or network blip, while the sender's process, ids and clock carry on —
    /// and discarding the statistic there is actively harmful: the first packet after
    /// the stall is the *most* delayed one, and with no reference to measure it
    /// against it becomes its own baseline and is filed as ordinary jitter. That is
    /// exactly the defect this module exists to prevent. The pipeline calls this only
    /// when `JitterBuffer::generation_resets` has increased, which is a proven id
    /// reset rather than a suspicion of one.
    ///
    /// [`REF_EXPIRY_MS`] remains the backstop for an offset change with no id reset
    /// behind it — NTP stepping the sender's clock mid-stream.
    pub fn on_generation_reset(&mut self) {
        self.samples.clear();
        self.ref_ms = None;
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
        // A genuine cap: `new` guarantees d_max_adaptive_ms is at or above the
        // two-packet floor, so it never has to be raised to accommodate it.
        let capped = want.min(self.d_max_adaptive_ms);
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

    /// The same load-bearing behaviour, at the boundary that used to defeat it,
    /// and for a *run* of late packets rather than one. A stall lasting longer
    /// than the window evicts the baseline in the very call that needs it; each
    /// late packet then became its own minimum, reported `above_min_ms: 0`, and
    /// was filed as jitter, after which the next normal arrival made the retained
    /// range 1000 ms and the target jumped to the 80 ms cap.
    #[test]
    fn a_sustained_outage_is_excluded_packet_after_packet() {
        let mut e = est();
        e.observe(0, 0, 0);
        assert_eq!(e.target_ms(), 20);

        for k in 0..5u64 {
            let t = WINDOW_MS + 1 + k * 10;
            assert_eq!(
                e.observe(t, t - 1000, 0),
                Arrival::Outage { above_min_ms: 1000 },
                "late packet {} slipped in as jitter once the window emptied",
                k
            );
            assert_eq!(e.target_ms(), 20, "an outage inflated the target");
        }

        // The following normal arrival must not find any of them in the window.
        e.observe(WINDOW_MS + 101, WINDOW_MS + 101, 0);
        assert_eq!(e.target_ms(), 20, "the outage contaminated the window");
    }

    /// The surviving reference must be the windowed *minimum*, not the newest
    /// sample. Taking the newest hides an outage: with samples at 0 and 20 ms, a
    /// packet 90 ms above the minimum measures only 70 above the newest, lands
    /// inside the threshold, and the next normal arrival drives the target to the
    /// cap.
    ///
    /// Three arrivals, not two: the reference is captured *before* the current
    /// sample joins the window, so with only two the last captured window holds a
    /// single sample and its minimum and newest coincide — which would leave this
    /// unable to tell the two rules apart.
    #[test]
    fn the_surviving_reference_is_the_minimum_not_the_newest() {
        let mut e = est();
        e.observe(1000, 1000, 0); // d = 0
        e.observe(1010, 990, 0); // d = 20
        e.observe(1020, 1000, 0); // captures a window whose min is 0 and newest 20
        assert_eq!(e.target_ms(), 40);

        let t = WINDOW_MS + 1021;
        assert_eq!(
            e.observe(t, t - 90, 0),
            Arrival::Outage { above_min_ms: 90 },
            "measured against the newest sample instead of the minimum"
        );
        e.observe(t + 10, t + 10, 0);
        assert!(e.target_ms() <= 40, "target rose to {}", e.target_ms());
    }

    /// The reference must not be immortal either. A sender restart brings a new
    /// clock offset; measured against the old one forever, every arrival would be
    /// an outage and the statistic would never adapt again.
    #[test]
    fn a_permanently_changed_offset_re_baselines_instead_of_never_recovering() {
        let mut e = est();
        e.observe(0, 0, 0);

        // +500 ms of offset, arriving steadily. The first is indistinguishable
        // from an outage, and so is the one after the window has emptied.
        let t0 = 1000;
        assert!(matches!(e.observe(t0, t0 - 500, 0), Arrival::Outage { .. }));
        let t1 = WINDOW_MS + 1000;
        assert!(matches!(e.observe(t1, t1 - 500, 0), Arrival::Outage { .. }));

        // Past the expiry it re-baselines, so the statistic works again.
        let t2 = REF_EXPIRY_MS + 1001;
        assert_eq!(
            e.observe(t2, t2 - 500, 0),
            Arrival::Jitter { above_min_ms: 0 },
            "the reference never expired, so every arrival stayed an outage"
        );
        assert_eq!(e.target_ms(), 20);
    }

    /// Time going backwards must also reset the shrink timestamp. One left in the
    /// future blocks every shrink until real time catches up with it, pinning the
    /// target at its peak for a minute.
    #[test]
    fn a_rollback_does_not_freeze_the_target_at_its_peak() {
        let mut e = est();
        e.observe(100_000, 100_000, 0);
        e.observe(100_060, 100_000, 0);
        assert_eq!(e.target_ms(), 80);

        e.observe(0, 0, 0);
        for k in 1..=7u64 {
            e.observe(k * SHRINK_INTERVAL_MS, k * SHRINK_INTERVAL_MS, 0);
        }
        assert!(e.target_ms() < 80, "target stuck at {}", e.target_ms());
    }

    /// The stale reference classifies, but must not enter the statistic: over a
    /// long arrival gap it has drifted, and a drifted sample setting `w` would
    /// inflate the target with no jitter having occurred.
    #[test]
    fn a_stale_reference_does_not_inflate_the_spread() {
        let mut e = est();
        e.observe(0, 0, 0);
        // 30 ms of accumulated drift after a long gap: within the threshold, so
        // it is recorded -- but it is then the only sample, so the spread is 0.
        let t = 2 * WINDOW_MS;
        assert_eq!(
            e.observe(t, t - 30, 0),
            Arrival::Jitter { above_min_ms: 30 }
        );
        assert_eq!(e.sample_count(), 1, "the stale sample was kept");
        assert_eq!(e.target_ms(), 20, "drift alone deepened the buffer");
    }

    /// The reference has to include the sample just accepted. Classifying against
    /// the pre-insertion minimum left it one sample stale, so a packet arriving late
    /// after the window emptied was measured against too high a value: with samples
    /// at 20 then 0, a 90 ms delay measured 70 and was filed as jitter.
    #[test]
    fn a_new_minimum_becomes_the_reference_immediately() {
        let mut e = est();
        e.observe(1000, 980, 0); // d = 20
        e.observe(1010, 1010, 0); // d = 0, the new minimum
        assert_eq!(e.target_ms(), 40);

        let t = WINDOW_MS + 1011;
        assert_eq!(
            e.observe(t, t - 90, 0),
            Arrival::Outage { above_min_ms: 90 },
            "measured against a reference one sample stale"
        );
        e.observe(t + 10, t + 10, 0);
        assert!(e.target_ms() <= 40, "target rose to {}", e.target_ms());
    }

    /// Nothing arrives half a second early, so a step *down* is a change of clock
    /// offset and not jitter. Keeping the samples taken on the old offset reported
    /// the step itself as the spread and pinned the target at the cap for a minute.
    #[test]
    fn a_downward_offset_step_does_not_manufacture_jitter() {
        let mut e = est();
        e.observe(1000, 1000, 0); // d = 0
        assert_eq!(e.target_ms(), 20);

        // The sender restarts with its clock 500 ms further ahead: d steps to -500.
        assert_eq!(
            e.observe(1010, 1510, 0),
            Arrival::Jitter { above_min_ms: 0 }
        );
        assert_eq!(e.sample_count(), 1, "the old offset's samples were kept");
        assert_eq!(
            e.target_ms(),
            20,
            "an offset step was read as 500 ms of jitter"
        );
    }

    /// The expiry clock must run from the last accepted sample. Winding it whenever
    /// a call merely found old samples still inside the window let an outage run
    /// extend the reference's life by a further window.
    #[test]
    fn an_outage_does_not_extend_the_references_life() {
        let mut e = est();
        e.observe(0, 0, 0);
        // An outage at the inclusive edge of the window: the sample is still there,
        // but this call must not count as fresh evidence.
        assert!(matches!(
            e.observe(WINDOW_MS, WINDOW_MS - 500, 0),
            Arrival::Outage { .. }
        ));
        assert_eq!(
            e.observe(REF_EXPIRY_MS + 1, REF_EXPIRY_MS + 1 - 500, 0),
            Arrival::Jitter { above_min_ms: 0 },
            "the reference outlived REF_EXPIRY_MS measured from its last sample"
        );
    }

    /// A proven id reset means a new sender process and so possibly a new clock
    /// offset, which re-baselines at once instead of waiting out the expiry while
    /// calling every arrival an outage.
    #[test]
    fn a_generation_reset_re_baselines_the_reference() {
        let mut e = est();
        e.observe(1000, 1000, 0);
        assert!(matches!(
            e.observe(1010, 510, 0),
            Arrival::Outage { above_min_ms: 500 }
        ));

        e.on_generation_reset();
        assert_eq!(
            e.observe(1020, 520, 0),
            Arrival::Jitter { above_min_ms: 0 },
            "a generation reset did not re-baseline"
        );
        assert_eq!(e.target_ms(), 20);
    }

    /// The trigger must be an id reset and not a transport reconnect. `TcpSource`
    /// reconnects after any blip while the sender's clock carries on, and the first
    /// packet after the stall is the most delayed one: with the reference thrown away
    /// it becomes its own baseline and is filed as ordinary jitter, which is the very
    /// defect this module exists to prevent.
    #[test]
    fn an_ordinary_stall_keeps_the_reference_that_measures_it() {
        let mut e = est();
        e.observe(1000, 950, 0); // d = 50
                                 // One second of stall, then the backlog arrives: same sender, same clock.
        assert_eq!(
            e.observe(2000, 950, 0),
            Arrival::Outage { above_min_ms: 1000 },
            "the stalled packet was admitted as jitter"
        );
        assert_eq!(e.target_ms(), 20);
    }

    /// The live window's minimum has to become the durable reference, or the value
    /// used after the window empties lags behind the drift the window tracked.
    #[test]
    fn the_live_window_minimum_becomes_the_durable_reference() {
        let mut e = est();
        // A rising staircase: each sample is 10 ms later than the last, so the
        // window's minimum climbs as the early samples age out.
        for k in 0..6u64 {
            e.observe(k * 10_000, k * 10_000 - k * 10, 0);
        }
        // By the last of those the window held delays 20..50, so the durable
        // reference is 20 rather than the original 0. A delay of 95 is then 75 above
        // it — jitter — where against a reference stuck at 0 it would be an outage.
        let t = 80_001;
        assert_eq!(
            e.observe(t, t - 95, 0),
            Arrival::Jitter { above_min_ms: 75 },
            "the durable reference lagged the window's minimum"
        );
    }

    /// Accepted samples are what wind the expiry clock. Without that, the reference
    /// expires a fixed interval after the *first* sample however long traffic has
    /// been flowing, and a later stall re-baselines on itself.
    #[test]
    fn accepted_samples_wind_the_expiry_clock() {
        let mut e = est();
        e.observe(0, 0, 0);
        e.observe(100_000, 100_000, 0);
        assert_eq!(
            e.observe(300_001, 300_001 - 500, 0),
            Arrival::Outage { above_min_ms: 500 },
            "the reference expired measured from the first sample"
        );
    }

    /// The clock-offset step is a step *beyond* the adaptive threshold. Exactly at it,
    /// the spec still calls the sample jitter, so the window is kept.
    #[test]
    fn a_step_exactly_at_the_threshold_is_still_jitter() {
        let mut e = est();
        e.observe(1000, 1000, 0); // d = 0
        e.observe(1010, 1090, 0); // d = -80: exactly the threshold
        assert_eq!(
            e.sample_count(),
            2,
            "a step at the boundary cleared the window"
        );
    }

    /// Monotonic time is a precondition; if it is violated the window must not
    /// silently stop evicting and grow without bound.
    #[test]
    fn time_going_backwards_discards_the_window_instead_of_wedging_it() {
        let mut e = est();
        e.observe(100_000, 100_000, 0);
        e.observe(0, 0, 0);
        assert_eq!(e.sample_count(), 1, "the future-dated sample was retained");
        e.observe(WINDOW_MS + 1, WINDOW_MS + 1, 0);
        assert_eq!(e.sample_count(), 1, "eviction stopped at a future sample");
    }

    /// Calling `d_max_adaptive_ms` a cap is only honest if the estimator cannot
    /// start above it. Config validation refuses such a pair; so does this.
    #[test]
    #[should_panic(expected = "below the two-packet floor")]
    fn a_ceiling_below_the_two_packet_floor_is_refused() {
        DepthEstimator::new(5, 10);
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
