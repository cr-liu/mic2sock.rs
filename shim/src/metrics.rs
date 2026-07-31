/// Counters and an arrival-delay histogram, flushed periodically as JSONL.
///
/// The histogram of `d = arrival - header timestamp` is the whole point: its
/// p99.9 is the answer to "how low can this link actually go", and it is the
/// instrument for judging whether the later ALSA rewrite helped or hurt.
///
/// Time is injected rather than read from a clock so this is testable.
#[derive(Debug)]
pub struct Metrics {
    /// Histogram of `d - d_min` in milliseconds, one bucket per ms up to
    /// `BUCKETS - 1`, with the last bucket collecting everything beyond.
    buckets: Vec<u64>,
    pub arrivals: u64,
    pub conceal_events: u64,
    pub conceal_samples: u64,
    pub outage_events: u64,
    pub resync_events: u64,
    pub late_discards: u64,
    pub duplicate_discards: u64,
    /// Should stay 0. Non-zero means the backlog exceeded `catchup_max` and the
    /// lossless promise has begun to degrade.
    pub catchup_overflow: u64,
    /// Should stay 0. Non-zero means the consumer stopped reading.
    pub max_depth_hit: u64,
    last_flush_ms: u64,
}

pub const BUCKETS: usize = 512;

/// Spelled out rather than derived: a derived `Default` leaves `buckets` empty,
/// and the first `record_delay` on such a value panics. `new()` is now the same
/// constructor, so the two cannot drift apart.
impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            buckets: vec![0; BUCKETS],
            arrivals: 0,
            conceal_events: 0,
            conceal_samples: 0,
            outage_events: 0,
            resync_events: 0,
            late_discards: 0,
            duplicate_discards: 0,
            catchup_overflow: 0,
            max_depth_hit: 0,
            last_flush_ms: 0,
        }
    }
}

impl Metrics {
    pub fn new() -> Self {
        Metrics::default()
    }

    /// Records one arrival whose delay above the running minimum was `ms`.
    pub fn record_delay(&mut self, ms: u64) {
        self.arrivals += 1;
        // Clamp before narrowing: `ms as usize` truncates on a 32-bit target, so
        // `1 << 32` became 0 and an out-of-range delay was recorded as 0 ms —
        // the opposite end of the histogram from where it belongs.
        let i = ms.min(BUCKETS as u64 - 1) as usize;
        self.buckets[i] += 1;
    }

    pub fn bucket(&self, i: usize) -> u64 {
        self.buckets[i]
    }

    /// The smallest millisecond bound covering `pct` percent of arrivals. An
    /// empty histogram reports 0, which is indistinguishable from a real 0 ms
    /// measurement — read it together with `arrivals`.
    ///
    /// # Panics
    ///
    /// If `pct` is outside `(0, 100]`. Outside that range the answer is not a
    /// percentile: p0 returned bucket 0 whether or not anything was measured
    /// there, and p101 returned the last bucket.
    pub fn percentile_ms(&self, pct: f64) -> u64 {
        assert!(
            pct > 0.0 && pct <= 100.0,
            "percentile must be in (0, 100], got {}",
            pct
        );
        let total: u64 = self.buckets.iter().sum();
        if total == 0 {
            return 0;
        }
        // ceil, so p100 needs the whole population rather than total-epsilon.
        // At least one sample must be covered, or the first bucket wins by
        // default however empty it is.
        let want = (((total as f64) * pct / 100.0).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= want {
                return i as u64;
            }
        }
        (BUCKETS - 1) as u64
    }

    pub fn set_last_flush(&mut self, now_ms: u64) {
        self.last_flush_ms = now_ms;
    }

    pub fn flush_due(&self, now_ms: u64, interval_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_flush_ms) >= interval_ms
    }

    /// Serialises one JSONL record. Hand-rolled rather than pulling in
    /// serde_json: the shape is fixed and tiny.
    ///
    /// A non-finite `step` is emitted as `null`. JSON has no NaN or Infinity
    /// literal, so `{:.9}` on one produced a line — the whole point of which is
    /// to be machine-readable — that no parser would accept.
    pub fn to_json_line(&self, now_ms: u64, d_target_ms: u64, step: f64) -> String {
        let step = if step.is_finite() {
            format!("{:.9}", step)
        } else {
            "null".to_string()
        };
        format!(
            concat!(
                "{{\"t_ms\":{},\"arrivals\":{},\"conceal_events\":{},",
                "\"conceal_samples\":{},\"outage_events\":{},\"resync_events\":{},",
                "\"late_discards\":{},\"duplicate_discards\":{},",
                "\"catchup_overflow\":{},\"max_depth_hit\":{},",
                "\"d_target_ms\":{},\"step\":{},",
                "\"p50_ms\":{},\"p99_ms\":{},\"p99_9_ms\":{}}}"
            ),
            now_ms,
            self.arrivals,
            self.conceal_events,
            self.conceal_samples,
            self.outage_events,
            self.resync_events,
            self.late_discards,
            self.duplicate_discards,
            self.catchup_overflow,
            self.max_depth_hit,
            d_target_ms,
            step,
            self.percentile_ms(50.0),
            self.percentile_ms(99.0),
            self.percentile_ms(99.9),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal validator for this one flat shape, because there is no
    /// serde_json here and adding it would move the lockfile. It exists because
    /// tests that merely checked the braces and a few substrings passed a
    /// serializer emitting `{"a":1,}` and one emitting `"step":NaN` — neither of
    /// which any parser accepts, which defeats the point of the format.
    fn parse_flat_json(line: &str) -> Vec<(String, String)> {
        let body = line
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or_else(|| panic!("not a JSON object: {}", line));
        body.split(',')
            .map(|field| {
                let (key, value) = field
                    .split_once(':')
                    .unwrap_or_else(|| panic!("not a key:value pair: {:?}", field));
                let key = key
                    .strip_prefix('"')
                    .and_then(|k| k.strip_suffix('"'))
                    .unwrap_or_else(|| panic!("unquoted key: {:?}", key));
                let numeric = !value.is_empty()
                    && value
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '-' || c == '.');
                assert!(
                    value == "null" || numeric,
                    "not a JSON number or null: {:?}",
                    value
                );
                (key.to_string(), value.to_string())
            })
            .collect()
    }

    fn field(line: &str, key: &str) -> String {
        parse_flat_json(line)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("missing {} in {}", key, line))
    }

    #[test]
    fn records_into_per_millisecond_buckets() {
        let mut m = Metrics::new();
        m.record_delay(0);
        m.record_delay(0);
        m.record_delay(7);
        assert_eq!(m.arrivals, 3);
        assert_eq!(m.bucket(0), 2);
        assert_eq!(m.bucket(7), 1);
        assert_eq!(m.bucket(1), 0);
    }

    #[test]
    fn delays_beyond_range_land_in_the_last_bucket() {
        let mut m = Metrics::new();
        m.record_delay(100_000);
        assert_eq!(m.bucket(BUCKETS - 1), 1);
    }

    /// The percentile is the headline number, so it must be right at the edges.
    #[test]
    fn percentile_of_a_known_distribution() {
        let mut m = Metrics::new();
        for _ in 0..990 {
            m.record_delay(5);
        }
        for _ in 0..10 {
            m.record_delay(200);
        }
        assert_eq!(m.percentile_ms(50.0), 5);
        assert_eq!(m.percentile_ms(99.0), 5);
        assert_eq!(m.percentile_ms(99.9), 200);
        assert_eq!(m.percentile_ms(100.0), 200);
    }

    #[test]
    fn percentile_of_empty_histogram_is_zero() {
        assert_eq!(Metrics::new().percentile_ms(99.9), 0);
    }

    /// A single sample must not be reported as 0 ms. `want` used to round down to
    /// zero, and the first bucket then satisfied `seen >= 0` however empty it was.
    #[test]
    fn a_lone_sample_is_its_own_percentile() {
        let mut m = Metrics::new();
        m.record_delay(5);
        assert_eq!(m.percentile_ms(1.0), 5);
        assert_eq!(m.percentile_ms(50.0), 5);
        assert_eq!(m.percentile_ms(100.0), 5);
    }

    /// The guard has to cover the whole excluded range, not just above 100: p0 was
    /// half of the original finding, and it returned bucket 0 whether or not
    /// anything had been measured there.
    #[test]
    fn every_percentile_outside_the_range_is_refused() {
        let m = Metrics::new();
        for bad in [0.0, -1.0, f64::NAN, 100.1, f64::INFINITY] {
            let refused = std::panic::catch_unwind(|| m.percentile_ms(bad)).is_err();
            assert!(refused, "percentile_ms({}) was accepted", bad);
        }
    }

    /// The derived Default left `buckets` empty, so the public constructor built
    /// an object that panicked on its first use. Every test used `new()`, which
    /// hid it.
    #[test]
    fn the_default_value_is_usable() {
        let mut m = Metrics::default();
        m.record_delay(0);
        assert_eq!(m.arrivals, 1);
    }

    /// JSON has no NaN or Infinity literal. `{:.9}` on one produced a line that
    /// no parser accepts, which defeats the only purpose of the format — so this
    /// parses the result rather than looking for substrings in it.
    #[test]
    fn a_non_finite_step_is_emitted_as_null() {
        let m = Metrics::new();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let line = m.to_json_line(1, 80, bad);
            assert_eq!(field(&line, "step"), "null", "{}", line);
        }
        // A finite step must still be a number, not null.
        let line = m.to_json_line(1, 80, 1.0005);
        assert_eq!(field(&line, "step"), "1.000500000");
    }

    #[test]
    fn flush_is_due_only_after_the_interval() {
        let mut m = Metrics::new();
        m.set_last_flush(1_000);
        assert!(!m.flush_due(1_000 + 59_999, 60_000));
        assert!(m.flush_due(1_000 + 60_000, 60_000));
    }

    /// The JSONL line must be machine-readable and carry the alarm counters, so
    /// a non-zero value is visible without reading prose logs.
    #[test]
    fn json_line_contains_the_alarm_counters_and_percentiles() {
        let mut m = Metrics::new();
        m.record_delay(3);
        m.catchup_overflow = 2;
        m.max_depth_hit = 1;
        let line = m.to_json_line(12_345, 80, 1.0005);
        let fields = parse_flat_json(&line);
        assert_eq!(fields.len(), 15, "field count changed: {}", line);
        for (key, want) in [
            ("t_ms", "12345"),
            ("arrivals", "1"),
            ("catchup_overflow", "2"),
            ("max_depth_hit", "1"),
            ("d_target_ms", "80"),
            ("p99_9_ms", "3"),
        ] {
            assert_eq!(field(&line, key), want, "{} in {}", key, line);
        }
    }
}
