//! The backdrop blur behind the Sight panel (design/SIGHT.md §14): while quiver-sight's
//! request in `hud.image` is live (`qv_hud_header.backdrop`), the layer blurs the game's own
//! frame at present time, under the HUD, so whatever compositor draws the panel on top, the
//! game behind it is blurred. Nothing here runs while no panel asks for it.
//!
//! A dual-filter (Kawase) blur in `shaders/blur.frag`, everything in one command buffer before
//! the HUD's render pass:
//!
//! 1. the swapchain image goes `PRESENT_SRC → SHADER_READ_ONLY` and is box-filtered (4 × 4,
//!    four bilinear taps) into L0, a quarter of its size;
//! 2. `n` Kawase down passes halve it (L1..Ln), `n` up passes double it back to L0;
//! 3. a full-size pass writes the upsampled L0 over the swapchain image (render pass `LOAD`,
//!    ending in `PRESENT_SRC`), darkened by the request's `dim`, blended at the fade's alpha.
//!
//! The intermediate images are `R16G16B16A16_SFLOAT` (enough range for scRGB and HDR10, no
//! banding from eight-bit steps) and hold nothing between presents: every pass starts from
//! `UNDEFINED`, so submissions in flight only need the render passes' own dependencies. The
//! blur works in whatever encoding the swapchain holds (sRGB-encoded UNORM, linear through
//! `_SRGB` views and scRGB, PQ for HDR10) and writes back into the same one.
//!
//! The number of levels and the filter offset follow the request's radius (a fraction of the
//! swapchain height), so the blur looks the same at 1080p and 4K ([`plan`]). Cost: two
//! full-size passes (one read, one blended write) and small ones; see SIGHT §14 for the
//! measurements.

use std::mem::size_of;

use ash::vk;

const BLUR_VERT: &[u8] = include_bytes!("../shaders/blur.vert.spv");
const BLUR_FRAG: &[u8] = include_bytes!("../shaders/blur.frag.spv");

/// The intermediate images' format (mandatory: sampled with linear filtering, colour
/// attachment).
pub const INTER_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// Fade in and out, CLOCK_MONOTONIC_RAW ns (driven by the game's own presents).
pub const FADE_IN_NS: i64 = 160_000_000;
pub const FADE_OUT_NS: i64 = 200_000_000;
/// Halvings after L0, at most.
pub const MAX_LEVELS: usize = 5;
/// The smallest level keeps at least this many texels a side.
const MIN_SIDE: u32 = 4;
/// L0 is this many times smaller than the swapchain image.
pub const PREFILTER: u32 = 4;
/// The blur radius, in texels of a level, that one down/up pair at offset 1 roughly gives.
const SPREAD: f32 = 1.5;

pub const MODE_PREFILTER: u32 = 0;
pub const MODE_DOWN: u32 = 1;
pub const MODE_UP: u32 = 2;
pub const MODE_COMPOSITE: u32 = 3;

/// `shaders/blur.frag`'s push constants.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BlurPush {
    pub texel: [f32; 2],
    pub offset: f32,
    pub alpha: f32,
    pub dim: f32,
    pub mode: u32,
}

impl BlurPush {
    fn bytes(&self) -> &[u8] {
        // SAFETY: repr(C), 24 bytes of f32/u32, no padding.
        unsafe { std::slice::from_raw_parts((self as *const BlurPush).cast::<u8>(), size_of::<BlurPush>()) }
    }
}

/// How a swapchain of `extent` is blurred for a radius of `radius` × its height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// L0's size.
    pub l0: (u32, u32),
    /// Down (and up) passes after the prefilter.
    pub levels: usize,
    /// Kawase offset, in texels of the level sampled.
    pub offset: f32,
}

/// The size of level `i` for an L0 of `l0`.
pub fn level_size(l0: (u32, u32), i: usize) -> (u32, u32) {
    let d = 1u32 << i;
    (l0.0.div_ceil(d).max(1), l0.1.div_ceil(d).max(1))
}

/// L0's size for a swapchain extent.
pub fn l0_size(extent: (u32, u32)) -> (u32, u32) {
    (extent.0.div_ceil(PREFILTER).max(1), extent.1.div_ceil(PREFILTER).max(1))
}

/// Levels a chain for `extent` can have (the smallest side stays ≥ 4 texels).
pub fn max_levels(extent: (u32, u32)) -> usize {
    let l0 = l0_size(extent);
    let mut n = 0;
    while n < MAX_LEVELS {
        let (w, h) = level_size(l0, n + 1);
        if w.min(h) < MIN_SIDE {
            break;
        }
        n += 1;
    }
    n
}

/// Chooses levels and offset so the blur's radius is about `radius × height` pixels: each
/// level doubles the spread, the offset (0.75–2) takes up the rest.
pub fn plan(extent: (u32, u32), radius: f32) -> Plan {
    let l0 = l0_size(extent);
    let r0 = (radius.clamp(0.001, 0.2) * extent.1 as f32 / PREFILTER as f32).max(1.0);
    let ideal = (r0 / SPREAD).max(1.0).log2().round();
    let levels = (ideal as usize).clamp(1, MAX_LEVELS).min(max_levels(extent)).max(if max_levels(extent) == 0 { 0 } else { 1 });
    let offset = (r0 / (SPREAD * (1u32 << levels) as f32)).clamp(0.75, 2.0);
    Plan { l0, levels, offset }
}

/// The fade between the game as it is and the blurred one, advanced on every present of the
/// target swapchain by the time since the last one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fade {
    pub alpha: f32,
    last: i64,
}

impl Fade {
    /// Moves toward 1 (`want`) or 0 and returns the alpha for this present.
    pub fn step(&mut self, want: bool, now: i64) -> f32 {
        // The first present after a pause counts as one frame at most a second long.
        let dt = if self.last == 0 { 0 } else { (now - self.last).clamp(0, 1_000_000_000) };
        self.last = now;
        self.alpha = if want { (self.alpha + dt as f32 / FADE_IN_NS as f32).min(1.0) } else { (self.alpha - dt as f32 / FADE_OUT_NS as f32).max(0.0) };
        // Never "wanted but invisible": the first frame of a request blurs a little.
        if want && self.alpha == 0.0 {
            self.alpha = 0.05;
        }
        self.alpha
    }
}

fn words(spv: &[u8]) -> Vec<u32> {
    spv.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// The blur's pipelines for one device and swapchain format.
pub struct BlurPipes {
    pub format: vk::Format,
    /// L0..Ln: `DONT_CARE`, ends `SHADER_READ_ONLY_OPTIMAL`.
    pub inter_pass: vk::RenderPass,
    /// The swapchain image: `LOAD` from `SHADER_READ_ONLY_OPTIMAL`, ends `PRESENT_SRC_KHR`.
    /// Compatible with the HUD's render pass (one attachment of the same format), so the
    /// HUD's framebuffers serve it.
    pub composite_pass: vk::RenderPass,
    pub set_layout: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    pub sampler: vk::Sampler,
    pub inter: vk::Pipeline,
    /// The fade: blended over the game.
    pub composite: vk::Pipeline,
    /// Fully faded in: written without reading the game's pixels back (RGB only; the
    /// game's alpha stays).
    pub composite_opaque: vk::Pipeline,
}

impl BlurPipes {
    /// # Safety
    /// `dev` is a live device.
    pub unsafe fn new(dev: &ash::Device, format: vk::Format) -> Result<BlurPipes, vk::Result> {
        let mut p = BlurPipes {
            format,
            inter_pass: vk::RenderPass::null(),
            composite_pass: vk::RenderPass::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            sampler: vk::Sampler::null(),
            inter: vk::Pipeline::null(),
            composite: vk::Pipeline::null(),
            composite_opaque: vk::Pipeline::null(),
        };
        // SAFETY: as documented; partial results are destroyed on error.
        match unsafe { p.build(dev) } {
            Ok(()) => Ok(p),
            Err(e) => {
                unsafe { p.destroy(dev) };
                Err(e)
            }
        }
    }

    unsafe fn render_pass(dev: &ash::Device, format: vk::Format, composite: bool) -> Result<vk::RenderPass, vk::Result> {
        let attachment = [vk::AttachmentDescription {
            format,
            samples: vk::SampleCountFlags::TYPE_1,
            load_op: if composite { vk::AttachmentLoadOp::LOAD } else { vk::AttachmentLoadOp::DONT_CARE },
            store_op: vk::AttachmentStoreOp::STORE,
            stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
            stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
            initial_layout: if composite { vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL } else { vk::ImageLayout::UNDEFINED },
            final_layout: if composite { vk::ImageLayout::PRESENT_SRC_KHR } else { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL },
            ..Default::default()
        }];
        let color_ref = [vk::AttachmentReference { attachment: 0, layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL }];
        let subpass = [vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&color_ref)];
        // Intermediates. In: earlier passes (this present's, and earlier presents' on this
        // queue) read the image in fragment shaders or wrote it as an attachment. Out: the
        // next pass samples it.
        let inter = [
            vk::SubpassDependency {
                src_subpass: vk::SUBPASS_EXTERNAL,
                dst_subpass: 0,
                src_stage_mask: vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                dst_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                dependency_flags: vk::DependencyFlags::empty(),
            },
            vk::SubpassDependency {
                src_subpass: 0,
                dst_subpass: vk::SUBPASS_EXTERNAL,
                src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                dst_stage_mask: vk::PipelineStageFlags::FRAGMENT_SHADER,
                src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                dst_access_mask: vk::AccessFlags::SHADER_READ,
                dependency_flags: vk::DependencyFlags::empty(),
            },
        ];
        // The swapchain image: the same dependencies as the HUD's pass (render pass
        // compatibility requires them identical, and its framebuffers are used); the prefilter's
        // read is ordered before by the barrier in `record`, the HUD's pass after by its own.
        let swap = crate::pipelines::Pipelines::dependencies();
        let deps: &[vk::SubpassDependency] = if composite { &swap } else { &inter };
        // SAFETY: valid create info.
        unsafe { dev.create_render_pass(&vk::RenderPassCreateInfo::default().attachments(&attachment).subpasses(&subpass).dependencies(deps), None) }
    }

    unsafe fn build(&mut self, dev: &ash::Device) -> Result<(), vk::Result> {
        // SAFETY: valid create infos throughout; handles are stored for destroy().
        unsafe {
            self.inter_pass = Self::render_pass(dev, INTER_FORMAT, false)?;
            self.composite_pass = Self::render_pass(dev, self.format, true)?;
            let binding = [vk::DescriptorSetLayoutBinding {
                binding: 0,
                descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::FRAGMENT,
                ..Default::default()
            }];
            self.set_layout = dev.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding), None)?;
            let push = [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::FRAGMENT, offset: 0, size: size_of::<BlurPush>() as u32 }];
            let sets = [self.set_layout];
            self.layout = dev.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&sets).push_constant_ranges(&push), None)?;
            self.sampler = dev.create_sampler(
                &vk::SamplerCreateInfo {
                    mag_filter: vk::Filter::LINEAR,
                    min_filter: vk::Filter::LINEAR,
                    mipmap_mode: vk::SamplerMipmapMode::NEAREST,
                    address_mode_u: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    address_mode_v: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    address_mode_w: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    max_lod: 0.0,
                    ..Default::default()
                },
                None,
            )?;
            let vcode = words(BLUR_VERT);
            let fcode = words(BLUR_FRAG);
            let vs = dev.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vcode), None)?;
            let fs = match dev.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&fcode), None) {
                Ok(f) => f,
                Err(e) => {
                    dev.destroy_shader_module(vs, None);
                    return Err(e);
                }
            };
            let r = self.build_pipelines(dev, vs, fs);
            dev.destroy_shader_module(vs, None);
            dev.destroy_shader_module(fs, None);
            r
        }
    }

    unsafe fn build_pipelines(&mut self, dev: &ash::Device, vs: vk::ShaderModule, fs: vk::ShaderModule) -> Result<(), vk::Result> {
        let main = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(main),
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(main),
        ];
        let no_input = vk::PipelineVertexInputStateCreateInfo::default();
        let list = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let opaque = [vk::PipelineColorBlendAttachmentState { blend_enable: vk::FALSE, color_write_mask: vk::ColorComponentFlags::RGBA, ..Default::default() }];
        // The blurred colour at the fade's alpha over the game's; the game's alpha untouched.
        let fade = [vk::PipelineColorBlendAttachmentState {
            blend_enable: vk::TRUE,
            src_color_blend_factor: vk::BlendFactor::SRC_ALPHA,
            dst_color_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            color_blend_op: vk::BlendOp::ADD,
            src_alpha_blend_factor: vk::BlendFactor::ZERO,
            dst_alpha_blend_factor: vk::BlendFactor::ONE,
            alpha_blend_op: vk::BlendOp::ADD,
            color_write_mask: vk::ColorComponentFlags::RGBA,
        }];
        let rgb = [vk::PipelineColorBlendAttachmentState {
            blend_enable: vk::FALSE,
            color_write_mask: vk::ColorComponentFlags::R | vk::ColorComponentFlags::G | vk::ColorComponentFlags::B,
            ..Default::default()
        }];
        let blend_inter = vk::PipelineColorBlendStateCreateInfo::default().attachments(&opaque);
        let blend_fade = vk::PipelineColorBlendStateCreateInfo::default().attachments(&fade);
        let blend_rgb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&rgb);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dyn_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let base = |blend, pass| {
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&no_input)
                .input_assembly_state(&list)
                .viewport_state(&viewport)
                .rasterization_state(&raster)
                .multisample_state(&ms)
                .color_blend_state(blend)
                .dynamic_state(&dyn_state)
                .layout(self.layout)
                .render_pass(pass)
                .subpass(0)
        };
        let infos = [base(&blend_inter, self.inter_pass), base(&blend_fade, self.composite_pass), base(&blend_rgb, self.composite_pass)];
        // SAFETY: valid create infos.
        match unsafe { dev.create_graphics_pipelines(vk::PipelineCache::null(), &infos, None) } {
            Ok(p) => {
                self.inter = p[0];
                self.composite = p[1];
                self.composite_opaque = p[2];
                Ok(())
            }
            Err((p, e)) => {
                for x in p {
                    if x != vk::Pipeline::null() {
                        // SAFETY: created just now, unused.
                        unsafe { dev.destroy_pipeline(x, None) };
                    }
                }
                Err(e)
            }
        }
    }

    /// # Safety
    /// Nothing that uses these is pending on the GPU.
    pub unsafe fn destroy(&self, dev: &ash::Device) {
        // SAFETY: as documented; null handles are ignored by Vulkan.
        unsafe {
            dev.destroy_pipeline(self.inter, None);
            dev.destroy_pipeline(self.composite, None);
            dev.destroy_pipeline(self.composite_opaque, None);
            dev.destroy_sampler(self.sampler, None);
            dev.destroy_pipeline_layout(self.layout, None);
            dev.destroy_descriptor_set_layout(self.set_layout, None);
            dev.destroy_render_pass(self.inter_pass, None);
            dev.destroy_render_pass(self.composite_pass, None);
        }
    }
}

struct Level {
    image: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
    fb: vk::Framebuffer,
    set: vk::DescriptorSet,
    size: (u32, u32),
}

/// A swapchain's blur images and descriptor sets, made the first time a panel asks (about
/// 5.5 MiB at 4K) and kept until the swapchain goes.
pub struct Chain {
    levels: Vec<Level>,
    pool: vk::DescriptorPool,
    /// Per swapchain image: a set sampling its view.
    image_sets: Vec<vk::DescriptorSet>,
    extent: (u32, u32),
    /// Two timestamps per compositor slot, when the queue family has timestamps.
    pub queries: Option<vk::QueryPool>,
}

impl Chain {
    /// # Safety
    /// `views` are the swapchain's image views (with `SAMPLED` usage), `d` its device.
    pub unsafe fn new(d: &crate::state::DeviceData, pipes: &BlurPipes, views: &[vk::ImageView], extent: (u32, u32), slots: usize, timestamps: bool) -> Result<Chain, vk::Result> {
        let mut c = Chain { levels: Vec::new(), pool: vk::DescriptorPool::null(), image_sets: Vec::new(), extent, queries: None };
        // SAFETY: as documented; partial results destroyed on error (nothing in flight yet).
        match unsafe { c.build(d, pipes, views, slots, timestamps) } {
            Ok(()) => Ok(c),
            Err(e) => {
                unsafe { c.destroy(&d.dev) };
                Err(e)
            }
        }
    }

    unsafe fn build(&mut self, d: &crate::state::DeviceData, pipes: &BlurPipes, views: &[vk::ImageView], slots: usize, timestamps: bool) -> Result<(), vk::Result> {
        let dev = &d.dev;
        let n = max_levels(self.extent) + 1;
        let l0 = l0_size(self.extent);
        let sets = (views.len() + n) as u32;
        // SAFETY: valid create infos; handles kept for destroy.
        unsafe {
            let sizes = [vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: sets }];
            self.pool = dev.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(sets).pool_sizes(&sizes), None)?;
            let layouts = vec![pipes.set_layout; sets as usize];
            let all = dev.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.pool).set_layouts(&layouts))?;
            let (img_sets, lvl_sets) = all.split_at(views.len());
            self.image_sets = img_sets.to_vec();
            for (&set, &view) in img_sets.iter().zip(views) {
                write_set(dev, set, pipes.sampler, view);
            }
            for (i, &set) in lvl_sets.iter().enumerate() {
                let size = level_size(l0, i);
                let mut lv = Level { image: vk::Image::null(), mem: vk::DeviceMemory::null(), view: vk::ImageView::null(), fb: vk::Framebuffer::null(), set, size };
                let made = make_level(d, pipes, &mut lv);
                self.levels.push(lv);
                made?;
                write_set(dev, set, pipes.sampler, self.levels[i].view);
            }
            if timestamps {
                self.queries = Some(dev.create_query_pool(&vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::TIMESTAMP).query_count(2 * slots as u32), None)?);
            }
        }
        Ok(())
    }

    /// Records the blur of swapchain image `index` (`image`, drawn through `framebuffer`) into
    /// `cmd`: it ends in `PRESENT_SRC_KHR`, ready for the HUD's render pass. `slot`'s two
    /// timestamps bracket it when there's a query pool.
    ///
    /// # Safety
    /// Recording `cmd` on the compositor's queue; the image is in `PRESENT_SRC_KHR` and its
    /// contents are visible (the submit waits on the app's semaphores).
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn record(&self, dev: &ash::Device, pipes: &BlurPipes, cmd: vk::CommandBuffer, index: usize, image: vk::Image, framebuffer: vk::Framebuffer, slot: usize, p: &Plan, alpha: f32, dim: f32) {
        let Some(&image_set) = self.image_sets.get(index) else { return };
        let levels = p.levels.min(self.levels.len().saturating_sub(1));
        let texel = |s: (u32, u32)| [1.0 / s.0 as f32, 1.0 / s.1 as f32];
        let pass = |rp: vk::RenderPass, fb: vk::Framebuffer, size: (u32, u32), pipe: vk::Pipeline, set: vk::DescriptorSet, push: BlurPush| {
            let extent = vk::Extent2D { width: size.0, height: size.1 };
            let area = vk::Rect2D { offset: vk::Offset2D::default(), extent };
            // SAFETY: recording into our command buffer with our objects.
            unsafe {
                dev.cmd_begin_render_pass(cmd, &vk::RenderPassBeginInfo::default().render_pass(rp).framebuffer(fb).render_area(area), vk::SubpassContents::INLINE);
                dev.cmd_set_viewport(cmd, 0, &[vk::Viewport { x: 0.0, y: 0.0, width: size.0 as f32, height: size.1 as f32, min_depth: 0.0, max_depth: 1.0 }]);
                dev.cmd_set_scissor(cmd, 0, &[area]);
                dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipe);
                dev.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS, pipes.layout, 0, &[set], &[]);
                dev.cmd_push_constants(cmd, pipes.layout, vk::ShaderStageFlags::FRAGMENT, 0, push.bytes());
                dev.cmd_draw(cmd, 3, 1, 0, 0);
                dev.cmd_end_render_pass(cmd);
            }
        };
        // SAFETY: as documented.
        unsafe {
            if let Some(q) = self.queries {
                dev.cmd_reset_query_pool(cmd, q, 2 * slot as u32, 2);
                dev.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, q, 2 * slot as u32);
            }
            // The app's writes are visible through the semaphores the submit waits on (all
            // stages); this orders the layout change after them.
            let to_read = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(crate::compositor::color_range());
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::DependencyFlags::empty(), &[], &[], &[to_read]);
            let l0 = &self.levels[0];
            pass(pipes.inter_pass, l0.fb, l0.size, pipes.inter, image_set, BlurPush { texel: texel(self.extent), offset: 1.0, mode: MODE_PREFILTER, ..Default::default() });
            for i in 1..=levels {
                let (src, dst) = (&self.levels[i - 1], &self.levels[i]);
                pass(pipes.inter_pass, dst.fb, dst.size, pipes.inter, src.set, BlurPush { texel: texel(src.size), offset: p.offset, mode: MODE_DOWN, ..Default::default() });
            }
            for i in (0..levels).rev() {
                let (src, dst) = (&self.levels[i + 1], &self.levels[i]);
                pass(pipes.inter_pass, dst.fb, dst.size, pipes.inter, src.set, BlurPush { texel: texel(src.size), offset: p.offset, mode: MODE_UP, ..Default::default() });
            }
            // The prefilter's read of the swapchain image before the composite writes it.
            let to_write = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(crate::compositor::color_range());
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT, vk::DependencyFlags::empty(), &[], &[], &[to_write]);
            pass(
                pipes.composite_pass,
                framebuffer,
                self.extent,
                if alpha >= 1.0 { pipes.composite_opaque } else { pipes.composite },
                l0.set,
                BlurPush { texel: texel(l0.size), offset: 1.0, alpha: alpha.clamp(0.0, 1.0), dim: dim.clamp(0.0, 1.0), mode: MODE_COMPOSITE },
            );
            if let Some(q) = self.queries {
                dev.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, q, 2 * slot as u32 + 1);
            }
        }
    }

    /// The GPU time `slot`'s last blur took, if its timestamps are ready (never waits).
    ///
    /// # Safety
    /// `slot`'s submission is complete (its fence signalled) and it recorded a blur.
    pub unsafe fn elapsed_ns(&self, dev: &ash::Device, slot: usize, period_ns: f32) -> Option<u64> {
        let q = self.queries?;
        let mut t = [0u64; 2];
        // SAFETY: our pool; both queries were written by a completed submission.
        unsafe { dev.get_query_pool_results(q, 2 * slot as u32, &mut t, vk::QueryResultFlags::TYPE_64) }.ok()?;
        Some((t[1].saturating_sub(t[0]) as f64 * period_ns as f64) as u64)
    }

    /// # Safety
    /// Nothing of the chain's is in flight.
    pub unsafe fn destroy(&mut self, dev: &ash::Device) {
        // SAFETY: as documented; null handles are ignored.
        unsafe {
            for l in self.levels.drain(..) {
                dev.destroy_framebuffer(l.fb, None);
                dev.destroy_image_view(l.view, None);
                dev.destroy_image(l.image, None);
                dev.free_memory(l.mem, None);
            }
            dev.destroy_descriptor_pool(self.pool, None);
            self.pool = vk::DescriptorPool::null();
            if let Some(q) = self.queries.take() {
                dev.destroy_query_pool(q, None);
            }
        }
    }
}

unsafe fn write_set(dev: &ash::Device, set: vk::DescriptorSet, sampler: vk::Sampler, view: vk::ImageView) {
    let info = [vk::DescriptorImageInfo { sampler, image_view: view, image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL }];
    let write = [vk::WriteDescriptorSet::default().dst_set(set).dst_binding(0).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&info)];
    // SAFETY: our set, not in use (fresh).
    unsafe { dev.update_descriptor_sets(&write, &[]) };
}

unsafe fn make_level(d: &crate::state::DeviceData, pipes: &BlurPipes, l: &mut Level) -> Result<(), vk::Result> {
    let dev = &d.dev;
    // SAFETY: valid create infos; whatever was made is in `l` for destroy.
    unsafe {
        l.image = dev.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(INTER_FORMAT)
                .extent(vk::Extent3D { width: l.size.0, height: l.size.1, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )?;
        let req = dev.get_image_memory_requirements(l.image);
        let ty = d.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::empty(), vk::MemoryPropertyFlags::DEVICE_LOCAL).ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
        l.mem = dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)?;
        dev.bind_image_memory(l.image, l.mem, 0)?;
        l.view = dev.create_image_view(
            &vk::ImageViewCreateInfo::default().image(l.image).view_type(vk::ImageViewType::TYPE_2D).format(INTER_FORMAT).subresource_range(crate::compositor::color_range()),
            None,
        )?;
        let att = [l.view];
        l.fb = dev.create_framebuffer(
            &vk::FramebufferCreateInfo::default().render_pass(pipes.inter_pass).attachments(&att).width(l.size.0).height(l.size.1).layers(1),
            None,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_scale_with_the_height() {
        // The same radius (2.5% of the height) at 1080p, 1440p, 1600p and 4K: levels grow with
        // the resolution, and the spread in swapchain pixels (≈ 4 × 1.5 × 2^levels × offset)
        // stays near the radius.
        for (extent, want_levels) in [((1920, 1080), 2), ((2560, 1440), 3), ((2560, 1600), 3), ((3840, 2160), 3), ((7680, 4320), 4)] {
            let p = plan(extent, 0.025);
            assert_eq!(p.l0, l0_size(extent));
            assert_eq!(p.levels, want_levels, "{extent:?}: {p:?}");
            let spread = PREFILTER as f32 * SPREAD * (1u32 << p.levels) as f32 * p.offset;
            let radius = 0.025 * extent.1 as f32;
            assert!((spread / radius - 1.0).abs() < 0.35, "{extent:?}: spread {spread} for radius {radius} ({p:?})");
            assert!((0.75..=2.0).contains(&p.offset));
        }
        // A bigger radius takes more levels; a tiny one at least one.
        assert!(plan((2560, 1440), 0.08).levels > plan((2560, 1440), 0.025).levels);
        assert_eq!(plan((1920, 1080), 0.0).levels, 1);
        // Tiny swapchains: levels stop at a 4-texel side, never below.
        assert_eq!(max_levels((64, 64)), 2);
        assert_eq!(plan((64, 64), 0.2).levels, 1);
    assert!(plan((256, 4096), 0.2).levels <= max_levels((256, 4096)));
        assert_eq!(max_levels((8, 8)), 0);
        assert_eq!(plan((8, 8), 0.025).levels, 0);
        assert_eq!(level_size((960, 540), 3), (120, 68));
        assert_eq!(l0_size((1, 1)), (1, 1));
        assert_eq!(max_levels((u32::MAX, u32::MAX)), MAX_LEVELS);
    }

    #[test]
    fn fades() {
        let ms = 1_000_000;
        let mut f = Fade::default();
        assert_eq!(f.step(false, 10 * ms), 0.0);
        // Wanted: a little at once, then up over FADE_IN_NS of presents.
        let a = f.step(true, 20 * ms);
        assert!(a > 0.0 && a < 0.1, "{a}");
        let mut t = 20 * ms;
        let mut n = 0;
        while f.alpha < 1.0 {
            t += 16 * ms;
            f.step(true, t);
            n += 1;
        }
        assert!((9..=11).contains(&n), "{n} frames at 60 fps to fade in");
        // A game presenting once a second jumps straight to the end.
        let mut slow = Fade::default();
        slow.step(false, ms);
        assert_eq!(slow.step(true, 1001 * ms), 1.0);
        // Released: down over FADE_OUT_NS.
        t += 100 * ms;
        let a = f.step(false, t);
        assert!((a - 0.5).abs() < 0.01, "{a}");
        t += 100 * ms;
        assert_eq!(f.step(false, t), 0.0);
        // Time going backwards (clock trouble) doesn't move it.
        assert_eq!(f.step(false, t - 50 * ms), 0.0);
    }
}
