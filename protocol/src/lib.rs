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

// re-exported once the module lands (Task 6)
// pub use backlog::Backlog;
// re-exported once the module lands (Task 7)
// pub use backoff::Backoff;
// re-exported once the module lands (Task 5)
// pub use gap::{GapAction, GapTracker};
// re-exported once the module lands (Task 2)
// pub use header::{Header, HEADER_LEN};
// re-exported once the module lands (Task 3)
// pub use layout::PacketLayout;
