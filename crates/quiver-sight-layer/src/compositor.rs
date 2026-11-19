//! Per-swapchain compositor resources and the per-present draw (SIGHT-HUD §2.8).
//!
//! Per present on the target swapchain:
//! 1. take a free *slot* (command buffer, fence, slice of the vertex buffer) without waiting:
//!    if every slot is still in flight, skip this present;
//! 2. if `hud.image`'s publish counter moved and the staging buffer isn't being read by the
//!    GPU, copy the new image into it (validated by quiver-perf) and record a copy into the
//!    texture, using `desc.stride` as the row pitch;
//! 3. if there's a current, fresh image laid out for this extent: one render pass that loads
//!    the swapchain image, a textured quad at the descriptor's position and opacity, and the
//!    live graph in each graph rectangle;
//! 4. submit, waiting on the app's present semaphores and signalling the semaphore of this
//!    image index, which the present then waits on instead.
//!
//! While the Sight panel's backdrop request in `hud.image` is live (or its fade-out runs),
//! the backdrop blur ([`crate::blur`]) goes into the same command buffer before step 3, so
//! the HUD is drawn sharp over the blurred game. A swapchain the HUD can't draw on (HDR) still
//! gets a compositor when it can be blurred.
//!
//! While the Sight panel is open in the game ([`crate::panel`], design/SIGHT.md §14), its
//! `panel.image` goes through a second plane the same way (staging buffer, texture, two
//! descriptor sets; only the rows that changed are copied when the image says which), drawn
//! after the HUD in a second render pass, with the panel's cursor on top; a present that drew
//! it tells the input preload ([`crate::panel::mark_drawn`]).
//!
//! Everything is allocated when the swapchain is created, except the staging buffers and the
//! textures (on the first image, and a texture again only when an image outgrows it) and the
//! blur's images (the first time a panel asks).

use std::ptr::NonNull;
use std::sync::Arc;

use ash::vk;
use quiver_perf::HudBufferHeader;
use quiver_perf::hud::{BackdropRequest, HudPoll};
use quiver_perf::layout::{BACKDROP_ERR_INTERNAL, DRAW_ERR_DEVICE_LOST, DRAW_ERR_INTERNAL, DRAW_ERR_NO_IMAGE, DRAW_ERR_QUEUE, HUD_MAX_BUFFER_BYTES, HUD_MAX_DIM, PANEL_MAX_DIM};

use crate::blur::{self, BlurPipes, Chain, Fade};
use crate::formats::Encoding;
use crate::graph::{self, History, VERTS_PER_GRAPH, Vertex};
use crate::log;
use crate::panel::{self as sight_panel, CURSOR_VERTS};
use crate::pipelines::{PUSH_SRGB_DECODE, Pipelines, Push};
use crate::state::DeviceData;
use crate::telemetry::{self, HUD};

/// Most wait semaphores a present may carry for us to draw (more: pass through).
pub const MAX_WAITS: usize = 16;
const VB_SLOT_BYTES: usize = (2 * VERTS_PER_GRAPH + CURSOR_VERTS) * std::mem::size_of::<Vertex>();
/// Where the cursor's vertices start in a slot's part of the vertex buffer.
const VB_CURSOR_OFFSET: usize = 2 * VERTS_PER_GRAPH * std::mem::size_of::<Vertex>();
/// Texture sizes grow in steps of this, so a HUD that changes size a little doesn't
/// reallocate.
const TEX_STEP: u32 = 256;

/// What a present got from the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drawn {
    /// This present only: every slot in flight, or the command pool is changing queue
    /// family. Nothing drawn, state unchanged.
    Skipped,
    /// `semaphore`: work submitted, the present waits on it (else pass the app's semaphores
    /// through); what happened to the HUD; whether the game was blurred.
    Done { semaphore: Option<vk::Semaphore>, hud: Hud, blurred: bool },
}

/// The Sight panel's plane (`panel.image`): like the HUD's, with partial uploads.
#[derive(Default)]
struct PanelPlane {
    desc_pool: vk::DescriptorPool,
    sets: [vk::DescriptorSet; 2],
    cur_set: usize,
    staging: Option<Buffer>,
    staging_reader: Option<usize>,
    texture: Option<Texture>,
    retired: Option<(Texture, u64)>,
    /// In staging, not yet in the texture; with the rows when only those changed.
    pending: Option<(HudBufferHeader, Option<(u32, u32)>)>,
    current: Option<HudBufferHeader>,
    /// The publish the texture holds whole (0: none), for partial uploads.
    have: u64,
    pending_published: u64,
    generation: u64,
    last_published: u64,
}

impl Drawn {
    fn nothing(reason: u32) -> Drawn {
        Drawn::Done { semaphore: None, hud: Hud::Nothing(reason), blurred: false }
    }
}

/// The HUD's part of a present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hud {
    Drawn,
    /// The current image has opacity 0: the HUD is toggled off.
    Off,
    /// Not drawn (`DRAW_ERR_*` reason).
    Nothing(u32),
}

struct Buffer {
    buf: vk::Buffer,
    mem: vk::DeviceMemory,
    ptr: NonNull<u8>,
    size: usize,
}

struct Texture {
    image: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
    w: u32,
    h: u32,
    /// Holds an image (layout SHADER_READ_ONLY_OPTIMAL); else UNDEFINED.
    filled: bool,
}

struct Slot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// The fence belongs to a submission that went through (a failed submit leaves it reset
    /// and never signalled).
    submitted: bool,
    /// That submission wrote the blur's timestamps for this slot.
    timed: bool,
}

pub struct Compositor {
    pipes: Arc<Pipelines>,
    /// How the HUD is blended; `None`: the HUD can't draw here (`hud_error`), only the blur.
    enc: Option<Encoding>,
    hud_error: u32,
    format: vk::Format,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    /// Signalled by our submit for image `i`, waited on by its present. Reusing semaphore `i`
    /// when image `i` comes back is safe: the image was re-acquired, so its previous present
    /// (and that wait) completed.
    semaphores: Vec<vk::Semaphore>,
    slots: Vec<Slot>,
    next_slot: usize,
    pool: Option<(u32, vk::CommandPool)>,
    vertices: Buffer,
    desc_pool: vk::DescriptorPool,
    /// Two sets, so a grown texture gets a set no submission in flight uses.
    desc_sets: [vk::DescriptorSet; 2],
    /// The set bound to `texture`.
    cur_set: usize,
    staging: Option<Buffer>,
    /// The slot whose submission last read the staging buffer.
    staging_reader: Option<usize>,
    texture: Option<Texture>,
    /// A texture replaced by a bigger one, kept until the slots in flight when it was
    /// replaced (a bit mask) are done with it.
    retired: Option<(Texture, u64)>,
    /// An image in the staging buffer, not yet copied to the texture.
    pending: Option<HudBufferHeader>,
    /// The image in the texture.
    current: Option<HudBufferHeader>,
    hud_generation: u64,
    last_published: u64,
    /// Take the current hud.image buffer again (the texture lost its contents).
    reupload: bool,
    last_queue: vk::Queue,
    /// Presents skipped because every slot was in flight.
    pub skipped: u64,
    /// Why the backdrop blur can't work here (`BACKDROP_ERR_*`), 0 when it can.
    blur_error: u32,
    /// The blur's pipelines and images, once a panel asked.
    blur: Option<(Arc<BlurPipes>, Chain)>,
    fade: Fade,
    /// The last live request (its dim and radius carry the fade-out).
    backdrop: BackdropRequest,
    panel: PanelPlane,
    /// Captures (crate::hdr): the images have `TRANSFER_SRC`.
    capture_ok: bool,
    color_space: vk::ColorSpaceKHR,
    swapchain_id: u32,
    /// A capture in flight: its buffer, request number and the slot whose submit fills it.
    capture: Option<(Buffer, u64, usize)>,
}

// SAFETY: the mapped pointers are only used under the swapchain's mutex.
unsafe impl Send for Compositor {}

impl Compositor {
    /// `hud`: the HUD's encoding, or why it can't draw here; `blur`: whether the backdrop
    /// blur can (the swapchain has `SAMPLED` usage), or why not.
    ///
    /// # Safety
    /// `images` are the swapchain's images; `d` is its live device.
    pub unsafe fn new(d: &DeviceData, images: &[vk::Image], format: vk::Format, extent: vk::Extent2D, hud: Result<Encoding, u32>, blur: Result<(), u32>) -> Result<Compositor, vk::Result> {
        let pipes = d.pipelines_for(format)?;
        let mut c = Compositor {
            pipes,
            enc: hud.ok(),
            hud_error: hud.err().unwrap_or(0),
            format,
            extent,
            images: images.to_vec(),
            views: Vec::with_capacity(images.len()),
            framebuffers: Vec::with_capacity(images.len()),
            semaphores: Vec::with_capacity(images.len()),
            slots: Vec::new(),
            next_slot: 0,
            pool: None,
            vertices: Buffer { buf: vk::Buffer::null(), mem: vk::DeviceMemory::null(), ptr: NonNull::dangling(), size: 0 },
            desc_pool: vk::DescriptorPool::null(),
            desc_sets: [vk::DescriptorSet::null(); 2],
            cur_set: 0,
            staging: None,
            staging_reader: None,
            texture: None,
            retired: None,
            pending: None,
            current: None,
            hud_generation: u64::MAX,
            last_published: 0,
            reupload: false,
            last_queue: vk::Queue::null(),
            skipped: 0,
            blur_error: blur.err().unwrap_or(0),
            blur: None,
            fade: Fade::default(),
            backdrop: BackdropRequest::NONE,
            panel: PanelPlane { generation: u64::MAX, ..Default::default() },
            capture_ok: false,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            swapchain_id: 0,
            capture: None,
        };
        // SAFETY: as documented; on error everything made so far is destroyed (nothing is in
        // flight yet).
        match unsafe { c.build(d, images, format) } {
            Ok(()) => Ok(c),
            Err(e) => {
                unsafe { c.destroy_now(d) };
                Err(e)
            }
        }
    }

    unsafe fn build(&mut self, d: &DeviceData, images: &[vk::Image], format: vk::Format) -> Result<(), vk::Result> {
        let dev = &d.dev;
        // SAFETY: valid create infos; handles kept for destroy.
        unsafe {
            for &img in images {
                let view = dev.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(img)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(color_range()),
                    None,
                )?;
                self.views.push(view);
                let att = [view];
                let fb = dev.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.pipes.render_pass)
                        .attachments(&att)
                        .width(self.extent.width)
                        .height(self.extent.height)
                        .layers(1),
                    None,
                )?;
                self.framebuffers.push(fb);
                self.semaphores.push(dev.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?);
            }
            // Images in flight plus one, so a free slot is normally there without waiting.
            for _ in 0..images.len() + 1 {
                let fence = dev.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)?;
                self.slots.push(Slot { cmd: vk::CommandBuffer::null(), fence, submitted: false, timed: false });
            }
            self.vertices = make_buffer(d, (self.slots.len() * VB_SLOT_BYTES) as u64, vk::BufferUsageFlags::VERTEX_BUFFER)?;
            let sizes = [vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: 2 }];
            self.desc_pool = dev.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(2).pool_sizes(&sizes), None)?;
            let layouts = [self.pipes.set_layout; 2];
            let sets = dev.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.desc_pool).set_layouts(&layouts))?;
            self.desc_sets = [sets[0], sets[1]];
            self.panel.desc_pool = dev.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(2).pool_sizes(&sizes), None)?;
            let sets = dev.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.panel.desc_pool).set_layouts(&layouts))?;
            self.panel.sets = [sets[0], sets[1]];
        }
        Ok(())
    }

    /// Fence of slot `i` signalled (its work is done). `Err` on device loss.
    unsafe fn idle(&self, dev: &ash::Device, i: usize) -> Result<bool, u32> {
        if !self.slots[i].submitted {
            return Ok(true);
        }
        // SAFETY: our fence.
        match unsafe { dev.get_fence_status(self.slots[i].fence) } {
            Ok(done) => Ok(done),
            Err(vk::Result::ERROR_DEVICE_LOST) => Err(DRAW_ERR_DEVICE_LOST),
            Err(_) => Err(DRAW_ERR_INTERNAL),
        }
    }

    unsafe fn all_idle(&self, dev: &ash::Device) -> Result<bool, u32> {
        for i in 0..self.slots.len() {
            // SAFETY: our fences.
            if !unsafe { self.idle(dev, i)? } {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The command pool for `family`, created on the first present (the family isn't known
    /// before). `None` if it must change while work is in flight.
    unsafe fn ensure_pool(&mut self, d: &DeviceData, family: u32) -> Result<Option<()>, u32> {
        let dev = &d.dev;
        if let Some((f, pool)) = self.pool {
            if f == family {
                return Ok(Some(()));
            }
            // SAFETY: our objects; idle checked before destroying.
            unsafe {
                if !self.all_idle(dev)? {
                    return Ok(None);
                }
                dev.destroy_command_pool(pool, None);
            }
            self.pool = None;
            // The texture is EXCLUSIVE to the old family: its contents are undefined on the
            // new one without an ownership transfer, so upload the current image again.
            if let Some(t) = self.texture.as_mut() {
                t.filled = false;
            }
            self.current = None;
            self.reupload = true;
            if let Some(t) = self.panel.texture.as_mut() {
                t.filled = false;
            }
            self.panel.current = None;
            self.panel.have = 0;
            self.panel.generation = u64::MAX;
        }
        let internal = |_| DRAW_ERR_INTERNAL;
        // SAFETY: valid create info; command buffers get the loader's dispatch pointer.
        unsafe {
            let pool = dev
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default().flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER).queue_family_index(family),
                    None,
                )
                .map_err(internal)?;
            let n = self.slots.len() as u32;
            let cmds = match dev.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(n),
            ) {
                Ok(c) => c,
                Err(_) => {
                    dev.destroy_command_pool(pool, None);
                    return Err(DRAW_ERR_INTERNAL);
                }
            };
            for (s, cmd) in self.slots.iter_mut().zip(cmds) {
                // Command buffers made below the loader need its dispatch pointer.
                if let Some(set) = d.set_loader_data
                    && set(d.handle, vk::Handle::as_raw(cmd) as usize as *mut _) != vk::Result::SUCCESS
                {
                    dev.destroy_command_pool(pool, None);
                    return Err(DRAW_ERR_INTERNAL);
                }
                s.cmd = cmd;
            }
            self.pool = Some((family, pool));
        }
        Ok(Some(()))
    }

    /// Makes sure the texture can hold `w × h`. The first one is made right away; a bigger
    /// one goes into the other descriptor set, and the old one is retired until the
    /// submissions in flight now are done with it (never waiting for them). `false`: not now
    /// (a previous retirement is still pending).
    unsafe fn ensure_texture(&mut self, d: &DeviceData, w: u32, h: u32) -> Result<bool, u32> {
        if self.texture.as_ref().is_some_and(|t| t.w >= w && t.h >= h) {
            return Ok(true);
        }
        if self.retired.is_some() {
            return Ok(false);
        }
        let dev = &d.dev;
        let step = |v: u32| v.div_ceil(TEX_STEP).saturating_mul(TEX_STEP).clamp(TEX_STEP, HUD_MAX_DIM);
        // SAFETY: our objects; the set written is one no pending submission uses (the other
        // set when a texture exists, and when none exists nothing has drawn yet).
        unsafe {
            let t = make_texture(d, self.enc.unwrap_or(Encoding::Unorm).texture_format(), step(w), step(h)).map_err(|_| DRAW_ERR_INTERNAL)?;
            let set = if self.texture.is_some() { 1 - self.cur_set } else { self.cur_set };
            let info = [vk::DescriptorImageInfo { sampler: self.pipes.sampler, image_view: t.view, image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL }];
            let write = [vk::WriteDescriptorSet::default()
                .dst_set(self.desc_sets[set])
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info)];
            dev.update_descriptor_sets(&write, &[]);
            if let Some(old) = self.texture.replace(t) {
                let mut busy = 0u64;
                for i in 0..self.slots.len().min(64) {
                    if !self.idle(dev, i)? {
                        busy |= 1 << i;
                    }
                }
                if busy == 0 {
                    destroy_texture(dev, &old);
                } else {
                    self.retired = Some((old, busy));
                }
            }
            self.cur_set = set;
        }
        Ok(true)
    }

    /// Frees a retired texture once the slots that might use it have finished.
    unsafe fn reap_retired(&mut self, dev: &ash::Device) -> Result<(), u32> {
        let Some((_, mut busy)) = self.retired else { return Ok(()) };
        for i in 0..self.slots.len().min(64) {
            // SAFETY: our fences.
            if busy & (1 << i) != 0 && unsafe { self.idle(dev, i)? } {
                busy &= !(1 << i);
            }
        }
        if busy == 0 {
            let (old, _) = self.retired.take().unwrap();
            // SAFETY: nothing in flight uses it any more.
            unsafe { destroy_texture(dev, &old) };
        } else if let Some(r) = self.retired.as_mut() {
            r.1 = busy;
        }
        Ok(())
    }

    /// Step 2: picks up a new `hud.image` buffer into staging if one was published and staging
    /// is free. Returns whether the HUD image is fresh (heartbeat within 3 s), and the panel's
    /// backdrop request.
    unsafe fn poll_hud(&mut self, d: &DeviceData, now: i64) -> Result<(bool, BackdropRequest), u32> {
        // The heartbeat thread holds the lock only while (re)opening the file: then skip.
        let mut hud = match HUD.try_lock() {
            Ok(h) => h,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok((self.current.is_some(), self.backdrop)),
        };
        let req = hud.reader.as_ref().map_or(BackdropRequest::NONE, |r| r.backdrop(now));
        if self.enc.is_none() {
            return Ok((false, req));
        }
        // SAFETY: as documented.
        unsafe { self.poll_image(d, now, &mut hud).map(|fresh| (fresh, req)) }
    }

    unsafe fn poll_image(&mut self, d: &DeviceData, now: i64, hud: &mut telemetry::Hud) -> Result<bool, u32> {
        if hud.generation != self.hud_generation || self.reupload {
            self.reupload = false;
            self.hud_generation = hud.generation;
            self.pending = None;
            self.current = None;
            self.last_published = 0;
            if let Some(r) = hud.reader.as_mut() {
                r.reset();
            }
        }
        let Some(reader) = hud.reader.as_mut() else { return Ok(false) };
        let fresh = !reader.is_stale(now);
        let published = reader.published();
        if published == self.last_published || self.pending.is_some() {
            return Ok(fresh);
        }
        // Staging must not be read by a submission still in flight.
        let dev = &d.dev;
        // SAFETY: our fences.
        unsafe {
            if let Some(s) = self.staging_reader
                && !self.idle(dev, s)?
            {
                return Ok(fresh);
            }
        }
        if self.staging.is_none() {
            // The largest buffer any hud.image can have, so a replaced file never needs more.
            // SAFETY: a fresh buffer on our device.
            self.staging = Some(unsafe { make_buffer(d, HUD_MAX_BUFFER_BYTES as u64, vk::BufferUsageFlags::TRANSFER_SRC) }.map_err(|_| DRAW_ERR_INTERNAL)?);
        }
        let staging = self.staging.as_ref().unwrap();
        // SAFETY: the staging buffer is mapped for `size` bytes, host-coherent, and not in use.
        let dst = unsafe { std::slice::from_raw_parts_mut(staging.ptr.as_ptr(), staging.size) };
        match reader.poll((self.extent.width, self.extent.height), dst) {
            HudPoll::Image { published, desc } => {
                self.last_published = published;
                self.pending = Some(desc);
                if let Some(t) = telemetry::get() {
                    t.hud_published.store(published, std::sync::atomic::Ordering::Relaxed);
                }
            }
            HudPoll::Invalid(e) => {
                // A bad (or other-extent) image isn't drawn; wait for the next publish.
                self.last_published = published;
                self.current = None;
                log!("hud.image rejected: {e}");
            }
            HudPoll::Unchanged => self.last_published = published,
            HudPoll::Busy | HudPoll::Torn => {}
        }
        Ok(fresh)
    }

    /// The panel's texture can hold `w × h` (as [`Compositor::ensure_texture`], its own sets).
    unsafe fn ensure_panel_texture(&mut self, d: &DeviceData, w: u32, h: u32) -> Result<bool, u32> {
        if self.panel.texture.as_ref().is_some_and(|t| t.w >= w && t.h >= h) {
            return Ok(true);
        }
        if self.panel.retired.is_some() {
            return Ok(false);
        }
        let dev = &d.dev;
        let step = |v: u32| v.div_ceil(TEX_STEP).saturating_mul(TEX_STEP).clamp(TEX_STEP, PANEL_MAX_DIM);
        // SAFETY: as ensure_texture, with the panel's sets.
        unsafe {
            let t = make_texture(d, self.enc.unwrap_or(Encoding::Unorm).texture_format(), step(w), step(h)).map_err(|_| DRAW_ERR_INTERNAL)?;
            let set = if self.panel.texture.is_some() { 1 - self.panel.cur_set } else { self.panel.cur_set };
            let info = [vk::DescriptorImageInfo { sampler: self.pipes.sampler, image_view: t.view, image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL }];
            let write = [vk::WriteDescriptorSet::default().dst_set(self.panel.sets[set]).dst_binding(0).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&info)];
            dev.update_descriptor_sets(&write, &[]);
            if let Some(old) = self.panel.texture.replace(t) {
                let mut busy = 0u64;
                for i in 0..self.slots.len().min(64) {
                    if !self.idle(dev, i)? {
                        busy |= 1 << i;
                    }
                }
                if busy == 0 {
                    destroy_texture(dev, &old);
                } else {
                    self.panel.retired = Some((old, busy));
                }
            }
            self.panel.cur_set = set;
            // A new texture holds nothing yet.
            self.panel.have = 0;
        }
        Ok(true)
    }

    unsafe fn reap_panel_retired(&mut self, dev: &ash::Device) -> Result<(), u32> {
        let Some((_, mut busy)) = self.panel.retired else { return Ok(()) };
        for i in 0..self.slots.len().min(64) {
            // SAFETY: our fences.
            if busy & (1 << i) != 0 && unsafe { self.idle(dev, i)? } {
                busy &= !(1 << i);
            }
        }
        if busy == 0 {
            let (old, _) = self.panel.retired.take().unwrap();
            // SAFETY: nothing in flight uses it any more.
            unsafe { destroy_texture(dev, &old) };
        } else if let Some(r) = self.panel.retired.as_mut() {
            r.1 = busy;
        }
        Ok(())
    }

    /// Picks up a new `panel.image` into the panel's staging buffer (only the changed rows when
    /// the texture holds the image they changed from). Returns whether the image is fresh.
    unsafe fn poll_panel(&mut self, d: &DeviceData, now: i64) -> Result<bool, u32> {
        let mut img = match sight_panel::IMAGE.try_lock() {
            Ok(h) => h,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(self.panel.current.is_some()),
        };
        if img.generation != self.panel.generation {
            self.panel.generation = img.generation;
            self.panel.pending = None;
            self.panel.current = None;
            self.panel.last_published = 0;
            self.panel.have = 0;
            if let Some(r) = img.reader.as_mut() {
                r.reset();
            }
        }
        let Some(reader) = img.reader.as_mut() else { return Ok(false) };
        let fresh = !reader.is_stale(now);
        let published = reader.published();
        if published == self.panel.last_published || self.panel.pending.is_some() {
            return Ok(fresh);
        }
        let dev = &d.dev;
        // SAFETY: our fences and buffers.
        unsafe {
            if let Some(s) = self.panel.staging_reader
                && !self.idle(dev, s)?
            {
                return Ok(fresh);
            }
            let need = reader.info().buffer_bytes as usize;
            if self.panel.staging.as_ref().is_some_and(|b| b.size < need) {
                // Not in use (checked above): a bigger file (a bigger swapchain) needs more.
                let old = self.panel.staging.take().unwrap();
                destroy_buffer(dev, &old);
                self.panel.staging_reader = None;
                self.panel.have = 0;
            }
            if self.panel.staging.is_none() {
                self.panel.staging = Some(make_buffer(d, need as u64, vk::BufferUsageFlags::TRANSFER_SRC).map_err(|_| DRAW_ERR_INTERNAL)?);
                // Fresh staging holds nothing: the next image is copied whole.
                self.panel.have = 0;
            }
        }
        let staging = self.panel.staging.as_ref().unwrap();
        // SAFETY: mapped for `size` bytes, host-coherent, not in use.
        let dst = unsafe { std::slice::from_raw_parts_mut(staging.ptr.as_ptr(), staging.size) };
        match reader.poll_since((self.extent.width, self.extent.height), dst, self.panel.have) {
            (HudPoll::Image { published, desc }, rows) => {
                self.panel.last_published = published;
                self.panel.pending = Some((desc, rows));
                self.panel.pending_published = published;
            }
            (HudPoll::Invalid(e), _) => {
                self.panel.last_published = published;
                self.panel.current = None;
                log!("panel.image rejected: {e}");
            }
            (HudPoll::Unchanged, _) => self.panel.last_published = published,
            (HudPoll::Busy | HudPoll::Torn, _) => {}
        }
        Ok(fresh)
    }

    unsafe fn record_panel_copy(&mut self, dev: &ash::Device, cmd: vk::CommandBuffer, p: &HudBufferHeader, rows: Option<(u32, u32)>) {
        let (Some(t), Some(s)) = (self.panel.texture.as_mut(), self.panel.staging.as_ref()) else { return };
        let (y0, y1) = rows.filter(|_| t.filled).unwrap_or((0, p.height));
        let to_dst = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(if t.filled { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL } else { vk::ImageLayout::UNDEFINED })
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(t.image)
            .subresource_range(color_range());
        let region = vk::BufferImageCopy {
            // Validated by quiver-perf: stride ≥ width × 4, a multiple of 4, rows within the
            // buffer; y0 < y1 ≤ height.
            buffer_offset: y0 as u64 * p.stride as u64,
            buffer_row_length: p.stride / 4,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 },
            image_offset: vk::Offset3D { x: 0, y: y0 as i32, z: 0 },
            image_extent: vk::Extent3D { width: p.width, height: y1 - y0, depth: 1 },
        };
        let to_read = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(t.image)
            .subresource_range(color_range());
        // SAFETY: recording into our command buffer; objects are ours.
        unsafe {
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_dst]);
            dev.cmd_copy_buffer_to_image(cmd, s.buf, t.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::DependencyFlags::empty(), &[], &[], &[to_read]);
        }
        t.filled = true;
        self.panel.have = self.panel.pending_published;
    }

    /// The panel's image and its cursor, in a render pass of their own after the HUD's.
    unsafe fn record_panel_draw(&self, dev: &ash::Device, cmd: vk::CommandBuffer, image: usize, slot: usize, c: &HudBufferHeader, cursor: Option<(i32, i32)>) {
        let (ew, eh) = (self.extent.width, self.extent.height);
        let full = vk::Rect2D { offset: vk::Offset2D::default(), extent: self.extent };
        let scissor = vk::Rect2D { offset: vk::Offset2D { x: c.x.max(0), y: c.y.max(0) }, extent: vk::Extent2D { width: c.width.min(ew), height: c.height.min(eh) } };
        let viewport = vk::Viewport { x: 0.0, y: 0.0, width: ew as f32, height: eh as f32, min_depth: 0.0, max_depth: 1.0 };
        let srgb = if self.enc == Some(Encoding::Srgb) { PUSH_SRGB_DECODE } else { 0 };
        let push = Push { extent: [ew as f32, eh as f32], origin: [c.x as f32, c.y as f32], size: [c.width as f32, c.height as f32], opacity: c.opacity, flags: srgb };
        let pc = vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT;
        let base = slot * VB_SLOT_BYTES + VB_CURSOR_OFFSET;
        if let Some(tip) = cursor {
            let mut v = [Vertex::default(); CURSOR_VERTS];
            sight_panel::cursor_vertices(tip, c.scale, &mut v);
            // SAFETY: this slot's part of the mapped vertex buffer (its fence is signalled).
            unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), self.vertices.ptr.as_ptr().add(base).cast::<Vertex>(), CURSOR_VERTS) };
        }
        // SAFETY: recording into our command buffer with our objects.
        unsafe {
            dev.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default().render_pass(self.pipes.render_pass).framebuffer(self.framebuffers[image]).render_area(full),
                vk::SubpassContents::INLINE,
            );
            dev.cmd_set_viewport(cmd, 0, &[viewport]);
            dev.cmd_set_scissor(cmd, 0, &[scissor]);
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipes.quad);
            dev.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipes.layout, 0, &[self.panel.sets[self.panel.cur_set]], &[]);
            dev.cmd_push_constants(cmd, self.pipes.layout, pc, 0, push.bytes());
            dev.cmd_draw(cmd, 4, 1, 0, 0);
            if cursor.is_some() {
                let cpush = Push { opacity: 1.0, ..push };
                dev.cmd_set_scissor(cmd, 0, &[full]);
                dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipes.graph);
                dev.cmd_push_constants(cmd, self.pipes.layout, pc, 0, cpush.bytes());
                dev.cmd_bind_vertex_buffers(cmd, 0, &[self.vertices.buf], &[base as u64]);
                dev.cmd_draw(cmd, CURSOR_VERTS as u32, 1, 0, 0);
            }
            dev.cmd_end_render_pass(cmd);
        }
    }

    /// Captures can be taken here (the swapchain was made with `TRANSFER_SRC`).
    pub fn set_capture(&mut self, ok: bool, color_space: vk::ColorSpaceKHR, swapchain_id: u32) {
        self.capture_ok = ok;
        self.color_space = color_space;
        self.swapchain_id = swapchain_id;
    }

    pub fn can_capture(&self) -> bool {
        self.capture_ok
    }

    /// A finished capture's bytes to [`crate::hdr::CAPTURED`] (id 0: a frame kept while
    /// holding, to [`crate::hdr::RETAINED`]), once its submit is done.
    unsafe fn reap_capture(&mut self, dev: &ash::Device) -> Result<(), u32> {
        let Some((_, _, slot)) = &self.capture else { return Ok(()) };
        // SAFETY: our fence.
        if !unsafe { self.idle(dev, *slot)? } {
            return Ok(());
        }
        let (buf, id, _) = self.capture.take().unwrap();
        let bpp = crate::hdr::bytes_per_pixel(self.format).unwrap_or(4);
        let len = (self.extent.width * self.extent.height * bpp) as usize;
        // SAFETY: the mapped, coherent buffer of `len` bytes or more; the GPU is done with it.
        let data = unsafe { std::slice::from_raw_parts(buf.ptr.as_ptr(), len.min(buf.size)) }.to_vec();
        // SAFETY: not in use any more.
        unsafe { destroy_buffer(dev, &buf) };
        let c = crate::hdr::Captured {
            id,
            format: self.format,
            color_space: self.color_space,
            width: self.extent.width,
            height: self.extent.height,
            bytes_per_pixel: bpp,
            swapchain: self.swapchain_id,
            data,
        };
        if id == 0 {
            *crate::hdr::RETAINED.lock().unwrap_or_else(|p| p.into_inner()) = Some((c, std::time::Instant::now()));
        } else {
            *crate::hdr::CAPTURED.lock().unwrap_or_else(|p| p.into_inner()) = Some(c);
        }
        Ok(())
    }

    /// Records the copy of `image` (in `PRESENT_SRC_KHR`, the game's pixels) into a new host
    /// buffer, leaving it in `PRESENT_SRC_KHR` for what follows.
    unsafe fn record_capture(&mut self, d: &DeviceData, cmd: vk::CommandBuffer, image: usize) -> Option<Buffer> {
        let dev = &d.dev;
        let bpp = crate::hdr::bytes_per_pixel(self.format)?;
        let size = self.extent.width as u64 * self.extent.height as u64 * bpp as u64;
        // SAFETY: a valid create; ours to destroy.
        let buf = match unsafe { make_buffer(d, size, vk::BufferUsageFlags::TRANSFER_DST) } {
            Ok(b) => b,
            Err(e) => {
                log!("capture buffer ({size} bytes): {e:?}");
                return None;
            }
        };
        let to_src = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.images[image])
            .subresource_range(color_range());
        let back = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.images[image])
            .subresource_range(color_range());
        let host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(buf.buf)
            .size(vk::WHOLE_SIZE);
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 })
            .image_extent(vk::Extent3D { width: self.extent.width, height: self.extent.height, depth: 1 });
        // SAFETY: recording into our command buffer; the image is the app's presented one.
        unsafe {
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_src]);
            dev.cmd_copy_image_to_buffer(cmd, self.images[image], vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf.buf, &[region]);
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::ALL_COMMANDS | vk::PipelineStageFlags::HOST, vk::DependencyFlags::empty(), &[], &[host], &[back]);
        }
        Some(buf)
    }

    /// The HUD can be drawn here (and so can the panel): a known blend encoding.
    pub fn can_draw_images(&self) -> bool {
        self.enc.is_some()
    }

    /// The per-present work (module docs). `waits` are the app's present semaphores.
    ///
    /// # Safety
    /// Called from vkQueuePresentKHR with the app's valid arguments; `d` owns `queue`.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn draw(
        &mut self,
        d: &DeviceData,
        queue: vk::Queue,
        family: u32,
        image_index: u32,
        waits: &[vk::Semaphore],
        present_id: Option<u64>,
        history: &History,
        now: i64,
    ) -> Result<Drawn, u32> {
        let dev = &d.dev;
        let image = image_index as usize;
        if image >= self.framebuffers.len() {
            return Ok(Drawn::nothing(DRAW_ERR_INTERNAL));
        }
        if !d.families.get(family as usize).is_some_and(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS)) {
            // Presenting from a queue that can't draw: nothing this time (the game may
            // present from another queue later).
            return Ok(Drawn::nothing(DRAW_ERR_QUEUE));
        }
        if waits.len() > MAX_WAITS {
            return Ok(Drawn::nothing(DRAW_ERR_INTERNAL));
        }
        // SAFETY: our objects throughout; the queue is externally synchronized by the app's
        // present call, which we are inside.
        unsafe {
            if self.ensure_pool(d, family)?.is_none() {
                return Ok(Drawn::Skipped);
            }
            let mut slot = None;
            for k in 0..self.slots.len() {
                let i = (self.next_slot + k) % self.slots.len();
                if self.idle(dev, i)? {
                    slot = Some(i);
                    break;
                }
            }
            let Some(slot) = slot else {
                self.skipped += 1;
                return Ok(Drawn::Skipped);
            };
            // The texture and staging are ordered by queue submission order and barriers on
            // one queue. When the app presents from another queue, wait (by skipping) until
            // the other queue's work is done.
            if self.last_queue != vk::Queue::null() && self.last_queue != queue && !self.all_idle(dev)? {
                self.skipped += 1;
                return Ok(Drawn::Skipped);
            }

            self.reap_capture(dev)?;
            self.reap_retired(dev)?;
            self.reap_panel_retired(dev)?;
            // The slot's last blur is done (its fence signalled): its GPU time.
            if std::mem::take(&mut self.slots[slot].timed)
                && let (Some((_, chain)), Some(period)) = (&self.blur, d.timestamps(family))
                && let Some(ns) = chain.elapsed_ns(dev, slot, period)
            {
                telemetry::note_blur_gpu(ns);
            }
            let (fresh, req) = self.poll_hud(d, now)?;

            // The backdrop blur: fade toward the panel's request; build on first use.
            let live = req.is_live(now);
            if live {
                self.backdrop = req;
            }
            let alpha = if self.blur_error == 0 { self.fade.step(live, now) } else { 0.0 };
            let blur = alpha > 0.0 && self.ensure_blur(d, family);

            let mut copy = None;
            if let Some(p) = self.pending
                && self.ensure_texture(d, p.width, p.height)?
            {
                copy = Some(p);
                self.current = self.pending.take();
            }
            let visible = self.current.filter(|c| {
                fresh && c.opacity > 0.0 && c.extent_w == self.extent.width && c.extent_h == self.extent.height && self.texture.as_ref().is_some_and(|t| t.filled || copy.is_some())
            });
            let hud = if self.enc.is_none() {
                Hud::Nothing(self.hud_error)
            } else if visible.is_some() {
                Hud::Drawn
            } else if fresh && self.current.is_some_and(|c| c.opacity == 0.0) {
                Hud::Off
            } else {
                Hud::Nothing(DRAW_ERR_NO_IMAGE)
            };
            // The Sight panel, while it's open in the game.
            let open = if self.enc.is_some() { sight_panel::open_now() } else { None };
            let panel_fresh = open.is_some() && self.poll_panel(d, now)?;
            let mut panel_copy = None;
            if open.is_some()
                && let Some((p, rows)) = self.panel.pending
            {
                let ready = if rows.is_some() { self.panel.texture.as_ref().is_some_and(|t| t.filled) } else { self.ensure_panel_texture(d, p.width, p.height)? };
                if ready {
                    panel_copy = Some((p, rows));
                    self.panel.current = Some(p);
                    self.panel.pending = None;
                }
            }
            let panel_shown = open.zip(self.panel.current.filter(|c| {
                panel_fresh && c.opacity > 0.0 && c.extent_w == self.extent.width && c.extent_h == self.extent.height && self.panel.texture.as_ref().is_some_and(|t| t.filled || panel_copy.is_some())
            }));
            // A capture asked for (crate::hdr) and none in flight.
            let want = crate::hdr::REQUEST.load(std::sync::atomic::Ordering::Relaxed);
            // Holding (the game's clock is paused or stepped): every frame is kept, since the
            // last one before the hold is what a capture must show (crate::hdr).
            let hold = crate::hdr::HOLD.load(std::sync::atomic::Ordering::Relaxed);
            let capture = self.capture_ok && self.capture.is_none() && (hold || want > crate::hdr::TAKEN.load(std::sync::atomic::Ordering::Relaxed));
            if visible.is_none() && copy.is_none() && !blur && panel_shown.is_none() && panel_copy.is_none() && !capture {
                return Ok(Drawn::Done { semaphore: None, hud, blurred: false });
            }

            let cmd = self.slots[slot].cmd;
            let fail = |_| DRAW_ERR_INTERNAL;
            dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).map_err(fail)?;
            dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).map_err(fail)?;
            // First, so it holds the game's pixels without the blur, HUD or panel.
            let captured = if capture { self.record_capture(d, cmd, image) } else { None };
            if capture && !hold {
                // Taken even when the buffer couldn't be made: no retry on every present.
                crate::hdr::TAKEN.store(want, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(p) = copy {
                self.record_copy(dev, cmd, &p);
                self.staging_reader = Some(slot);
            }
            if let Some((p, rows)) = panel_copy {
                self.record_panel_copy(dev, cmd, &p, rows);
                self.panel.staging_reader = Some(slot);
            }
            let mut timed = false;
            if blur && let Some((pipes, chain)) = &self.blur {
                let plan = blur::plan((self.extent.width, self.extent.height), self.backdrop.radius_fraction());
                chain.record(dev, pipes, cmd, image, self.images[image], self.framebuffers[image], slot, &plan, alpha, self.backdrop.dim_fraction());
                timed = chain.queries.is_some();
            }
            if let Some(c) = visible {
                self.record_draw(dev, cmd, image, slot, &c, history);
            }
            if let Some((o, c)) = &panel_shown {
                self.record_panel_draw(dev, cmd, image, slot, c, o.cursor);
            }
            dev.end_command_buffer(cmd).map_err(fail)?;

            let stages = [vk::PipelineStageFlags::ALL_COMMANDS; MAX_WAITS];
            let cmds = [cmd];
            let signal = [self.semaphores[image]];
            let mut latency = vk::LatencySubmissionPresentIdNV::default().present_id(present_id.unwrap_or(0));
            let mut submit = vk::SubmitInfo::default().wait_semaphores(waits).wait_dst_stage_mask(&stages[..waits.len()]).command_buffers(&cmds).signal_semaphores(&signal);
            if d.low_latency2 && present_id.is_some() {
                submit = submit.push_next(&mut latency);
            }
            let fence = self.slots[slot].fence;
            self.slots[slot].submitted = false;
            dev.reset_fences(&[fence]).map_err(fail)?;
            match dev.queue_submit(queue, &[submit], fence) {
                Ok(()) => {
                    self.slots[slot].submitted = true;
                    self.slots[slot].timed = timed;
                    if let Some(b) = captured {
                        if hold {
                            // Kept now, not at a later present: a held game may not present
                            // again. Waiting here costs a frame's copy, only while holding.
                            self.capture = Some((b, 0, slot));
                            if dev.wait_for_fences(&[fence], true, 2_000_000_000).is_ok() {
                                self.reap_capture(dev)?;
                            }
                        } else {
                            self.capture = Some((b, want, slot));
                        }
                    }
                    if let Some((o, _)) = panel_shown {
                        sight_panel::mark_drawn(o.epoch, now);
                    }
                }
                Err(vk::Result::ERROR_DEVICE_LOST) => return Err(DRAW_ERR_DEVICE_LOST),
                Err(_) => return Err(DRAW_ERR_INTERNAL),
            }
            self.next_slot = (slot + 1) % self.slots.len();
            self.last_queue = queue;
            Ok(Drawn::Done { semaphore: Some(signal[0]), hud, blurred: blur })
        }
    }

    /// Why the backdrop blur can't work on this swapchain (`BACKDROP_ERR_*`), 0 when it can.
    pub fn blur_error(&self) -> u32 {
        self.blur_error
    }

    /// The blur's pipelines and images, made the first time a panel asks. `false` (and the
    /// blur off for this swapchain, the status saying why) if they can't be made.
    unsafe fn ensure_blur(&mut self, d: &DeviceData, family: u32) -> bool {
        if self.blur.is_some() {
            return true;
        }
        if self.blur_error != 0 {
            return false;
        }
        let extent = (self.extent.width, self.extent.height);
        let made = d.blur_pipes_for(self.format).and_then(|p| {
            // SAFETY: the swapchain's views (it has SAMPLED usage: blur_error is 0), our device.
            unsafe { Chain::new(d, &p, &self.views, extent, self.slots.len(), d.timestamps(family).is_some()) }.map(|c| (p, c))
        });
        match made {
            Ok(b) => {
                log!("backdrop blur set up for {}x{} {:?}", extent.0, extent.1, self.format);
                self.blur = Some(b);
                true
            }
            Err(e) => {
                log!("backdrop blur setup failed: {e:?}");
                self.blur_error = BACKDROP_ERR_INTERNAL;
                if let Some(t) = telemetry::get() {
                    t.blur_error.store(BACKDROP_ERR_INTERNAL, std::sync::atomic::Ordering::Relaxed);
                }
                false
            }
        }
    }

    unsafe fn record_copy(&mut self, dev: &ash::Device, cmd: vk::CommandBuffer, p: &HudBufferHeader) {
        let (Some(t), Some(s)) = (self.texture.as_mut(), self.staging.as_ref()) else { return };
        let to_dst = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(if t.filled { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL } else { vk::ImageLayout::UNDEFINED })
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(t.image)
            .subresource_range(color_range());
        let region = vk::BufferImageCopy {
            buffer_offset: 0,
            // quiver-perf validated stride ≥ width × 4, a multiple of 4, and stride × height
            // within the buffer (= staging size).
            buffer_row_length: p.stride / 4,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 },
            image_offset: vk::Offset3D::default(),
            image_extent: vk::Extent3D { width: p.width, height: p.height, depth: 1 },
        };
        let to_read = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(t.image)
            .subresource_range(color_range());
        // SAFETY: recording into our command buffer; objects are ours.
        unsafe {
            // Earlier submissions on this queue may still sample the texture: wait for their
            // fragment shading before overwriting it.
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_dst]);
            dev.cmd_copy_buffer_to_image(cmd, s.buf, t.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::DependencyFlags::empty(), &[], &[], &[to_read]);
        }
        t.filled = true;
    }

    unsafe fn record_draw(&self, dev: &ash::Device, cmd: vk::CommandBuffer, image: usize, slot: usize, c: &HudBufferHeader, history: &History) {
        let (ew, eh) = (self.extent.width, self.extent.height);
        // Inside the swapchain: validated against this extent by quiver-perf.
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: c.x.max(0), y: c.y.max(0) },
            extent: vk::Extent2D { width: c.width.min(ew), height: c.height.min(eh) },
        };
        let viewport = vk::Viewport { x: 0.0, y: 0.0, width: ew as f32, height: eh as f32, min_depth: 0.0, max_depth: 1.0 };
        let push = Push {
            extent: [ew as f32, eh as f32],
            origin: [c.x as f32, c.y as f32],
            size: [c.width as f32, c.height as f32],
            opacity: c.opacity,
            flags: if self.enc == Some(Encoding::Srgb) { PUSH_SRGB_DECODE } else { 0 },
        };
        // Graph vertices into this slot's part of the vertex buffer (not in use: its fence
        // is signalled).
        let base = slot * VB_SLOT_BYTES;
        // SAFETY: the vertex buffer is mapped and sized slots × VB_SLOT_BYTES, Vertex-aligned.
        let out = unsafe { std::slice::from_raw_parts_mut(self.vertices.ptr.as_ptr().add(base).cast::<Vertex>(), 2 * VERTS_PER_GRAPH) };
        let mut n = 0;
        for g in c.graphs() {
            n += graph::build(history, g, (c.x, c.y), c.scale.max(1.0), &mut out[n..n + VERTS_PER_GRAPH]);
        }
        let pc = vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT;
        // SAFETY: recording into our command buffer with our objects.
        unsafe {
            dev.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.pipes.render_pass)
                    .framebuffer(self.framebuffers[image])
                    .render_area(vk::Rect2D { offset: vk::Offset2D::default(), extent: self.extent }),
                vk::SubpassContents::INLINE,
            );
            dev.cmd_set_viewport(cmd, 0, &[viewport]);
            dev.cmd_set_scissor(cmd, 0, &[scissor]);
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipes.quad);
            dev.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipes.layout, 0, &[self.desc_sets[self.cur_set]], &[]);
            dev.cmd_push_constants(cmd, self.pipes.layout, pc, 0, push.bytes());
            dev.cmd_draw(cmd, 4, 1, 0, 0);
            if n > 0 {
                dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipes.graph);
                dev.cmd_push_constants(cmd, self.pipes.layout, pc, 0, push.bytes());
                dev.cmd_bind_vertex_buffers(cmd, 0, &[self.vertices.buf], &[base as u64]);
                dev.cmd_draw(cmd, n as u32, 1, 0, 0);
            }
            dev.cmd_end_render_pass(cmd);
        }
    }

    /// Destroys everything after waiting (bounded) for our submissions. If they don't finish
    /// in time, the objects are leaked rather than destroyed while in use.
    ///
    /// Returns false if it leaked them (the device's shared objects must then stay too).
    ///
    /// # Safety
    /// The swapchain is being destroyed; `d` is its device.
    pub unsafe fn destroy(mut self, d: &DeviceData) -> bool {
        let fences: Vec<vk::Fence> = self.slots.iter().filter(|s| s.submitted).map(|s| s.fence).collect();
        // SAFETY: our fences.
        let done = fences.is_empty() || unsafe { d.dev.wait_for_fences(&fences, true, 1_000_000_000) }.is_ok();
        if done {
            // SAFETY: idle now.
            unsafe { self.destroy_now(d) };
        } else {
            log!("compositor work still pending at swapchain destroy; leaking its objects");
            self.slots.clear();
        }
        done
    }

    unsafe fn destroy_now(&mut self, d: &DeviceData) {
        let dev = &d.dev;
        // A present may still be waiting on the per-image semaphores (our fences cover only
        // our submits): the device destroys them later.
        d.retire_semaphores(self.semaphores.drain(..));
        // SAFETY: nothing of ours is in flight; null handles are ignored.
        unsafe {
            if let Some((_, pool)) = self.pool.take() {
                dev.destroy_command_pool(pool, None);
            }
            for s in self.slots.drain(..) {
                dev.destroy_fence(s.fence, None);
            }
            for f in self.framebuffers.drain(..) {
                dev.destroy_framebuffer(f, None);
            }
            for v in self.views.drain(..) {
                dev.destroy_image_view(v, None);
            }
            dev.destroy_descriptor_pool(self.desc_pool, None);
            dev.destroy_descriptor_pool(self.panel.desc_pool, None);
            destroy_buffer(dev, &self.vertices);
            for s in self.staging.take().into_iter().chain(self.panel.staging.take()) {
                destroy_buffer(dev, &s);
            }
            for t in self.panel.texture.take().into_iter().chain(self.panel.retired.take().map(|r| r.0)) {
                destroy_texture(dev, &t);
            }
            for t in self.texture.take().into_iter().chain(self.retired.take().map(|r| r.0)) {
                destroy_texture(dev, &t);
            }
            if let Some((_, mut c)) = self.blur.take() {
                c.destroy(dev);
            }
            if let Some((b, _, _)) = self.capture.take() {
                destroy_buffer(dev, &b);
            }
        }
    }
}

pub(crate) fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 }
}

/// A host-visible, coherent, persistently mapped buffer.
unsafe fn make_buffer(d: &DeviceData, size: u64, usage: vk::BufferUsageFlags) -> Result<Buffer, vk::Result> {
    let dev = &d.dev;
    // SAFETY: valid create infos; cleaned up on error.
    unsafe {
        let buf = dev.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE), None)?;
        let req = dev.get_buffer_memory_requirements(buf);
        let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let Some(ty) = d.memory_type(req.memory_type_bits, host, vk::MemoryPropertyFlags::empty()) else {
            dev.destroy_buffer(buf, None);
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        };
        let mem = match dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None) {
            Ok(m) => m,
            Err(e) => {
                dev.destroy_buffer(buf, None);
                return Err(e);
            }
        };
        let mapped = dev.bind_buffer_memory(buf, mem, 0).and_then(|_| dev.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()));
        match mapped.ok().and_then(|p| NonNull::new(p.cast::<u8>())) {
            Some(ptr) => Ok(Buffer { buf, mem, ptr, size: size as usize }),
            None => {
                dev.destroy_buffer(buf, None);
                dev.free_memory(mem, None);
                Err(vk::Result::ERROR_MEMORY_MAP_FAILED)
            }
        }
    }
}

unsafe fn destroy_buffer(dev: &ash::Device, b: &Buffer) {
    // SAFETY: not in use; freeing mapped memory unmaps it.
    unsafe {
        dev.destroy_buffer(b.buf, None);
        dev.free_memory(b.mem, None);
    }
}

unsafe fn make_texture(d: &DeviceData, format: vk::Format, w: u32, h: u32) -> Result<Texture, vk::Result> {
    let dev = &d.dev;
    // SAFETY: valid create infos; cleaned up on error.
    unsafe {
        let image = dev.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )?;
        let req = dev.get_image_memory_requirements(image);
        let Some(ty) = d.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::empty(), vk::MemoryPropertyFlags::DEVICE_LOCAL) else {
            dev.destroy_image(image, None);
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        };
        let mem = match dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None) {
            Ok(m) => m,
            Err(e) => {
                dev.destroy_image(image, None);
                return Err(e);
            }
        };
        let view = dev.bind_image_memory(image, mem, 0).and_then(|_| {
            dev.create_image_view(
                &vk::ImageViewCreateInfo::default().image(image).view_type(vk::ImageViewType::TYPE_2D).format(format).subresource_range(color_range()),
                None,
            )
        });
        match view {
            Ok(view) => Ok(Texture { image, mem, view, w, h, filled: false }),
            Err(e) => {
                dev.destroy_image(image, None);
                dev.free_memory(mem, None);
                Err(e)
            }
        }
    }
}

unsafe fn destroy_texture(dev: &ash::Device, t: &Texture) {
    // SAFETY: not in use.
    unsafe {
        dev.destroy_image_view(t.view, None);
        dev.destroy_image(t.image, None);
        dev.free_memory(t.mem, None);
    }
}
