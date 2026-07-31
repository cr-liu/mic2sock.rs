//! Library face of the shim, so integration tests can drive the pipeline
//! directly instead of spawning the binary — a failure then points at a module
//! rather than a process.

pub mod config;
pub mod depth;
pub mod jitter;
pub mod metrics;

pub use config::Config;

/// Parses a config from TOML text.
pub fn parse_config(text: &str) -> Result<Config, String> {
    config::parse(text)
}
