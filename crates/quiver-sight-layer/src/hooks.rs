//! Instance, device, queue and swapchain hooks. Each one calls down the chain first (or
//! instead, on any failure of ours) and does its own bookkeeping under a panic guard.

use std::ffi::{CStr, c_void};
use std::sync::{Arc, Mutex};

use ash::vk;
use quiver_perf::LayerStatus;
use quiver_perf::layout::{DRAW_ERR_FORMAT, DRAW_ERR_INTERNAL, DRAW_ERR_USAGE};

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
    // SAFETY: the app's arguments.
    let r = unsafe { create(physical, ci, alloc, out) };
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
    )));
    r
}

pub unsafe extern "system" fn destroy_device(device: vk::Device, alloc: *const vk::AllocationCallbacks<'_>) {
    let Some(d) = state::remove_device(device) else { return };
    guard("vkDestroyDevice", || {
        // Swapchains the app didn't destroy, then the per-format pipelines.
        let scs: Vec<_> = state::write(&d.swapchains).drain(..).collect();
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
    // Decide before creating, so COLOR_ATTACHMENT usage can be added for the compositor.
    let plan = guard("vkCreateSwapchainKHR", || {
        telemetry::init()?;
        Some(if !draw_enabled() { Err(DRAW_ERR_INTERNAL) } else { formats::classify(&orig).and_then(|enc| check_blend(&d, orig.image_format).map(|_| enc)) })
    })
    .flatten();
    let mut plan = plan.unwrap_or(Err(DRAW_ERR_INTERNAL));
    let mut patched = orig;
    if plan.is_ok() {
        patched.image_usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
    }
    // SAFETY: the app's arguments, maybe with one more usage bit (always supported for
    // swapchains: VkSurfaceCapabilitiesKHR::supportedUsageFlags includes COLOR_ATTACHMENT).
    let mut r = unsafe { next(device, &patched, alloc, out) };
    if r != vk::Result::SUCCESS && patched.image_usage != orig.image_usage && orig.old_swapchain == vk::SwapchainKHR::null() {
        // SAFETY: as the app asked.
        r = unsafe { next(device, ci, alloc, out) };
        plan = Err(DRAW_ERR_USAGE);
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

/// The compositor blends into the swapchain image: its format must support blending as a
/// colour attachment with optimal tiling.
fn check_blend(d: &DeviceData, format: vk::Format) -> Result<(), u32> {
    let Some(i) = &d.instance else { return Err(DRAW_ERR_INTERNAL) };
    // SAFETY: the device's physical device.
    let fp = unsafe { i.fns.get_physical_device_format_properties(d.physical, format) };
    let want = vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND;
    if fp.optimal_tiling_features.contains(want) { Ok(()) } else { Err(DRAW_ERR_FORMAT) }
}

unsafe fn register(d: &DeviceData, handle: vk::SwapchainKHR, ci: &vk::SwapchainCreateInfoKHR<'_>, plan: Result<Encoding, u32>) {
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
    let mut draw_error = 0;
    let comp = match plan {
        Ok(_) if images.is_empty() => {
            draw_error = DRAW_ERR_INTERNAL;
            None
        }
        // SAFETY: the swapchain's own images.
        Ok(enc) => match unsafe { Compositor::new(d, &images, ci.image_format, ci.image_extent, enc) } {
            Ok(c) => Some(c),
            Err(e) => {
                log!("compositor setup failed: {e:?}");
                draw_error = DRAW_ERR_INTERNAL;
                None
            }
        },
        Err(e) => {
            draw_error = e;
            None
        }
    };
    log!(
        "swapchain {id}: {}x{} {:?} {:?} {:?}, {} images, {}",
        ci.image_extent.width,
        ci.image_extent.height,
        ci.image_format,
        ci.image_color_space,
        ci.present_mode,
        images.len(),
        if comp.is_some() { "compositing".to_owned() } else { format!("not drawing (draw_error {draw_error})") }
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
    };
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
    if swapchain != vk::SwapchainKHR::null() {
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
