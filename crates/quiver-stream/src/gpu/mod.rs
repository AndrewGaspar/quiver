//! Vulkan: the shared device, Stage A (decoded picture → R'G'B', on the decode thread) and
//! Stage B (R'G'B' + overlay → swapchain, on the present thread).

pub mod convert;
pub mod device;
pub mod present;
pub mod util;

pub use device::Gpu;
