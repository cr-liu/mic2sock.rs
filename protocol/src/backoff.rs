use std::time::Duration;

/// Exponential backoff with full jitter.
///
/// Uses a deterministic LCG rather than the `rand` crate: no new dependency for a
/// zero-dependency crate, and the sequence becomes testable.
pub struct Backoff {
    base_ms: u64,
    max_ms: u64,
    attempt: u32,
    rng: u64,
}

impl Backoff {
    /// * `base_ms` — the ceiling for the first attempt.
    /// * `max_ms` — a hard ceiling for any attempt.
    /// * `seed` — LCG seed; different connections should use different seeds so
    ///   they do not reconnect in lockstep.
    ///
    /// # Panics
    /// Panics if `base_ms > max_ms`, which is a configuration or programming
    /// error; this project's convention is to fail loudly on those rather than
    /// silently using `min(base_ms, max_ms)` as the real first-attempt ceiling.
    pub fn new(base_ms: u64, max_ms: u64, seed: u64) -> Self {
        assert!(base_ms <= max_ms, "base_ms must be <= max_ms");
        Backoff {
            base_ms,
            max_ms,
            attempt: 0,
            rng: seed.wrapping_mul(2) | 1,
        }
    }

    /// Call after a successful connection so the next disconnect starts from the
    /// shortest delay again.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Returns how long to wait this time, and advances the attempt counter.
    ///
    /// Full jitter: uniform over `[0, min(max_ms, base_ms << attempt)]`.
    pub fn next_delay(&mut self) -> Duration {
        // saturating_mul rather than checked_shl: the latter only validates the
        // shift amount, not the value, so `200u64.checked_shl(57)` silently wraps
        // to a tiny number and the backoff would collapse back to milliseconds
        // after a few dozen reconnects.
        let shift = self.attempt.min(63);
        let ceiling = self.base_ms.saturating_mul(1u64 << shift).min(self.max_ms);
        self.attempt = self.attempt.saturating_add(1);

        // LCG with Knuth's MMIX parameters; the high bits have the better
        // distribution.
        self.rng = self
            .rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let r = self.rng >> 33;

        Duration::from_millis(if ceiling == 0 {
            0
        } else {
            r % ceiling.saturating_add(1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "base_ms must be <= max_ms")]
    fn base_greater_than_max_is_a_configuration_error() {
        Backoff::new(5000, 200, 1);
    }

    #[test]
    fn first_delay_is_within_base() {
        let mut b = Backoff::new(200, 5000, 1);
        let d = b.next_delay();
        assert!(d <= Duration::from_millis(200), "got {:?}", d);
    }

    #[test]
    fn never_exceeds_max() {
        let mut b = Backoff::new(200, 5000, 42);
        for _ in 0..100 {
            assert!(b.next_delay() <= Duration::from_millis(5000));
        }
    }

    /// The ceiling should grow exponentially then saturate. Observed via the max
    /// over many seeds, because full jitter makes any single draw uninformative.
    #[test]
    fn ceiling_grows_then_saturates() {
        let sample_max = |attempts: u32| -> u64 {
            let mut best = 0;
            for seed in 0..200u64 {
                let mut b = Backoff::new(100, 1600, seed);
                for _ in 0..attempts {
                    b.next_delay();
                }
                best = best.max(b.next_delay().as_millis() as u64);
            }
            best
        };
        // attempt 0 has ceiling 100, attempt 3 has 800, attempt 6 is capped at 1600.
        assert!(sample_max(0) <= 100);
        assert!(sample_max(3) > 400 && sample_max(3) <= 800);
        assert!(sample_max(6) > 800 && sample_max(6) <= 1600);
    }

    #[test]
    fn reset_returns_to_first_attempt() {
        let mut b = Backoff::new(100, 10_000, 7);
        for _ in 0..10 {
            b.next_delay();
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(100));
    }

    #[test]
    fn same_seed_gives_same_sequence() {
        let seq = |seed| {
            let mut b = Backoff::new(100, 5000, seed);
            (0..5).map(|_| b.next_delay()).collect::<Vec<_>>()
        };
        assert_eq!(seq(9), seq(9));
        assert_ne!(seq(9), seq(10));
    }

    /// The point of full jitter is to spread reconnect instants out, so the
    /// sequence must not be constant.
    #[test]
    fn jitter_actually_varies() {
        let mut b = Backoff::new(1000, 1000, 3);
        let ds: Vec<_> = (0..10).map(|_| b.next_delay()).collect();
        assert!(ds.iter().any(|d| *d != ds[0]), "no jitter: {:?}", ds);
    }
}
