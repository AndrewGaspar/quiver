//! The compositor's two pre-compiled pipelines (SIGHT-HUD §2.8): a textured quad for the HUD
//! image and coloured triangles for the graph, both premultiplied-alpha "over", in one render
//! pass that loads the swapchain image and leaves it in `PRESENT_SRC_KHR`. One set per device
//! and swapchain format.

use std::mem::size_of;

use ash::vk;

use crate::graph::Vertex;

const QUAD_VERT: &[u8] = include_bytes!("../shaders/quad.vert.spv");
const QUAD_FRAG: &[u8] = include_bytes!("../shaders/quad.frag.spv");
const GRAPH_VERT: &[u8] = include_bytes!("../shaders/graph.vert.spv");
const GRAPH_FRAG: &[u8] = include_bytes!("../shaders/graph.frag.spv");

/// `shaders/common.glsl`'s push constants.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Push {
    pub extent: [f32; 2],
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub opacity: f32,
    pub flags: u32,
}

pub const PUSH_SRGB_DECODE: u32 = 1;

impl Push {
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: repr(C), 32 bytes of f32/u32, no padding.
        unsafe { std::slice::from_raw_parts((self as *const Push).cast::<u8>(), size_of::<Push>()) }
    }
}

pub struct Pipelines {
    pub format: vk::Format,
    pub render_pass: vk::RenderPass,
    pub set_layout: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    pub sampler: vk::Sampler,
    pub quad: vk::Pipeline,
    pub graph: vk::Pipeline,
}

fn words(spv: &[u8]) -> Vec<u32> {
    spv.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

impl Pipelines {
    /// # Safety
    /// `dev` is a live device.
    pub unsafe fn new(dev: &ash::Device, format: vk::Format) -> Result<Pipelines, vk::Result> {
        let mut p = Pipelines {
            format,
            render_pass: vk::RenderPass::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            sampler: vk::Sampler::null(),
            quad: vk::Pipeline::null(),
            graph: vk::Pipeline::null(),
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

    unsafe fn build(&mut self, dev: &ash::Device) -> Result<(), vk::Result> {
        let attachment = [vk::AttachmentDescription {
            format: self.format,
            samples: vk::SampleCountFlags::TYPE_1,
            load_op: vk::AttachmentLoadOp::LOAD,
            store_op: vk::AttachmentStoreOp::STORE,
            stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
            stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
            initial_layout: vk::ImageLayout::PRESENT_SRC_KHR,
            final_layout: vk::ImageLayout::PRESENT_SRC_KHR,
            ..Default::default()
        }];
        let color_ref = [vk::AttachmentReference { attachment: 0, layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL }];
        let subpass = [vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&color_ref)];
        // The app's rendering reaches us through the semaphores our submit waits on (all
        // commands); these order our attachment access after it and before the present.
        let deps = [
            vk::SubpassDependency {
                src_subpass: vk::SUBPASS_EXTERNAL,
                dst_subpass: 0,
                src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                dst_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                dependency_flags: vk::DependencyFlags::empty(),
            },
            vk::SubpassDependency {
                src_subpass: 0,
                dst_subpass: vk::SUBPASS_EXTERNAL,
                src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                dst_stage_mask: vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                dst_access_mask: vk::AccessFlags::empty(),
                dependency_flags: vk::DependencyFlags::empty(),
            },
        ];
        let rp = vk::RenderPassCreateInfo::default().attachments(&attachment).subpasses(&subpass).dependencies(&deps);
        // SAFETY: valid create infos throughout; handles are stored for destroy().
        unsafe {
            self.render_pass = dev.create_render_pass(&rp, None)?;

            let binding = [vk::DescriptorSetLayoutBinding {
                binding: 0,
                descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::FRAGMENT,
                ..Default::default()
            }];
            self.set_layout = dev.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding), None)?;
            let push = [vk::PushConstantRange {
                stage_flags: vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                offset: 0,
                size: size_of::<Push>() as u32,
            }];
            let sets = [self.set_layout];
            self.layout = dev.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&sets).push_constant_ranges(&push), None)?;
            self.sampler = dev.create_sampler(
                &vk::SamplerCreateInfo {
                    mag_filter: vk::Filter::NEAREST,
                    min_filter: vk::Filter::NEAREST,
                    mipmap_mode: vk::SamplerMipmapMode::NEAREST,
                    address_mode_u: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    address_mode_v: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    address_mode_w: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    max_lod: 0.0,
                    ..Default::default()
                },
                None,
            )?;

            let module = |spv: &[u8]| {
                let code = words(spv);
                dev.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
            };
            let qv = module(QUAD_VERT)?;
            let qf = module(QUAD_FRAG);
            let gv = module(GRAPH_VERT);
            let gf = module(GRAPH_FRAG);
            let built = match (qf, gv, gf) {
                (Ok(qf), Ok(gv), Ok(gf)) => {
                    let r = self.build_pipelines(dev, [qv, qf, gv, gf]);
                    for m in [qf, gv, gf] {
                        dev.destroy_shader_module(m, None);
                    }
                    r
                }
                (a, b, c) => {
                    for m in [a, b, c].into_iter().flatten() {
                        dev.destroy_shader_module(m, None);
                    }
                    Err(vk::Result::ERROR_INITIALIZATION_FAILED)
                }
            };
            dev.destroy_shader_module(qv, None);
            built
        }
    }

    unsafe fn build_pipelines(&mut self, dev: &ash::Device, m: [vk::ShaderModule; 4]) -> Result<(), vk::Result> {
        let main = c"main";
        let stages = |v, f| {
            [
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(v).name(main),
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(f).name(main),
            ]
        };
        let quad_stages = stages(m[0], m[1]);
        let graph_stages = stages(m[2], m[3]);
        let no_input = vk::PipelineVertexInputStateCreateInfo::default();
        let vb = [vk::VertexInputBindingDescription { binding: 0, stride: size_of::<Vertex>() as u32, input_rate: vk::VertexInputRate::VERTEX }];
        let attrs = [
            vk::VertexInputAttributeDescription { location: 0, binding: 0, format: vk::Format::R32G32_SFLOAT, offset: 0 },
            vk::VertexInputAttributeDescription { location: 1, binding: 0, format: vk::Format::R8G8B8A8_UNORM, offset: 8 },
        ];
        let graph_input = vk::PipelineVertexInputStateCreateInfo::default().vertex_binding_descriptions(&vb).vertex_attribute_descriptions(&attrs);
        let strip = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
        let list = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // Premultiplied "over".
        let blend_att = [vk::PipelineColorBlendAttachmentState {
            blend_enable: vk::TRUE,
            src_color_blend_factor: vk::BlendFactor::ONE,
            dst_color_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            color_blend_op: vk::BlendOp::ADD,
            src_alpha_blend_factor: vk::BlendFactor::ONE,
            dst_alpha_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            alpha_blend_op: vk::BlendOp::ADD,
            color_write_mask: vk::ColorComponentFlags::RGBA,
        }];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_att);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dyn_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        macro_rules! base {
            ($st:expr) => {
                vk::GraphicsPipelineCreateInfo::default()
                    .stages($st)
                    .viewport_state(&viewport)
                    .rasterization_state(&raster)
                    .multisample_state(&ms)
                    .color_blend_state(&blend)
                    .dynamic_state(&dyn_state)
                    .layout(self.layout)
                    .render_pass(self.render_pass)
                    .subpass(0)
            };
        }
        let infos = [
            base!(&quad_stages).vertex_input_state(&no_input).input_assembly_state(&strip),
            base!(&graph_stages).vertex_input_state(&graph_input).input_assembly_state(&list),
        ];
        // SAFETY: valid create infos.
        let made = unsafe { dev.create_graphics_pipelines(vk::PipelineCache::null(), &infos, None) };
        match made {
            Ok(p) => {
                self.quad = p[0];
                self.graph = p[1];
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
            dev.destroy_pipeline(self.quad, None);
            dev.destroy_pipeline(self.graph, None);
            dev.destroy_sampler(self.sampler, None);
            dev.destroy_pipeline_layout(self.layout, None);
            dev.destroy_descriptor_set_layout(self.set_layout, None);
            dev.destroy_render_pass(self.render_pass, None);
        }
    }
}
