use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Runtime configuration, read from `shim.toml`.
///
/// Unlike `mic2sock`, a missing or malformed file is a **hard error**. That
/// daemon silently writes defaults to a differently-named file and keeps
/// running, which makes a misconfiguration look like a mysterious runtime bug.
#[derive(Debug, Clone, Deserialize, PartialEq)]
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

fn default_sink_port() -> u16 { 7998 }
fn default_n_ch() -> usize { 17 }
fn default_spp_out() -> usize { 160 }
fn default_header_len() -> usize { 12 }
fn default_sample_rate() -> usize { 16000 }
fn default_d_max_adaptive_ms() -> u64 { 80 }
fn default_catchup_max_ms() -> u64 { 3000 }
fn default_catchup_clamp() -> f64 { 0.025 }
fn default_catchup_slew_per_sec() -> f64 { 0.002 }
fn default_max_depth_ms() -> u64 { 500 }
fn default_outage_threshold_ms() -> u64 { 200 }
fn default_keep_source_when_idle() -> bool { true }

use protocol::PacketLayout;

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
        if self.n_ch == 0 {
            return Err("n_ch must be > 0".into());
        }
        if self.spp_out == 0 {
            return Err("spp_out must be > 0".into());
        }
        if self.sample_rate == 0 {
            return Err("sample_rate must be > 0".into());
        }
        if self.packet_ms() == 0 {
            return Err("spp_out is too small for sample_rate: one packet rounds to 0 ms".into());
        }
        if self.catchup_max_ms < self.packet_ms() {
            return Err(format!(
                "catchup_max_ms ({}) must be at least one packet ({} ms)",
                self.catchup_max_ms,
                self.packet_ms()
            ));
        }
        if !(0.0..1.0).contains(&self.catchup_clamp) || self.catchup_clamp == 0.0 {
            return Err("catchup_clamp must be in (0, 1)".into());
        }
        if self.catchup_slew_per_sec <= 0.0 {
            return Err("catchup_slew_per_sec must be > 0".into());
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

    /// Wall duration of one output packet, in milliseconds.
    pub fn packet_ms(&self) -> u64 {
        (self.spp_out * 1000 / self.sample_rate) as u64
    }

    /// `catchup_max_ms` expressed in packets.
    pub fn catchup_max_packets(&self) -> usize {
        (self.catchup_max_ms / self.packet_ms().max(1)) as usize
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
        assert!(c.keep_source_when_idle);
        assert_eq!(c.metrics_path, None);
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
        let c = parse("source_host = \"h\"\nsource_port = 1\nn_ch = 0\n");
        assert!(c.is_err(), "n_ch = 0 must be rejected");
        let c = parse("source_host = \"h\"\nsource_port = 1\nspp_out = 0\n");
        assert!(c.is_err(), "spp_out = 0 must be rejected");
    }

    /// catchup_max below one packet cannot express anything useful.
    #[test]
    fn catchup_max_shorter_than_one_packet_is_rejected() {
        let c = parse("source_host = \"h\"\nsource_port = 1\ncatchup_max_ms = 5\n");
        assert!(c.is_err());
    }

    /// Only the default (keep the connection) is implemented. Rejecting the
    /// other value is the honest thing to do: silently ignoring a setting the
    /// user deliberately changed is exactly the class of confusion this config
    /// module exists to avoid.
    #[test]
    fn disabling_keep_source_when_idle_is_rejected_as_unimplemented() {
        let c = parse("source_host = \"h\"\nsource_port = 1\nkeep_source_when_idle = false\n");
        assert!(c.is_err(), "an unimplemented setting must not be silently ignored");
        assert!(c.unwrap_err().contains("not implemented"));
    }
}
