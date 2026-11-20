//! Instance, device, queue and swapchain hooks. Each one calls down the chain first (or
//! instead, on any failure of ours) and does its own bookkeeping under a panic guard.

use std::ffi::{CStr, c_void};
use std::sync::{Arc, Mutex};

use ash::vk::{self, Handle as _};
use quiver_perf::LayerStatus;
use quiver_perf::layout::{
    BACKDROP_ERR_FORMAT, BACKDROP_ERR_INTERNAL, BACKDROP_ERR_OFF, BACKDROP_ERR_USAGE, DRAW_ERR_FORMAT, DRAW_ERR_INTERNAL, DRAW_ERR_USAGE, LAYER_WAYLAND_SURFACE,
    LAYER_X11_SURFACE,
};

use crate::compositor::Compositor;
use crate::ffi::{self, LayerDeviceCreateInfo, LayerInstanceCreateInfo, find_in_chain};
use crate::formats::{self, Encoding};
use crate::state::{self, DeviceData, InstanceData, Swapchain, lock};
use crate::{draw_enabled, guard, log, telemetry};

unsafe fn load_fn(gipa: vk::PFN_vkGetInstanceProcAddr, instance: vk::Instance, name: &CStr) -> *const c_void {
    // SAFETY: a valid query down the chain.
    unsafe { gipa(instance, name.as_ptr()) }.map_or(std::ptr::null(), |f| f as *const c_void)
}

pub unsafe extern "system" fn create_instance(
    ci: *const vk::InstanceCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Instance,
) -> vk::Result {
    // SAFETY: the loader's create info carries our link in its pNext chain.
    let link_info = unsafe {
        find_in_chain((*ci).p_next, vk::StructureType::LOADER_INSTANCE_CREATE_INFO, |p| (*(p as *const LayerInstanceCreateInfo)).function == ffi::LAYER_LINK_INFO)
    } as *mut LayerInstanceCreateInfo;
    if link_info.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the loader's link list; advancing it for the next layer is the protocol.
    let (gipa, create) = unsafe {
        let link = (*link_info).u.layer_info;
        if link.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let Some(gipa) = (*link).next_get_instance_proc_addr else { return vk::Result::ERROR_INITIALIZATION_FAILED };
        (*link_info).u.layer_info = (*link).next;
        let create = load_fn(gipa, vk::Instance::null(), c"vkCreateInstance");
        if create.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        (gipa, std::mem::transmute::<*const c_void, vk::PFN_vkCreateInstance>(create))
    };
    // SAFETY: the app's arguments.
    let r = unsafe { create(ci, alloc, out) };
    if r != vk::Result::SUCCESS {
        return r;
    }
    guard("vkCreateInstance", || {
        // SAFETY: a fresh instance; the app info strings are valid for the call.
        unsafe {
            let instance = *out;
            let fns = ash::Instance::load_with(|n| load_fn(gipa, instance, n), instance);
            let mut data = InstanceData { handle: instance, next_gipa: gipa, fns, app_name: [0; 64], engine_name: [0; 32], engine_version: 0 };
            let app = (*ci).p_application_info;
            if !app.is_null() {
                if let Some(s) = ffi::c_str((*app).p_application_name) {
                    LayerStatus::set_str(&mut data.app_name, &s.to_string_lossy());
                }
                if let Some(s) = ffi::c_str((*app).p_engine_name) {
                    LayerStatus::set_str(&mut data.engine_name, &s.to_string_lossy());
                }
                data.engine_version = (*app).engine_version;
            }
            log!(
                "instance for {:?} (engine {:?})",
                LayerStatus::get_str(&data.app_name).unwrap_or(""),
                LayerStatus::get_str(&data.engine_name).unwrap_or("")
            );
            state::add_instance(data);
        }
    });
    r
}

pub unsafe extern "system" fn destroy_instance(instance: vk::Instance, alloc: *const vk::AllocationCallbacks<'_>) {
    // Surfaces the app didn't destroy: a keyboard of ours on their display goes first.
    guard("vkDestroyInstance", || crate::wayland::instance_destroyed(instance.as_raw()));
    let Some(data) = state::remove_instance(instance) else { return };
    // SAFETY: the app's arguments.
    unsafe { (data.fns.fp_v1_0().destroy_instance)(instance, alloc) };
}

pub unsafe extern "system" fn create_device(
    physical: vk::PhysicalDevice,
    ci: *const vk::DeviceCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Device,
) -> vk::Result {
    let is = |f: i32| move |p: *const c_void| unsafe { (*(p as *const LayerDeviceCreateInfo)).function == f };
    // SAFETY: the loader's create info carries our link and its loader-data callback.
    let (link_info, data_cb) = unsafe {
        (
            find_in_chain((*ci).p_next, vk::StructureType::LOADER_DEVICE_CREATE_INFO, is(ffi::LAYER_LINK_INFO)) as *mut LayerDeviceCreateInfo,
            find_in_chain((*ci).p_next, vk::StructureType::LOADER_DEVICE_CREATE_INFO, is(ffi::LOADER_DATA_CALLBACK)) as *const LayerDeviceCreateInfo,
        )
    };
    if link_info.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let instance = state::instance_of_physical(physical);
    // SAFETY: as in create_instance.
    let (gdpa, create, set_loader_data) = unsafe {
        let link = (*link_info).u.layer_info;
        if link.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let (Some(gipa), Some(gdpa)) = ((*link).next_get_instance_proc_addr, (*link).next_get_device_proc_addr) else {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        };
        (*link_info).u.layer_info = (*link).next;
        let inst = instance.as_ref().map_or(vk::Instance::null(), |i| i.handle);
        let create = load_fn(gipa, inst, c"vkCreateDevice");
        if create.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let set = if data_cb.is_null() { None } else { (*data_cb).u.set_device_loader_data };
        (gdpa, std::mem::transmute::<*const c_void, vk::PFN_vkCreateDevice>(create), set)
    };
    // The lab's GPU limit (labgpu.rs, `quiver lab` games only): every queue at low priority.
    let lowered = match (&instance, crate::labgpu::wants_low()) {
        // SAFETY: the app's create info; the instance that owns `physical`.
        (Some(i), true) => guard("lab priority", || unsafe { crate::labgpu::lower(&i.fns, physical, ci) }),
        _ => None,
    };
    // SAFETY: the app's arguments, or the same with low-priority queues. The loader's link
    // advances once per call down the chain, so a retry puts it back first.
    let link_next = unsafe { (*link_info).u.layer_info };
    let r = match &lowered {
        Some(Ok(l)) => {
            let r = unsafe { create(physical, l.info(), alloc, out) };
            if r == vk::Result::SUCCESS {
                crate::labgpu::note(if l.app_chose { crate::labgpu::Priority::AppChose } else { crate::labgpu::Priority::Low });
                r
            } else {
                log!("vkCreateDevice with low queue priority failed ({r:?}); again without it");
                crate::labgpu::note(crate::labgpu::Priority::Refused);
                unsafe { (*link_info).u.layer_info = link_next };
                unsafe { create(physical, ci, alloc, out) }
            }
        }
        Some(Err(p)) => {
            crate::labgpu::note(*p);
            unsafe { create(physical, ci, alloc, out) }
        }
        None => unsafe { create(physical, ci, alloc, out) },
    };
    if r != vk::Result::SUCCESS {
        return r;
    }
    // SAFETY: a fresh device.
    let device = unsafe { *out };
    let load = |n: &CStr| unsafe { gdpa(device, n.as_ptr()) }.map_or(std::ptr::null(), |f| f as *const c_void);
    // The pass-through needs these whatever happens below.
    // SAFETY: function pointers from the next layer.
    let dev = unsafe { ash::Device::load_with(load, device) };
    let swapchain_fn = ash::khr::swapchain::DeviceFn::load(load);
    // SAFETY: the next layer's pointers (null when the device lacks the extension).
    let ll2 = unsafe { crate::latency::Fns::load(load) };
    let mut families = Vec::new();
    let mut memory = vk::PhysicalDeviceMemoryProperties::default();
    let mut props = vk::PhysicalDeviceProperties::default();
    let (mut low_latency2, mut present_wait) = (false, false);
    guard("vkCreateDevice", || {
        // SAFETY: the app's create info; instance functions of the instance that owns
        // `physical`.
        unsafe {
            if let Some(i) = &instance {
                families = i.fns.get_physical_device_queue_family_properties(physical);
                memory = i.fns.get_physical_device_memory_properties(physical);
                props = i.fns.get_physical_device_properties(physical);
            }
            let exts = (*ci).pp_enabled_extension_names;
            for k in 0..(*ci).enabled_extension_count as usize {
                if exts.is_null() {
                    break;
                }
                match ffi::c_str(*exts.add(k)).map(CStr::to_bytes) {
                    Some(b"VK_NV_low_latency2") => low_latency2 = true,
                    Some(b"VK_KHR_present_wait") => present_wait = true,
                    _ => {}
                }
            }
        }
    });
    let name = props.device_name_as_c_str().ok().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
    log!("device {name} (low_latency2 {low_latency2}, present_wait {present_wait})");
    state::add_device(Arc::new(DeviceData::new(
        device,
        physical,
        gdpa,
        dev,
        swapchain_fn,
        set_loader_data,
        instance,
        families,
        memory,
        props,
        low_latency2,
        present_wait,
        ll2,
    )));
    r
}

pub unsafe extern "system" fn destroy_device(device: vk::Device, alloc: *const vk::AllocationCallbacks<'_>) {
    // No Reflex timings query on this device's swapchains from here on (latency.rs).
    let _q = lock(&crate::latency::QUERY);
    let Some(d) = state::remove_device(device) else { return };
    guard("vkDestroyDevice", || {
        // Swapchains the app didn't destroy, then the per-format pipelines.
        let scs: Vec<_> = state::write(&d.swapchains).drain(..).collect();
        d.clear_latency();
        let mut done = true;
        for (_, sc) in scs {
            done &= destroy_compositors(&d, &mut lock(&sc));
        }
        // A compositor that timed out leaked its objects; then these must leak too.
        if done {
            let pipes: Vec<_> = lock(&d.pipelines).drain(..).collect();
            for p in pipes {
                // SAFETY: nothing of ours is in flight (compositors waited).
                unsafe { p.destroy(&d.dev) };
            }
            let blur: Vec<_> = lock(&d.blur_pipes).drain(..).collect();
            for p in blur {
                // SAFETY: as above.
                unsafe { p.destroy(&d.dev) };
            }
            d.destroy_retired_semaphores();
        }
        state::retarget();
    });
    // SAFETY: the app's arguments.
    unsafe { (d.dev.fp_v1_0().destroy_device)(device, alloc) };
}

pub unsafe extern "system" fn get_device_queue(device: vk::Device, family: u32, index: u32, out: *mut vk::Queue) {
    let Some(d) = state::device(device) else { return };
    // SAFETY: the app's arguments.
    unsafe { (d.dev.fp_v1_0().get_device_queue)(device, family, index, out) };
    // SAFETY: written by the call.
    let q = unsafe { *out };
    if q != vk::Queue::null() {
        guard("vkGetDeviceQueue", || d.add_queue(q, family));
    }
}

pub unsafe extern "system" fn get_device_queue2(device: vk::Device, info: *const vk::DeviceQueueInfo2<'_>, out: *mut vk::Queue) {
    let Some(d) = state::device(device) else { return };
    // SAFETY: the app's arguments.
    unsafe { (d.dev.fp_v1_1().get_device_queue2)(device, info, out) };
    // SAFETY: valid after the call.
    let (q, family) = unsafe { (*out, (*info).queue_family_index) };
    if q != vk::Queue::null() {
        guard("vkGetDeviceQueue2", || d.add_queue(q, family));
    }
}

pub unsafe extern "system" fn create_swapchain_khr(
    device: vk::Device,
    ci: *const vk::SwapchainCreateInfoKHR<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::SwapchainKHR,
) -> vk::Result {
    let Some(d) = state::device(device) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let next = d.swapchain_fn.create_swapchain_khr;
    // SAFETY: the app's create info.
    let orig = unsafe { *ci };
    // Decide before creating, so COLOR_ATTACHMENT usage can be added for the compositor (and
    // SAMPLED for the backdrop blur, which reads the image).
    let plan = guard("vkCreateSwapchainKHR", || {
        telemetry::init()?;
        Some(plan_swapchain(&d, &orig))
    })
    .flatten();
    let mut plan = plan.unwrap_or(Plan { hud: Err(DRAW_ERR_INTERNAL), blur: Err(BACKDROP_ERR_INTERNAL), capture: false });
    // Captures (hdr.rs) need TRANSFER_SRC; added with whatever else is added, dropped with it.
    let extra = if plan.capture { vk::ImageUsageFlags::TRANSFER_SRC } else { vk::ImageUsageFlags::empty() };
    let with = |usage: vk::ImageUsageFlags| {
        let mut p = orig;
        p.image_usage |= usage | extra;
        p
    };
    let blur_usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
    let patched = if plan.blur.is_ok() {
        with(blur_usage)
    } else if plan.hud.is_ok() || plan.capture {
        with(vk::ImageUsageFlags::COLOR_ATTACHMENT)
    } else {
        orig
    };
    // SAFETY: the app's arguments, maybe with more usage bits: COLOR_ATTACHMENT is always
    // supported for swapchains (VkSurfaceCapabilitiesKHR::supportedUsageFlags includes it),
    // SAMPLED only added when the surface lists it.
    let mut r = unsafe { next(device, &patched, alloc, out) };
    // A failed create retires a non-null oldSwapchain: retries only without one.
    let retry = orig.old_swapchain == vk::SwapchainKHR::null();
    if r != vk::Result::SUCCESS && retry && plan.blur.is_ok() {
        log!("swapchain creation with SAMPLED usage failed ({r:?}): no backdrop blur");
        plan.blur = Err(BACKDROP_ERR_USAGE);
        if plan.hud.is_ok() {
            // SAFETY: as above, COLOR_ATTACHMENT only.
            r = unsafe { next(device, &with(vk::ImageUsageFlags::COLOR_ATTACHMENT), alloc, out) };
        }
    }
    if r != vk::Result::SUCCESS && retry && patched.image_usage != orig.image_usage {
        // SAFETY: as the app asked.
        r = unsafe { next(device, ci, alloc, out) };
        plan.hud = Err(DRAW_ERR_USAGE);
        plan.blur = Err(BACKDROP_ERR_USAGE);
        plan.capture = false;
    }
    if r != vk::Result::SUCCESS {
        return r;
    }
    // SAFETY: just created.
    let handle = unsafe { *out };
    guard("vkCreateSwapchainKHR", || {
        // SAFETY: a live swapchain of `d`.
        unsafe { register(&d, handle, &orig, plan) }
    });
    r
}

/// What the compositor will do on a new swapchain: draw the HUD (and how), blur behind the
/// Sight panel; each `Err` is why not (`DRAW_ERR_*`, `BACKDROP_ERR_*`).
struct Plan {
    hud: Result<Encoding, u32>,
    blur: Result<(), u32>,
    /// `TRANSFER_SRC` usage for captures (`QUIVER_SIGHT_CAPTURE=1`, a known format, the
    /// surface allows it).
    capture: bool,
}

fn plan_swapchain(d: &DeviceData, ci: &vk::SwapchainCreateInfoKHR<'_>) -> Plan {
    if !draw_enabled() {
        return Plan { hud: Err(DRAW_ERR_INTERNAL), blur: Err(BACKDROP_ERR_OFF), capture: false };
    }
    let features = format_features(d, ci.image_format);
    let blend = vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND;
    // The compositor blends into the swapchain image: its format must support blending as a
    // colour attachment with optimal tiling.
    let hud = formats::classify(ci).and_then(|enc| if features.contains(blend) { Ok(enc) } else { Err(DRAW_ERR_FORMAT) });
    let blur = if !crate::blur_enabled() {
        Err(BACKDROP_ERR_OFF)
    } else {
        formats::blur_class(ci).and_then(|()| {
            if !features.contains(formats::BLUR_FEATURES) {
                Err(BACKDROP_ERR_FORMAT)
            } else if !surface_usage(d, ci.surface).is_some_and(|u| u.contains(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::COLOR_ATTACHMENT)) {
                Err(BACKDROP_ERR_USAGE)
            } else {
                Ok(())
            }
        })
    };
    let capture = crate::capture_enabled()
        && crate::hdr::bytes_per_pixel(ci.image_format).is_some()
        && formats::drawable(ci).is_ok()
        && surface_usage(d, ci.surface).is_some_and(|u| u.contains(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::COLOR_ATTACHMENT));
    Plan { hud, blur, capture }
}

fn format_features(d: &DeviceData, format: vk::Format) -> vk::FormatFeatureFlags {
    let Some(i) = &d.instance else { return vk::FormatFeatureFlags::empty() };
    // SAFETY: the device's physical device.
    unsafe { i.fns.get_physical_device_format_properties(d.physical, format) }.optimal_tiling_features
}

/// The usage flags the surface allows on swapchain images, asked down the chain.
fn surface_usage(d: &DeviceData, surface: vk::SurfaceKHR) -> Option<vk::ImageUsageFlags> {
    let i = d.instance.as_ref()?;
    // SAFETY: a valid query down the chain; the function has this type.
    let f = unsafe { load_fn(i.next_gipa, i.handle, c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR") };
    if f.is_null() || surface == vk::SurfaceKHR::null() {
        return None;
    }
    let f = unsafe { std::mem::transmute::<*const c_void, vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR>(f) };
    let mut caps = vk::SurfaceCapabilitiesKHR::default();
    // SAFETY: the app's surface (valid for its vkCreateSwapchainKHR), our output struct.
    (unsafe { f(d.physical, surface, &mut caps) } == vk::Result::SUCCESS).then_some(caps.supported_usage_flags)
}

unsafe fn register(d: &DeviceData, handle: vk::SwapchainKHR, ci: &vk::SwapchainCreateInfoKHR<'_>, plan: Plan) {
    if telemetry::get().is_none() {
        return;
    }
    // SAFETY: a live swapchain.
    let images = unsafe {
        let get = d.swapchain_fn.get_swapchain_images_khr;
        let mut n = 0u32;
        let mut v = Vec::new();
        if get(d.handle, handle, &mut n, std::ptr::null_mut()) == vk::Result::SUCCESS {
            v.resize(n as usize, vk::Image::null());
            if get(d.handle, handle, &mut n, v.as_mut_ptr()) != vk::Result::SUCCESS {
                v.clear();
            }
            v.truncate(n as usize);
        }
        v
    };
    let id = state::next_swapchain_id();
    let latency = Arc::new(crate::latency::Latency::default());
    // The HUD's reason sets draw_error; a compositor exists when either the HUD or the blur
    // can draw.
    let mut draw_error = plan.hud.err().unwrap_or(0);
    let mut blur_error = plan.blur.err().unwrap_or(0);
    let comp = if plan.hud.is_err() && plan.blur.is_err() && !plan.capture {
        None
    } else if images.is_empty() {
        draw_error = DRAW_ERR_INTERNAL;
        blur_error = BACKDROP_ERR_INTERNAL;
        None
    } else {
        // SAFETY: the swapchain's own images.
        match unsafe { Compositor::new(d, &images, ci.image_format, ci.image_extent, plan.hud, plan.blur) } {
            Ok(mut c) => {
                c.set_capture(plan.capture, ci.image_color_space, id);
                Some(c)
            }
            Err(e) => {
                log!("compositor setup failed: {e:?}");
                draw_error = DRAW_ERR_INTERNAL;
                blur_error = BACKDROP_ERR_INTERNAL;
                None
            }
        }
    };
    log!(
        "swapchain {id}: {}x{} {:?} {:?} {:?}, {} images, {}, {}",
        ci.image_extent.width,
        ci.image_extent.height,
        ci.image_format,
        ci.image_color_space,
        ci.present_mode,
        images.len(),
        if comp.is_some() && draw_error == 0 { "compositing".to_owned() } else { format!("no HUD (draw_error {draw_error})") },
        if comp.is_some() && blur_error == 0 { "backdrop blur ready".to_owned() } else { format!("no backdrop blur (error {blur_error})") }
    );
    let sc = Swapchain {
        id,
        extent: ci.image_extent,
        format: ci.image_format,
        color_space: ci.image_color_space,
        present_mode: ci.present_mode,
        image_count: images.len() as u32,
        seq: 0,
        comp,
        disabled: Vec::new(),
        draw_error,
        blur_error,
        latency: latency.clone(),
    };
    d.add_latency(handle, latency);
    state::write(&d.swapchains).push((handle, Arc::new(Mutex::new(sc))));
    state::retarget();
}

/// Destroys a swapchain's compositors; false if one had to leak its objects.
fn destroy_compositors(d: &DeviceData, s: &mut Swapchain) -> bool {
    let mut all: Vec<Compositor> = s.disabled.drain(..).collect();
    all.extend(s.comp.take());
    let mut done = true;
    for c in all {
        // SAFETY: the swapchain (and so its images) is going away; waits for our work.
        done &= unsafe { c.destroy(d) };
    }
    done
}

pub unsafe extern "system" fn destroy_swapchain_khr(device: vk::Device, swapchain: vk::SwapchainKHR, alloc: *const vk::AllocationCallbacks<'_>) {
    let Some(d) = state::device(device) else { return };
    // No Reflex timings query on it from here on (latency.rs).
    let _q = lock(&crate::latency::QUERY);
    if swapchain != vk::SwapchainKHR::null() {
        d.remove_latency(swapchain);
        guard("vkDestroySwapchainKHR", || {
            let sc = {
                let mut v = state::write(&d.swapchains);
                v.iter().position(|(h, _)| *h == swapchain).map(|i| v.remove(i).1)
            };
            if let Some(sc) = sc {
                destroy_compositors(&d, &mut lock(&sc));
                state::retarget();
            }
        });
    }
    // SAFETY: the app's arguments.
    unsafe { (d.swapchain_fn.destroy_swapchain_khr)(device, swapchain, alloc) };
}

/// `vkCreate{Xlib,Xcb,Wayland}SurfaceKHR` share this shape (their create infos differ).
type PfnCreateSurface = unsafe extern "system" fn(vk::Instance, *const c_void, *const vk::AllocationCallbacks<'_>, *mut vk::SurfaceKHR) -> vk::Result;

/// Calls `name` down the chain and notes the surface kind for the HUD keys (`keys.rs`:
/// XWayland never sees a Wayland window's keys).
unsafe fn create_surface(name: &CStr, flag: u32, instance: vk::Instance, ci: *const c_void, alloc: *const vk::AllocationCallbacks<'_>, out: *mut vk::SurfaceKHR) -> vk::Result {
    let Some(inst) = state::instance(instance) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    // SAFETY: a valid query down the chain; GetInstanceProcAddr only returned our hook
    // because the next layer has this function.
    let next = unsafe { load_fn(inst.next_gipa, instance, name) };
    if next.is_null() {
        return vk::Result::ERROR_EXTENSION_NOT_PRESENT;
    }
    // SAFETY: the function's real type is PfnCreateSurface's shape; the app's arguments.
    let r = unsafe { std::mem::transmute::<*const c_void, PfnCreateSurface>(next)(instance, ci, alloc, out) };
    if r == vk::Result::SUCCESS {
        crate::keys::note_surface(flag);
    }
    r
}

pub unsafe extern "system" fn create_xlib_surface_khr(
    instance: vk::Instance,
    ci: *const vk::XlibSurfaceCreateInfoKHR<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::SurfaceKHR,
) -> vk::Result {
    // SAFETY: the app's arguments.
    let r = unsafe { create_surface(c"vkCreateXlibSurfaceKHR", LAYER_X11_SURFACE, instance, ci.cast(), alloc, out) };
    if r == vk::Result::SUCCESS {
        // SAFETY: the app's create info. The window, for whether it's in front (focus.rs).
        // Xlib's `Window` is a C `unsigned long`: 32 bits in the i686 layer, 64 in x86_64.
        #[allow(clippy::useless_conversion)]
        guard("vkCreateXlibSurfaceKHR", || unsafe { crate::focus::note_x11_window(u64::from((*ci).window)) });
    }
    r
}

pub unsafe extern "system" fn create_xcb_surface_khr(
    instance: vk::Instance,
    ci: *const vk::XcbSurfaceCreateInfoKHR<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::SurfaceKHR,
) -> vk::Result {
    // SAFETY: the app's arguments.
    let r = unsafe { create_surface(c"vkCreateXcbSurfaceKHR", LAYER_X11_SURFACE, instance, ci.cast(), alloc, out) };
    if r == vk::Result::SUCCESS {
        // SAFETY: as for Xlib.
        guard("vkCreateXcbSurfaceKHR", || unsafe { crate::focus::note_x11_window(u64::from((*ci).window)) });
    }
    r
}

pub unsafe extern "system" fn create_wayland_surface_khr(
    instance: vk::Instance,
    ci: *const vk::WaylandSurfaceCreateInfoKHR<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::SurfaceKHR,
) -> vk::Result {
    // SAFETY: the app's arguments.
    let r = unsafe { create_surface(c"vkCreateWaylandSurfaceKHR", LAYER_WAYLAND_SURFACE, instance, ci.cast(), alloc, out) };
    if r == vk::Result::SUCCESS {
        guard("vkCreateWaylandSurfaceKHR", || {
            // SAFETY: the app's create info; the surface was just written. Only the display
            // pointer is kept (wayland.rs: our keyboard goes on the game's connection).
            unsafe { crate::wayland::surface_created(instance.as_raw(), (*out).as_raw(), (*ci).display.cast()) }
        });
    }
    r
}

pub unsafe extern "system" fn destroy_surface_khr(instance: vk::Instance, surface: vk::SurfaceKHR, alloc: *const vk::AllocationCallbacks<'_>) {
    // First: the app may disconnect the surface's wl_display as soon as this returns, so a
    // keyboard of ours on it goes now.
    if surface != vk::SurfaceKHR::null() {
        guard("vkDestroySurfaceKHR", || crate::wayland::surface_destroyed(instance.as_raw(), surface.as_raw()));
    }
    let Some(inst) = state::instance(instance) else { return };
    // SAFETY: a valid query down the chain (we only hook it when the chain has it).
    let next = unsafe { load_fn(inst.next_gipa, instance, c"vkDestroySurfaceKHR") };
    if next.is_null() {
        return;
    }
    // SAFETY: the function's type; the app's arguments.
    unsafe { std::mem::transmute::<*const c_void, vk::PFN_vkDestroySurfaceKHR>(next)(instance, surface, alloc) };
}
