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
/// The invariants the rest of the program relies on — the consumer's geometry, an
/// ordered depth chain, packet counts inside every downstream horizon — hold only
/// for a value that came out of [`parse`] or [`load`]. Building one field by field
/// skips all of them.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the Pi's sender listens.
    pub source_host: String,
    pub source_port: u16,
    /// Port the black box connects to. It must be pointed at 127.0.0.1:this.
    #[serde(default = "default_sink_port")]
    pub sink_port: u16,

    /// Packet geometry. The packet length is derived from these, never hardcoded —
    /// but they are held to the deployed consumer's values (see `CONSUMER_*`), so
    /// these are what the shim *reports* the geometry to be, not a free choice.
    #[serde(default = "default_n_ch")]
    pub n_ch: usize,
    #[serde(default = "default_spp_out")]
    pub spp_out: usize,
    /// Samples per packet on the SOURCE side. Our side of the wire, so unlike the
    /// consumer geometry above it is a free choice; the reframer accumulates
    /// whatever arrives into `spp_out`-sample output packets. Unset means the
    /// source still sends consumer-sized packets (the legacy layout).
    #[serde(default)]
    pub spp_in: Option<usize>,
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
    /// Must sit **above** `d_max_adaptive_ms + catchup_max_ms`, the ceiling on what
    /// the resync anchor retains (spec §6.2's anchoring formula). Spec §6.6's
    /// 500 ms predates the 3 s catchup decision (§4.2) and would have made the
    /// valve fire on every outage, discarding exactly the backlog catchup exists to
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
    16
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

use crate::jitter::MAX_HORIZON_PACKETS;
use protocol::{PacketLayout, HEADER_LEN};

/// The geometry the deployed consumer parses.
///
/// Constants rather than free parameters, because the black box is closed-source
/// and parses by byte offset: it cannot report a disagreement, it just mis-reads
/// every field of every packet for as long as the shim runs. The three values
/// stay in `shim.toml` so the packet length is still *derived* — 5132 is never
/// hardcoded, per spec §6.7 — but one that disagrees with the consumer is refused
/// at load. Changing the rig means changing these and rebuilding the consumer;
/// that coupling is real, and naming it is more honest than accepting a geometry
/// we cannot deliver.
///
/// This does not remove the need for the runtime cross-check in `source.rs`: the
/// Pi clamps `mic.n_channel` down to whatever the hardware enumerates, so the
/// stream can disagree with a config that is internally valid.
const CONSUMER_N_CH: usize = 16;
const CONSUMER_SPP_OUT: usize = 160;
const CONSUMER_SAMPLE_RATE: usize = 16_000;
const CONSUMER_PACKET_LEN: usize =
    PacketLayout::new(CONSUMER_N_CH, CONSUMER_SPP_OUT, HEADER_LEN).packet_len();

/// Bounds on the timing knobs. No depth in this program is ever minutes long,
/// and bounding them is what makes every derived packet count provably fit both
/// a 32-bit `usize` and the jitter buffer's horizon.
const MAX_D_MAX_ADAPTIVE_MS: u64 = 1_000;
const MAX_CATCHUP_MS: u64 = 30_000;
const MAX_OUTAGE_THRESHOLD_MS: u64 = 10_000;
const MAX_MAX_DEPTH_MS: u64 = 60_000;

/// `max_depth_ms` dominates every other depth (validated below), and one packet
/// is at least 1 ms, so this one bound caps every packet count the pipeline hands
/// to `JitterBuffer::new` — which panics above its horizon. Asserted at compile
/// time so raising a bound cannot silently produce a config that panics at
/// startup, and so no runtime check has to pretend it might fire.
const _: () = assert!(MAX_MAX_DEPTH_MS as usize <= MAX_HORIZON_PACKETS);

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
        // field of every packet for as long as the shim runs. `n_ch = 16` is as
        // fatal as `header_len = 13` and just as invisible, so all four are held
        // to the deployed values rather than merely bounded.
        for (name, got, want) in [
            ("header_len", self.header_len, HEADER_LEN),
            ("n_ch", self.n_ch, CONSUMER_N_CH),
            ("spp_out", self.spp_out, CONSUMER_SPP_OUT),
            ("sample_rate", self.sample_rate, CONSUMER_SAMPLE_RATE),
        ] {
            if got != want {
                return Err(format!(
                    "{} must be {} (got {}): the consumer parses {}-byte packets by byte offset \
                     and cannot report a disagreement. A different rig means changing the \
                     CONSUMER_* constants in shim/src/config.rs and rebuilding the consumer.",
                    name, want, got, CONSUMER_PACKET_LEN
                ));
            }
        }

        // The source packet size is our choice, but not an unbounded one: the
        // depth/jitter arithmetic is integer milliseconds, so a source packet
        // must last a whole number of them; and larger than the output packet
        // would mean the reframer holds audio back to split it, adding latency
        // for nothing.
        if let Some(spp_in) = self.spp_in {
            if spp_in == 0 || spp_in > self.spp_out {
                return Err(format!(
                    "spp_in must be in 1..={} (got {})",
                    self.spp_out, spp_in
                ));
            }
            if (spp_in * 1000) % self.sample_rate != 0 {
                return Err(format!(
                    "spp_in = {} does not last a whole number of milliseconds at {} Hz;                      the depth accounting is integer-ms and would silently round",
                    spp_in, self.sample_rate
                ));
            }
        }

        // Depth chain: target <= retained <= safety valve. Out of order, the
        // stage below discards what the stage above is waiting for.
        let packet_ms = self.packet_ms_in();
        if !(2 * packet_ms..=MAX_D_MAX_ADAPTIVE_MS).contains(&self.d_max_adaptive_ms) {
            return Err(format!(
                "d_max_adaptive_ms must be in {}..={} (got {}): two packets is the structural \
                 floor of the buffer, so anything less would not be a cap at all",
                2 * packet_ms,
                MAX_D_MAX_ADAPTIVE_MS,
                self.d_max_adaptive_ms
            ));
        }
        if !(packet_ms..=MAX_CATCHUP_MS).contains(&self.catchup_max_ms) {
            return Err(format!(
                "catchup_max_ms must be in {}..={} (got {})",
                packet_ms, MAX_CATCHUP_MS, self.catchup_max_ms
            ));
        }
        if !(packet_ms..=MAX_OUTAGE_THRESHOLD_MS).contains(&self.outage_threshold_ms) {
            return Err(format!(
                "outage_threshold_ms must be in {}..={} (got {})",
                packet_ms, MAX_OUTAGE_THRESHOLD_MS, self.outage_threshold_ms
            ));
        }
        let retained_ms = self.d_max_adaptive_ms + self.catchup_max_ms;
        if !(retained_ms..=MAX_MAX_DEPTH_MS).contains(&self.max_depth_ms) {
            return Err(format!(
                "max_depth_ms must be in {}..={} (got {}): below d_max_adaptive_ms + \
                 catchup_max_ms the safety valve discards the backlog catchup exists to absorb",
                retained_ms, MAX_MAX_DEPTH_MS, self.max_depth_ms
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

    pub fn spp_in(&self) -> usize {
        self.spp_in.unwrap_or(self.spp_out)
    }

    /// The layout of packets ARRIVING from the source. The jitter buffer, the
    /// depth estimator and every packet-count derivation below operate on these,
    /// not on the consumer-sized output packets.
    pub fn in_layout(&self) -> PacketLayout {
        PacketLayout::new(self.n_ch, self.spp_in(), self.header_len)
    }

    /// Duration of one SOURCE packet in ms — the granularity of the jitter
    /// buffer and the release pacer.
    pub fn packet_ms_in(&self) -> u64 {
        (self.spp_in() * 1000 / self.sample_rate) as u64
    }

    /// Wall duration of one output packet, in milliseconds. Exact and non-zero:
    /// `validate` pins the geometry to 160 samples at 16 kHz.
    pub fn packet_ms(&self) -> u64 {
        (self.spp_out * 1000 / self.sample_rate) as u64
    }

    /// `catchup_max_ms` expressed in packets.
    ///
    /// Every count below is bounded by `MAX_MAX_DEPTH_MS` (60_000) because
    /// `max_depth_ms` dominates the chain and `packet_ms` is at least 1, so none
    /// of these conversions can narrow on a 32-bit build.
    pub fn catchup_max_packets(&self) -> usize {
        (self.catchup_max_ms / self.packet_ms_in()) as usize
    }

    /// Ceiling on what the resync anchor may retain, in packets.
    ///
    /// Spec §6.2's anchoring formula uses the *live* `D_target + catchup_max`;
    /// `D_target` varies at runtime, so this is its conservative maximum —
    /// `d_max_adaptive_ms` is the cap `D_target` can never exceed. It is the
    /// jitter buffer's `retain_cap` (a hard latency bound, so a fixed ceiling is
    /// the right shape); trimming to the live target is the pipeline's job.
    pub fn retain_cap_packets(&self) -> usize {
        ((self.d_max_adaptive_ms + self.catchup_max_ms) / self.packet_ms_in()) as usize
    }

    /// Safety-valve depth in packets. Never below `retain_cap_packets()`.
    pub fn max_depth_packets(&self) -> usize {
        (self.max_depth_ms / self.packet_ms_in()) as usize
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
        assert_eq!(c.n_ch, 16);
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

    /// The source packet size is our side of the wire and a free choice within
    /// bounds; the consumer geometry stays pinned regardless. Unset means the
    /// legacy layout where the source sends consumer-sized packets.
    #[test]
    fn source_packet_size_is_free_within_bounds() {
        let c = with("spp_in = 32").unwrap();
        assert_eq!(c.spp_in(), 32);
        assert_eq!(c.packet_ms_in(), 2);
        assert_eq!(c.in_layout().packet_len(), 12 + 16 * 32 * 2);
        // Packet-count derivations follow the SOURCE packet duration: the jitter
        // buffer holds source packets, so 3 s of catchup is 1500 of them at 2 ms.
        assert_eq!(c.catchup_max_packets(), 1500);

        let c: Config = parse(MINIMAL).unwrap();
        assert_eq!(c.spp_in(), c.spp_out);
        assert_eq!(c.in_layout(), c.layout());
        assert_eq!(c.packet_ms_in(), c.packet_ms());

        // Larger than the output would hold audio back to split it; zero is
        // meaningless; 48 lasts exactly 3 ms and is fine; 33 does not last a
        // whole number of milliseconds and the integer-ms depth math would
        // silently round.
        assert!(with("spp_in = 0").is_err());
        assert!(with("spp_in = 320").is_err());
        assert!(with("spp_in = 48").is_ok());
        assert!(with("spp_in = 33").is_err());
    }

    /// The production geometry, derived rather than hardcoded. 5132 is what the
    /// closed-source consumer expects.
    #[test]
    fn derives_the_production_packet_length() {
        let c: Config = parse(MINIMAL).unwrap();
        assert_eq!(c.layout().packet_len(), 5132);
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
    /// offset, and it cannot report a disagreement — it just mis-reads every
    /// field forever. So every value is held to the deployed rig, not merely
    /// bounded: `n_ch = 17` (5452 bytes) is exactly as fatal as `header_len = 13`
    /// (5133 bytes), and bounding one while allowing the other was incoherent.
    #[test]
    fn a_geometry_the_consumer_cannot_parse_is_rejected() {
        for bad in [
            "header_len = 13",
            "header_len = 16",
            "n_ch = 17",
            "n_ch = 18",
            "spp_out = 320",
            "spp_out = 100",
            "sample_rate = 48000",
        ] {
            let e = with(bad).unwrap_err();
            assert!(e.contains("5132"), "{} -> {}", bad, e);
        }
        // Stating the deployed values explicitly must still parse, and the packet
        // length must still be derived from them rather than hardcoded.
        let c = with("header_len = 12\nn_ch = 16\nspp_out = 160\nsample_rate = 16000").unwrap();
        assert_eq!(c.layout().packet_len(), 5132);
    }

    /// Unbounded geometry turned a typo into a panic (debug) or a wrapped frame
    /// size (release): `n_ch = 57646075230342349` used to parse and then
    /// multiply out to 76 bytes.
    #[test]
    fn absurd_geometry_is_rejected_before_it_can_overflow() {
        assert!(with("n_ch = 57646075230342349").is_err());
        assert!(with("spp_out = 18446744073709552").is_err());
        assert!(with("sample_rate = 1").is_err());
    }

    /// Unbounded timing let an accepted config produce packet counts that panic
    /// `JitterBuffer::new`, or narrow to nonsense on a 32-bit build: at
    /// `d_max_adaptive_ms = 42949672960` the retained depth is 4.29e9 packets,
    /// which becomes 300 in a 32-bit `usize` and inverts the validated chain.
    #[test]
    fn timing_beyond_its_bounds_is_rejected() {
        assert!(with("d_max_adaptive_ms = 10485760\nmax_depth_ms = 10488760").is_err());
        assert!(with("d_max_adaptive_ms = 42949672960\nmax_depth_ms = 85899346920").is_err());
        assert!(with("max_depth_ms = 85899346920").is_err());
        assert!(with("outage_threshold_ms = 99999").is_err());
        // Every count a downstream constructor sees stays inside its horizon.
        let c = parse(MINIMAL).unwrap();
        assert!(c.max_depth_packets() <= MAX_HORIZON_PACKETS);
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

    /// The shipped example must be a config the shim will actually accept. It is the
    /// first thing an operator copies, and `deny_unknown_fields` plus the pinned
    /// geometry mean a stale example fails at *their* startup rather than in CI.
    /// Compiled in, so it cannot drift from this crate.
    #[test]
    fn the_shipped_example_config_parses() {
        let example = include_str!("../shim.toml.example");
        let c = parse(example).expect("shim.toml.example must be valid");
        assert_eq!(c.layout().packet_len(), 5132);
        // And the values it documents are the ones the code defaults to, so the
        // example never quietly disagrees with the built-in behaviour.
        let defaults = parse(MINIMAL).unwrap();
        assert_eq!(c.d_max_adaptive_ms, defaults.d_max_adaptive_ms);
        assert_eq!(c.catchup_max_ms, defaults.catchup_max_ms);
        assert_eq!(c.catchup_clamp, defaults.catchup_clamp);
        assert_eq!(c.catchup_slew_per_sec, defaults.catchup_slew_per_sec);
        assert_eq!(c.max_depth_ms, defaults.max_depth_ms);
        assert_eq!(c.outage_threshold_ms, defaults.outage_threshold_ms);
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
