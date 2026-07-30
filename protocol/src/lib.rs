//! mic2sock 线路格式与包流原语。
//!
//! 本 crate 刻意不含任何 IO 与平台依赖：Pi 侧守护进程（Linux/JACK/ALSA）与
//! Windows 侧 shim 共用它，所以它必须能在两边都编译。

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
