/// Four-point Catmull-Rom / Hermite cubic interpolation.
///
/// `x` in `[0, 1)` is the fractional position between `y1` and `y2`; `y0` and
/// `y3` are the outer neighbours.
///
/// This is a fractional-delay filter, so its magnitude response varies with `x`.
/// That is harmless in the multi-channel case only because `Resampler` evaluates
/// every channel at the **same** `x`, making the variation identical across
/// channels and therefore invisible to inter-channel relationships.
#[inline]
pub fn interpolate(y0: f64, y1: f64, y2: f64, y3: f64, x: f64) -> f64 {
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * x + c2) * x + c1) * x + c0
}

/// Rounds to nearest and saturates into `i16`.
#[inline]
pub fn to_i16(v: f64) -> i16 {
    let r = v.round();
    if r > i16::MAX as f64 {
        i16::MAX
    } else if r < i16::MIN as f64 {
        i16::MIN
    } else {
        r as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The endpoints must reproduce the input samples exactly, or the definition
    /// of phase is wrong.
    #[test]
    fn reproduces_endpoints() {
        assert!((interpolate(1.0, 2.0, 3.0, 4.0, 0.0) - 2.0).abs() < 1e-12);
        assert!((interpolate(1.0, 2.0, 3.0, 4.0, 1.0) - 3.0).abs() < 1e-12);
    }

    /// A cubic kernel must reproduce a linear signal exactly. The resampler's
    /// decisive inter-channel-delay test depends on this property.
    #[test]
    fn reproduces_linear_signal_exactly() {
        for i in 0..=100 {
            let x = i as f64 / 100.0;
            let got = interpolate(10.0, 20.0, 30.0, 40.0, x);
            let want = 20.0 + 10.0 * x;
            assert!(
                (got - want).abs() < 1e-9,
                "x={} got={} want={}",
                x,
                got,
                want
            );
        }
    }

    /// A constant signal must stay constant, i.e. no ringing.
    #[test]
    fn constant_stays_constant() {
        for i in 0..=10 {
            let x = i as f64 / 10.0;
            assert!((interpolate(5.0, 5.0, 5.0, 5.0, x) - 5.0).abs() < 1e-12);
        }
    }

    /// For monotonic input the result must stay between the two inner samples.
    #[test]
    fn stays_between_inner_samples_for_monotonic_input() {
        for i in 0..=10 {
            let x = i as f64 / 10.0;
            let y = interpolate(0.0, 10.0, 20.0, 30.0, x);
            assert!((10.0..=20.0).contains(&y), "x={} y={}", x, y);
        }
    }

    #[test]
    fn clamps_to_i16_range() {
        assert_eq!(to_i16(40000.0), i16::MAX);
        assert_eq!(to_i16(-40000.0), i16::MIN);
        assert_eq!(to_i16(1.4), 1);
        assert_eq!(to_i16(1.6), 2);
        assert_eq!(to_i16(-1.6), -2);
    }
}
