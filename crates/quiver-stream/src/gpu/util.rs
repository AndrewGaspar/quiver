//! Small Vulkan helpers: images, buffers, shaders and barriers.

use anyhow::{Context, Result};
use ash::vk;

use super::Gpu;

pub struct Image {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
}

impl Image {
    pub fn new(gpu: &Gpu, format: vk::Format, extent: vk::Extent2D, usage: vk::ImageUsageFlags) -> Result<Self> {
        let d = &gpu.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: extent.width.max(1), height: extent.height.max(1), depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { d.create_image(&info, None) }.context("vkCreateImage")?;
        let req = unsafe { d.get_image_memory_requirements(image) };
        let ty = gpu.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).context("no device-local memory")?;
        let memory =
            unsafe { d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None) }?;
        unsafe { d.bind_image_memory(image, memory, 0) }?;
        let view = unsafe {
            d.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(color_range()),
                None,
            )
        }?;
        Ok(Self { image, memory, view, format, extent })
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            gpu.device.destroy_image_view(self.view, None);
            gpu.device.destroy_image(self.image, None);
            gpu.device.free_memory(self.memory, None);
        }
    }
}

/// A host-visible, coherent buffer, mapped for its lifetime.
pub struct Buffer {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub ptr: *mut u8,
    pub size: usize,
}

unsafe impl Send for Buffer {}

impl Buffer {
    pub fn new(gpu: &Gpu, size: usize, usage: vk::BufferUsageFlags) -> Result<Self> {
        let d = &gpu.device;
        let buffer = unsafe { d.create_buffer(&vk::BufferCreateInfo::default().size(size as u64).usage(usage), None) }?;
        let req = unsafe { d.get_buffer_memory_requirements(buffer) };
        let props = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let ty = gpu
            .memory_type(req.memory_type_bits, props | vk::MemoryPropertyFlags::HOST_CACHED)
            .or_else(|| gpu.memory_type(req.memory_type_bits, props))
            .context("no host-visible memory")?;
        let memory =
            unsafe { d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None) }?;
        unsafe { d.bind_buffer_memory(buffer, memory, 0) }?;
        let ptr = unsafe { d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }? as *mut u8;
        Ok(Self { buffer, memory, ptr, size })
    }

    pub fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) }
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            gpu.device.unmap_memory(self.memory);
            gpu.device.destroy_buffer(self.buffer, None);
            gpu.device.free_memory(self.memory, None);
        }
    }
}

pub fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)
}

pub fn shader(gpu: &Gpu, spv: &[u8]) -> Result<vk::ShaderModule> {
    let words: Vec<u32> = spv.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    Ok(unsafe { gpu.device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None) }?)
}

/// A layout transition (no queue transfer) for a whole colour image.
pub fn barrier(
    image: vk::Image,
    from: (vk::ImageLayout, vk::PipelineStageFlags2, vk::AccessFlags2),
    to: (vk::ImageLayout, vk::PipelineStageFlags2, vk::AccessFlags2),
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .image(image)
        .old_layout(from.0)
        .src_stage_mask(from.1)
        .src_access_mask(from.2)
        .new_layout(to.0)
        .dst_stage_mask(to.1)
        .dst_access_mask(to.2)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .subresource_range(color_range())
}

/// A graphics pipeline for dynamic rendering into `format`: no depth, optional blending.
pub struct PipelineDesc<'a> {
    pub vert: &'a [u8],
    pub frag: &'a [u8],
    pub format: vk::Format,
    pub layout: vk::PipelineLayout,
    pub blend: bool,
    pub vertex_bindings: &'a [vk::VertexInputBindingDescription],
    pub vertex_attributes: &'a [vk::VertexInputAttributeDescription],
}

pub fn pipeline(gpu: &Gpu, desc: &PipelineDesc) -> Result<vk::Pipeline> {
    let d = &gpu.device;
    let vs = shader(gpu, desc.vert)?;
    let fs = shader(gpu, desc.frag)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main"),
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main"),
    ];
    let vi = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(desc.vertex_bindings)
        .vertex_attribute_descriptions(desc.vertex_attributes);
    let ia = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let vp = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
    let rs = vk::PipelineRasterizationStateCreateInfo::default().cull_mode(vk::CullModeFlags::NONE).line_width(1.0);
    let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let att = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(desc.blend)
        .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&att);
    let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let ds = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
    let formats = [desc.format];
    let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vi)
        .input_assembly_state(&ia)
        .viewport_state(&vp)
        .rasterization_state(&rs)
        .multisample_state(&ms)
        .color_blend_state(&cb)
        .dynamic_state(&ds)
        .layout(desc.layout)
        .push_next(&mut rendering);
    let p = unsafe { d.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }.map_err(|(_, e)| e)?[0];
    unsafe {
        d.destroy_shader_module(vs, None);
        d.destroy_shader_module(fs, None);
    }
    Ok(p)
}

/// A push-descriptor set layout of `n` combined image samplers, for the fragment shader.
pub fn sampler_set_layout(gpu: &Gpu, n: u32) -> Result<vk::DescriptorSetLayout> {
    let bindings: Vec<vk::DescriptorSetLayoutBinding> = (0..n)
        .map(|i| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(i)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        })
        .collect();
    Ok(unsafe {
        gpu.device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default()
                .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR)
                .bindings(&bindings),
            None,
        )
    }?)
}

pub fn linear_sampler(gpu: &Gpu) -> Result<vk::Sampler> {
    Ok(unsafe {
        gpu.device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )
    }?)
}

/// Push `views` as combined image samplers at bindings 0.. of set 0.
pub fn push_samplers(gpu: &Gpu, cmd: vk::CommandBuffer, layout: vk::PipelineLayout, sampler: vk::Sampler, views: &[vk::ImageView]) {
    let infos: Vec<[vk::DescriptorImageInfo; 1]> = views
        .iter()
        .map(|&v| {
            [vk::DescriptorImageInfo::default().sampler(sampler).image_view(v).image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        })
        .collect();
    let writes: Vec<vk::WriteDescriptorSet> = infos
        .iter()
        .enumerate()
        .map(|(i, info)| {
            vk::WriteDescriptorSet::default()
                .dst_binding(i as u32)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(info)
        })
        .collect();
    let pd = gpu.push_descriptor.as_ref().expect("push descriptors are required");
    unsafe { pd.cmd_push_descriptor_set(cmd, vk::PipelineBindPoint::GRAPHICS, layout, 0, &writes) };
}

pub fn wait_timeline(gpu: &Gpu, sem: vk::Semaphore, value: u64, timeout_ns: u64) -> Result<bool> {
    let sems = [sem];
    let values = [value];
    match unsafe { gpu.device.wait_semaphores(&vk::SemaphoreWaitInfo::default().semaphores(&sems).values(&values), timeout_ns) } {
        Ok(()) => Ok(true),
        Err(vk::Result::TIMEOUT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn timeline_semaphore(gpu: &Gpu) -> Result<vk::Semaphore> {
    let mut t = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(0);
    Ok(unsafe { gpu.device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut t), None) }?)
}

/// The SPIR-V the crate embeds (shaders/build.sh makes it).
pub mod spv {
    pub const FULLSCREEN_VERT: &[u8] = include_bytes!("../../shaders/fullscreen.vert.spv");
    pub const CONVERT_FRAG: &[u8] = include_bytes!("../../shaders/convert.frag.spv");
    pub const BLIT_FRAG: &[u8] = include_bytes!("../../shaders/blit.frag.spv");
    pub const UI_VERT: &[u8] = include_bytes!("../../shaders/ui.vert.spv");
    pub const UI_FRAG: &[u8] = include_bytes!("../../shaders/ui.frag.spv");
}
