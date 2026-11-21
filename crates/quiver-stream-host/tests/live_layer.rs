//! Capture in the Sight layer (`layer_capture.rs`, `quiver-sight-layer`'s `stream.rs`) against
//! gamescope's PipeWire screencast, with a Vulkan program in headless gamescope.
//!
//! The program is this test binary run again ([`source_program`]): an ash app on gamescope's
//! Xwayland that clears each frame, draws the frame stamp (`stamp::paint`: the seq and the
//! moment it drew the frame) and a moving block, and presents, paced at the display's rate
//! with `MAILBOX` (or `FIFO`) as a game with its frame cap would. In HDR it presents an
//! `A2B10G10R10` `HDR10_ST2084` swapchain through gamescope's WSI layer and sets HDR metadata.
//! It loads the layer from this build through `VK_ADD_IMPLICIT_LAYER_PATH` (never installed), in the
//! program's environment only.
//!
//! Each run streams a few seconds as HEVC, decodes the stamp band with ffmpeg and reads the
//! stamps back: program draws → capture (the frame's capture time), capture → encode start,
//! and draws → encoded. The present overhead run times `vkQueuePresentKHR` in the program
//! without the layer, with it and no stream, and with a stream capturing.
//!
//! Opt-in, a few seconds of GPU per run:
//! `cargo test --release -p quiver-stream-host --test live_layer -- --ignored --nocapture --test-threads 1`.
//! `QUIVER_STREAM_TEST_MODES=2560x1440@120,3840x2160@60` picks the modes (both SDR and HDR
//! each); `QUIVER_STREAM_TEST_LAYER_ONLY=1` skips the PipeWire runs. Skips without gamescope,
//! ffmpeg, the NVIDIA driver or the layer's library.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use quiver_stream_host::recovery::Recovery;
use quiver_stream_host::stats::Samples;
use quiver_stream_host::{Host, HostOptions, Target};
use quiver_stream_proto::{AudioCodec, ColorInfo, Control, EncodedFrame, HdrMetadata, StreamConfig, VideoCodec, now_micros, stamp};

const SOURCE: &str = "QUIVER_STREAM_TEST_SOURCE";

/// The metadata the program sets: BT.2020, D65, 1000 / 0.001 nits, MaxCLL 1000, MaxFALL 400.
fn program_metadata() -> HdrMetadata {
    HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 10, max_cll: 1000, max_fall: 400 }
}

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/stream-host-test/layer");
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// The layer library built with this test (a dev-dependency: `deps/` next to this binary).
fn layer_so() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("QUIVER_SIGHT_LAYER_SO") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..3 {
        let p = dir.join("libquiver_sight_layer.so");
        if p.exists() {
            return Some(p);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

#[derive(Clone, Debug)]
struct Spec {
    w: u32,
    h: u32,
    fps: u32,
    hdr: bool,
    seconds: f64,
    out: PathBuf,
}

impl Spec {
    fn encode(&self) -> String {
        format!("{} {} {} {} {} {}", self.w, self.h, self.fps, self.hdr as u8, self.seconds, self.out.display())
    }
    fn parse(s: &str) -> Spec {
        let v: Vec<&str> = s.splitn(6, ' ').collect();
        Spec { w: v[0].parse().unwrap(), h: v[1].parse().unwrap(), fps: v[2].parse().unwrap(), hdr: v[3] == "1", seconds: v[4].parse().unwrap(), out: PathBuf::from(v[5]) }
    }
}

/// Not a test: the source program, when this binary is run as one (module docs).
#[test]
#[ignore]
fn source_program() {
    if let Ok(spec) = std::env::var(SOURCE) {
        source::run(&Spec::parse(&spec));
    }
}

mod source {
    use super::*;
    use ash::vk;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, WindowClass};

    const BLOCK: u32 = 64;

    /// Where the frame loop is (1 the fence, 2 acquire, 4 submit, 5 present) and in which frame:
    /// a watchdog says where it stopped, if it does.
    static PHASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn phase(p: u64, seq: u64) {
        PHASE.store(seq << 8 | p, std::sync::atomic::Ordering::Relaxed);
    }

    fn watchdog() {
        let mut last = (u64::MAX, Instant::now());
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let v = PHASE.load(std::sync::atomic::Ordering::Relaxed);
            if v != last.0 {
                last = (v, Instant::now());
            } else if last.1.elapsed() > Duration::from_millis(1500) {
                eprintln!("source program: stuck in frame {} phase {}", v >> 8, v & 0xff);
                last.1 = Instant::now() + Duration::from_secs(3600);
            }
        }
    }

    /// One pixel, white or black, in the swapchain's format.
    fn pixel(format: vk::Format, white: bool) -> u32 {
        match format {
            vk::Format::A2B10G10R10_UNORM_PACK32 | vk::Format::A2R10G10B10_UNORM_PACK32 => (3 << 30) | if white { 0x3fff_ffff } else { 0 },
            _ => 0xff00_0000 | if white { 0x00ff_ffff } else { 0 },
        }
    }

    pub fn run(s: &Spec) {
        // SAFETY: test code; every handle is used as Vulkan requires.
        unsafe { run_vk(s) }
    }

    unsafe fn run_vk(s: &Spec) {
        let entry = unsafe { ash::Entry::load() }.expect("the Vulkan loader");
        let (conn, screen) = x11rb::xcb_ffi::XCBConnection::connect(None).expect("connect to the display");
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id().unwrap();
        conn.create_window(x11rb::COPY_DEPTH_FROM_PARENT, win, root, 0, 0, s.w as u16, s.h as u16, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new()).unwrap();
        conn.map_window(win).unwrap();
        conn.flush().unwrap();
        unsafe {
            let have: Vec<String> = entry.enumerate_instance_extension_properties(None).unwrap().iter().map(|e| e.extension_name_as_c_str().unwrap().to_string_lossy().into_owned()).collect();
            let mut exts = vec![ash::khr::surface::NAME.as_ptr(), ash::khr::xcb_surface::NAME.as_ptr()];
            if s.hdr && have.iter().any(|e| e.as_bytes() == ash::ext::swapchain_colorspace::NAME.to_bytes()) {
                exts.push(ash::ext::swapchain_colorspace::NAME.as_ptr());
            }
            let app = vk::ApplicationInfo::default().application_name(c"quiver-stream-source").api_version(vk::API_VERSION_1_3);
            let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts), None).unwrap();
            let surface_fn = ash::khr::surface::Instance::new(&entry, &instance);
            let surface = ash::khr::xcb_surface::Instance::new(&entry, &instance)
                .create_xcb_surface(&vk::XcbSurfaceCreateInfoKHR::default().connection(conn.get_raw_xcb_connection()).window(win), None)
                .unwrap();
            let mut pick = None;
            let mut pds = instance.enumerate_physical_devices().unwrap();
            pds.sort_by_key(|&pd| instance.get_physical_device_properties(pd).vendor_id != 0x10de);
            for pd in pds {
                let fams = instance.get_physical_device_queue_family_properties(pd);
                if let Some(f) = (0..fams.len() as u32).find(|&f| fams[f as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS) && surface_fn.get_physical_device_surface_support(pd, f, surface).unwrap_or(false)) {
                    pick = Some((pd, f));
                    break;
                }
            }
            let (pd, family) = pick.expect("a device that presents to the window");
            let dev_exts: Vec<String> = instance.enumerate_device_extension_properties(pd).unwrap().iter().map(|e| e.extension_name_as_c_str().unwrap().to_string_lossy().into_owned()).collect();
            let has_md = s.hdr && dev_exts.iter().any(|e| e.as_bytes() == ash::ext::hdr_metadata::NAME.to_bytes());
            let mut dexts = vec![ash::khr::swapchain::NAME.as_ptr()];
            if has_md {
                dexts.push(ash::ext::hdr_metadata::NAME.as_ptr());
            }
            let prio = [1.0];
            let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
            let device = instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&qci).enabled_extension_names(&dexts), None).unwrap();
            let queue = device.get_device_queue(family, 0);

            let formats = surface_fn.get_physical_device_surface_formats(pd, surface).unwrap();
            let want = if s.hdr {
                [(vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT), (vk::Format::A2R10G10B10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT)]
            } else {
                [(vk::Format::B8G8R8A8_UNORM, vk::ColorSpaceKHR::SRGB_NONLINEAR), (vk::Format::R8G8B8A8_UNORM, vk::ColorSpaceKHR::SRGB_NONLINEAR)]
            };
            let Some(sf) = want.iter().find_map(|&(f, cs)| formats.iter().find(|x| x.format == f && x.color_space == cs).copied()) else {
                panic!("the surface offers none of {want:?}: {formats:?}");
            };
            let caps = surface_fn.get_physical_device_surface_capabilities(pd, surface).unwrap();
            let modes = surface_fn.get_physical_device_surface_present_modes(pd, surface).unwrap();
            let mode = [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO].into_iter().find(|m| modes.contains(m)).unwrap();
            let extent = if caps.current_extent.width != u32::MAX { caps.current_extent } else { vk::Extent2D { width: s.w, height: s.h } };
            let count = caps.min_image_count.max(3).min(if caps.max_image_count == 0 { u32::MAX } else { caps.max_image_count });
            let sc_fn = ash::khr::swapchain::Device::new(&instance, &device);
            let swapchain = sc_fn
                .create_swapchain(
                    &vk::SwapchainCreateInfoKHR::default()
                        .surface(surface)
                        .min_image_count(count)
                        .image_format(sf.format)
                        .image_color_space(sf.color_space)
                        .image_extent(extent)
                        .image_array_layers(1)
                        .image_usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::COLOR_ATTACHMENT)
                        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .pre_transform(caps.current_transform)
                        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                        .present_mode(mode)
                        .clipped(true),
                    None,
                )
                .unwrap();
            if has_md {
                let m = program_metadata();
                let xy = |p: [u16; 2]| vk::XYColorEXT { x: p[0] as f32 / 50_000.0, y: p[1] as f32 / 50_000.0 };
                let md = vk::HdrMetadataEXT::default()
                    .display_primary_red(xy(m.primaries[0]))
                    .display_primary_green(xy(m.primaries[1]))
                    .display_primary_blue(xy(m.primaries[2]))
                    .white_point(xy(m.white_point))
                    .max_luminance(m.max_luminance as f32 / 10_000.0)
                    .min_luminance(m.min_luminance as f32 / 10_000.0)
                    .max_content_light_level(m.max_cll as f32)
                    .max_frame_average_light_level(m.max_fall as f32);
                ash::ext::hdr_metadata::Device::new(&instance, &device).set_hdr_metadata(&[swapchain], &[md]);
            }
            let images = sc_fn.get_swapchain_images(swapchain).unwrap();
            let (w, h) = (extent.width, extent.height);
            let band = stamp::layout(w, h).unwrap().size();
            let band_bytes = (band.0 * band.1 * 4) as u64;
            let block_bytes = (BLOCK * BLOCK * 4) as u64;
            let mem_props = instance.get_physical_device_memory_properties(pd);
            let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            const FRAMES: usize = 2;
            let mut staging = Vec::new();
            for _ in 0..FRAMES {
                let buf = device.create_buffer(&vk::BufferCreateInfo::default().size(band_bytes + block_bytes).usage(vk::BufferUsageFlags::TRANSFER_SRC), None).unwrap();
                let req = device.get_buffer_memory_requirements(buf);
                let ty = (0..mem_props.memory_type_count).find(|&i| req.memory_type_bits & (1 << i) != 0 && mem_props.memory_types[i as usize].property_flags.contains(host)).unwrap();
                let mem = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None).unwrap();
                device.bind_buffer_memory(buf, mem, 0).unwrap();
                let ptr = device.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()).unwrap().cast::<u32>();
                // The block never changes.
                let white = pixel(sf.format, true);
                for i in 0..(BLOCK * BLOCK) as usize {
                    *ptr.add((band_bytes / 4) as usize + i) = white;
                }
                staging.push((buf, mem, ptr));
            }
            let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER).queue_family_index(family), None).unwrap();
            let cmds = device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(FRAMES as u32)).unwrap();
            let fences: Vec<vk::Fence> = (0..FRAMES).map(|_| device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None).unwrap()).collect();
            let acquired: Vec<vk::Semaphore> = (0..FRAMES).map(|_| device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap()).collect();
            let rendered: Vec<vk::Semaphore> = images.iter().map(|_| device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap()).collect();
            // The background: 25 % grey in SDR; PQ 0.5 (about 92 nits) in HDR.
            let bg = vk::ClearColorValue { float32: if s.hdr { [0.5, 0.5, 0.5, 1.0] } else { [0.25, 0.25, 0.25, 1.0] } };
            let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
            let layers = vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 };
            let (on, off) = (pixel(sf.format, true), pixel(sf.format, false));
            let mut present_us = Samples::default();
            let period = Duration::from_nanos(1_000_000_000 / s.fps as u64);
            let start = Instant::now();
            let mut next = start;
            let mut seq = 0u64;
            std::thread::spawn(watchdog);
            while start.elapsed().as_secs_f64() < s.seconds {
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
                next += period;
                let f = (seq as usize) % FRAMES;
                phase(1, seq);
                if let Err(e) = device.wait_for_fences(&[fences[f]], true, 2_000_000_000) {
                    panic!("frame {seq}: the frame fence: {e:?}");
                }
                device.reset_fences(&[fences[f]]).unwrap();
                phase(2, seq);
                let (idx, _) = match sc_fn.acquire_next_image(swapchain, 1_000_000_000, acquired[f], vk::Fence::null()) {
                    Ok(r) => r,
                    Err(e) => panic!("acquire: {e:?}"),
                };
                // Draw: the stamp says when.
                let (_, _, ptr) = staging[f];
                let drawn = now_micros();
                stamp::paint(w, h, stamp::Stamp { seq, captured: drawn }, |x, y, cw, ch, white| {
                    let px = if white { on } else { off };
                    for row in y..(y + ch).min(band.1) {
                        for col in x..(x + cw).min(band.0) {
                            *ptr.add((row * band.0 + col) as usize) = px;
                        }
                    }
                });
                let img = images[idx as usize];
                let cmd = cmds[f];
                device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
                device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).unwrap();
                let to_dst = vk::ImageMemoryBarrier::default()
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(range);
                device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_dst]);
                device.cmd_clear_color_image(cmd, img, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &bg, &[range]);
                let mid = vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::TRANSFER_WRITE).dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[mid], &[], &[]);
                let bx = ((seq * 8) % (w - BLOCK) as u64) as i32;
                let regions = [
                    vk::BufferImageCopy::default().image_subresource(layers).image_extent(vk::Extent3D { width: band.0, height: band.1, depth: 1 }),
                    vk::BufferImageCopy::default()
                        .buffer_offset(band_bytes)
                        .image_subresource(layers)
                        .image_offset(vk::Offset3D { x: bx, y: (band.1 + 8) as i32, z: 0 })
                        .image_extent(vk::Extent3D { width: BLOCK, height: BLOCK, depth: 1 }),
                ];
                device.cmd_copy_buffer_to_image(cmd, staging[f].0, img, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &regions);
                let to_present = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(range);
                device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), &[], &[], &[to_present]);
                device.end_command_buffer(cmd).unwrap();
                let waits = [acquired[f]];
                let stages = [vk::PipelineStageFlags::TRANSFER];
                let signal = [rendered[idx as usize]];
                let cb = [cmd];
                phase(4, seq);
                device.queue_submit(queue, &[vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages).command_buffers(&cb).signal_semaphores(&signal)], fences[f]).unwrap();
                let scs = [swapchain];
                let idxs = [idx];
                phase(5, seq);
                let t = Instant::now();
                let r = sc_fn.queue_present(queue, &vk::PresentInfoKHR::default().wait_semaphores(&signal).swapchains(&scs).image_indices(&idxs));
                present_us.push(t.elapsed().as_micros() as u32);
                if let Err(e) = r {
                    panic!("present: {e:?}");
                }
                seq += 1;
            }
            device.device_wait_idle().unwrap();
            let line = format!(
                "{} {} {} {} {} {:?} {:?} {:?}\n",
                present_us.len(),
                present_us.median(),
                present_us.percentile(90.0).unwrap_or(0),
                present_us.percentile(99.0).unwrap_or(0),
                present_us.0.iter().copied().sum::<u32>() as f64 / present_us.len().max(1) as f64,
                mode,
                sf.format,
                sf.color_space
            );
            std::fs::write(&s.out, line).unwrap();
            for f in fences {
                device.destroy_fence(f, None);
            }
            for sem in acquired.into_iter().chain(rendered) {
                device.destroy_semaphore(sem, None);
            }
            device.destroy_command_pool(pool, None);
            for (b, m, _) in staging {
                device.destroy_buffer(b, None);
                device.free_memory(m, None);
            }
            sc_fn.destroy_swapchain(swapchain, None);
            device.destroy_device(None);
            surface_fn.destroy_surface(surface, None);
            instance.destroy_instance(None);
        }
    }
}

fn available() -> Option<PathBuf> {
    for (c, a) in [("gamescope", "--help"), ("ffmpeg", "-version"), ("pw-cli", "--version")] {
        if Command::new(c).arg(a).output().is_err() {
            eprintln!("skip: no {c}");
            return None;
        }
    }
    if quiver_stream_host::cuda::Cuda::open().is_err() {
        eprintln!("skip: no CUDA driver");
        return None;
    }
    let so = layer_so();
    if so.is_none() {
        eprintln!("skip: no libquiver_sight_layer.so near the test binary");
    }
    so
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Capture {
    /// No layer in the program: gamescope's screencast.
    PipeWire,
    /// The layer is in the program, no stream socket: what the layer costs a game when no
    /// stream is on.
    LayerIdle,
    /// The layer captures.
    Layer,
}

struct Measured {
    frames: usize,
    read: usize,
    from_layer: usize,
    draw_to_capture: Samples,
    capture_to_encode: Samples,
    draw_to_encoded: Samples,
    encode: Samples,
    /// The program's own vkQueuePresentKHR times: count, p50, p90, p99, mean, mode, format.
    present: String,
    configure: Vec<StreamConfig>,
    layer: Option<quiver_stream_host::layer_capture::LayerTimes>,
    /// The decoded background's luma (median).
    background: u32,
    probe: String,
}

async fn run(so: &Path, w: u32, h: u32, fps: u32, hdr: bool, capture: Capture, seconds: f64) -> Measured {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_env("QUIVER_TEST_LOG").unwrap_or_else(|_| "quiver_stream_host=info".into())).with_writer(std::io::stderr).try_init();
    let tag = format!("{w}x{h}@{fps}-{}-{capture:?}", if hdr { "hdr" } else { "sdr" });
    let dir = scratch().join(&tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join("layers");
    std::fs::create_dir_all(&manifest).unwrap();
    std::fs::write(manifest.join("VK_LAYER_QUIVER_sight.json"), quiver_sight_layer::manifest_json(so.to_str().unwrap(), true)).unwrap();
    let out = dir.join("present.txt");
    let spec = Spec { w, h, fps, hdr, seconds, out: out.clone() };
    let exe = std::env::current_exe().unwrap();
    // The layer goes in the program's environment only, not gamescope's.
    let mut argv: Vec<String> = vec!["env".into(), format!("{SOURCE}={}", spec.encode())];
    if capture != Capture::PipeWire {
        // As quiverd launches with gamescope's WSI layer on (sightenv::layer_order): below Sight.
        let layers = if hdr { "VK_LAYER_QUIVER_sight:VK_LAYER_FROG_gamescope_wsi_x86_64" } else { "VK_LAYER_QUIVER_sight" };
        // As the layer's own tests: its implicit manifest from this build, searched before an
        // installed one.
        argv.extend([format!("VK_ADD_IMPLICIT_LAYER_PATH={}", manifest.display()), "QUIVER_SIGHT=1".into(), format!("VK_INSTANCE_LAYERS={layers}"), "QUIVER_SIGHT_LOG=1".into()]);
    }
    argv.extend([exe.display().to_string(), "--exact".into(), "source_program".into(), "--ignored".into(), "--nocapture".into(), "--test-threads=1".into()]);
    let shm = dir.join("shm");
    std::fs::create_dir_all(&shm).unwrap();
    let mut env = vec![("QUIVER_SHM_ROOT".to_string(), shm.display().to_string())];
    if capture != Capture::Layer {
        env.push((quiver_stream_host::layer_capture::ENV_CAPTURE.into(), "pipewire".into()));
    }
    let color = if hdr { ColorInfo { bit_depth: 10, hdr: Some(quiver_stream_host::layer_capture::default_hdr()), ..ColorInfo::sdr() } } else { ColorInfo::sdr() };
    let bitrate = if w > 3000 { 60_000 } else { 40_000 };
    let config = StreamConfig { codec: VideoCodec::Hevc, width: w, height: h, refresh_mhz: fps * 1000, color, bitrate_kbps: bitrate, audio: AudioCodec::Opus, audio_channels: 2, audio_frame_ms: 5 };
    let opts = HostOptions {
        target: Target::Command { argv, env },
        config,
        session: format!("layer-{}", std::process::id()),
        dir: dir.join("run"),
        preset: 1,
        recovery: Recovery::RefInvalidation,
        headless_server: false,
        max_frames: None,
        force_sandbox_gid: None,
        exe: Some(PathBuf::from(env!("CARGO_BIN_EXE_quiver-stream-host"))),
        constant_bitrate_audio: false,
    };
    let (_btx, brx) = tokio::sync::watch::channel(bitrate);
    let mut host = Host::start(opts, brx).await.expect("start");
    let mut frames: Vec<EncodedFrame> = Vec::new();
    let mut configure = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs_f64(seconds + 1.0) {
        tokio::select! {
            Some(f) = host.frames.recv() => frames.push(f),
            Some(c) = host.control_out.recv() => if let Control::Configure(c) = c { configure.push(c) },
            Some(_) = host.audio.recv() => {}
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    let report = host.stop().await.unwrap();
    let layer = if capture == Capture::Layer { quiver_stream_host::layer_capture::LAST.lock().unwrap().take() } else { None };
    // Skip the first second: start-up, and the switch from PipeWire to the layer.
    let t0 = frames.first().map_or(0, |f| f.captured) + 1_000_000;
    let stream = dir.join("out.hevc");
    std::fs::write(&stream, frames.iter().flat_map(|f| f.data.iter().copied()).collect::<Vec<u8>>()).unwrap();
    let band = stamp::layout(w, h).unwrap().size();
    let probe_h = band.1 + 8 + 64 + 16;
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&stream).args(["-vf", &format!("crop={}:{probe_h}:0:0", band.0), "-f", "rawvideo", "-pix_fmt", "gray", "-"]).output().unwrap();
    let lumas: Vec<&[u8]> = o.stdout.chunks((band.0 * probe_h) as usize).collect();
    let probe = String::from_utf8_lossy(
        &Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=pix_fmt,color_transfer,color_primaries,color_space:side_data", "-show_frames", "-read_intervals", "%+#1", "-of", "compact"]).arg(&stream).output().unwrap().stdout,
    )
    .lines()
    .filter(|l| !l.is_empty())
    .collect::<Vec<_>>()
    .join(" | ");
    let mut m = Measured {
        frames: 0,
        read: 0,
        from_layer: 0,
        draw_to_capture: Samples::default(),
        capture_to_encode: Samples::default(),
        draw_to_encoded: Samples::default(),
        encode: Samples::default(),
        present: std::fs::read_to_string(&out).unwrap_or_default().trim().to_owned(),
        configure,
        layer,
        background: 0,
        probe,
    };
    let mut background = Samples::default();
    for (i, (f, l)) in frames.iter().zip(&lumas).enumerate() {
        if f.captured < t0 {
            continue;
        }
        m.frames += 1;
        if let Some(cap) = report.capture.0.get(i) {
            m.capture_to_encode.push(*cap);
        }
        if let Some(e) = report.encode.0.get(i) {
            m.encode.push(*e);
        }
        if let Some(s) = stamp::read(w, h, |x, y| l[(y * band.0 + x) as usize]) {
            m.read += 1;
            let d = f.captured.saturating_sub(s.captured);
            // Under a refresh from draw to capture: the layer's (PipeWire's is over one).
            if d < 1_000_000 / fps as u64 {
                m.from_layer += 1;
            }
            m.draw_to_capture.push(d as u32);
            m.draw_to_encoded.push(f.encoded.saturating_sub(s.captured) as u32);
        }
        // The background, below the block's row.
        background.push(l[((probe_h - 4) * band.0 + band.0 / 2) as usize] as u32);
    }
    m.background = background.median();
    let log = std::fs::read_to_string(dir.join("run/gamescope.log")).unwrap_or_default();
    for l in log.lines().filter(|l| l.contains("stream capture") || l.contains("panicked") || l.contains("frame fence") || l.contains("source program")) {
        eprintln!("  [layer] {l}");
    }
    m
}

fn modes() -> Vec<(u32, u32, u32)> {
    let s = std::env::var("QUIVER_STREAM_TEST_MODES").unwrap_or("2560x1440@120,3840x2160@60".into());
    s.split(',')
        .map(|m| {
            let (size, hz) = m.split_once('@').unwrap();
            let (w, h) = size.split_once('x').unwrap();
            (w.parse().unwrap(), h.parse().unwrap(), hz.parse().unwrap())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn layer_capture_against_pipewire() {
    let Some(so) = available() else { return };
    let mut rows = Vec::new();
    for (w, h, fps) in modes() {
        for hdr in [false, true] {
            for capture in [Capture::PipeWire, Capture::Layer] {
                if std::env::var_os("QUIVER_STREAM_TEST_LAYER_ONLY").is_some() && capture != Capture::Layer {
                    continue;
                }
                let m = run(&so, w, h, fps, hdr, capture, 4.0).await;
                let tag = format!("{w}x{h}@{fps} {} {capture:?}", if hdr { "HDR" } else { "SDR" });
                eprintln!("{tag}: {} frames, stamps read in {}, {} captured within a refresh of drawing", m.frames, m.read, m.from_layer);
                eprintln!("  program draws → capture: {}", m.draw_to_capture.summary());
                eprintln!("  capture → encode start:  {}", m.capture_to_encode.summary());
                eprintln!("  encode:                  {}", m.encode.summary());
                eprintln!("  program draws → encoded: {}", m.draw_to_encoded.summary());
                eprintln!("  program's present: {}", m.present);
                eprintln!("  background luma {}; ffprobe: {}", m.background, m.probe);
                if let Some(l) = &m.layer {
                    eprintln!("  layer: {} frames, {} from PipeWire, {} skipped, {} switches; present → message {}; message → ready {}", l.layer_frames, l.pipewire_frames, l.skipped, l.switches, l.message_us.summary(), l.ready_us.summary());
                }
                if let Some(c) = m.configure.last() {
                    eprintln!("  last Configure: {}x{} {:?} {}-bit {:?} hdr {:?}", c.width, c.height, c.codec, c.color.bit_depth, c.color.chroma, c.color.hdr);
                }
                assert!(m.frames as f64 > fps as f64 * 2.0, "{tag}: {} frames", m.frames);
                assert!(m.read as f64 > m.frames as f64 * 0.9, "{tag}: stamps read in {} of {}", m.read, m.frames);
                if capture == Capture::Layer {
                    let l = m.layer.as_ref().expect("layer times");
                    assert!(l.layer_frames as f64 > fps as f64 * 2.0, "{tag}: {} frames from the layer", l.layer_frames);
                    assert!(m.from_layer as f64 > m.read as f64 * 0.95, "{tag}: {} of {} frames came from the layer", m.from_layer, m.read);
                    let c = m.configure.last().unwrap();
                    if hdr {
                        assert_eq!(c.color.bit_depth, 10, "{tag}");
                        assert_eq!(c.color.hdr, Some(program_metadata()), "{tag}: the game's metadata reaches the stream");
                        assert!(m.probe.contains("smpte2084") && m.probe.contains("bt2020"), "{tag}: {}", m.probe);
                    } else {
                        assert!(c.color.hdr.is_none());
                    }
                }
                rows.push((tag, m));
            }
        }
    }
    eprintln!("\n| | draws → capture p50 / p99 | capture → encode start p50 / p99 | encode p50 | draws → encoded p50 / p99 |\n|---|---|---|---|---|");
    for (tag, m) in &rows {
        let ms = |s: &Samples, p: f64| s.percentile(p).map_or("–".into(), |v| format!("{:.2}", v as f64 / 1000.0));
        eprintln!("| {tag} | {} / {} ms | {} / {} ms | {} ms | {} / {} ms |", ms(&m.draw_to_capture, 50.0), ms(&m.draw_to_capture, 99.0), ms(&m.capture_to_encode, 50.0), ms(&m.capture_to_encode, 99.0), ms(&m.encode, 50.0), ms(&m.draw_to_encoded, 50.0), ms(&m.draw_to_encoded, 99.0));
    }
}

/// What the layer adds to the game's `vkQueuePresentKHR`, by the program's own clock.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn layer_present_overhead() {
    let Some(so) = available() else { return };
    let (w, h, fps) = modes()[0];
    for hdr in [false, true] {
        for capture in [Capture::PipeWire, Capture::LayerIdle, Capture::Layer] {
            let m = run(&so, w, h, fps, hdr, capture, 4.0).await;
            let what = match capture {
                Capture::PipeWire => "no layer",
                Capture::LayerIdle => "layer, no stream",
                Capture::Layer => "layer, capturing",
            };
            eprintln!("{w}x{h}@{fps} {}: {what}: present count p50 p90 p99 mean (µs), mode, format: {}", if hdr { "HDR" } else { "SDR" }, m.present);
        }
    }
}
