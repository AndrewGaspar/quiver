//! The Vulkan instance and device, shared by the presenter and libav's Vulkan Video decoder.
//!
//! The client makes the device itself, not libav, so that it can enable what presenting
//! needs (the swapchain, HDR metadata, present wait) next to what decoding needs, and then
//! hands it to libav ([`Gpu::libav_device`]). Every feature the device supports in the core
//! 1.1–1.3 structs is enabled (except robust buffer access), because libav reads the enabled
//! set to decide what it may use.
//!
//! Queues: the graphics family gets up to three queues. Index 0 is libav's (it may submit
//! layout transitions there), 1 is the decode thread's (the YUV→RGB pass) and 2 the present
//! thread's, so no queue is shared between threads and nothing needs a lock. A device with
//! fewer queues shares them behind a mutex ([`Gpu::convert_queue`]).

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use ash::vk;

use crate::av::{QsVkDevice, QsVkQueueFamily};

/// Extensions a device may have that this client turns on when present.
const OPTIONAL_DEVICE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_video_queue",
    c"VK_KHR_video_decode_queue",
    c"VK_KHR_video_decode_h264",
    c"VK_KHR_video_decode_h265",
    c"VK_KHR_video_decode_av1",
    c"VK_KHR_video_maintenance1",
    c"VK_KHR_push_descriptor",
    c"VK_EXT_hdr_metadata",
    c"VK_KHR_present_id",
    c"VK_KHR_present_wait",
    c"VK_KHR_external_memory_fd",
    c"VK_KHR_external_semaphore_fd",
    c"VK_EXT_external_memory_dma_buf",
    c"VK_EXT_image_drm_format_modifier",
];

/// A queue, behind a mutex when threads share it.
pub type Queue = Arc<Mutex<vk::Queue>>;

/// The feature chain, kept alive for libav (it reads it at init and later).
struct Features {
    f2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    _v11: Box<vk::PhysicalDeviceVulkan11Features<'static>>,
    _v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    _vm1: Box<vk::PhysicalDeviceVideoMaintenance1FeaturesKHR<'static>>,
    _pid: Box<vk::PhysicalDevicePresentIdFeaturesKHR<'static>>,
    _pw: Box<vk::PhysicalDevicePresentWaitFeaturesKHR<'static>>,
}

unsafe impl Send for Features {}
unsafe impl Sync for Features {}

pub struct Gpu {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical: vk::PhysicalDevice,
    pub device: ash::Device,
    pub name: String,
    pub graphics_family: u32,
    pub libav_queue: Queue,
    pub convert_queue: Queue,
    pub present_queue: Queue,
    pub memory: vk::PhysicalDeviceMemoryProperties,
    pub instance_extensions: Vec<CString>,
    pub device_extensions: Vec<CString>,
    /// Queue families with video decode, and the codec operations each supports.
    pub decode_families: Vec<(u32, vk::VideoCodecOperationFlagsKHR)>,
    /// (family, queue count) for libav, preferentially ordered.
    libav_families: Vec<QsVkQueueFamily>,
    pub push_descriptor: Option<ash::khr::push_descriptor::Device>,
    pub present_wait: Option<ash::khr::present_wait::Device>,
    pub hdr_metadata: Option<ash::ext::hdr_metadata::Device>,
    pub surface_fn: Option<ash::khr::surface::Instance>,
    pub swapchain_fn: Option<ash::khr::swapchain::Device>,
    pub swapchain_colorspace: bool,
    features: Features,
    _ext_ptrs: (Vec<*const c_char>, Vec<*const c_char>),
}

unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

fn has(list: &[vk::ExtensionProperties], name: &CStr) -> bool {
    list.iter().any(|e| e.extension_name_as_c_str().ok() == Some(name))
}

impl Gpu {
    /// The instance, with `surface_extensions` (what SDL asks for; empty for headless use).
    pub fn instance(surface_extensions: &[CString]) -> Result<(ash::Entry, ash::Instance, Vec<CString>)> {
        let entry = unsafe { ash::Entry::load() }.context("loading libvulkan")?;
        let available = unsafe { entry.enumerate_instance_extension_properties(None) }?;
        let mut exts: Vec<CString> = surface_extensions.to_vec();
        for e in [c"VK_EXT_swapchain_colorspace", c"VK_KHR_get_surface_capabilities2"] {
            if !surface_extensions.is_empty() && has(&available, e) {
                exts.push(e.to_owned());
            }
        }
        let mut layers: Vec<*const c_char> = Vec::new();
        if std::env::var_os("QUIVER_STREAM_VALIDATE").is_some() {
            layers.push(c"VK_LAYER_KHRONOS_validation".as_ptr());
        }
        let ptrs: Vec<*const c_char> = exts.iter().map(|e| e.as_ptr()).collect();
        let app = vk::ApplicationInfo::default()
            .application_name(c"quiver-stream")
            .engine_name(c"quiver")
            .api_version(vk::API_VERSION_1_3);
        let info = vk::InstanceCreateInfo::default()
            .application_info(&app)
            .enabled_extension_names(&ptrs)
            .enabled_layer_names(&layers);
        let instance = unsafe { entry.create_instance(&info, None) }.context("vkCreateInstance")?;
        Ok((entry, instance, exts))
    }

    /// The device: the first GPU that can present to `surface` (any GPU without one),
    /// preferring a discrete one.
    pub fn new(entry: ash::Entry, instance: ash::Instance, instance_extensions: Vec<CString>, surface: Option<vk::SurfaceKHR>) -> Result<Self> {
        let surface_fn = surface.map(|_| ash::khr::surface::Instance::new(&entry, &instance));
        let mut candidates = Vec::new();
        for pd in unsafe { instance.enumerate_physical_devices() }? {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            if props.api_version < vk::API_VERSION_1_3 {
                continue;
            }
            let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            let graphics = families.iter().enumerate().position(|(i, f)| {
                f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                    && match (surface, &surface_fn) {
                        (Some(s), Some(sf)) => unsafe { sf.get_physical_device_surface_support(pd, i as u32, s) }.unwrap_or(false),
                        _ => true,
                    }
            });
            if let Some(g) = graphics {
                let rank = match props.device_type {
                    vk::PhysicalDeviceType::DISCRETE_GPU => 0,
                    vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                    _ => 2,
                };
                candidates.push((rank, pd, g as u32));
            }
        }
        candidates.sort_by_key(|c| c.0);
        let Some(&(_, physical, graphics_family)) = candidates.first() else {
            bail!("no Vulkan 1.3 GPU that can present here");
        };
        let props = unsafe { instance.get_physical_device_properties(physical) };
        let name = props.device_name_as_c_str().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();

        // Queue families, with their video codec operations.
        let n = unsafe { instance.get_physical_device_queue_family_properties2_len(physical) };
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut families: Vec<vk::QueueFamilyProperties2> =
            video.iter_mut().map(|v| vk::QueueFamilyProperties2::default().push_next(v)).collect();
        unsafe { instance.get_physical_device_queue_family_properties2(physical, &mut families) };
        let families: Vec<(vk::QueueFlags, u32)> =
            families.iter().map(|f| (f.queue_family_properties.queue_flags, f.queue_family_properties.queue_count)).collect();
        let video: Vec<vk::VideoCodecOperationFlagsKHR> = video.iter().map(|v| v.video_codec_operations).collect();

        let available = unsafe { instance.enumerate_device_extension_properties(physical) }?;
        let mut dev_exts: Vec<CString> = Vec::new();
        if surface.is_some() {
            dev_exts.push(c"VK_KHR_swapchain".to_owned());
        }
        for e in OPTIONAL_DEVICE_EXTENSIONS {
            if has(&available, e) {
                dev_exts.push((*e).to_owned());
            }
        }
        // Present id and wait only go with a swapchain.
        if surface.is_none() {
            dev_exts.retain(|e| !e.to_bytes().starts_with(b"VK_KHR_present") && e.as_c_str() != c"VK_EXT_hdr_metadata");
        }
        let has_ext = |n: &CStr| dev_exts.iter().any(|e| e.as_c_str() == n);
        let video_ok = has_ext(c"VK_KHR_video_queue") && has_ext(c"VK_KHR_video_decode_queue");

        // Features: everything supported, as libav would enable.
        let mut v11 = Box::new(vk::PhysicalDeviceVulkan11Features::default());
        let mut v12 = Box::new(vk::PhysicalDeviceVulkan12Features::default());
        let mut v13 = Box::new(vk::PhysicalDeviceVulkan13Features::default());
        let mut vm1 = Box::new(vk::PhysicalDeviceVideoMaintenance1FeaturesKHR::default());
        let mut pid = Box::new(vk::PhysicalDevicePresentIdFeaturesKHR::default());
        let mut pw = Box::new(vk::PhysicalDevicePresentWaitFeaturesKHR::default());
        let mut f2 = Box::new(vk::PhysicalDeviceFeatures2::default());
        // Link the chain by hand (the boxes don't move).
        f2.p_next = &mut *v11 as *mut _ as *mut c_void;
        v11.p_next = &mut *v12 as *mut _ as *mut c_void;
        v12.p_next = &mut *v13 as *mut _ as *mut c_void;
        let mut tail: *mut *mut c_void = &mut v13.p_next;
        if has_ext(c"VK_KHR_video_maintenance1") {
            unsafe { *tail = &mut *vm1 as *mut _ as *mut c_void };
            tail = &mut vm1.p_next;
        }
        if has_ext(c"VK_KHR_present_id") {
            unsafe { *tail = &mut *pid as *mut _ as *mut c_void };
            tail = &mut pid.p_next;
        }
        if has_ext(c"VK_KHR_present_wait") {
            unsafe { *tail = &mut *pw as *mut _ as *mut c_void };
            tail = &mut pw.p_next;
        }
        let _ = tail;
        unsafe { instance.get_physical_device_features2(physical, &mut f2) };
        f2.features.robust_buffer_access = vk::FALSE;
        if v13.dynamic_rendering == vk::FALSE || v13.synchronization2 == vk::FALSE || v12.timeline_semaphore == vk::FALSE {
            bail!("{name} lacks dynamic rendering, synchronization2 or timeline semaphores");
        }
        let present_wait_ok = pid.present_id == vk::TRUE && pw.present_wait == vk::TRUE;
        if !present_wait_ok {
            dev_exts.retain(|e| !e.to_bytes().starts_with(b"VK_KHR_present"));
        }

        // Queues.
        let g_count = families[graphics_family as usize].1.min(3);
        let mut queue_infos: Vec<(u32, u32)> = vec![(graphics_family, g_count)];
        let mut decode_families = Vec::new();
        let mut libav_families = Vec::new();
        if video_ok {
            for (i, ops) in video.iter().enumerate() {
                if families[i].0.contains(vk::QueueFlags::VIDEO_DECODE_KHR) && !ops.is_empty() {
                    let count = families[i].1.min(4);
                    if i as u32 != graphics_family {
                        queue_infos.push((i as u32, count));
                    }
                    decode_families.push((i as u32, *ops));
                    libav_families.push(QsVkQueueFamily { idx: i as i32, num: count as i32, flags: families[i].0.as_raw(), video_caps: ops.as_raw() });
                }
            }
        }
        // A compute family and a transfer family for libav's own work, if separate.
        for want in [vk::QueueFlags::COMPUTE, vk::QueueFlags::TRANSFER] {
            if let Some(i) = families.iter().enumerate().position(|(i, f)| {
                f.0.contains(want) && !f.0.contains(vk::QueueFlags::GRAPHICS) && !queue_infos.iter().any(|q| q.0 == i as u32)
            }) {
                queue_infos.push((i as u32, 1));
                libav_families.push(QsVkQueueFamily { idx: i as i32, num: 1, flags: families[i].0.as_raw(), video_caps: 0 });
            }
        }
        libav_families.push(QsVkQueueFamily {
            idx: graphics_family as i32,
            num: 1,
            flags: families[graphics_family as usize].0.as_raw(),
            video_caps: 0,
        });
        let priorities = [1.0f32; 4];
        let qcis: Vec<vk::DeviceQueueCreateInfo> = queue_infos
            .iter()
            .map(|&(f, c)| vk::DeviceQueueCreateInfo::default().queue_family_index(f).queue_priorities(&priorities[..c as usize]))
            .collect();
        let ext_ptrs: Vec<*const c_char> = dev_exts.iter().map(|e| e.as_ptr()).collect();
        let mut info = vk::DeviceCreateInfo::default().queue_create_infos(&qcis).enabled_extension_names(&ext_ptrs);
        info.p_next = &*f2 as *const _ as *const c_void;
        let device = unsafe { instance.create_device(physical, &info, None) }.context("vkCreateDevice")?;

        let q = |i: u32| Arc::new(Mutex::new(unsafe { device.get_device_queue(graphics_family, i.min(g_count - 1)) }));
        let libav_queue = q(0);
        let convert_queue = if g_count >= 2 { q(1) } else { libav_queue.clone() };
        let present_queue = if g_count >= 3 { q(2) } else { convert_queue.clone() };
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        let has_ext = |n: &CStr| dev_exts.iter().any(|e| e.as_c_str() == n);
        let push_descriptor = has_ext(c"VK_KHR_push_descriptor").then(|| ash::khr::push_descriptor::Device::new(&instance, &device));
        let present_wait = has_ext(c"VK_KHR_present_wait").then(|| ash::khr::present_wait::Device::new(&instance, &device));
        let hdr_metadata = has_ext(c"VK_EXT_hdr_metadata").then(|| ash::ext::hdr_metadata::Device::new(&instance, &device));
        let swapchain_fn = surface.map(|_| ash::khr::swapchain::Device::new(&instance, &device));
        if push_descriptor.is_none() {
            bail!("{name} lacks VK_KHR_push_descriptor");
        }
        let swapchain_colorspace = instance_extensions.iter().any(|e| e.as_c_str() == c"VK_EXT_swapchain_colorspace");
        let inst_ptrs: Vec<*const c_char> = instance_extensions.iter().map(|e| e.as_ptr()).collect();
        let dev_ptrs: Vec<*const c_char> = dev_exts.iter().map(|e| e.as_ptr()).collect();
        Ok(Self {
            entry,
            instance,
            physical,
            device,
            name,
            graphics_family,
            libav_queue,
            convert_queue,
            present_queue,
            memory,
            instance_extensions,
            device_extensions: dev_exts,
            decode_families,
            libav_families,
            push_descriptor,
            present_wait,
            hdr_metadata,
            surface_fn,
            swapchain_fn,
            swapchain_colorspace,
            features: Features { f2, _v11: v11, _v12: v12, _v13: v13, _vm1: vm1, _pid: pid, _pw: pw },
            _ext_ptrs: (inst_ptrs, dev_ptrs),
        })
    }

    pub fn has_device_extension(&self, name: &CStr) -> bool {
        self.device_extensions.iter().any(|e| e.as_c_str() == name)
    }

    /// The device as libav wants it described. The pointers are into `self`.
    pub fn libav_device(&self) -> QsVkDevice {
        let mut qf = [QsVkQueueFamily::default(); 16];
        for (i, f) in self.libav_families.iter().take(16).enumerate() {
            qf[i] = *f;
        }
        QsVkDevice {
            get_instance_proc_addr: self.entry.static_fn().get_instance_proc_addr as *const c_void,
            instance: vk::Handle::as_raw(self.instance.handle()) as *const c_void,
            physical_device: vk::Handle::as_raw(self.physical) as *const c_void,
            device: vk::Handle::as_raw(self.device.handle()) as *const c_void,
            instance_extensions: self._ext_ptrs.0.as_ptr(),
            nb_instance_extensions: self._ext_ptrs.0.len() as i32,
            device_extensions: self._ext_ptrs.1.as_ptr(),
            nb_device_extensions: self._ext_ptrs.1.len() as i32,
            features: &*self.features.f2 as *const _ as *const c_void,
            qf,
            nb_qf: self.libav_families.len().min(16) as i32,
        }
    }

    /// A memory type with these properties among `bits`.
    pub fn memory_type(&self, bits: u32, props: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.memory.memory_type_count)
            .find(|&i| bits & (1 << i) != 0 && self.memory.memory_types[i as usize].property_flags.contains(props))
    }

    /// Video decode support for a codec: max coded size and whether 10-bit and 4:4:4 work.
    pub fn video_decode_caps(&self, codec: quiver_stream_proto::VideoCodec) -> Option<VideoDecodeCaps> {
        if self.decode_families.is_empty() {
            return None;
        }
        let vq = ash::khr::video_queue::Instance::new(&self.entry, &self.instance);
        let query = |depth: vk::VideoComponentBitDepthFlagsKHR, chroma: vk::VideoChromaSubsamplingFlagsKHR| -> Option<vk::Extent2D> {
            let op;
            let mut h264 = vk::VideoDecodeH264ProfileInfoKHR::default()
                .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE);
            let mut h265 = vk::VideoDecodeH265ProfileInfoKHR::default();
            let mut av1 = vk::VideoDecodeAV1ProfileInfoKHR::default();
            let ten = depth == vk::VideoComponentBitDepthFlagsKHR::TYPE_10;
            let c444 = chroma == vk::VideoChromaSubsamplingFlagsKHR::TYPE_444;
            let mut profile = vk::VideoProfileInfoKHR::default()
                .chroma_subsampling(chroma)
                .luma_bit_depth(depth)
                .chroma_bit_depth(depth);
            let mut c_h264 = vk::VideoDecodeH264CapabilitiesKHR::default();
            let mut c_h265 = vk::VideoDecodeH265CapabilitiesKHR::default();
            let mut c_av1 = vk::VideoDecodeAV1CapabilitiesKHR::default();
            let mut dcaps = vk::VideoDecodeCapabilitiesKHR::default();
            let mut caps = vk::VideoCapabilitiesKHR::default();
            use ash::vk::native::*;
            match codec {
                quiver_stream_proto::VideoCodec::H264 => {
                    op = vk::VideoCodecOperationFlagsKHR::DECODE_H264;
                    h264.std_profile_idc = if c444 || ten {
                        StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
                    } else {
                        StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
                    };
                    profile = profile.push_next(&mut h264);
                    caps = caps.push_next(&mut c_h264);
                }
                quiver_stream_proto::VideoCodec::Hevc => {
                    op = vk::VideoCodecOperationFlagsKHR::DECODE_H265;
                    h265.std_profile_idc = if c444 {
                        StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS
                    } else if ten {
                        StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
                    } else {
                        StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
                    };
                    profile = profile.push_next(&mut h265);
                    caps = caps.push_next(&mut c_h265);
                }
                quiver_stream_proto::VideoCodec::Av1 => {
                    op = vk::VideoCodecOperationFlagsKHR::DECODE_AV1;
                    av1.std_profile = if c444 { StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_HIGH } else { StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN };
                    profile = profile.push_next(&mut av1);
                    caps = caps.push_next(&mut c_av1);
                }
            }
            if !self.decode_families.iter().any(|(_, ops)| ops.contains(op)) {
                return None;
            }
            profile = profile.video_codec_operation(op);
            caps = caps.push_next(&mut dcaps);
            let r = unsafe { (vq.fp().get_physical_device_video_capabilities_khr)(self.physical, &profile, &mut caps) };
            (r == vk::Result::SUCCESS).then_some(caps.max_coded_extent)
        };
        let base = query(vk::VideoComponentBitDepthFlagsKHR::TYPE_8, vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)?;
        let ten = query(vk::VideoComponentBitDepthFlagsKHR::TYPE_10, vk::VideoChromaSubsamplingFlagsKHR::TYPE_420).is_some();
        let c444 = query(vk::VideoComponentBitDepthFlagsKHR::TYPE_8, vk::VideoChromaSubsamplingFlagsKHR::TYPE_444).is_some();
        Some(VideoDecodeCaps { max_width: base.width, max_height: base.height, ten_bit: ten, yuv444: c444 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoDecodeCaps {
    pub max_width: u32,
    pub max_height: u32,
    pub ten_bit: bool,
    pub yuv444: bool,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
