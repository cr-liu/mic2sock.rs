//! 多通道样本流的速率对齐。
//!
//! 核心不变量：**所有通道共享同一个相位累加器**。每通道独立取整会产生系统性的
//! ±1 样本通道间偏差，在 16 kHz 下折合 2.1 cm 等效位置偏移，直接偏置 DOA 估计。
//! 见 spec §5.2。

pub mod depth;
pub mod hermite;
pub mod resampler;

// re-exported once the module lands (Task 10)
// pub use depth::DepthController;
// re-exported once the module lands (Task 9)
// pub use resampler::Resampler;
