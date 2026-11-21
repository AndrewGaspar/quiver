//! Stage B, on the present thread: the newest converted picture, scaled with its aspect kept,
//! plus the overlay, onto the swapchain.
//!
//! Present modes follow [`Pacing`]: lowest latency is IMMEDIATE (tearing, or VRR where the
//! compositor does it), else MAILBOX, else FIFO; smooth is FIFO with two images (one frame
//! buffered). Two frames may be in flight on the GPU.
//!
//! Colour: an HDR10 stream on a surface that offers `HDR10_ST2084` gets an HDR swapchain
//! (A2B10G10R10) and the stream's metadata (`VK_EXT_hdr_metadata`); the picture is PQ
//! already and passes through. An HDR stream on an SDR surface is tone-mapped; SDR passes
//! through. The overlay is SDR, lifted to 203 nits on an HDR swapchain.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use ash::vk;
use quiver_stream_proto::{HdrMetadata, Pacing};
use tracing::{info, warn};

use super::Gpu;
use super::convert::{Converted, SlotReads};
use super::util::{self, Buffer, Image, PipelineDesc, barrier, spv};
use crate::ui::atlas::Atlas;
use crate::ui::draw::Vertex;

const FRAMES: usize = 2;
const SDR_WHITE_NITS: f32 = 203.0;

/// What the presenter is asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentConfig {
    pub pacing: Pacing,
    /// A present mode forced from the command line.
    pub mode_override: Option<vk::PresentModeKHR>,
    /// The stream is HDR10 (and the person didn't turn HDR off): this metadata.
    pub hdr: Option<HdrMetadata>,
    /// The window's size in pixels.
    pub window: (u32, u32),
}

/// A swapchain image, acquired and not yet presented.
#[derive(Debug, Clone, Copy)]
pub struct Acquired {
    frame: usize,
    index: u32,
    suboptimal: bool,
}

/// What a present did.
#[derive(Debug, Clone, Copy)]
pub struct Presented {
    /// For `vkWaitForPresentKHR`, when present wait is on.
    pub present_id: Option<u64>,
    pub swapchain: vk::SwapchainKHR,
    pub at: Instant,
}

struct Frame {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    acquired: vk::Semaphore,
    verts: Option<Buffer>,
}

struct Chain {
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    render_done: Vec<vk::Semaphore>,
    format: vk::SurfaceFormatKHR,
    extent: vk::Extent2D,
    mode: vk::PresentModeKHR,
    hdr: bool,
    blit: vk::Pipeline,
    ui: vk::Pipeline,
    key: (Pacing, Option<vk::PresentModeKHR>, bool, (u32, u32)),
}

pub struct Presenter {
    gpu: Arc<Gpu>,
    surface: vk::SurfaceKHR,
    pool: vk::CommandPool,
    frames: Vec<Frame>,
    frame: usize,
    blit_set: vk::DescriptorSetLayout,
    blit_layout: vk::PipelineLayout,
    ui_set: vk::DescriptorSetLayout,
    ui_layout: vk::PipelineLayout,
    sampler: vk::Sampler,
    atlas: Image,
    chain: Option<Chain>,
    reads: Arc<SlotReads>,
    read_value: u64,
    next_present_id: u64,
    stale: bool,
    /// Held around `vkQueuePresentKHR` and each `vkWaitForPresentKHR` poll: both need the
    /// swapchain externally synchronised, and they run on different threads.
    swap_lock: SwapLock,
}

/// See [`Presenter::swap_lock`].
pub type SwapLock = Arc<std::sync::Mutex<()>>;

#[repr(C)]
#[derive(Clone, Copy)]
struct BlitPush {
    mode: i32,
    sdr_nits: f32,
    peak_nits: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UiPush {
    screen: [f32; 2],
    mode: i32,
    ui_nits: f32,
}

fn bytes<T: Copy>(t: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(t as *const T as *const u8, std::mem::size_of::<T>()) }
}

fn is_srgb(f: vk::Format) -> bool {
    matches!(f, vk::Format::B8G8R8A8_SRGB | vk::Format::R8G8B8A8_SRGB | vk::Format::A8B8G8R8_SRGB_PACK32)
}

/// The rectangle a `pic`-sized picture fills in a `screen`-sized target, aspect kept.
pub fn fit(pic: (u32, u32), screen: (u32, u32)) -> (i32, i32, u32, u32) {
    let (pw, ph) = (pic.0.max(1) as f64, pic.1.max(1) as f64);
    let (sw, sh) = (screen.0 as f64, screen.1 as f64);
    let k = (sw / pw).min(sh / ph);
    let (w, h) = ((pw * k).round() as u32, (ph * k).round() as u32);
    (((screen.0 - w.min(screen.0)) / 2) as i32, ((screen.1 - h.min(screen.1)) / 2) as i32, w.min(screen.0), h.min(screen.1))
}

/// The present mode for a pacing, from what the surface offers.
pub fn choose_mode(pacing: Pacing, available: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    let order: &[vk::PresentModeKHR] = match pacing {
        Pacing::LowestLatency => &[vk::PresentModeKHR::IMMEDIATE, vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO_RELAXED],
        Pacing::Smooth => &[],
    };
    order.iter().copied().find(|m| available.contains(m)).unwrap_or(vk::PresentModeKHR::FIFO)
}

impl Presenter {
    pub fn new(gpu: Arc<Gpu>, surface: vk::SurfaceKHR, atlas: &Atlas, reads: Arc<SlotReads>) -> Result<Self> {
        if gpu.swapchain_fn.is_none() {
            bail!("the device was made without a swapchain");
        }
        let d = &gpu.device;
        let pool = unsafe {
            d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(gpu.graphics_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }?;
        let cmds = unsafe {
            d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(FRAMES as u32))
        }?;
        let mut frames = Vec::new();
        for cmd in cmds {
            frames.push(Frame {
                cmd,
                fence: unsafe { d.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None) }?,
                acquired: unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?,
                verts: None,
            });
        }
        let blit_set = util::sampler_set_layout(&gpu, 1)?;
        let ui_set = util::sampler_set_layout(&gpu, 1)?;
        let blit_layout = unsafe {
            d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&[blit_set]).push_constant_ranges(&[vk::PushConstantRange::default()
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                    .size(std::mem::size_of::<BlitPush>() as u32)]),
                None,
            )
        }?;
        let ui_layout = unsafe {
            d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&[ui_set]).push_constant_ranges(&[vk::PushConstantRange::default()
                    .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                    .size(std::mem::size_of::<UiPush>() as u32)]),
                None,
            )
        }?;
        let sampler = util::linear_sampler(&gpu)?;
        let atlas_img = Self::upload_atlas(&gpu, pool, atlas)?;
        Ok(Self {
            gpu,
            surface,
            pool,
            frames,
            frame: 0,
            blit_set,
            blit_layout,
            ui_set,
            ui_layout,
            sampler,
            atlas: atlas_img,
            chain: None,
            reads,
            read_value: 0,
            next_present_id: 1,
            stale: false,
            swap_lock: Arc::new(std::sync::Mutex::new(())),
        })
    }

    fn upload_atlas(gpu: &Gpu, pool: vk::CommandPool, atlas: &Atlas) -> Result<Image> {
        let d = &gpu.device;
        let extent = vk::Extent2D { width: atlas.width, height: atlas.height };
        let img = Image::new(gpu, vk::Format::R8G8B8A8_UNORM, extent, vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)?;
        let mut staging = Buffer::new(gpu, atlas.pixels.len(), vk::BufferUsageFlags::TRANSFER_SRC)?;
        staging.bytes().copy_from_slice(&atlas.pixels);
        let cmd =
            unsafe { d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1)) }?[0];
        unsafe {
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                    img.image,
                    (vk::ImageLayout::UNDEFINED, vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
                    (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_WRITE),
                )]),
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
                .image_extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 });
            d.cmd_copy_buffer_to_image(cmd, staging.buffer, img.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                    img.image,
                    (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_WRITE),
                    (
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::PipelineStageFlags2::FRAGMENT_SHADER,
                        vk::AccessFlags2::SHADER_SAMPLED_READ,
                    ),
                )]),
            );
            d.end_command_buffer(cmd)?;
            let q = gpu.present_queue.lock().unwrap();
            d.queue_submit2(
                *q,
                &[vk::SubmitInfo2::default().command_buffer_infos(&[vk::CommandBufferSubmitInfo::default().command_buffer(cmd)])],
                vk::Fence::null(),
            )?;
            d.queue_wait_idle(*q)?;
            d.free_command_buffers(pool, &[cmd]);
        }
        staging.destroy(gpu);
        Ok(img)
    }

    /// The surface's HDR10 support: whether an HDR swapchain could be made.
    pub fn surface_hdr(&self) -> bool {
        let sf = self.gpu.surface_fn.as_ref().unwrap();
        let formats = unsafe { sf.get_physical_device_surface_formats(self.gpu.physical, self.surface) }.unwrap_or_default();
        formats.iter().any(|f| f.color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT)
    }

    pub fn present_modes(&self) -> Vec<vk::PresentModeKHR> {
        let sf = self.gpu.surface_fn.as_ref().unwrap();
        unsafe { sf.get_physical_device_surface_present_modes(self.gpu.physical, self.surface) }.unwrap_or_default()
    }

    /// The mode and format in use, for the stats chip.
    pub fn describe(&self) -> String {
        match &self.chain {
            Some(c) => {
                let mode = match c.mode {
                    vk::PresentModeKHR::IMMEDIATE => "immediate",
                    vk::PresentModeKHR::MAILBOX => "mailbox",
                    vk::PresentModeKHR::FIFO_RELAXED => "fifo relaxed",
                    _ => "fifo",
                };
                format!(
                    "{mode} {}×{}{}{}",
                    c.extent.width,
                    c.extent.height,
                    if c.hdr { " HDR10" } else { "" },
                    if self.gpu.present_wait.is_some() { "" } else { " (no present wait)" }
                )
            }
            None => "no swapchain".into(),
        }
    }

    /// Wait for everything in flight, then destroy the swapchain (before a new one, and at
    /// the end). The present-wait thread must be idle by then.
    fn drop_chain(&mut self) {
        let Some(c) = self.chain.take() else { return };
        let d = &self.gpu.device;
        unsafe {
            let fences: Vec<vk::Fence> = self.frames.iter().map(|f| f.fence).collect();
            let _ = d.wait_for_fences(&fences, true, u64::MAX);
            let _ = d.queue_wait_idle(*self.gpu.present_queue.lock().unwrap());
            for v in c.views {
                d.destroy_image_view(v, None);
            }
            for s in c.render_done {
                d.destroy_semaphore(s, None);
            }
            d.destroy_pipeline(c.blit, None);
            d.destroy_pipeline(c.ui, None);
            self.gpu.swapchain_fn.as_ref().unwrap().destroy_swapchain(c.swapchain, None);
        }
    }

    fn create_chain(&mut self, cfg: &PresentConfig) -> Result<()> {
        let old = self.chain.as_ref().map(|c| c.swapchain).unwrap_or_default();
        let gpu = self.gpu.clone();
        let sf = gpu.surface_fn.as_ref().unwrap();
        let sc = gpu.swapchain_fn.as_ref().unwrap();
        let caps = unsafe { sf.get_physical_device_surface_capabilities(self.gpu.physical, self.surface) }?;
        let formats = unsafe { sf.get_physical_device_surface_formats(self.gpu.physical, self.surface) }?;
        let modes = self.present_modes();
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: cfg.window.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width.max(1)),
                height: cfg.window.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height.max(1)),
            }
        };
        if extent.width == 0 || extent.height == 0 {
            bail!("the window has no size");
        }
        let hdr_format = cfg.hdr.as_ref().and_then(|_| {
            formats.iter().copied().find(|f| {
                f.color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT
                    && matches!(f.format, vk::Format::A2B10G10R10_UNORM_PACK32 | vk::Format::A2R10G10B10_UNORM_PACK32)
            })
        });
        let format = hdr_format
            .or_else(|| {
                formats.iter().copied().find(|f| {
                    f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                        && matches!(f.format, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM)
                })
            })
            .or_else(|| formats.iter().copied().find(|f| f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR))
            .context("the surface offers no usable format")?;
        let hdr = hdr_format.is_some();
        let mode = cfg.mode_override.filter(|m| modes.contains(m)).unwrap_or_else(|| choose_mode(cfg.pacing, &modes));
        let mut count = match mode {
            vk::PresentModeKHR::MAILBOX => 3,
            _ => 2,
        }
        .max(caps.min_image_count);
        if caps.max_image_count != 0 {
            count = count.min(caps.max_image_count);
        }
        let alpha = [vk::CompositeAlphaFlagsKHR::OPAQUE, vk::CompositeAlphaFlagsKHR::INHERIT, vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED]
            .into_iter()
            .find(|a| caps.supported_composite_alpha.contains(*a))
            .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .composite_alpha(alpha)
            .present_mode(mode)
            .clipped(true)
            .old_swapchain(old);
        let swapchain = unsafe { sc.create_swapchain(&info, None) }.context("vkCreateSwapchainKHR")?;
        self.drop_chain();
        let images = unsafe { sc.get_swapchain_images(swapchain) }?;
        let d = &self.gpu.device;
        let mut views = Vec::new();
        let mut render_done = Vec::new();
        for &i in &images {
            views.push(unsafe {
                d.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(i)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format.format)
                        .subresource_range(util::color_range()),
                    None,
                )
            }?);
            render_done.push(unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?);
        }
        if let (true, Some(hm), Some(md)) = (hdr, self.gpu.hdr_metadata.as_ref(), cfg.hdr.as_ref()) {
            let xy = |p: [u16; 2]| vk::XYColorEXT { x: p[0] as f32 * 0.00002, y: p[1] as f32 * 0.00002 };
            let meta = vk::HdrMetadataEXT::default()
                .display_primary_red(xy(md.primaries[0]))
                .display_primary_green(xy(md.primaries[1]))
                .display_primary_blue(xy(md.primaries[2]))
                .white_point(xy(md.white_point))
                .max_luminance(md.max_luminance as f32 * 0.0001)
                .min_luminance(md.min_luminance as f32 * 0.0001)
                .max_content_light_level(md.max_cll as f32)
                .max_frame_average_light_level(md.max_fall as f32);
            unsafe { hm.set_hdr_metadata(&[swapchain], &[meta]) };
        }
        let blit = util::pipeline(
            &self.gpu,
            &PipelineDesc {
                vert: spv::FULLSCREEN_VERT,
                frag: spv::BLIT_FRAG,
                format: format.format,
                layout: self.blit_layout,
                blend: false,
                vertex_bindings: &[],
                vertex_attributes: &[],
            },
        )?;
        let stride = std::mem::size_of::<Vertex>() as u32;
        let ui = util::pipeline(
            &self.gpu,
            &PipelineDesc {
                vert: spv::UI_VERT,
                frag: spv::UI_FRAG,
                format: format.format,
                layout: self.ui_layout,

                blend: true,
                vertex_bindings: &[vk::VertexInputBindingDescription { binding: 0, stride, input_rate: vk::VertexInputRate::VERTEX }],
                vertex_attributes: &[
                    vk::VertexInputAttributeDescription { location: 0, binding: 0, format: vk::Format::R32G32_SFLOAT, offset: 0 },
                    vk::VertexInputAttributeDescription { location: 1, binding: 0, format: vk::Format::R32G32_SFLOAT, offset: 8 },
                    vk::VertexInputAttributeDescription { location: 2, binding: 0, format: vk::Format::R8G8B8A8_UNORM, offset: 16 },
                    vk::VertexInputAttributeDescription { location: 3, binding: 0, format: vk::Format::R32G32B32A32_SFLOAT, offset: 20 },
                ],
            },
        )?;
        info!(
            "swapchain {}×{} {:?} {:?} {:?}, {} images",
            extent.width,
            extent.height,
            format.format,
            format.color_space,
            mode,
            images.len()
        );
        self.chain = Some(Chain {
            swapchain,
            images,
            views,
            render_done,
            format,
            extent,
            mode,
            hdr,
            blit,
            ui,
            key: (cfg.pacing, cfg.mode_override, cfg.hdr.is_some(), cfg.window),
        });
        self.stale = false;
        Ok(())
    }

    /// The swapchain must be remade (the window changed, or present said so).
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// The swapchain in use (for present wait), if any.
    pub fn swapchain(&self) -> Option<vk::SwapchainKHR> {
        self.chain.as_ref().map(|c| c.swapchain)
    }

    /// Whether presenting would remake the swapchain (the caller drains present waits
    /// first).
    pub fn needs_rebuild(&self, cfg: &PresentConfig) -> bool {
        self.stale || self.chain.as_ref().is_none_or(|c| c.key != (cfg.pacing, cfg.mode_override, cfg.hdr.is_some(), cfg.window))
    }

    /// Note that `pic` is about to be read by the next present, so its slot isn't reused
    /// before that present is done. Call it when taking the picture from the mailbox.
    pub fn claim(&self, pic: &Converted) {
        self.reads.values[pic.slot].store(self.read_value + 1, Ordering::Release);
    }

    /// Get the next swapchain image: this is where FIFO blocks, so the caller takes the
    /// picture to show only afterwards (late latching) and hands it to [`Presenter::present`].
    /// None: the swapchain went out of date (try again).
    pub fn acquire(&mut self, cfg: &PresentConfig) -> Result<Option<Acquired>> {
        if self.needs_rebuild(cfg) {
            self.create_chain(cfg)?;
        }
        let gpu = self.gpu.clone();
        let d = &gpu.device;
        let sc = gpu.swapchain_fn.as_ref().unwrap();
        let fi = self.frame;
        let (fence, acquired) = (self.frames[fi].fence, self.frames[fi].acquired);
        unsafe { d.wait_for_fences(&[fence], true, u64::MAX) }?;
        let chain = self.chain.as_ref().unwrap();
        match unsafe { sc.acquire_next_image(chain.swapchain, u64::MAX, acquired, vk::Fence::null()) } {
            Ok((index, suboptimal)) => {
                self.frame = (self.frame + 1) % FRAMES;
                Ok(Some(Acquired { frame: fi, index, suboptimal }))
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.stale = true;
                Ok(None)
            }
            Err(e) => Err(e).context("vkAcquireNextImageKHR"),
        }
    }

    /// Draw `pic` (the picture to show; None draws only the overlay) and the overlay into the
    /// acquired image, and present it.
    pub fn present(&mut self, acq: Acquired, pic: Option<&Converted>, ui: &[Vertex], cfg: &PresentConfig) -> Result<Option<Presented>> {
        let gpu = self.gpu.clone();
        let d = &gpu.device;
        let sc = gpu.swapchain_fn.as_ref().unwrap();
        let Acquired { frame: fi, index, suboptimal } = acq;
        let (fence, acquired, cmd) = (self.frames[fi].fence, self.frames[fi].acquired, self.frames[fi].cmd);
        unsafe { d.reset_fences(&[fence]) }?;
        // Overlay vertices into this frame's buffer.
        let vbytes = std::mem::size_of_val(ui);
        if !ui.is_empty() && self.frames[fi].verts.as_ref().is_none_or(|b| b.size < vbytes) {
            if let Some(old) = self.frames[fi].verts.take() {
                old.destroy(&gpu);
            }
            self.frames[fi].verts = Some(Buffer::new(&gpu, vbytes.next_power_of_two().max(65536), vk::BufferUsageFlags::VERTEX_BUFFER)?);
        }
        if let Some(b) = self.frames[fi].verts.as_mut()
            && !ui.is_empty()
        {
            let src = unsafe { std::slice::from_raw_parts(ui.as_ptr() as *const u8, vbytes) };
            b.bytes()[..vbytes].copy_from_slice(src);
        }
        let chain = self.chain.as_ref().unwrap();
        let image = chain.images[index as usize];
        let view = chain.views[index as usize];
        let extent = chain.extent;
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                    image,
                    (vk::ImageLayout::UNDEFINED, vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::NONE),
                    (
                        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                    ),
                )]),
            );
            let att = [vk::RenderingAttachmentInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } })];
            let full = vk::Rect2D { offset: vk::Offset2D::default(), extent };
            d.cmd_begin_rendering(cmd, &vk::RenderingInfo::default().render_area(full).layer_count(1).color_attachments(&att));
            d.cmd_set_scissor(cmd, 0, &[full]);
        }
        if let Some(p) = pic {
            let (x, y, w, h) = fit((p.extent.width, p.extent.height), (extent.width, extent.height));
            let mode = match (chain.hdr, p.pq) {
                (true, true) => 0,
                (true, false) => 2,
                (false, true) => 1,
                (false, false) => {
                    if is_srgb(chain.format.format) {
                        3
                    } else {
                        0
                    }
                }
            };
            let peak = cfg
                .hdr
                .as_ref()
                .map(|m| if m.max_cll > 0 { m.max_cll as f32 } else { m.max_luminance as f32 * 0.0001 })
                .unwrap_or(1000.0)
                .max(400.0);
            let push = BlitPush { mode, sdr_nits: SDR_WHITE_NITS, peak_nits: peak };
            unsafe {
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, chain.blit);
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport { x: x as f32, y: y as f32, width: w as f32, height: h as f32, min_depth: 0.0, max_depth: 1.0 }],
                );
            }
            util::push_samplers(&gpu, cmd, self.blit_layout, self.sampler, &[p.view]);
            unsafe {
                d.cmd_push_constants(cmd, self.blit_layout, vk::ShaderStageFlags::FRAGMENT, 0, bytes(&push));
                d.cmd_draw(cmd, 3, 1, 0, 0);
            }
        }
        if !ui.is_empty() {
            let mode = if chain.hdr {
                2
            } else if is_srgb(chain.format.format) {
                3
            } else {
                0
            };
            let push = UiPush { screen: [extent.width as f32, extent.height as f32], mode, ui_nits: SDR_WHITE_NITS };
            let buf = self.frames[fi].verts.as_ref().unwrap().buffer;
            unsafe {
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, chain.ui);
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: extent.width as f32,
                        height: extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                d.cmd_bind_vertex_buffers(cmd, 0, &[buf], &[0]);
            }
            util::push_samplers(&gpu, cmd, self.ui_layout, self.sampler, &[self.atlas.view]);
            unsafe {
                d.cmd_push_constants(cmd, self.ui_layout, vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT, 0, bytes(&push));
                d.cmd_draw(cmd, ui.len() as u32, 1, 0, 0);
            }
        }
        unsafe {
            d.cmd_end_rendering(cmd);
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                    image,
                    (
                        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                    ),
                    (vk::ImageLayout::PRESENT_SRC_KHR, vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
                )]),
            );
            d.end_command_buffer(cmd)?;
        }
        self.read_value += 1;
        let render_done = chain.render_done[index as usize];
        let mut waits =
            vec![vk::SemaphoreSubmitInfo::default().semaphore(acquired).stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
        if let Some(p) = pic {
            waits.push(
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(p.semaphore)
                    .value(p.value)
                    .stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER),
            );
        }
        let signals = [
            vk::SemaphoreSubmitInfo::default().semaphore(render_done).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
            vk::SemaphoreSubmitInfo::default()
                .semaphore(self.reads.semaphore)
                .value(self.read_value)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
        ];
        let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        let submit = vk::SubmitInfo2::default().wait_semaphore_infos(&waits).command_buffer_infos(&cmds).signal_semaphore_infos(&signals);
        let q = gpu.present_queue.lock().unwrap();
        unsafe { d.queue_submit2(*q, &[submit], fence) }.context("submitting a present")?;
        let swapchains = [chain.swapchain];
        let indices = [index];
        let wait = [render_done];
        let present_id = gpu.present_wait.as_ref().map(|_| {
            let id = self.next_present_id;
            self.next_present_id += 1;
            id
        });
        let ids = [present_id.unwrap_or(0)];
        let mut pid = vk::PresentIdKHR::default().present_ids(&ids);
        let mut pinfo = vk::PresentInfoKHR::default().wait_semaphores(&wait).swapchains(&swapchains).image_indices(&indices);
        if present_id.is_some() {
            pinfo = pinfo.push_next(&mut pid);
        }
        let r = {
            let _g = self.swap_lock.lock().unwrap();
            unsafe { sc.queue_present(*q, &pinfo) }
        };
        drop(q);
        let at = Instant::now();
        match r {
            Ok(sub) => {
                if sub || suboptimal {
                    self.stale = true;
                }
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.stale = true,
            Err(e) => return Err(e).context("vkQueuePresentKHR"),
        }
        Ok(Some(Presented { present_id, swapchain: swapchains[0], at }))
    }

    pub fn swap_lock(&self) -> SwapLock {
        self.swap_lock.clone()
    }

    /// Wait until present `id` is on screen (present wait), up to `timeout`. It polls with a
    /// zero timeout under the swap lock every 250 µs rather than blocking in the driver, so
    /// the present thread is never held up; the answer is accurate to about that.
    pub fn wait_present(gpu: &Gpu, lock: &SwapLock, swapchain: vk::SwapchainKHR, id: u64, timeout: std::time::Duration) -> bool {
        let Some(pw) = gpu.present_wait.as_ref() else {
            return false;
        };
        let end = Instant::now() + timeout;
        loop {
            let r = {
                let _g = lock.lock().unwrap();
                unsafe { pw.wait_for_present(swapchain, id, 0) }
            };
            match r {
                Ok(()) => return true,
                Err(vk::Result::TIMEOUT) => {}
                Err(e) => {
                    warn!("vkWaitForPresentKHR: {e:?}");
                    return false;
                }
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_micros(250));
        }
    }

    /// The present mode in use.
    pub fn mode(&self) -> Option<vk::PresentModeKHR> {
        self.chain.as_ref().map(|c| c.mode)
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.drop_chain();
        let gpu = self.gpu.clone();
        let d = &gpu.device;
        unsafe {
            let _ = d.device_wait_idle();
            for f in self.frames.drain(..) {
                d.destroy_fence(f.fence, None);
                d.destroy_semaphore(f.acquired, None);
                if let Some(b) = f.verts {
                    b.destroy(&gpu);
                }
            }
            self.atlas.destroy(&gpu);
            d.destroy_sampler(self.sampler, None);
            d.destroy_pipeline_layout(self.blit_layout, None);
            d.destroy_pipeline_layout(self.ui_layout, None);
            d.destroy_descriptor_set_layout(self.blit_set, None);
            d.destroy_descriptor_set_layout(self.ui_set, None);
            d.destroy_command_pool(self.pool, None);
            if let Some(sf) = gpu.surface_fn.as_ref() {
                sf.destroy_surface(self.surface, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_centres() {
        assert_eq!(fit((1920, 1080), (1920, 1080)), (0, 0, 1920, 1080));
        assert_eq!(fit((1920, 1080), (3840, 2160)), (0, 0, 3840, 2160));
        // 16:9 in 16:10: bars top and bottom.
        assert_eq!(fit((1920, 1080), (1920, 1200)), (0, 60, 1920, 1080));
        // 4:3 in 16:9: bars left and right.
        assert_eq!(fit((1440, 1080), (1920, 1080)), (240, 0, 1440, 1080));
        // SBS at the window's own size: 1:1.
        assert_eq!(fit((3840, 1080), (3840, 1080)), (0, 0, 3840, 1080));
    }

    #[test]
    fn modes_by_pacing() {
        use vk::PresentModeKHR as M;
        assert_eq!(choose_mode(Pacing::LowestLatency, &[M::FIFO, M::MAILBOX, M::IMMEDIATE]), M::IMMEDIATE);
        assert_eq!(choose_mode(Pacing::LowestLatency, &[M::FIFO, M::MAILBOX]), M::MAILBOX);
        assert_eq!(choose_mode(Pacing::LowestLatency, &[M::FIFO]), M::FIFO);
        assert_eq!(choose_mode(Pacing::Smooth, &[M::FIFO, M::MAILBOX, M::IMMEDIATE]), M::FIFO);
    }
}
