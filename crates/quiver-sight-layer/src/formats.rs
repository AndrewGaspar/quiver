//! Which swapchains the compositor draws on (SIGHT-HUD §2.8 "Formats and colour").
//!
//! The HUD image is sRGB-encoded and premultiplied. UNORM swapchains blend it as is (how the
//! app's own windows look); sRGB swapchains sample it through an `_SRGB` texture and blend in
//! linear. HDR10 (ST 2084) and scRGB swapchains are phase 2: the layer reports "can't draw"
//! and quiverd uses the desktop presenter.

use ash::vk;
use quiver_perf::layout::{DRAW_ERR_FORMAT, DRAW_ERR_NO_IMAGE, DRAW_ERR_USAGE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// Write sRGB-encoded values straight into a UNORM image.
    Unorm,
    /// The target is an `_SRGB` format: the GPU encodes on write, so the shaders produce
    /// linear values (texture through an `_SRGB` format, vertex colours decoded).
    Srgb,
}

impl Encoding {
    /// The format of the HUD texture for this target.
    pub fn texture_format(self) -> vk::Format {
        match self {
            Encoding::Unorm => vk::Format::R8G8B8A8_UNORM,
            Encoding::Srgb => vk::Format::R8G8B8A8_SRGB,
        }
    }
}

/// The swapchain formats the compositor knows how to blend into.
pub fn encoding_of(format: vk::Format) -> Option<Encoding> {
    use vk::Format as F;
    match format {
        F::B8G8R8A8_UNORM
        | F::R8G8B8A8_UNORM
        | F::A8B8G8R8_UNORM_PACK32
        | F::A2B10G10R10_UNORM_PACK32
        | F::A2R10G10B10_UNORM_PACK32
        | F::R5G6B5_UNORM_PACK16
        | F::B5G6R5_UNORM_PACK16 => Some(Encoding::Unorm),
        F::B8G8R8A8_SRGB | F::R8G8B8A8_SRGB | F::A8B8G8R8_SRGB_PACK32 => Some(Encoding::Srgb),
        _ => None,
    }
}

/// Decides from the create info whether the compositor can draw on a swapchain, and how.
/// `Err` is the `DRAW_ERR_*` reason the status block reports.
pub fn classify(ci: &vk::SwapchainCreateInfoKHR<'_>) -> Result<Encoding, u32> {
    if ci.image_color_space != vk::ColorSpaceKHR::SRGB_NONLINEAR {
        // HDR10_ST2084, EXTENDED_SRGB_LINEAR (scRGB), and the rest: phase 2 or never.
        return Err(DRAW_ERR_FORMAT);
    }
    if ci.image_extent.width == 0 || ci.image_extent.height == 0 {
        // Not valid Vulkan, but cheap to refuse: nothing to draw on.
        return Err(DRAW_ERR_NO_IMAGE);
    }
    if ci.image_array_layers != 1 || ci.flags.contains(vk::SwapchainCreateFlagsKHR::PROTECTED) {
        return Err(DRAW_ERR_FORMAT);
    }
    if ci.flags.contains(vk::SwapchainCreateFlagsKHR::DEFERRED_MEMORY_ALLOCATION_EXT) {
        // Images have no memory until first acquired: no views at creation.
        return Err(DRAW_ERR_USAGE);
    }
    if matches!(ci.present_mode, vk::PresentModeKHR::SHARED_DEMAND_REFRESH | vk::PresentModeKHR::SHARED_CONTINUOUS_REFRESH) {
        // Shared presentable images live in SHARED_PRESENT layout; not handled.
        return Err(DRAW_ERR_USAGE);
    }
    encoding_of(ci.image_format).ok_or(DRAW_ERR_FORMAT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ci(format: vk::Format, cs: vk::ColorSpaceKHR) -> vk::SwapchainCreateInfoKHR<'static> {
        vk::SwapchainCreateInfoKHR {
            image_format: format,
            image_color_space: cs,
            image_array_layers: 1,
            image_extent: vk::Extent2D { width: 1920, height: 1080 },
            present_mode: vk::PresentModeKHR::FIFO,
            ..Default::default()
        }
    }

    #[test]
    fn classifies() {
        let srgb = vk::ColorSpaceKHR::SRGB_NONLINEAR;
        assert_eq!(classify(&ci(vk::Format::B8G8R8A8_UNORM, srgb)), Ok(Encoding::Unorm));
        assert_eq!(classify(&ci(vk::Format::A2B10G10R10_UNORM_PACK32, srgb)), Ok(Encoding::Unorm));
        assert_eq!(classify(&ci(vk::Format::B8G8R8A8_SRGB, srgb)), Ok(Encoding::Srgb));
        assert_eq!(classify(&ci(vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT)), Err(DRAW_ERR_FORMAT));
        assert_eq!(classify(&ci(vk::Format::R16G16B16A16_SFLOAT, vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT)), Err(DRAW_ERR_FORMAT));
        assert_eq!(classify(&ci(vk::Format::R16G16B16A16_SFLOAT, srgb)), Err(DRAW_ERR_FORMAT));
        let mut layered = ci(vk::Format::B8G8R8A8_UNORM, srgb);
        layered.image_array_layers = 2;
        assert_eq!(classify(&layered), Err(DRAW_ERR_FORMAT));
        let mut shared = ci(vk::Format::B8G8R8A8_UNORM, srgb);
        shared.present_mode = vk::PresentModeKHR::SHARED_DEMAND_REFRESH;
        assert_eq!(classify(&shared), Err(DRAW_ERR_USAGE));
        let mut deferred = ci(vk::Format::B8G8R8A8_UNORM, srgb);
        deferred.flags = vk::SwapchainCreateFlagsKHR::DEFERRED_MEMORY_ALLOCATION_EXT;
        assert_eq!(classify(&deferred), Err(DRAW_ERR_USAGE));
        let mut empty = ci(vk::Format::B8G8R8A8_UNORM, srgb);
        empty.image_extent.height = 0;
        assert_eq!(classify(&empty), Err(DRAW_ERR_NO_IMAGE));
        assert_eq!(Encoding::Srgb.texture_format(), vk::Format::R8G8B8A8_SRGB);
    }
}
