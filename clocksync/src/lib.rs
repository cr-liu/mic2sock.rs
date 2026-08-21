//! Rate alignment for multi-channel sample streams.
//!
//! Core invariant: **every channel shares one phase accumulator.** Rounding per
//! channel independently produces a systematic inter-channel offset of up to one
//! sample, which at 16 kHz is 62.5 us — equivalent to 2.1 cm of apparent source
//! displacement. That biases DOA estimation, so it is not a rounding nicety.

pub mod depth;
pub mod hermite;
pub mod resampler;

pub use depth::DepthController;
// hermite deliberately has no re-export: it exposes free functions
// (interpolate / to_i16), meant to be reached as clocksync::hermite::...
pub use resampler::Resampler;
