//! A tiny ash Vulkan app that presents through `VK_LAYER_QUIVER_sight` from this build, with
//! a scratch session dir and a test `HudWriter`, used by `tests/vulkan.rs` and
//! `examples/sight_harness.rs`. Headless (`VK_EXT_headless_surface`, read back after
//! presenting) or in a short-lived X11 window (XWayland) for screenshots.
//!
//! Never installs anything: the layer is found through `VK_ADD_LAYER_PATH` pointing at a
//! manifest written into the scratch dir.

#![allow(dead_code)]

use std::ffi::{CStr, CString, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ash::vk;
use ubi_perf::hud::{HudWriter, buffer_bytes_for, rgba_premul};
use ubi_perf::layout::{GRAPH_SERIES_PRESENTED, HUD_FORMAT_RGBA8_PREMUL_SRGB};
use ubi_perf::session::{SessionDir, SessionId};
use ubi_perf::shm::Mapping;
use ubi_perf::{HudBufferHeader, HudGraph};

pub const LAYER: &str = "VK_LAYER_QUIVER_sight";
pub const VALIDATION: &str = "VK_LAYER_KHRONOS_validation";

/// The freshly built layer: next to the test or example binary's `deps/`.
pub fn layer_so() -> PathBuf {
    if let Some(p) = std::env::var_os("QUIVER_SIGHT_LAYER_SO") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().unwrap();
    let mut dir = exe.parent().unwrap().to_path_buf();
    for _ in 0..3 {
        let p = dir.join("libquiver_sight_layer.so");
        if p.exists() {
            return p;
        }
        dir = dir.parent().unwrap().to_path_buf();
    }
    panic!("libquiver_sight_layer.so not found near {}", exe.display());
}

/// Writes the layer manifest for `so` into `dir` (the install template, filled in).
pub fn write_manifest(dir: &Path, so: &Path) {
    let json = quiver_sight_layer::manifest_json(so.to_str().unwrap(), cfg!(target_pointer_width = "64"));
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("VK_LAYER_QUIVER_sight.json"), json).unwrap();
}

/// A scratch session dir under /dev/shm with `hud.image` and a heartbeat thread, standing in
/// for quiverd and quiver-sight.
pub struct TestSession {
    _root: tempfile::TempDir,
    pub dir: SessionDir,
    writer: Arc<Mutex<HudWriter<Mapping>>>,
    stop: Arc<AtomicBool>,
    beat: Option<std::thread::JoinHandle<()>>,
}

impl TestSession {
    pub fn new() -> TestSession {
        let root = tempfile::Builder::new().prefix("quiver-sight-test-").tempdir_in("/dev/shm").unwrap();
        let dir = SessionDir::create(root.path(), &SessionId::parse("s-sighttest").unwrap()).unwrap();
        dir.create_hud_image(buffer_bytes_for(1024, 512).unwrap()).unwrap();
        let w = dir.open_hud_writer().unwrap();
        w.set_writer_pid(std::process::id());
        w.heartbeat(ubi_perf::clock::mono_raw_ns());
        let writer = Arc::new(Mutex::new(w));
        let stop = Arc::new(AtomicBool::new(false));
        let (w2, s2) = (writer.clone(), stop.clone());
        let beat = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                w2.lock().unwrap().heartbeat(ubi_perf::clock::mono_raw_ns());
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        TestSession { _root: root, dir, writer, stop, beat: Some(beat) }
    }

    pub fn publish(&self, img: &TestImage) -> u64 {
        self.writer.lock().unwrap().publish(&img.desc, &img.pixels).unwrap()
    }

    /// Stops the heartbeat (the layer must stop drawing within 3 s).
    pub fn stop_heartbeat(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(b) = self.beat.take() {
            b.join().unwrap();
        }
    }

    pub fn ring_path(&self) -> PathBuf {
        self.dir.file(&ubi_perf::session::FileKind::Layer { pid: std::process::id() })
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

/// A HUD image like quiver-sight would publish: premultiplied, padded rows (the layer must
/// use `stride`), a border, bars whose length depends on `variant`, and a graph rectangle.
pub struct TestImage {
    pub desc: HudBufferHeader,
    pub pixels: Vec<u8>,
}

pub const BG: [u8; 4] = [16, 18, 26, 200];
pub const BORDER: [u8; 4] = [255, 255, 255, 255];
pub const BAR: [u8; 4] = [80, 170, 255, 255];
pub const LINE: [u8; 4] = [90, 230, 120, 255];

pub fn test_image(extent: (u32, u32), pos: (i32, i32), size: (u32, u32), variant: u32, opacity: f32) -> TestImage {
    let (w, h) = size;
    let stride = w * 4 + 64;
    let mut px = vec![0xEEu8; (stride * h) as usize]; // padding garbage the layer must skip
    let prem = |c: [u8; 4]| rgba_premul(c[0], c[1], c[2], c[3]).to_le_bytes();
    for y in 0..h {
        for x in 0..w {
            let border = x < 2 || y < 2 || x >= w - 2 || y >= h - 2;
            let bar_row = (12..h / 2 - 8).contains(&y) && (y - 12) % 14 < 8;
            let bar_len = 30 + ((y.saturating_sub(12) / 14 + variant) * 37) % (w - 60);
            let c = if border {
                BORDER
            } else if bar_row && (12..12 + bar_len).contains(&x) {
                BAR
            } else {
                BG
            };
            let o = (y * stride + x * 4) as usize;
            px[o..o + 4].copy_from_slice(&prem(c));
        }
    }
    let mut d = HudBufferHeader::ZERO;
    d.width = w;
    d.height = h;
    d.stride = stride;
    d.format = HUD_FORMAT_RGBA8_PREMUL_SRGB;
    d.scale = 1.5;
    d.extent_w = extent.0;
    d.extent_h = extent.1;
    d.x = pos.0;
    d.y = pos.1;
    d.opacity = opacity;
    d.model_rev = variant as u64 + 1;
    d.ngraphs = 1;
    d.graph[0] = HudGraph {
        x: 8,
        y: (h / 2) as i32,
        w: w as i32 - 16,
        h: (h / 2) as i32 - 10,
        span_ms: 3000,
        min_ms: 0.0,
        max_ms: 50.0,
        line: rgba_premul(LINE[0], LINE[1], LINE[2], LINE[3]),
        fill: rgba_premul(LINE[0], LINE[1], LINE[2], 50),
        muted: 0,
        warn_above_ms: 20,
        bad_above_ms: 35,
        warn: rgba_premul(250, 200, 60, 255),
        bad: rgba_premul(240, 70, 60, 255),
        series: GRAPH_SERIES_PRESENTED,
    };
    TestImage { desc: d, pixels: px }
}

pub enum Surface {
    Headless,
    /// An X11 window (XWayland), shown for the run.
    Window { title: String },
}

pub struct Opts<'a> {
    pub frames: u32,
    pub srgb: bool,
    pub extent: (u32, u32),
    /// Frame pacing (sleep between presents).
    pub fps: f64,
    /// Frames that take 45 ms instead (graph hitches).
    pub hitches: Vec<u32>,
    pub surface: Surface,
    /// Read back the presented image at the end (headless).
    pub readback: bool,
    /// Substring of the device name to use.
    pub device: Option<String>,
    /// Called before each frame (publish new HUD images, take screenshots).
    pub on_frame: Option<Box<dyn FnMut(u32, Duration) + 'a>>,
    /// Chain `VkPresentIdKHR` (DXVK does) when the device supports it.
    pub present_id: bool,
    /// A debug messenger collects validation messages (needs the validation layer enabled).
    pub debug_utils: bool,
}

impl Default for Opts<'_> {
    fn default() -> Self {
        Opts {
            frames: 120,
            srgb: false,
            extent: (1280, 720),
            fps: 120.0,
            hitches: Vec::new(),
            surface: Surface::Headless,
            readback: true,
            device: std::env::var("QUIVER_TEST_VK_DEVICE").ok(),
            on_frame: None,
            present_id: true,
            debug_utils: false,
        }
    }
}

pub struct Report {
    pub device_name: String,
    pub format: vk::Format,
    pub present_id: bool,
    /// RGBA8 (converted from BGRA when needed), `extent` sized.
    pub pixels: Option<Vec<u8>>,
    /// CPU time of each vkQueuePresentKHR call as the app saw it.
    pub present_ns: Vec<u64>,
    pub messages: Vec<String>,
}

static MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "system" fn on_message(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user: *mut c_void,
) -> vk::Bool32 {
    if severity.intersects(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        let msg = unsafe { (*data).message_as_c_str() }.map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
        MESSAGES.lock().unwrap().push(format!("{severity:?}: {msg}"));
    }
    vk::FALSE
}

struct Xcb {
    conn: x11rb::xcb_ffi::XCBConnection,
    window: u32,
}

fn open_window(title: &str, w: u32, h: u32) -> Xcb {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass};
    use x11rb::wrapper::ConnectionExt as _;
    let (conn, screen) = x11rb::xcb_ffi::XCBConnection::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        w as u16,
        h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
    )
    .unwrap();
    conn.change_property8(PropMode::REPLACE, window, x11rb::protocol::xproto::AtomEnum::WM_NAME, x11rb::protocol::xproto::AtomEnum::STRING, title.as_bytes())
        .unwrap();
    conn.map_window(window).unwrap();
    conn.flush().unwrap();
    Xcb { conn, window }
}

/// Runs the app. The caller sets the loader environment first.
pub fn run(mut o: Opts<'_>) -> Report {
    MESSAGES.lock().unwrap().clear();
    // SAFETY: test code; every handle is used as Vulkan requires and destroyed at the end.
    unsafe {
        let entry = ash::Entry::load().unwrap();
        let app = CString::new("quiver-sight-harness").unwrap();
        let engine = CString::new("QuiverSightTest").unwrap();
        let app_info = vk::ApplicationInfo::default().application_name(&app).engine_name(&engine).engine_version(7).api_version(vk::API_VERSION_1_1);
        let mut exts: Vec<*const i8> = vec![ash::khr::surface::NAME.as_ptr()];
        let xcb = match &o.surface {
            Surface::Headless => {
                exts.push(ash::ext::headless_surface::NAME.as_ptr());
                None
            }
            Surface::Window { title } => {
                exts.push(ash::khr::xcb_surface::NAME.as_ptr());
                Some(open_window(title, o.extent.0, o.extent.1))
            }
        };
        if o.debug_utils {
            exts.push(ash::ext::debug_utils::NAME.as_ptr());
        }
        let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app_info).enabled_extension_names(&exts), None).unwrap();
        let messenger = o.debug_utils.then(|| {
            let du = ash::ext::debug_utils::Instance::new(&entry, &instance);
            let ci = vk::DebugUtilsMessengerCreateInfoEXT::default()
                .message_severity(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING)
                .message_type(vk::DebugUtilsMessageTypeFlagsEXT::GENERAL | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE)
                .pfn_user_callback(Some(on_message));
            (du.create_debug_utils_messenger(&ci, None).unwrap(), du)
        });
        let surface_fn = ash::khr::surface::Instance::new(&entry, &instance);
        let surface = match &xcb {
            None => ash::ext::headless_surface::Instance::new(&entry, &instance).create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None).unwrap(),
            Some(x) => {
                use x11rb::connection::Connection as _;
                let _ = x.conn.flush();
                ash::khr::xcb_surface::Instance::new(&entry, &instance)
                    .create_xcb_surface(&vk::XcbSurfaceCreateInfoKHR::default().connection(x.conn.get_raw_xcb_connection()).window(x.window), None)
                    .unwrap()
            }
        };

        // A device and graphics queue family that can present to the surface.
        let mut pick = None;
        for pd in instance.enumerate_physical_devices().unwrap() {
            let props = instance.get_physical_device_properties(pd);
            let name = props.device_name_as_c_str().unwrap().to_string_lossy().into_owned();
            if o.device.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
                continue;
            }
            let fams = instance.get_physical_device_queue_family_properties(pd);
            if let Some(f) = (0..fams.len() as u32)
                .find(|&f| fams[f as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS) && surface_fn.get_physical_device_surface_support(pd, f, surface).unwrap_or(false))
            {
                pick = Some((pd, f, name));
                break;
            }
        }
        let (pd, family, device_name) = pick.expect("no Vulkan device can present to the test surface");
        let dev_exts = instance.enumerate_device_extension_properties(pd).unwrap();
        let has = |n: &CStr| dev_exts.iter().any(|e| e.extension_name_as_c_str().ok() == Some(n));
        let present_id = o.present_id && has(ash::khr::present_id::NAME);
        let mut dexts = vec![ash::khr::swapchain::NAME.as_ptr()];
        let mut pid_features = vk::PhysicalDevicePresentIdFeaturesKHR::default().present_id(true);
        let prio = [1.0];
        let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
        let mut dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci);
        if present_id {
            dexts.push(ash::khr::present_id::NAME.as_ptr());
            dci = dci.push_next(&mut pid_features);
        }
        dci = dci.enabled_extension_names(&dexts);
        let device = instance.create_device(pd, &dci, None).unwrap();
        let queue = device.get_device_queue(family, 0);
        let sc_fn = ash::khr::swapchain::Device::new(&instance, &device);

        let want = if o.srgb { vk::Format::B8G8R8A8_SRGB } else { vk::Format::B8G8R8A8_UNORM };
        let formats = surface_fn.get_physical_device_surface_formats(pd, surface).unwrap();
        let fmt = formats.iter().find(|f| f.format == want && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR).copied().unwrap_or(formats[0]);
        let caps = surface_fn.get_physical_device_surface_capabilities(pd, surface).unwrap();
        let extent = if caps.current_extent.width != u32::MAX { caps.current_extent } else { vk::Extent2D { width: o.extent.0, height: o.extent.1 } };
        assert_eq!((extent.width, extent.height), o.extent, "the window manager resized the test window");
        let modes = surface_fn.get_physical_device_surface_present_modes(pd, surface).unwrap();
        let mode = [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::IMMEDIATE].into_iter().find(|m| modes.contains(m)).unwrap_or(vk::PresentModeKHR::FIFO);
        let count = (caps.min_image_count + 1).min(if caps.max_image_count == 0 { u32::MAX } else { caps.max_image_count });
        // No COLOR_ATTACHMENT usage on purpose: the layer adds it.
        let swapchain = sc_fn
            .create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(surface)
                    .min_image_count(count)
                    .image_format(fmt.format)
                    .image_color_space(fmt.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                    .present_mode(mode)
                    .clipped(true),
                None,
            )
            .unwrap();
        let images = sc_fn.get_swapchain_images(swapchain).unwrap();

        let pool = device
            .create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None)
            .unwrap();
        const INFLIGHT: usize = 2;
        let cmds = device
            .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(INFLIGHT as u32))
            .unwrap();
        let fences: Vec<_> = (0..INFLIGHT).map(|_| device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None).unwrap()).collect();
        let acquired: Vec<_> = (0..INFLIGHT).map(|_| device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap()).collect();
        let rendered: Vec<_> = images.iter().map(|_| device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap()).collect();
        let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };

        let mut present_ns = Vec::with_capacity(o.frames as usize);
        let start = Instant::now();
        let frame_time = Duration::from_secs_f64(1.0 / o.fps);
        let mut next_at = start;
        for frame in 0..o.frames {
            if let Some(cb) = o.on_frame.as_mut() {
                cb(frame, start.elapsed());
            }
            if let Some(x) = &xcb {
                use x11rb::connection::Connection as _;
                while let Ok(Some(_)) = x.conn.poll_for_event() {}
            }
            let k = frame as usize % INFLIGHT;
            device.wait_for_fences(&[fences[k]], true, u64::MAX).unwrap();
            device.reset_fences(&[fences[k]]).unwrap();
            let (index, _) = sc_fn.acquire_next_image(swapchain, u64::MAX, acquired[k], vk::Fence::null()).unwrap();
            let img = images[index as usize];
            let cmd = cmds[k];
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
            device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()).unwrap();
            let barrier = |old, new, src, dst| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(old)
                    .new_layout(new)
                    .src_access_mask(src)
                    .dst_access_mask(dst)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(range)
            };
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE)],
            );
            // A slowly cycling, mid-grey-ish clear so blending is checkable.
            let t = frame as f32 / o.frames.max(1) as f32;
            let clear = vk::ClearColorValue { float32: [0.25 + 0.2 * t, 0.35, 0.45 - 0.2 * t, 1.0] };
            device.cmd_clear_color_image(cmd, img, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &clear, &[range]);
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::PRESENT_SRC_KHR, vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::empty())],
            );
            device.end_command_buffer(cmd).unwrap();
            let waits = [acquired[k]];
            let stages = [vk::PipelineStageFlags::TRANSFER];
            let signal = [rendered[index as usize]];
            let cb = [cmd];
            device
                .queue_submit(queue, &[vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages).command_buffers(&cb).signal_semaphores(&signal)], fences[k])
                .unwrap();
            let scs = [swapchain];
            let idx = [index];
            let ids = [frame as u64 + 1];
            let mut pid = vk::PresentIdKHR::default().present_ids(&ids);
            let mut pi = vk::PresentInfoKHR::default().wait_semaphores(&signal).swapchains(&scs).image_indices(&idx);
            if present_id {
                pi = pi.push_next(&mut pid);
            }
            let t0 = Instant::now();
            let r = sc_fn.queue_present(queue, &pi);
            present_ns.push(t0.elapsed().as_nanos() as u64);
            r.unwrap();
            next_at += if o.hitches.contains(&frame) { Duration::from_millis(45) } else { frame_time };
            let now = Instant::now();
            if next_at > now {
                std::thread::sleep(next_at - now);
            } else {
                next_at = now;
            }
        }
        device.device_wait_idle().unwrap();

        let pixels = o.readback.then(|| {
            // The next image to come back was presented a few frames ago, with the HUD in it.
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).unwrap();
            let (index, _) = sc_fn.acquire_next_image(swapchain, u64::MAX, vk::Semaphore::null(), fence).unwrap();
            device.wait_for_fences(&[fence], true, u64::MAX).unwrap();
            let img = images[index as usize];
            let size = (extent.width * extent.height * 4) as u64;
            let buf = device.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_DST), None).unwrap();
            let req = device.get_buffer_memory_requirements(buf);
            let mp = instance.get_physical_device_memory_properties(pd);
            let ty = (0..mp.memory_type_count)
                .find(|&i| {
                    req.memory_type_bits & (1 << i) != 0
                        && mp.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
                })
                .unwrap();
            let mem = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None).unwrap();
            device.bind_buffer_memory(buf, mem, 0).unwrap();
            let cmd = cmds[0];
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
            device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()).unwrap();
            let b = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(range);
            device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[b]);
            let region = vk::BufferImageCopy {
                image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 },
                image_extent: vk::Extent3D { width: extent.width, height: extent.height, depth: 1 },
                ..Default::default()
            };
            device.cmd_copy_image_to_buffer(cmd, img, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf, &[region]);
            device.end_command_buffer(cmd).unwrap();
            let f2 = fences[0];
            device.reset_fences(&[f2]).unwrap();
            let cb = [cmd];
            device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cb)], f2).unwrap();
            device.wait_for_fences(&[f2], true, u64::MAX).unwrap();
            let p = device.map_memory(mem, 0, size, vk::MemoryMapFlags::empty()).unwrap() as *const u8;
            let mut v = std::slice::from_raw_parts(p, size as usize).to_vec();
            device.unmap_memory(mem);
            device.destroy_buffer(buf, None);
            device.free_memory(mem, None);
            device.destroy_fence(fence, None);
            if matches!(fmt.format, vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB) {
                for px in v.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
            }
            v
        });

        device.device_wait_idle().unwrap();
        for s in acquired.into_iter().chain(rendered) {
            device.destroy_semaphore(s, None);
        }
        for f in fences {
            device.destroy_fence(f, None);
        }
        device.destroy_command_pool(pool, None);
        sc_fn.destroy_swapchain(swapchain, None);
        device.destroy_device(None);
        surface_fn.destroy_surface(surface, None);
        if let Some((m, du)) = messenger {
            du.destroy_debug_utils_messenger(m, None);
        }
        instance.destroy_instance(None);
        drop(xcb);
        Report { device_name, format: fmt.format, present_id, pixels, present_ns, messages: std::mem::take(&mut *MESSAGES.lock().unwrap()) }
    }
}

/// The loader environment for a run: our layer (and optionally validation below it).
pub fn layer_env(manifest_dir: &Path, validation: Option<&Path>) -> Vec<(String, String)> {
    let mut path = manifest_dir.display().to_string();
    let mut layers = LAYER.to_owned();
    if let Some(v) = validation {
        path = format!("{path}:{}", v.display());
        // First listed = closest to the app: validation sits below Sight and checks its calls.
        layers = format!("{LAYER}:{VALIDATION}");
    }
    vec![("VK_ADD_LAYER_PATH".into(), path), ("VK_INSTANCE_LAYERS".into(), layers)]
}

/// Where a validation layer manifest lives, if there's one on this machine (never installed by
/// us: `QUIVER_TEST_VALIDATION_DIR`, or the copy the Android SDK ships).
pub fn validation_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("QUIVER_TEST_VALIDATION_DIR") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let p = Path::new(&home).join("Android/Sdk/emulator/lib64/vulkan/layers");
    p.join("VkLayer_khronos_validation.json").exists().then_some(p)
}

pub fn median(v: &[u64]) -> u64 {
    let mut s = v.to_vec();
    s.sort_unstable();
    s.get(s.len() / 2).copied().unwrap_or(0)
}

pub fn percentile(v: &[u64], p: f64) -> u64 {
    let mut s = v.to_vec();
    s.sort_unstable();
    if s.is_empty() {
        return 0;
    }
    s[((s.len() - 1) as f64 * p).round() as usize]
}
