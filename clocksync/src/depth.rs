/// Proportional controller with a clamp and a rate-of-change limit, producing the
/// `step` value for `Resampler::pull`.
///
/// Two timescales (see the crate docs):
/// * steady state — ppm-level corrections cancelling crystal drift, always on
/// * catch-up — up to 2.5%, working off backlog after an outage
///
/// **The slew limit is what buys inaudibility.** A static 2.5% offset is nearly
/// imperceptible on speech with nothing to compare against; the *glide* into it is
/// what gets heard. An unlimited proportional controller slams `step` to the clamp
/// the moment error appears, which is precisely the most audible artifact. Rate
/// limiting means a larger clamp is *less* audible, not more.
///
/// **The steady-state correction is proportional-only, so it does not cancel
/// drift exactly.** Buffer depth integrates `(drift - (step - 1))`, so holding
/// depth constant requires `step = 1 + drift` exactly — but a pure proportional
/// law only ever outputs `1 + kp * error_secs`, so sustaining that output takes a
/// permanent nonzero `error_secs`. The residual steady-state offset works out to
/// approximately `drift / kp`: at `kp = 1.0` and typical crystal drift of ~50 ppm,
/// that settles to a persistent depth error of roughly 50 us. This is a
/// deliberate trade, not an oversight — an integral term would drive the
/// residual to zero, but at the cost of windup risk (the accumulated integral
/// term overshooting) during outages, when depth error can spike and stay
/// nonzero for a while. ~50 us is negligible against the 80 ms target depth, so
/// the simpler proportional-only controller is used instead.
pub struct DepthController {
    kp: f64,
    clamp: f64,
    slew_per_sec: f64,
    step: f64,
}

impl DepthController {
    /// * `kp` — proportional gain, in step per second-of-error.
    /// * `clamp` — the largest deviation of `step` from 1.0 (production: 0.025).
    /// * `slew_per_sec` — the largest change in `step` per second (production:
    ///   0.002).
    ///
    /// # Panics
    /// Panics if `clamp` is not in `(0, 1)` or `slew_per_sec` is not positive.
    pub fn new(kp: f64, clamp: f64, slew_per_sec: f64) -> Self {
        assert!(clamp > 0.0 && clamp < 1.0, "clamp must be in (0, 1)");
        assert!(slew_per_sec > 0.0, "slew_per_sec must be positive");
        DepthController {
            kp,
            clamp,
            slew_per_sec,
            step: 1.0,
        }
    }

    /// The current `step`.
    pub fn step(&self) -> f64 {
        self.step
    }

    /// * `error_secs` — `measured_depth - target_depth`, in seconds. Positive
    ///   means too much backlog, which needs `step > 1` to consume input faster.
    /// * `dt_secs` — elapsed time since the last call.
    ///
    /// Returns the new `step`, ready to hand to `Resampler::pull`.
    pub fn update(&mut self, error_secs: f64, dt_secs: f64) -> f64 {
        let target = 1.0 + (self.kp * error_secs).clamp(-self.clamp, self.clamp);
        let max_delta = self.slew_per_sec * dt_secs;
        let delta = (target - self.step).clamp(-max_delta, max_delta);
        self.step += delta;
        self.step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production parameters.
    fn controller() -> DepthController {
        DepthController::new(1.0, 0.025, 0.002)
    }

    #[test]
    fn zero_error_holds_unity() {
        let mut c = controller();
        for _ in 0..100 {
            assert_eq!(c.update(0.0, 0.1), 1.0);
        }
    }

    #[test]
    fn never_exceeds_clamp() {
        let mut c = controller();
        for _ in 0..10_000 {
            let s = c.update(100.0, 0.1);
            assert!(s <= 1.025 + 1e-12, "step {} exceeded clamp", s);
        }
    }

    #[test]
    fn never_goes_below_negative_clamp() {
        let mut c = controller();
        for _ in 0..10_000 {
            let s = c.update(-100.0, 0.1);
            assert!(s >= 0.975 - 1e-12, "step {} below clamp", s);
        }
    }

    /// clamp / slew = 0.025 / 0.002 = 12.5 seconds. This pins the slew
    /// requirement, so changing the parameter has to be a deliberate act.
    #[test]
    fn takes_twelve_point_five_seconds_to_reach_clamp() {
        let mut c = controller();
        let dt = 0.01;
        let mut elapsed = 0.0;
        loop {
            elapsed += dt;
            if c.update(100.0, dt) >= 1.025 - 1e-9 {
                break;
            }
            assert!(elapsed < 20.0, "never reached clamp");
        }
        assert!(
            (elapsed - 12.5).abs() < 1e-6,
            "reached clamp in {} s, expected 12.5",
            elapsed
        );
    }

    /// No single update may move `step` by more than slew * dt.
    #[test]
    fn respects_slew_rate_per_step() {
        let mut c = controller();
        let dt = 0.5;
        let mut prev = 1.0;
        for _ in 0..40 {
            let s = c.update(100.0, dt);
            assert!(
                (s - prev).abs() <= 0.002 * dt + 1e-12,
                "jumped from {} to {} in {} s",
                prev,
                s,
                dt
            );
            prev = s;
        }
    }

    /// Once the error clears, `step` must glide back to 1.0 rather than stick.
    #[test]
    fn returns_to_unity_after_error_clears() {
        let mut c = controller();
        for _ in 0..2000 {
            c.update(100.0, 0.01);
        }
        assert!(c.step() > 1.02);
        for _ in 0..2000 {
            c.update(0.0, 0.01);
        }
        assert!((c.step() - 1.0).abs() < 1e-9, "step stuck at {}", c.step());
    }

    /// A small error should produce a ppm-scale correction, well inside the clamp.
    #[test]
    fn small_error_gives_small_correction() {
        let mut c = DepthController::new(0.01, 0.025, 1.0);
        let s = c.update(0.05, 1.0);
        assert!((s - 1.0005).abs() < 1e-9, "got {}", s);
    }
}
