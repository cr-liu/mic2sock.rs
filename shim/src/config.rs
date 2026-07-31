use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Runtime configuration, read from `shim.toml`.
///
/// Unlike `mic2sock`, a missing or malformed file is a **hard error**. That
/// daemon silently writes defaults to a differently-named file and keeps
/// running, which makes a misconfiguration look like a mysterious runtime bug.
///
/// `deny_unknown_fields` is part of that promise: a misspelled key that is
/// silently ignored leaves the operator believing a setting took effect, which
/// is the same class of confusion as the defaults-file behaviour.
///
/// The invariants the rest of the program relies on — geometry bounds, whole-
/// millisecond packets, an ordered depth chain — hold only for a value that came
/// out of [`parse`] or [`load`]. Constructing one field by field skips them.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the Pi's sender listens.
    pub source_host: String,
    pub source_port: u16,
    /// Port the black box connects to. It must be pointed at 127.0.0.1:this.
    #[serde(default = "default_sink_port")]
    pub sink_port: u16,

    /// Packet geometry. Must match what the black box expects; the packet length
    /// is derived, never hardcoded.
    #[serde(default = "default_n_ch")]
    pub n_ch: usize,
    #[serde(default = "default_spp_out")]
    pub spp_out: usize,
    #[serde(default = "default_header_len")]
    pub header_len: usize,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: usize,

    /// Jitter-versus-outage classification threshold. Arrival delays beyond this
    /// are outages and are excluded from the depth statistic, so that one long
    /// stall cannot inflate the target depth to its own length.
    #[serde(default = "default_d_max_adaptive_ms")]
    pub d_max_adaptive_ms: u64,
    /// How long a backlog may be absorbed by time compression instead of being
    /// discarded. **Must equal the Pi's `tcp_sender.backlog_ms`** or content is
    /// simply discarded at whichever end has the smaller value.
    #[serde(default = "default_catchup_max_ms")]
    pub catchup_max_ms: u64,
    #[serde(default = "default_catchup_clamp")]
    pub catchup_clamp: f64,
    #[serde(default = "default_catchup_slew_per_sec")]
    pub catchup_slew_per_sec: f64,
    /// Safety valve: if the consumer stops reading, discard rather than grow
    /// without bound. Should never fire in normal operation.
    ///
    /// Must sit **above** `d_max_adaptive_ms + catchup_max_ms`, the depth the
    /// resync anchor deliberately retains (spec §6.4). Spec §6.6's 500 ms
    /// predates the 3 s catchup decision (§4.2) and would have made the valve
    /// fire on every outage, discarding exactly the backlog catchup exists to
    /// absorb.
    #[serde(default = "default_max_depth_ms")]
    pub max_depth_ms: u64,
    /// No arrivals for this long means an outage has begun.
    #[serde(default = "default_outage_threshold_ms")]
    pub outage_threshold_ms: u64,
    /// Keep the source connection open while no consumer is attached, so the
    /// arrival-delay statistic stays warm instead of guessing for 30 s after the
    /// consumer appears.
    #[serde(default = "default_keep_source_when_idle")]
    pub keep_source_when_idle: bool,
    /// Where to append JSONL metrics. `None` disables metrics output.
    #[serde(default)]
    pub metrics_path: Option<PathBuf>,
}

fn default_sink_port() -> u16 {
    7998
}
fn default_n_ch() -> usize {
    17
}
fn default_spp_out() -> usize {
    160
}
fn default_header_len() -> usize {
    12
}
fn default_sample_rate() -> usize {
    16000
}
fn default_d_max_adaptive_ms() -> u64 {
    80
}
fn default_catchup_max_ms() -> u64 {
    3000
}
fn default_catchup_clamp() -> f64 {
    0.025
}
fn default_catchup_slew_per_sec() -> f64 {
    0.002
}
fn default_max_depth_ms() -> u64 {
    // d_max_adaptive (80) + catchup_max (3000), plus slack. See the field doc:
    // anything below the retained depth turns the valve into a guillotine.
    3500
}
fn default_outage_threshold_ms() -> u64 {
    200
}
fn default_keep_source_when_idle() -> bool {
    true
}

use protocol::{PacketLayout, HEADER_LEN};

/// Bounds on the packet geometry. Their job is not taste but arithmetic: they
/// are what makes `packet_len()` provably unable to overflow, and what turns a
/// fat-fingered `n_ch` into a refusal at load rather than a wrapped frame size
/// (`n_ch = 57646075230342349` used to parse, then wrap to a 76-byte packet).
const MAX_N_CH: usize = 64;
const MAX_SPP_OUT: usize = 4096;
const MIN_SAMPLE_RATE: usize = 8_000;
const MAX_SAMPLE_RATE: usize = 192_000;
/// Bound on `catchup_max_ms`. Also what makes `catchup_max_packets()` provably
/// fit a 32-bit `usize`, which a 32-bit Windows build would have.
const MAX_CATCHUP_MS: u64 = 60_000;

/// Parses and validates a config from TOML text.
pub fn parse(text: &str) -> Result<Config, String> {
    let c: Config = toml::from_str(text).map_err(|e| format!("invalid shim.toml: {}", e))?;
    c.validate()?;
    Ok(c)
}

/// Loads `shim.toml`, defaulting to the directory containing the executable
/// rather than the current directory. On Windows the exe is routinely invoked
/// from an unrelated CWD, so a CWD-relative default would fail confusingly.
pub fn load(explicit: Option<&Path>) -> Result<Config, String> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => exe_dir()?.join("shim.toml"),
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    parse(&text)
}

fn exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {}", e))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "executable has no parent directory".to_string())
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if self.source_host.trim().is_empty() {
            return Err("source_host must not be empty".into());
        }
        if self.source_port == 0 || self.sink_port == 0 {
            return Err("source_port and sink_port must be > 0".into());
        }

        // Geometry. A wrong value here does not fail loudly at the far end: the
        // consumer parses by fixed byte offset, so it silently mis-reads every
        // field of every packet for as long as the shim runs.
        if self.header_len != HEADER_LEN {
            return Err(format!(
                "header_len must be {} (got {}): protocol::Header encodes at fixed offsets and \
                 the consumer decodes the same way, so any other value shifts every field",
                HEADER_LEN, self.header_len
            ));
        }
        if !(1..=MAX_N_CH).contains(&self.n_ch) {
            return Err(format!(
                "n_ch must be in 1..={} (got {})",
                MAX_N_CH, self.n_ch
            ));
        }
        if !(1..=MAX_SPP_OUT).contains(&self.spp_out) {
            return Err(format!(
                "spp_out must be in 1..={} (got {})",
                MAX_SPP_OUT, self.spp_out
            ));
        }
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&self.sample_rate) {
            return Err(format!(
                "sample_rate must be in {}..={} (got {})",
                MIN_SAMPLE_RATE, MAX_SAMPLE_RATE, self.sample_rate
            ));
        }
        // Every depth in this program is carried in whole milliseconds, so a
        // packet that is not a whole number of ms makes every ms<->packet
        // conversion lossy in a way that accumulates. This also subsumes the
        // "rounds to 0 ms" case: a sub-millisecond packet cannot be exact.
        if self.spp_out * 1000 % self.sample_rate != 0 {
            return Err(format!(
                "spp_out {} at {} Hz is {:.4} ms; one packet must be a whole number of \
                 milliseconds, because every depth here is carried in ms",
                self.spp_out,
                self.sample_rate,
                self.spp_out as f64 * 1000.0 / self.sample_rate as f64
            ));
        }

        // Depth chain: target <= retained <= safety valve. Out of order, the
        // stage below discards what the stage above is waiting for.
        let packet_ms = self.packet_ms();
        if self.d_max_adaptive_ms < 2 * packet_ms {
            return Err(format!(
                "d_max_adaptive_ms ({}) must be at least two packets ({} ms): two packets is the \
                 structural floor of the buffer, so a smaller value would not be a cap at all",
                self.d_max_adaptive_ms,
                2 * packet_ms
            ));
        }
        if !(packet_ms..=MAX_CATCHUP_MS).contains(&self.catchup_max_ms) {
            return Err(format!(
                "catchup_max_ms must be in {}..={} (got {})",
                packet_ms, MAX_CATCHUP_MS, self.catchup_max_ms
            ));
        }
        if self.outage_threshold_ms < packet_ms {
            return Err(format!(
                "outage_threshold_ms ({}) must be at least one packet ({} ms)",
                self.outage_threshold_ms, packet_ms
            ));
        }
        let retained_ms = self.d_max_adaptive_ms + self.catchup_max_ms;
        if self.max_depth_ms < retained_ms {
            return Err(format!(
                "max_depth_ms ({}) must be at least d_max_adaptive_ms + catchup_max_ms ({} ms), \
                 or the safety valve discards the backlog catchup exists to absorb",
                self.max_depth_ms, retained_ms
            ));
        }

        // Spelled out rather than using a range: `(0.0..1.0)` contains 0.0, so a
        // range test needs a second, non-obvious check to exclude it.
        if !self.catchup_clamp.is_finite() || self.catchup_clamp <= 0.0 || self.catchup_clamp >= 1.0
        {
            return Err("catchup_clamp must be finite and in (0, 1)".into());
        }
        // `nan <= 0.0` is false, so without the finiteness test NaN passed here
        // and panicked later inside DepthController; infinity passed and removed
        // the slew limit altogether.
        if !self.catchup_slew_per_sec.is_finite() || self.catchup_slew_per_sec <= 0.0 {
            return Err("catchup_slew_per_sec must be finite and > 0".into());
        }
        if self.catchup_slew_per_sec > self.catchup_clamp {
            return Err(format!(
                "catchup_slew_per_sec ({}) must not exceed catchup_clamp ({}): a slew rate above \
                 the clamp traverses the whole range within a second, which is the audible step \
                 the slew limit exists to prevent",
                self.catchup_slew_per_sec, self.catchup_clamp
            ));
        }
        if !self.keep_source_when_idle {
            // `TcpSource` reconnects unconditionally, so only the default is
            // implemented. Reject the other value rather than ignore it: a
            // setting that is silently disregarded is worse than one that is
            // refused, because the operator believes it took effect.
            return Err("keep_source_when_idle = false is not implemented".into());
        }
        Ok(())
    }

    /// Geometry of the packets handed to the consumer.
    pub fn layout(&self) -> PacketLayout {
        PacketLayout::new(self.n_ch, self.spp_out, self.header_len)
    }

    /// Wall duration of one output packet, in milliseconds. Exact, because
    /// `validate` rejects a geometry whose packet is not a whole number of ms.
    pub fn packet_ms(&self) -> u64 {
        (self.spp_out * 1000 / self.sample_rate) as u64
    }

    /// `catchup_max_ms` expressed in packets.
    pub fn catchup_max_packets(&self) -> usize {
        // Bounded by MAX_CATCHUP_MS, so the conversion cannot narrow even on a
        // 32-bit target; `packet_ms` is non-zero for the same reason as above.
        (self.catchup_max_ms / self.packet_ms()) as usize
    }

    /// Depth the resync anchor is allowed to retain, in packets: the adaptive
    /// target plus the catchup budget (spec §6.4). This is the jitter buffer's
    /// `retain_cap`, and by validation it never exceeds `max_depth_ms`.
    pub fn retain_cap_packets(&self) -> usize {
        ((self.d_max_adaptive_ms + self.catchup_max_ms) / self.packet_ms()) as usize
    }

    /// Safety-valve depth in packets.
    pub fn max_depth_packets(&self) -> usize {
        (self.max_depth_ms / self.packet_ms()) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
source_host = "192.168.1.50"
source_port = 7998
"#;

    #[test]
    fn minimal_config_fills_production_defaults() {
        let c: Config = parse(MINIMAL).unwrap();
        assert_eq!(c.source_host, "192.168.1.50");
        assert_eq!(c.source_port, 7998);
        assert_eq!(c.sink_port, 7998);
        assert_eq!(c.n_ch, 17);
        assert_eq!(c.spp_out, 160);
        assert_eq!(c.header_len, 12);
        assert_eq!(c.d_max_adaptive_ms, 80);
        assert_eq!(c.catchup_max_ms, 3000);
        assert_eq!(c.catchup_clamp, 0.025);
        assert_eq!(c.catchup_slew_per_sec, 0.002);
        assert_eq!(c.max_depth_ms, 3500);
        assert_eq!(c.outage_threshold_ms, 200);
        assert!(c.keep_source_when_idle);
        assert_eq!(c.metrics_path, None);
    }

    fn with(extra: &str) -> Result<Config, String> {
        parse(&format!("{}{}\n", MINIMAL, extra))
    }

    /// The production geometry, derived rather than hardcoded. 5452 is what the
    /// closed-source consumer expects.
    #[test]
    fn derives_the_production_packet_length() {
        let c: Config = parse(MINIMAL).unwrap();
        assert_eq!(c.layout().packet_len(), 5452);
    }

    #[test]
    fn packet_duration_is_ten_milliseconds_at_production_settings() {
        let c: Config = parse(MINIMAL).unwrap();
        assert_eq!(c.packet_ms(), 10);
    }

    #[test]
    fn missing_required_field_is_an_error() {
        assert!(parse("source_port = 7998\n").is_err());
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse("this is not toml {{{").is_err());
    }

    /// A zero channel count or spp would make packet_len meaningless and divide
    /// by zero downstream, so it is rejected at load rather than at 3am.
    #[test]
    fn zero_geometry_is_rejected() {
        assert!(with("n_ch = 0").is_err(), "n_ch = 0 must be rejected");
        assert!(with("spp_out = 0").is_err(), "spp_out = 0 must be rejected");
        assert!(with("sample_rate = 0").is_err());
        assert!(with("sink_port = 0").is_err());
        assert!(parse("source_host = \"h\"\nsource_port = 0\n").is_err());
        assert!(parse("source_host = \"\"\nsource_port = 1\n").is_err());
    }

    /// The geometry is what the closed-source consumer parses by fixed byte
    /// offset. `header_len = 13` produced 5453-byte packets that still parsed
    /// here and mis-framed *every* field at the far end, forever. It is not a
    /// free parameter: `protocol::Header` writes at hardcoded offsets.
    #[test]
    fn a_header_len_other_than_twelve_is_rejected() {
        let e = with("header_len = 13").unwrap_err();
        assert!(e.contains("header_len must be 12"), "{}", e);
        assert!(with("header_len = 16").is_err());
        assert!(with("header_len = 12").is_ok(), "the real value must pass");
    }

    /// Unbounded geometry turned a typo into a panic (debug) or a wrapped frame
    /// size (release): `n_ch = 57646075230342349` used to parse and then
    /// multiply out to 76 bytes.
    #[test]
    fn absurd_geometry_is_rejected_before_it_can_overflow() {
        assert!(with("n_ch = 57646075230342349").is_err());
        assert!(with("spp_out = 18446744073709552").is_err());
        assert!(with("sample_rate = 1").is_err());
        // The bounds must still admit a plausible non-default rig.
        let c = with("n_ch = 33\nspp_out = 480\nsample_rate = 48000").unwrap();
        assert_eq!(c.packet_ms(), 10);
        assert_eq!(c.layout().packet_len(), 12 + 33 * 480 * 2);
    }

    /// Depths are carried in whole milliseconds throughout, so a packet of
    /// 6.25 ms would silently truncate to 6 in every conversion.
    #[test]
    fn a_packet_that_is_not_a_whole_millisecond_is_rejected() {
        let e = with("spp_out = 100").unwrap_err();
        assert!(e.contains("whole number of milliseconds"), "{}", e);
    }

    /// catchup_max below one packet cannot express anything useful, and above
    /// the bound it would narrow when converted to packets on a 32-bit build.
    #[test]
    fn catchup_max_outside_its_bounds_is_rejected() {
        assert!(with("catchup_max_ms = 5").is_err());
        assert!(with("catchup_max_ms = 4294967296000").is_err());
    }

    /// `nan <= 0.0` is false, so NaN slipped through the old check and panicked
    /// later inside `DepthController::new`; infinity removed the slew limit,
    /// which is the one thing making the catchup ramp inaudible.
    #[test]
    fn non_finite_catchup_parameters_are_rejected() {
        assert!(with("catchup_slew_per_sec = nan").is_err());
        assert!(with("catchup_slew_per_sec = inf").is_err());
        assert!(with("catchup_clamp = nan").is_err());
        assert!(with("catchup_clamp = inf").is_err());
    }

    /// A slew rate above the clamp traverses the whole range inside a second,
    /// so it is not a limit at all.
    #[test]
    fn a_slew_rate_above_the_clamp_is_rejected() {
        assert!(with("catchup_slew_per_sec = 0.05").is_err());
        assert!(
            with("catchup_slew_per_sec = 0.025").is_ok(),
            "slew == clamp is the fastest sensible ramp, not an error"
        );
    }

    /// The depth chain must be ordered: target <= retained <= safety valve.
    /// Spec §6.6's 500 ms valve sits *below* the 3 s catchup budget of §4.2, so
    /// it would have discarded the backlog on every single outage.
    #[test]
    fn a_safety_valve_below_the_retained_depth_is_rejected() {
        let e = with("max_depth_ms = 500").unwrap_err();
        assert!(e.contains("max_depth_ms"), "{}", e);
        assert!(with("max_depth_ms = 3080").is_ok(), "exactly enough passes");
        let c = parse(MINIMAL).unwrap();
        assert!(c.retain_cap_packets() <= c.max_depth_packets());
    }

    /// `d_max_adaptive_ms` is described as a cap, so it must not sit below the
    /// buffer's structural two-packet floor -- `new(5, 10)` would start at 20.
    #[test]
    fn a_d_max_adaptive_below_two_packets_is_rejected() {
        assert!(with("d_max_adaptive_ms = 5").is_err());
        assert!(with("d_max_adaptive_ms = 20").is_ok());
        assert!(with("outage_threshold_ms = 5").is_err());
    }

    /// A misspelled key that is silently ignored leaves the operator believing
    /// the setting took effect -- the same confusion the fail-loud policy is for.
    #[test]
    fn an_unknown_key_is_rejected() {
        let e = with("max_dept_ms = 9000").unwrap_err();
        assert!(e.contains("max_dept_ms"), "{}", e);
    }

    /// Only the default (keep the connection) is implemented. Rejecting the
    /// other value is the honest thing to do: silently ignoring a setting the
    /// user deliberately changed is exactly the class of confusion this config
    /// module exists to avoid.
    #[test]
    fn disabling_keep_source_when_idle_is_rejected_as_unimplemented() {
        let c = parse("source_host = \"h\"\nsource_port = 1\nkeep_source_when_idle = false\n");
        assert!(
            c.is_err(),
            "an unimplemented setting must not be silently ignored"
        );
        assert!(c.unwrap_err().contains("not implemented"));
    }
}
