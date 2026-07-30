//! mic2sock wire format and packet-stream primitives.
//!
//! This crate deliberately has no IO and no platform dependencies: the Pi-side
//! daemon (Linux/JACK/ALSA) and the Windows-side shim both use it, so it has to
//! compile on either.

pub mod backlog;
pub mod backoff;
pub mod block;
pub mod gap;
pub mod header;
pub mod layout;

// re-exported once the module lands
// pub use backlog::Backlog;
// re-exported once the module lands
// pub use backoff::Backoff;
// block deliberately has no re-export: it exposes free functions
// (deblock_channel / reblock_channel), meant to be reached as
// protocol::block::...
// re-exported once the module lands
// pub use gap::{GapAction, GapTracker};
// re-exported once the module lands
// pub use header::{Header, HEADER_LEN};
// re-exported once the module lands
// pub use layout::PacketLayout;
