//! Per-instance, per-device and per-swapchain state, keyed by dispatch key. The maps are small
//! `Vec`s behind `RwLock`s: a present takes one read lock and clones an `Arc` (no allocation);
//! writers are creation and destruction calls.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use ash::vk;
use ash::vk::Handle;
use quiver_perf::LayerStatus;
use quiver_perf::layout::{LAYER_LOW_LATENCY2, LAYER_PRESENT_WAIT};

use crate::compositor::Compositor;
use crate::ffi::{PfnSetDeviceLoaderData, dispatch_key};
use crate::graph::History;
use crate::latency::{self, Latency};
use crate::pipelines::Pipelines;
use crate::telemetry;

pub struct InstanceData {
    pub handle: vk::Instance,
    pub next_gipa: vk::PFN_vkGetInstanceProcAddr,
    pub fns: ash::Instance,
    pub app_name: [u8; 64],
    pub engine_name: [u8; 32],
    pub engine_version: u32,
}

pub struct DeviceData {
    pub handle: vk::Device,
    pub physical: vk::PhysicalDevice,
    pub next_gdpa: vk::PFN_vkGetDeviceProcAddr,
    pub dev: ash::Device,
    pub swapchain_fn: ash::khr::swapchain::DeviceFn,
    pub set_loader_data: Option<PfnSetDeviceLoaderData>,
    pub instance: Option<Arc<InstanceData>>,
    pub families: Vec<vk::QueueFamilyProperties>,
    pub memory: vk::PhysicalDeviceMemoryProperties,
    pub props: vk::PhysicalDeviceProperties,
    /// `VK_NV_low_latency2` enabled: our submits carry `VkLatencySubmissionPresentIdNV`.
    pub low_latency2: bool,
    pub present_wait: bool,
    /// The next layer's `VK_NV_low_latency2` commands (latency.rs).
    pub ll2: latency::Fns,
    /// Each registered swapchain's Reflex state, apart from the swapchain lock (the app's
    /// latency calls must not wait for a present's HUD work).
    latency: RwLock<Vec<(vk::SwapchainKHR, Arc<Latency>)>>,
    queues: RwLock<Vec<(vk::Queue, u32)>>,
    pub swapchains: RwLock<Vec<(vk::SwapchainKHR, Arc<Mutex<Swapchain>>)>>,
    pub pipelines: Mutex<Vec<Arc<Pipelines>>>,
    /// Semaphores of destroyed compositors a present may still wait on: destroyed with the
    /// device, or once they're long retired.
    semaphores: Mutex<Vec<vk::Semaphore>>,
    /// The device was lost (our submit or the app's present said so): no more drawing.
    pub lost: AtomicBool,
}

impl DeviceData {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        handle: vk::Device,
        physical: vk::PhysicalDevice,
        next_gdpa: vk::PFN_vkGetDeviceProcAddr,
        dev: ash::Device,
        swapchain_fn: ash::khr::swapchain::DeviceFn,
        set_loader_data: Option<PfnSetDeviceLoaderData>,
        instance: Option<Arc<InstanceData>>,
        families: Vec<vk::QueueFamilyProperties>,
        memory: vk::PhysicalDeviceMemoryProperties,
        props: vk::PhysicalDeviceProperties,
        low_latency2: bool,
        present_wait: bool,
        ll2: latency::Fns,
    ) -> DeviceData {
        DeviceData {
            handle,
            physical,
            next_gdpa,
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
            latency: RwLock::new(Vec::new()),
            queues: RwLock::new(Vec::new()),
            swapchains: RwLock::new(Vec::new()),
            pipelines: Mutex::new(Vec::new()),
            semaphores: Mutex::new(Vec::new()),
            lost: AtomicBool::new(false),
        }
    }

    pub fn add_queue(&self, queue: vk::Queue, family: u32) {
        let mut q = write(&self.queues);
        if !q.iter().any(|(h, _)| *h == queue) {
            q.push((queue, family));
        }
    }

    pub fn queue_family(&self, queue: vk::Queue) -> Option<u32> {
        read(&self.queues).iter().find(|(h, _)| *h == queue).map(|(_, f)| *f)
    }

    pub fn swapchain(&self, handle: vk::SwapchainKHR) -> Option<Arc<Mutex<Swapchain>>> {
        read(&self.swapchains).iter().find(|(h, _)| *h == handle).map(|(_, s)| s.clone())
    }

    pub fn latency(&self, handle: vk::SwapchainKHR) -> Option<Arc<Latency>> {
        read(&self.latency).iter().find(|(h, _)| *h == handle).map(|(_, l)| l.clone())
    }

    pub fn add_latency(&self, handle: vk::SwapchainKHR, l: Arc<Latency>) {
        let mut v = write(&self.latency);
        v.retain(|(h, _)| *h != handle);
        v.push((handle, l));
    }

    pub fn remove_latency(&self, handle: vk::SwapchainKHR) {
        write(&self.latency).retain(|(h, _)| *h != handle);
    }

    pub fn clear_latency(&self) {
        write(&self.latency).clear();
    }

    /// Keeps per-image semaphores of a destroyed compositor. The oldest are destroyed once
    /// there are many: by then many swapchains (and presents) have come and gone.
    pub fn retire_semaphores(&self, sems: impl Iterator<Item = vk::Semaphore>) {
        let mut v = lock(&self.semaphores);
        v.extend(sems);
        if v.len() > 256 {
            for s in v.drain(..128) {
                // SAFETY: retired long ago.
                unsafe { self.dev.destroy_semaphore(s, None) };
            }
        }
    }

    /// At vkDestroyDevice (the app has waited for the device).
    pub fn destroy_retired_semaphores(&self) {
        for s in lock(&self.semaphores).drain(..) {
            // SAFETY: the device is idle and going away.
            unsafe { self.dev.destroy_semaphore(s, None) };
        }
    }

    /// A memory type in `bits` with all of `want` (and preferably `prefer` too).
    pub fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags, prefer: vk::MemoryPropertyFlags) -> Option<u32> {
        let n = (self.memory.memory_type_count as usize).min(vk::MAX_MEMORY_TYPES);
        let ok = |i: usize, f: vk::MemoryPropertyFlags| bits & (1 << i) != 0 && self.memory.memory_types[i].property_flags.contains(f);
        (0..n).find(|&i| ok(i, want | prefer)).or_else(|| (0..n).find(|&i| ok(i, want))).map(|i| i as u32)
    }

    /// The compositor pipelines for a swapchain format, built once per device and format.
    pub fn pipelines_for(&self, format: vk::Format) -> Result<Arc<Pipelines>, vk::Result> {
        let mut v = self.pipelines.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(p) = v.iter().find(|p| p.format == format) {
            return Ok(p.clone());
        }
        // SAFETY: a live device.
        let p = Arc::new(unsafe { Pipelines::new(&self.dev, format)? });
        v.push(p.clone());
        Ok(p)
    }
}

pub struct Swapchain {
    /// The layer's id, in present records; new on every vkCreateSwapchainKHR.
    pub id: u32,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    pub color_space: vk::ColorSpaceKHR,
    pub present_mode: vk::PresentModeKHR,
    pub image_count: u32,
    pub seq: u64,
    pub comp: Option<Compositor>,
    /// Compositors switched off after an error, destroyed with the swapchain (their work may
    /// still be in flight when they fail).
    pub disabled: Vec<Compositor>,
    /// Why there's no compositor (`DRAW_ERR_*`), or why it was disabled.
    pub draw_error: u32,
    /// Reflex state (also in the device's latency list).
    pub latency: Arc<Latency>,
}

/// Locks, ignoring poisoning (a panic is already handled by [`crate::guard`]).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|p| p.into_inner())
}

pub fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|p| p.into_inner())
}

static INSTANCES: RwLock<Vec<(usize, Arc<InstanceData>)>> = RwLock::new(Vec::new());
static DEVICES: RwLock<Vec<(usize, Arc<DeviceData>)>> = RwLock::new(Vec::new());

/// The swapchain the HUD draws on: the largest one (the game window).
pub static TARGET: AtomicU32 = AtomicU32::new(0);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// Present times of the target swapchain, for the graph. Kept across recreation so a resize
/// doesn't restart the graph.
pub static HISTORY: Mutex<History> = Mutex::new(History::new());

pub fn next_swapchain_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn key_of(raw: u64) -> Option<usize> {
    // SAFETY: non-null dispatchable handles from the loader.
    (raw != 0).then(|| unsafe { dispatch_key(raw) })
}

pub fn add_instance(i: InstanceData) {
    let Some(k) = key_of(i.handle.as_raw()) else { return };
    let mut v = write(&INSTANCES);
    v.retain(|(kk, _)| *kk != k);
    v.push((k, Arc::new(i)));
}

pub fn remove_instance(h: vk::Instance) -> Option<Arc<InstanceData>> {
    let k = key_of(h.as_raw())?;
    let mut v = write(&INSTANCES);
    let i = v.iter().position(|(kk, _)| *kk == k)?;
    Some(v.remove(i).1)
}

pub fn instance(h: vk::Instance) -> Option<Arc<InstanceData>> {
    let k = key_of(h.as_raw())?;
    read(&INSTANCES).iter().find(|(kk, _)| *kk == k).map(|(_, i)| i.clone())
}

/// The instance a physical device belongs to: physical devices share their instance's
/// dispatch key. Falls back to the newest instance.
pub fn instance_of_physical(p: vk::PhysicalDevice) -> Option<Arc<InstanceData>> {
    let k = key_of(p.as_raw())?;
    let v = read(&INSTANCES);
    v.iter().find(|(kk, _)| *kk == k).or(v.last()).map(|(_, i)| i.clone())
}

pub fn add_device(d: Arc<DeviceData>) {
    let Some(k) = key_of(d.handle.as_raw()) else { return };
    let mut v = write(&DEVICES);
    v.retain(|(kk, _)| *kk != k);
    v.push((k, d));
}

pub fn remove_device(h: vk::Device) -> Option<Arc<DeviceData>> {
    let k = key_of(h.as_raw())?;
    let mut v = write(&DEVICES);
    let i = v.iter().position(|(kk, _)| *kk == k)?;
    Some(v.remove(i).1)
}

/// The device of any of its dispatchable handles (device, queue, command buffer).
pub fn device_by_raw(raw: u64) -> Option<Arc<DeviceData>> {
    let k = key_of(raw)?;
    read(&DEVICES).iter().find(|(kk, _)| *kk == k).map(|(_, d)| d.clone())
}

pub fn device(h: vk::Device) -> Option<Arc<DeviceData>> {
    device_by_raw(h.as_raw())
}

/// Picks the swapchain to draw on (largest area, newest on ties) and puts its details in the
/// status block. Called on swapchain creation and destruction.
pub fn retarget() {
    let mut best: Option<(u64, u32, LayerStatus)> = None;
    let mut best_latency: Option<(vk::Device, vk::SwapchainKHR, Arc<Latency>)> = None;
    let mut count = 0;
    {
        let devs = read(&DEVICES);
        for (_, d) in devs.iter() {
            let scs = read(&d.swapchains);
            for (handle, sc) in scs.iter() {
                let s = lock(sc);
                count += 1;
                let area = s.extent.width as u64 * s.extent.height as u64;
                if best.as_ref().is_some_and(|(a, id, _)| (*a, *id) > (area, s.id)) {
                    continue;
                }
                let mut st = LayerStatus::ZERO;
                st.extent_w = s.extent.width;
                st.extent_h = s.extent.height;
                st.vk_format = s.format.as_raw() as u32;
                st.color_space = s.color_space.as_raw() as u32;
                st.present_mode = s.present_mode.as_raw() as u32;
                st.image_count = s.image_count;
                st.swapchain = s.id;
                st.vendor_id = d.props.vendor_id;
                st.device_id = d.props.device_id;
                st.driver_version = d.props.driver_version;
                st.api_version = d.props.api_version;
                let name = d.props.device_name_as_c_str().ok().and_then(|c| c.to_str().ok()).unwrap_or("");
                LayerStatus::set_str(&mut st.device_name, name);
                if let Some(i) = &d.instance {
                    st.app_name = i.app_name;
                    st.engine_name = i.engine_name;
                    st.engine_version = i.engine_version;
                }
                if d.low_latency2 {
                    st.flags |= LAYER_LOW_LATENCY2;
                }
                if d.present_wait {
                    st.flags |= LAYER_PRESENT_WAIT;
                }
                st.draw_error = s.draw_error;
                best = Some((area, s.id, st));
                best_latency = Some((d.handle, *handle, s.latency.clone()));
            }
        }
    }
    let target = best.as_ref().map_or(0, |(_, id, _)| *id);
    *lock(&latency::TARGET) = best_latency;
    let prev = TARGET.swap(target, Ordering::Relaxed);
    if let Some(t) = telemetry::get() {
        t.update_status(|st| {
            *st = match best {
                Some((_, _, s)) => s,
                // No swapchain: keep who we are (app, engine, device), clear the rest.
                None => LayerStatus {
                    vendor_id: st.vendor_id,
                    device_id: st.device_id,
                    driver_version: st.driver_version,
                    api_version: st.api_version,
                    engine_version: st.engine_version,
                    engine_name: st.engine_name,
                    app_name: st.app_name,
                    device_name: st.device_name,
                    ..LayerStatus::ZERO
                },
            };
            st.swapchain_count = count;
        });
        if prev != target {
            t.draw_error.store(0, Ordering::Relaxed);
        }
    }
}
