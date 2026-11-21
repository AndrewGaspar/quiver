//! Stage A, on the decode thread: a decoded picture into an R'G'B' image the presenter
//! samples.
//!
//! A Vulkan Video picture is sampled where the decoder wrote it (zero-copy): per-plane views
//! of its multi-planar image, waited on and signalled through the frame's own timeline
//! semaphore, as libav asks. A picture in memory (software, or NVDEC/VA-API copied down) is
//! copied into a staging buffer and uploaded to plane images first.
//!
//! The pass writes one of four [`SLOTS`] (A2B10G10R10, the stream's size) and signals the
//! converter's timeline; the presenter's submit waits on that value. Converting at once,
//! rather than when the presenter next draws, hands the decoder's image back after a pass of
//! a fraction of a millisecond, so a presenter waiting for a vblank (FIFO) never holds up
//! the next decode. A slot is reused only once both its last conversion and the last present
//! that read it are done ([`SlotReads`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use ash::vk;

use super::Gpu;
use super::util::{self, Buffer, Image, PipelineDesc, barrier, spv};
use crate::av::Picture;
use crate::color::YuvParams;

pub const SLOTS: usize = 4;
pub const PICTURE_FORMAT: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;

/// The presenter's timeline value of its last submit that read each slot, and the
/// presenter's timeline semaphore. Shared between the two stages.
pub struct SlotReads {
    pub values: [AtomicU64; SLOTS],
    pub semaphore: vk::Semaphore,
}

/// A converted picture, ready for the presenter once `value` is signalled on `semaphore`.
#[derive(Debug, Clone, Copy)]
pub struct Converted {
    pub slot: usize,
    pub view: vk::ImageView,
    pub extent: vk::Extent2D,
    pub semaphore: vk::Semaphore,
    pub value: u64,
    /// PQ (HDR10) content.
    pub pq: bool,
}

struct Upload {
    key: (u32, u32, u32, u32, u32, u32),
    staging: Buffer,
    planes: Vec<Image>,
}

struct Slot {
    image: Option<Image>,
    cmd: vk::CommandBuffer,
    last_value: u64,
    upload: Option<Upload>,
    /// The test pattern's corner, copied out for `--measure`.
    readback: Option<(Buffer, vk::Extent2D)>,
}

pub struct Converter {
    gpu: Arc<Gpu>,
    pool: vk::CommandPool,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    timeline: vk::Semaphore,
    value: u64,
    slots: Vec<Slot>,
    next: usize,
    reads: Arc<SlotReads>,
    /// Per-plane views of decoder images, by VkImage.
    views: HashMap<u64, (Vec<vk::ImageView>, vk::Format)>,
    /// Copy the test pattern's corner out of every picture.
    pub readback: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConvertPush {
    m: [[f32; 4]; 4],
    uv_max: [f32; 2],
    planes: i32,
}

fn as_bytes<T: Copy>(t: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(t as *const T as *const u8, std::mem::size_of::<T>()) }
}

/// The plane view formats of a multi-planar format, and the code scale of its samples.
fn plane_formats(f: vk::Format) -> Option<(Vec<vk::Format>, f32, u32)> {
    use vk::Format as F;
    Some(match f {
        F::G8_B8R8_2PLANE_420_UNORM | F::G8_B8R8_2PLANE_422_UNORM | F::G8_B8R8_2PLANE_444_UNORM => {
            (vec![F::R8_UNORM, F::R8G8_UNORM], 255.0, 8)
        }
        F::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
        | F::G10X6_B10X6R10X6_2PLANE_422_UNORM_3PACK16
        | F::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16 => (vec![F::R10X6_UNORM_PACK16, F::R10X6G10X6_UNORM_2PACK16], 1023.0, 10),
        F::G12X4_B12X4R12X4_2PLANE_420_UNORM_3PACK16 | F::G12X4_B12X4R12X4_2PLANE_444_UNORM_3PACK16 => {
            (vec![F::R12X4_UNORM_PACK16, F::R12X4G12X4_UNORM_2PACK16], 4095.0, 12)
        }
        F::G16_B16R16_2PLANE_420_UNORM | F::G16_B16R16_2PLANE_444_UNORM => (vec![F::R16_UNORM, F::R16G16_UNORM], 65535.0, 16),
        F::G8_B8_R8_3PLANE_420_UNORM | F::G8_B8_R8_3PLANE_444_UNORM => (vec![F::R8_UNORM; 3], 255.0, 8),
        _ => return None,
    })
}

const PLANE_ASPECTS: [vk::ImageAspectFlags; 3] =
    [vk::ImageAspectFlags::PLANE_0, vk::ImageAspectFlags::PLANE_1, vk::ImageAspectFlags::PLANE_2];

impl Converter {
    pub fn new(gpu: Arc<Gpu>) -> Result<Self> {
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
            d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(SLOTS as u32))
        }?;
        let set_layout = util::sampler_set_layout(&gpu, 3)?;
        let ranges =
            [vk::PushConstantRange::default().stage_flags(vk::ShaderStageFlags::FRAGMENT).size(std::mem::size_of::<ConvertPush>() as u32)];
        let layouts = [set_layout];
        let layout = unsafe {
            d.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts).push_constant_ranges(&ranges), None)
        }?;
        let pipeline = util::pipeline(
            &gpu,
            &PipelineDesc {
                vert: spv::FULLSCREEN_VERT,
                frag: spv::CONVERT_FRAG,
                format: PICTURE_FORMAT,
                layout,
                blend: false,
                vertex_bindings: &[],
                vertex_attributes: &[],
            },
        )?;
        let sampler = util::linear_sampler(&gpu)?;
        let timeline = util::timeline_semaphore(&gpu)?;
        let reads = Arc::new(SlotReads { values: Default::default(), semaphore: util::timeline_semaphore(&gpu)? });
        let slots = cmds.into_iter().map(|cmd| Slot { image: None, cmd, last_value: 0, upload: None, readback: None }).collect();
        Ok(Self {
            gpu,
            pool,
            set_layout,
            layout,
            pipeline,
            sampler,
            timeline,
            value: 0,
            slots,
            next: 0,
            reads,
            views: HashMap::new(),
            readback: false,
        })
    }

    /// What the presenter needs to say it has read a slot.
    pub fn reads(&self) -> Arc<SlotReads> {
        self.reads.clone()
    }

    /// Wait until the last conversion is done on the GPU (for measuring decode latency).
    pub fn wait_done(&self, c: &Converted, timeout_ns: u64) -> Result<bool> {
        util::wait_timeline(&self.gpu, c.semaphore, c.value, timeout_ns)
    }

    /// Read the test pattern from a converted picture (`readback` must be on). Waits for the
    /// conversion.
    pub fn read_pattern(&mut self, c: &Converted) -> Result<Option<quiver_stream_proto::stamp::Stamp>> {
        if !self.wait_done(c, 1_000_000_000)? {
            bail!("a conversion didn't finish in 1 s");
        }
        let Some((buf, r)) = self.slots[c.slot].readback.as_mut() else {
            return Ok(None);
        };
        let (w, h) = (c.extent.width, c.extent.height);
        let rw = r.width as usize;
        let px = buf.bytes();
        // A2B10G10R10: red in the low 10 bits; the pattern is grey, so red is its luma.
        Ok(quiver_stream_proto::stamp::read(w, h, |x, y| {
            let o = (y as usize * rw + x as usize) * 4;
            match px.get(o..o + 4) {
                Some(b) => ((u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0x3ff) >> 2) as u8,
                None => 0,
            }
        }))
    }

    /// Forget the decoder's image views (its frame pool is going away).
    pub fn forget_decoder_images(&mut self) {
        unsafe { self.gpu.device.queue_wait_idle(*self.gpu.convert_queue.lock().unwrap()) }.ok();
        for (_, (views, _)) in self.views.drain() {
            for v in views {
                unsafe { self.gpu.device.destroy_image_view(v, None) };
            }
        }
    }

    /// Take the next slot: wait for its previous conversion and the presenter's last read.
    fn take_slot(&mut self, extent: vk::Extent2D) -> Result<usize> {
        let s = self.next;
        self.next = (self.next + 1) % SLOTS;
        let last = self.slots[s].last_value;
        if last > 0 && !util::wait_timeline(&self.gpu, self.timeline, last, 1_000_000_000)? {
            bail!("a picture conversion didn't finish in 1 s");
        }
        let read = self.reads.values[s].load(Ordering::Acquire);
        if read > 0 && !util::wait_timeline(&self.gpu, self.reads.semaphore, read, 1_000_000_000)? {
            bail!("the presenter held a picture for 1 s");
        }
        let slot = &mut self.slots[s];
        if slot.image.as_ref().is_none_or(|i| i.extent != extent) {
            if let Some(old) = slot.image.take() {
                old.destroy(&self.gpu);
            }
            slot.image = Some(Image::new(
                &self.gpu,
                PICTURE_FORMAT,
                extent,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC,
            )?);
        }
        if self.readback {
            let (rw, rh) = quiver_stream_proto::stamp::layout(extent.width, extent.height).map_or((1, 1), |l| l.size());
            let r = vk::Extent2D { width: rw.min(extent.width), height: rh.min(extent.height) };
            if slot.readback.as_ref().is_none_or(|(_, e)| *e != r) {
                if let Some((b, _)) = slot.readback.take() {
                    b.destroy(&self.gpu);
                }
                slot.readback = Some((Buffer::new(&self.gpu, (r.width * r.height * 4) as usize, vk::BufferUsageFlags::TRANSFER_DST)?, r));
            }
        }
        Ok(s)
    }

    /// Convert a decoded picture. `hdr` and `full_range` are the stream's (Configure).
    pub fn convert(&mut self, pic: &mut Picture, hdr: bool, full_range: bool) -> Result<Converted> {
        let extent = vk::Extent2D { width: pic.width(), height: pic.height() };
        let s = self.take_slot(extent)?;
        if pic.is_vulkan() { self.convert_vulkan(s, pic, hdr, full_range) } else { self.convert_cpu(s, pic, hdr, full_range) }
    }

    fn begin(&self, s: usize) -> Result<vk::CommandBuffer> {
        let cmd = self.slots[s].cmd;
        unsafe {
            self.gpu.device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            self.gpu
                .device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
        }
        Ok(cmd)
    }

    /// The conversion pass itself, into slot `s`.
    fn record_pass(&self, cmd: vk::CommandBuffer, s: usize, views: &[vk::ImageView], push: &ConvertPush) {
        let d = &self.gpu.device;
        let img = self.slots[s].image.as_ref().unwrap();
        let to_attach = barrier(
            img.image,
            (vk::ImageLayout::UNDEFINED, vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::NONE),
            (
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            ),
        );
        unsafe { d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[to_attach])) };
        let att = [vk::RenderingAttachmentInfo::default()
            .image_view(img.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)];
        let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: img.extent };
        unsafe {
            d.cmd_begin_rendering(cmd, &vk::RenderingInfo::default().render_area(area).layer_count(1).color_attachments(&att));
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            d.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: img.extent.width as f32,
                    height: img.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            d.cmd_set_scissor(cmd, 0, &[area]);
        }
        // Three bindings always; a 2-plane picture repeats its CbCr view.
        let v3 = [views[0], views[1], *views.get(2).unwrap_or(&views[1])];
        util::push_samplers(&self.gpu, cmd, self.layout, self.sampler, &v3);
        unsafe {
            d.cmd_push_constants(cmd, self.layout, vk::ShaderStageFlags::FRAGMENT, 0, as_bytes(push));
            d.cmd_draw(cmd, 3, 1, 0, 0);
            d.cmd_end_rendering(cmd);
        }
        let mut from = (
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        );
        if let (true, Some((buf, r))) = (self.readback, self.slots[s].readback.as_ref()) {
            let to_src = (vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_READ);
            unsafe {
                d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[barrier(img.image, from, to_src)]))
            };
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
                .image_extent(vk::Extent3D { width: r.width, height: r.height, depth: 1 });
            unsafe { d.cmd_copy_image_to_buffer(cmd, img.image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf.buffer, &[region]) };
            from = to_src;
        }
        let to_read = barrier(
            img.image,
            from,
            (vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_SAMPLED_READ),
        );
        unsafe { d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[to_read])) };
    }

    fn submit(
        &mut self,
        s: usize,
        cmd: vk::CommandBuffer,
        wait: Option<(vk::Semaphore, u64)>,
        extra_signal: Option<(vk::Semaphore, u64)>,
    ) -> Result<u64> {
        unsafe { self.gpu.device.end_command_buffer(cmd) }?;
        self.value += 1;
        let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        let waits: Vec<vk::SemaphoreSubmitInfo> = wait
            .into_iter()
            .map(|(sem, v)| vk::SemaphoreSubmitInfo::default().semaphore(sem).value(v).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS))
            .collect();
        let mut signals = vec![
            vk::SemaphoreSubmitInfo::default().semaphore(self.timeline).value(self.value).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
        ];
        if let Some((sem, v)) = extra_signal {
            signals.push(vk::SemaphoreSubmitInfo::default().semaphore(sem).value(v).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS));
        }
        let submit = vk::SubmitInfo2::default().command_buffer_infos(&cmds).wait_semaphore_infos(&waits).signal_semaphore_infos(&signals);
        let q = self.gpu.convert_queue.lock().unwrap();
        unsafe { self.gpu.device.queue_submit2(*q, &[submit], vk::Fence::null()) }.context("submitting a conversion")?;
        self.slots[s].last_value = self.value;
        Ok(self.value)
    }

    fn converted(&self, s: usize, value: u64, pq: bool) -> Converted {
        let img = self.slots[s].image.as_ref().unwrap();
        Converted { slot: s, view: img.view, extent: img.extent, semaphore: self.timeline, value, pq }
    }

    fn convert_vulkan(&mut self, s: usize, pic: &mut Picture, hdr: bool, full_range: bool) -> Result<Converted> {
        let mut info = pic.vk_lock().context("locking a Vulkan frame")?;
        let result = (|| -> Result<(u64, bool)> {
            if info.nb_images != 1 {
                bail!("the decoder's frames have {} images; one multi-planar image is supported", info.nb_images);
            }
            if info.img_flags & vk::ImageCreateFlags::MUTABLE_FORMAT.as_raw() == 0 {
                bail!("the decoder's images aren't MUTABLE_FORMAT, so their planes can't be viewed");
            }
            let format = vk::Format::from_raw(info.format);
            let (fmts, code_scale, depth) = plane_formats(format).with_context(|| format!("decoder image format {format:?}"))?;
            let image = vk::Handle::from_raw(info.image);
            let views = match self.views.get(&info.image) {
                Some((v, f)) if *f == format => v.clone(),
                _ => {
                    let mut v = Vec::new();
                    for (i, f) in fmts.iter().enumerate() {
                        let mut usage = vk::ImageViewUsageCreateInfo::default().usage(vk::ImageUsageFlags::SAMPLED);
                        let ci = vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(*f)
                            .subresource_range(
                                vk::ImageSubresourceRange::default().aspect_mask(PLANE_ASPECTS[i]).level_count(1).layer_count(1),
                            )
                            .push_next(&mut usage);
                        v.push(unsafe { self.gpu.device.create_image_view(&ci, None) }?);
                    }
                    self.views.insert(info.image, (v.clone(), format));
                    v
                }
            };
            let params = YuvParams::pick(hdr, full_range, pic.color(), depth, code_scale);
            let push = ConvertPush {
                m: params.matrix(),
                uv_max: [pic.width() as f32 / info.width.max(1) as f32, pic.height() as f32 / info.height.max(1) as f32],
                planes: fmts.len() as i32,
            };
            let cmd = self.begin(s)?;
            let (src_q, dst_q) = if info.queue_family == vk::QUEUE_FAMILY_IGNORED {
                (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED)
            } else {
                (info.queue_family, self.gpu.graphics_family)
            };
            let b = vk::ImageMemoryBarrier2::default()
                .image(image)
                .old_layout(vk::ImageLayout::from_raw(info.layout))
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::from_raw(info.access))
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .src_queue_family_index(src_q)
                .dst_queue_family_index(dst_q)
                .subresource_range(
                    vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1),
                );
            unsafe { self.gpu.device.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[b])) };
            self.record_pass(cmd, s, &views, &push);
            let sem: vk::Semaphore = vk::Handle::from_raw(info.semaphore);
            let v = self.submit(s, cmd, Some((sem, info.sem_value)), Some((sem, info.sem_value + 1)))?;
            info.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw();
            info.access = vk::AccessFlags2::SHADER_SAMPLED_READ.as_raw();
            info.sem_value += 1;
            info.queue_family = dst_q;
            Ok((v, params.pq))
        })();
        pic.vk_unlock(&info);
        let (v, pq) = result?;
        Ok(self.converted(s, v, pq))
    }

    fn convert_cpu(&mut self, s: usize, pic: &mut Picture, hdr: bool, full_range: bool) -> Result<Converted> {
        let l = pic.layout();
        let (w, h) = (pic.width(), pic.height());
        let (cw, ch) = (w.div_ceil(1 << l.log2_chroma_w), h.div_ceil(1 << l.log2_chroma_h));
        let (r1, r2) =
            if l.bytes == 2 { (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM) } else { (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM) };
        // (format, width, height, bytes per pixel) per plane.
        let planes: Vec<(vk::Format, u32, u32, u32)> = if l.planes == 2 {
            vec![(r1, w, h, l.bytes), (r2, cw, ch, 2 * l.bytes)]
        } else {
            vec![(r1, w, h, l.bytes), (r1, cw, ch, l.bytes), (r1, cw, ch, l.bytes)]
        };
        let total: usize = planes.iter().map(|p| (p.1 * p.2 * p.3) as usize).sum();
        let key = (w, h, l.planes, l.bytes, l.log2_chroma_w, l.log2_chroma_h);
        let gpu = self.gpu.clone();
        let slot = &mut self.slots[s];
        if slot.upload.as_ref().is_none_or(|u| u.key != key) {
            if let Some(u) = slot.upload.take() {
                u.staging.destroy(&gpu);
                for p in &u.planes {
                    p.destroy(&gpu);
                }
            }
            let staging = Buffer::new(&gpu, total, vk::BufferUsageFlags::TRANSFER_SRC)?;
            let mut imgs = Vec::new();
            for p in &planes {
                imgs.push(Image::new(
                    &gpu,
                    p.0,
                    vk::Extent2D { width: p.1, height: p.2 },
                    vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
                )?);
            }
            slot.upload = Some(Upload { key, staging, planes: imgs });
        }
        let up = slot.upload.as_mut().unwrap();
        // Rows into the staging buffer, tightly packed.
        let mut offsets = Vec::new();
        let mut off = 0usize;
        let dst = up.staging.bytes();
        for (i, p) in planes.iter().enumerate() {
            let (src, stride, rows) = pic.plane(i).context("a picture plane")?;
            let row = (p.1 * p.3) as usize;
            for y in 0..(p.2 as usize).min(rows) {
                dst[off + y * row..off + (y + 1) * row].copy_from_slice(&src[y * stride..y * stride + row]);
            }
            offsets.push(off);
            off += row * p.2 as usize;
        }
        let views: Vec<vk::ImageView> = up.planes.iter().map(|p| p.view).collect();
        let images: Vec<vk::Image> = up.planes.iter().map(|p| p.image).collect();
        let buffer = up.staging.buffer;
        let code_scale = if l.bytes == 2 { 65535.0 / (1u32 << l.shift) as f32 } else { 255.0 };
        let params = YuvParams::pick(hdr, full_range, pic.color(), l.depth, code_scale);
        let push = ConvertPush { m: params.matrix(), uv_max: [1.0, 1.0], planes: l.planes as i32 };
        let cmd = self.begin(s)?;
        let d = &self.gpu.device;
        let pre: Vec<_> = images
            .iter()
            .map(|&i| {
                barrier(
                    i,
                    (vk::ImageLayout::UNDEFINED, vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::NONE),
                    (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_WRITE),
                )
            })
            .collect();
        unsafe { d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&pre)) };
        for (i, p) in planes.iter().enumerate() {
            let region = vk::BufferImageCopy::default()
                .buffer_offset(offsets[i] as u64)
                .image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
                .image_extent(vk::Extent3D { width: p.1, height: p.2, depth: 1 });
            unsafe { d.cmd_copy_buffer_to_image(cmd, buffer, images[i], vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]) };
        }
        let post: Vec<_> = images
            .iter()
            .map(|&i| {
                barrier(
                    i,
                    (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_WRITE),
                    (
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::PipelineStageFlags2::FRAGMENT_SHADER,
                        vk::AccessFlags2::SHADER_SAMPLED_READ,
                    ),
                )
            })
            .collect();
        unsafe { d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&post)) };
        self.record_pass(cmd, s, &views, &push);
        let v = self.submit(s, cmd, None, None)?;
        Ok(self.converted(s, v, params.pq))
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        let gpu = self.gpu.clone();
        unsafe { gpu.device.queue_wait_idle(*gpu.convert_queue.lock().unwrap()) }.ok();
        self.forget_decoder_images();
        unsafe {
            for s in self.slots.drain(..) {
                if let Some(i) = s.image {
                    i.destroy(&gpu);
                }
                if let Some(u) = s.upload {
                    u.staging.destroy(&gpu);
                    for p in &u.planes {
                        p.destroy(&gpu);
                    }
                }
                if let Some((b, _)) = s.readback {
                    b.destroy(&gpu);
                }
            }
            gpu.device.destroy_pipeline(self.pipeline, None);
            gpu.device.destroy_pipeline_layout(self.layout, None);
            gpu.device.destroy_descriptor_set_layout(self.set_layout, None);
            gpu.device.destroy_sampler(self.sampler, None);
            gpu.device.destroy_semaphore(self.timeline, None);
            gpu.device.destroy_semaphore(self.reads.semaphore, None);
            gpu.device.destroy_command_pool(self.pool, None);
        }
    }
}
