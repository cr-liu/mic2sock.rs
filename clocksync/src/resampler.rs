use crate::hermite::{interpolate, to_i16};
use std::collections::VecDeque;

/// Multi-channel resampler in which **all channels share one phase accumulator**.
pub struct Resampler {
    bufs: Vec<VecDeque<i16>>,
    /// The shared fractional phase, positioned between `bufs[*][1]` and
    /// `bufs[*][2]`, held as Q32 fixed point (32 fractional bits).
    ///
    /// This being shared is the one hard invariant of this design. Rounding per
    /// channel independently yields a systematic inter-channel offset of up to one
    /// sample; at 16 kHz that is 62.5 us, equivalent to 2.1 cm of apparent source
    /// displacement, which biases direction-of-arrival estimation.
    ///
    /// Fixed point rather than `f64` is what makes that guarantee *exact* instead
    /// of dependent on binade alignment. For **locally linear** input the cubic's
    /// `c2` and `c3` coefficients are exactly zero, so the Horner evaluation
    /// collapses to `y1 + d*x`; a dyadic phase keeps that sum exactly
    /// representable at any channel magnitude (15 sample bits + 32 fraction bits
    /// < 53), so no channel's final interpolation add can snap onto a rounding tie
    /// while another's does not. With an `f64` phase that snapping happens whenever
    /// the trajectory passes within half an ulp of .5, and delayed channels sit in
    /// different binades, so they snap differently and drift one LSB apart.
    ///
    /// For general, non-linear windows this exactness is not proven: `c2` and `c3`
    /// are generally nonzero, the intermediate Horner products can consume the
    /// full 53-bit mantissa, and the final add is `y1 + (arbitrary double)` rather
    /// than a bounded dyadic sum. Empirically the guarantee still holds — fuzzing
    /// 5,000,000 random non-linear 4-tap windows across the full Q32 phase range
    /// found zero non-saturating violations, since ulps at i16 magnitudes sit far
    /// below the 0.5 rounding-tie threshold — but that is an observation, not a
    /// proof, for the non-linear case.
    phase: u64,
}

/// Interpolation needs four taps (y0..y3), with the phase between y1 and y2.
const TAPS: usize = 4;

/// Fractional bits in the Q32 phase. 2^-32 is 2.3e-10 of a sample, orders of
/// magnitude finer than the ppm-scale ratios the controller asks for.
const FRAC_BITS: u32 = 32;
/// One whole sample of phase.
const ONE: u64 = 1 << FRAC_BITS;

/// Largest accepted `step`. The controller drives `step` within 0.975..=1.025;
/// this bound is far looser than that while still keeping the per-output pop
/// count small, so an out-of-domain value (a unit-confusion bug, or infinity
/// arriving through the saturating float-to-int cast) fails loudly instead of
/// spinning for billions of iterations.
///
/// Bounding `step` rather than clamping the pop count per channel is deliberate:
/// a per-channel clamp would let channels with less buffered data pop fewer
/// samples than their siblings, silently desynchronizing them.
const MAX_STEP: f64 = 16.0;

impl Resampler {
    pub fn new(n_ch: usize) -> Self {
        Resampler {
            bufs: (0..n_ch).map(|_| VecDeque::new()).collect(),
            phase: 0,
        }
    }

    pub fn n_ch(&self) -> usize {
        self.bufs.len()
    }

    /// Appends input samples for channel `ch`.
    ///
    /// Callers are expected to push all channels in lockstep. `pending()` gates
    /// production on the *minimum* buffer length across channels, so a channel
    /// whose producer stalls will stall output for every other channel too. The
    /// per-channel buffers are unbounded, so if one channel's producer stalls
    /// permanently while the others keep pushing, those healthy channels' buffers
    /// grow without limit — nothing pops them until the stalled channel catches
    /// up.
    ///
    /// # Panics
    /// Panics if `ch` is out of range.
    pub fn push(&mut self, ch: usize, samples: &[i16]) {
        self.bufs[ch].extend(samples.iter().copied());
    }

    /// Pending input samples per channel, taken as the minimum across channels.
    pub fn pending(&self) -> usize {
        self.bufs.iter().map(|b| b.len()).min().unwrap_or(0)
    }

    /// Produces at most `want` output samples, appending to `out[ch]`; returns how
    /// many were produced.
    ///
    /// `step` is **input samples consumed per output sample**: `step > 1` drains
    /// input faster than it produces output (catch-up mode, for working off a
    /// backlog), `step < 1` the reverse.
    ///
    /// # Panics
    /// Panics if `out.len() != n_ch`, if `step` is not finite or not in
    /// `(0, MAX_STEP]`, or if `step` is so small it rounds to zero in Q32 and so
    /// could never advance the phase.
    pub fn pull(&mut self, step: f64, want: usize, out: &mut [Vec<i16>]) -> usize {
        assert_eq!(
            out.len(),
            self.bufs.len(),
            "out must have one Vec per channel"
        );
        assert!(
            step.is_finite() && step > 0.0 && step <= MAX_STEP,
            "step must be finite and in (0, {}], got {}",
            MAX_STEP,
            step
        );
        // Converted once per call, never per sample, so the phase itself stays
        // dyadic and every channel sees a bit-identical x.
        let step_q = (step * ONE as f64).round() as u64;
        assert!(
            step_q > 0,
            "step {} rounds to zero in Q32; a step below 2^-32 cannot make progress",
            step
        );

        let mut produced = 0;
        while produced < want {
            if self.pending() < TAPS {
                break;
            }
            // The invariant: one phase value, applied to every channel this
            // iteration.
            let x = self.phase as f64 / ONE as f64;
            for (ch, buf) in self.bufs.iter().enumerate() {
                let y = interpolate(
                    buf[0] as f64,
                    buf[1] as f64,
                    buf[2] as f64,
                    buf[3] as f64,
                    x,
                );
                out[ch].push(to_i16(y));
            }

            self.phase += step_q;
            let advance = (self.phase >> FRAC_BITS) as usize;
            self.phase &= ONE - 1;
            if advance > 0 {
                for buf in self.bufs.iter_mut() {
                    for _ in 0..advance {
                        buf.pop_front();
                    }
                }
            }
            produced += 1;
        }
        produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushes `n` samples into each channel, channel `ch` delayed by `delays[ch]`
    /// samples. A linear ramp is used because the cubic kernel reproduces linear
    /// input exactly, so any inter-channel difference can only come from phase
    /// inconsistency.
    fn push_delayed_ramps(r: &mut Resampler, n: usize, delays: &[i64]) {
        for (ch, d) in delays.iter().enumerate() {
            let s: Vec<i16> = (0..n).map(|i| (i as i64 - d) as i16).collect();
            r.push(ch, &s);
        }
    }

    /// **The decisive test.** Two channels get the same ramp 16 samples apart; at
    /// every output sample the inter-channel difference must be exactly 16.
    ///
    /// Per-channel phase would let accumulated error push this off 16.
    #[test]
    fn shared_phase_preserves_inter_channel_delay_exactly() {
        for step in [1.0, 1.025, 0.975, 1.0001] {
            let mut r = Resampler::new(2);
            push_delayed_ramps(&mut r, 20_000, &[0, 16]);

            let mut out = vec![Vec::new(), Vec::new()];
            let produced = r.pull(step, 15_000, &mut out);
            assert!(produced > 10_000, "step={} produced={}", step, produced);
            assert_eq!(out[0].len(), out[1].len());

            for k in 0..out[0].len() {
                let diff = out[0][k] as i32 - out[1][k] as i32;
                assert_eq!(
                    diff, 16,
                    "step={} k={} ch0={} ch1={} - inter-channel delay drifted",
                    step, k, out[0][k], out[1][k]
                );
            }
        }
    }

    /// Also holds at the production channel count.
    #[test]
    fn holds_for_seventeen_channels() {
        let delays: Vec<i64> = (0..17).map(|c| c as i64).collect();
        let mut r = Resampler::new(17);
        push_delayed_ramps(&mut r, 8_000, &delays);

        let mut out: Vec<Vec<i16>> = (0..17).map(|_| Vec::new()).collect();
        let produced = r.pull(1.025, 6_000, &mut out);
        // Without this the test would pass vacuously on zero output.
        assert!(produced > 5_000, "produced={}", produced);

        for k in 0..out[0].len() {
            for ch in 1..17 {
                let diff = out[0][k] as i32 - out[ch][k] as i32;
                assert_eq!(diff, ch as i32, "k={} ch={}", k, ch);
            }
        }
    }

    /// step > 1 consumes input faster than it produces output (catch-up mode);
    /// step < 1 is the reverse.
    #[test]
    fn step_controls_consumption_rate() {
        let mut fast = Resampler::new(1);
        fast.push(0, &vec![0i16; 10_000]);
        let mut out = vec![Vec::new()];
        let n_fast = fast.pull(2.0, 100_000, &mut out);

        let mut slow = Resampler::new(1);
        slow.push(0, &vec![0i16; 10_000]);
        let mut out2 = vec![Vec::new()];
        let n_slow = slow.pull(0.5, 100_000, &mut out2);

        assert!(n_slow > n_fast, "slow={} fast={}", n_slow, n_fast);
        // step=2 yields about 5k output from 10k input; step=0.5 about 20k.
        assert!((4_000..6_000).contains(&n_fast), "n_fast={}", n_fast);
        assert!((18_000..21_000).contains(&n_slow), "n_slow={}", n_slow);
    }

    /// Running out of input must stop production rather than panic or emit junk.
    #[test]
    fn stops_when_input_exhausted() {
        let mut r = Resampler::new(1);
        r.push(0, &[1, 2, 3]); // fewer than the four taps needed
        let mut out = vec![Vec::new()];
        assert_eq!(r.pull(1.0, 100, &mut out), 0);
        assert!(out[0].is_empty());
    }

    /// Feeding in chunks must not change the result: phase has to persist across
    /// calls.
    #[test]
    fn incremental_push_matches_bulk_push() {
        let bulk = {
            let mut r = Resampler::new(1);
            let s: Vec<i16> = (0..4000).map(|i| i as i16).collect();
            r.push(0, &s);
            let mut out = vec![Vec::new()];
            r.pull(1.025, 3000, &mut out);
            out.remove(0)
        };
        let incremental = {
            let mut r = Resampler::new(1);
            let mut out = vec![Vec::new()];
            for chunk in 0..40 {
                let s: Vec<i16> = (chunk * 100..(chunk + 1) * 100).map(|i| i as i16).collect();
                r.push(0, &s);
                r.pull(1.025, 3000, &mut out);
            }
            out.remove(0)
        };
        let n = bulk.len().min(incremental.len());
        assert!(n > 2000, "n={}", n);
        assert_eq!(bulk[..n], incremental[..n]);
    }

    /// Guards against the interpolation kernel being wired up wrongly.
    #[test]
    fn sine_tracks_analytic_within_one_percent() {
        const PERIOD: f64 = 64.0;
        const AMP: f64 = 10_000.0;
        let step = 1.025;

        let mut r = Resampler::new(1);
        let s: Vec<i16> = (0..5000)
            .map(|i| (AMP * (2.0 * std::f64::consts::PI * i as f64 / PERIOD).sin()) as i16)
            .collect();
        r.push(0, &s);
        let mut out = vec![Vec::new()];
        r.pull(step, 4000, &mut out);

        // Output sample k corresponds to input position 1 + k*step, because the
        // first output sits at bufs[1].
        assert!(
            out[0].len() >= 3000,
            "produced only {} samples, comparison would be vacuous",
            out[0].len()
        );
        let mut worst = 0.0f64;
        for (k, got) in out[0].iter().enumerate().take(3000) {
            let pos = 1.0 + k as f64 * step;
            let want = AMP * (2.0 * std::f64::consts::PI * pos / PERIOD).sin();
            worst = worst.max((*got as f64 - want).abs());
        }
        assert!(
            worst < AMP * 0.01,
            "worst error {} exceeds 1% of {}",
            worst,
            AMP
        );
    }

    #[test]
    #[should_panic(expected = "step must be finite")]
    fn rejects_step_above_max() {
        let mut r = Resampler::new(1);
        r.push(0, &[0i16; 100]);
        let mut out = vec![Vec::new()];
        r.pull(5.0e9, 5, &mut out);
    }

    #[test]
    #[should_panic(expected = "step must be finite")]
    fn rejects_non_finite_step() {
        let mut r = Resampler::new(1);
        r.push(0, &[0i16; 100]);
        let mut out = vec![Vec::new()];
        r.pull(f64::INFINITY, 5, &mut out);
    }
}
