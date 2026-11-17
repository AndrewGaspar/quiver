//! `VK_LAYER_QUIVER_sight` (design/SIGHT-HUD.md §2.8, §3.6–3.8; design/SIGHT.md §4.3): a
//! Vulkan layer loaded into game processes (DXVK and vkd3d-proton under Wine, native Vulkan
//! games) that
//!
//! - **records every present** into the session dir's `layer-<pid>.ring` (CPU times at entry
//!   and return, the app's `VkPresentIdKHR`, image index, present mode, result) and keeps the
//!   ring's status block (swapchain extent/format, counters, why it can't draw) current, with
//!   a heartbeat at least once a second;
//! - **composites the HUD**: a premultiplied RGBA image `quiver-sight` renders into
//!   `hud.image`, uploaded only when its publish counter changes, blended at the descriptor's
//!   position and opacity onto the swapchain image just before present, plus the live
//!   frame-time graph drawn from the layer's own present history.
//!
//! It is inert unless Quiver enables it: the loader only loads it when `VK_INSTANCE_LAYERS`
//! names it (manifest/), and without `QUIVER_PERF_DIR` it passes every call through.
//!
//! Robustness rules, since this runs inside the game: every entry point catches panics (the
//! first one disables drawing for the process; telemetry and pass-through stay); every
//! failure disables drawing and is reported in the status block's `draw_error`; the present
//! path doesn't allocate, block, sleep or make syscalls beyond the Vulkan calls it needs, and
//! its work is bounded; everything read from shared memory is validated (ubi-perf).

#![allow(clippy::missing_safety_doc)]

mod compositor;
mod ffi;
pub mod formats;
pub mod graph;
mod hooks;
mod pipelines;
mod present;
mod state;
mod telemetry;

use std::ffi::{CStr, c_char};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use ash::vk;

pub use ffi::NegotiateLayerInterface;

/// The layer's name, as in the manifest.
pub const LAYER_NAME: &str = "VK_LAYER_QUIVER_sight";

/// Environment the layer reads (the loader-level switch is `VK_INSTANCE_LAYERS`).
pub mod env {
    /// The session dir (ubi-perf): without it the layer passes everything through.
    pub use ubi_perf::session::ENV_PERF_DIR as PERF_DIR;
    /// `0`: telemetry only, never composite (debugging; the per-game kill switch doesn't load
    /// the layer at all).
    pub const DRAW: &str = "QUIVER_SIGHT_DRAW";
    /// `1`: log events (not per-present work) to stderr.
    pub const LOG: &str = "QUIVER_SIGHT_LOG";
}

struct Config {
    draw: bool,
    log: bool,
}

fn config() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| {
        let flag = |k: &str| std::env::var(k).ok();
        Config { draw: flag(env::DRAW).as_deref() != Some("0"), log: flag(env::LOG).is_some_and(|v| !v.is_empty() && v != "0") }
    })
}

#[doc(hidden)]
pub fn log_enabled() -> bool {
    config().log
}

/// Logs to stderr when `QUIVER_SIGHT_LOG=1`. Only for events (creation, errors), never per
/// present.
#[macro_export]
#[doc(hidden)]
macro_rules! log {
    ($($t:tt)*) => {
        if $crate::log_enabled() {
            eprintln!("quiver-sight-layer: {}", format_args!($($t)*));
        }
    };
}

/// Set on the first panic in the layer: drawing stays off for the rest of the process.
static PANICKED: AtomicBool = AtomicBool::new(false);

pub(crate) fn draw_enabled() -> bool {
    config().draw && !PANICKED.load(Ordering::Relaxed)
}

/// Runs `f`, catching a panic: marks the layer panicked (drawing off) and returns `None`.
pub(crate) fn guard<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(_) => {
            PANICKED.store(true, Ordering::Relaxed);
            if let Some(t) = telemetry::get() {
                t.mark_panicked();
            }
            log!("panic in {what}: drawing disabled for this process");
            None
        }
    }
}

/// The loader's entry point (interface version 2).
///
/// # Safety
/// Called by the Vulkan loader with a valid `VkNegotiateLayerInterface`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(p: *mut NegotiateLayerInterface) -> vk::Result {
    if p.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the loader passes a valid struct.
    let p = unsafe { &mut *p };
    if p.s_type != ffi::LAYER_NEGOTIATE_INTERFACE_STRUCT || p.loader_layer_interface_version < ffi::LAYER_INTERFACE_VERSION {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    p.loader_layer_interface_version = ffi::LAYER_INTERFACE_VERSION;
    p.get_instance_proc_addr = Some(get_instance_proc_addr);
    p.get_device_proc_addr = Some(get_device_proc_addr);
    p.get_physical_device_proc_addr = None;
    vk::Result::SUCCESS
}

type Void = unsafe extern "system" fn();

macro_rules! hook {
    ($f:path) => {
        // SAFETY: every Vulkan function pointer is returned as PFN_vkVoidFunction.
        Some(unsafe { std::mem::transmute::<*const (), Void>($f as *const ()) })
    };
}

/// Instance-level functions the layer intercepts.
fn instance_hook(name: &CStr) -> vk::PFN_vkVoidFunction {
    match name.to_bytes() {
        b"vkGetInstanceProcAddr" => hook!(get_instance_proc_addr),
        b"vkCreateInstance" => hook!(hooks::create_instance),
        b"vkDestroyInstance" => hook!(hooks::destroy_instance),
        b"vkCreateDevice" => hook!(hooks::create_device),
        _ => None,
    }
}

/// Device-level functions the layer intercepts.
fn device_hook(name: &CStr) -> vk::PFN_vkVoidFunction {
    match name.to_bytes() {
        b"vkGetDeviceProcAddr" => hook!(get_device_proc_addr),
        b"vkDestroyDevice" => hook!(hooks::destroy_device),
        b"vkGetDeviceQueue" => hook!(hooks::get_device_queue),
        b"vkGetDeviceQueue2" => hook!(hooks::get_device_queue2),
        b"vkCreateSwapchainKHR" => hook!(hooks::create_swapchain_khr),
        b"vkDestroySwapchainKHR" => hook!(hooks::destroy_swapchain_khr),
        b"vkQueuePresentKHR" => hook!(present::queue_present_khr),
        _ => None,
    }
}

unsafe extern "system" fn get_instance_proc_addr(instance: vk::Instance, name: *const c_char) -> vk::PFN_vkVoidFunction {
    // SAFETY: the caller passes a NUL-terminated name.
    let n = (unsafe { ffi::c_str(name) })?;
    if let Some(f) = instance_hook(n) {
        return Some(f);
    }
    if instance == vk::Instance::null() {
        return None;
    }
    let inst = state::instance(instance)?;
    // SAFETY: forwarding a valid query down the chain.
    let next = unsafe { (inst.next_gipa)(instance, name) };
    // Device functions through the instance: ours, if the chain has them at all.
    if next.is_some()
        && let Some(f) = device_hook(n)
    {
        return Some(f);
    }
    next
}

unsafe extern "system" fn get_device_proc_addr(device: vk::Device, name: *const c_char) -> vk::PFN_vkVoidFunction {
    // SAFETY: the caller passes a NUL-terminated name.
    let n = unsafe { ffi::c_str(name) }?;
    let dev = state::device(device)?;
    // SAFETY: forwarding a valid query down the chain.
    let next = unsafe { (dev.next_gdpa)(device, name) };
    // Only for functions the device has (an extension function of a disabled extension must
    // stay NULL).
    next?;
    device_hook(n).or(next)
}
