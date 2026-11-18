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
//!   frame-time graph drawn from the layer's own present history;
//! - **blurs the game behind the Sight panel** ([`blur`], design/SIGHT.md §14): while
//!   quiver-sight's request in `hud.image`'s header is live, the game's frame is blurred under
//!   the HUD at present time, so the panel's backdrop works under any compositor (`SAMPLED`
//!   usage is added at swapchain creation for it);
//! - **watches Reflex** (`VK_NV_low_latency2`, [`latency`]): the sleep mode and frame cap the
//!   app sets, its latency markers and sleeps, so the status block carries the Reflex mode,
//!   PC latency and the frame-generation multiplier the layer can see;
//! - **reads the HUD's keys** (Shift+F3, Ctrl+Shift+F3) by polling X11's keymap on its own
//!   connection ([`keys`]) and, for Wayland-native windows, from a `wl_keyboard` of its own
//!   on the game's `wl_display` ([`wayland`]), and counts presses in the status block for
//!   quiverd to act on.
//!
//! **Enabling.** The manifest (manifest/) makes it an *implicit* layer that the loader loads
//! only when `QUIVER_SIGHT=1` is set (`QUIVER_SIGHT_DISABLE=1` vetoes it), or when
//! `VK_INSTANCE_LAYERS` names it; anything else on the machine never loads it. Without
//! `QUIVER_PERF_DIR` it passes every call through. Installed as
//! `<datadir>/vulkan/implicit_layer.d/VK_LAYER_QUIVER_sight.<arch>.json`.
//!
//! **Order** (SIGHT-HUD §2.4): the HUD must be composited *below* frame-generation and
//! post-processing layers (closer to the driver). Measured with loader 1.4.363: implicit
//! layers enabled by their environment variables are ordered by manifest discovery, which can
//! put Sight above them; layers named in `VK_INSTANCE_LAYERS` move after the remaining
//! implicit layers, in the listed order (application first), and a named implicit layer is
//! enabled even without its `enable_environment` variable. So a launch should always set
//! `VK_INSTANCE_LAYERS`, listing only layers that are on, with Sight after vkBasalt/lsfg-vk
//! (and before or after obs-vkcapture for captures shown/hidden);
//! `VK_INSTANCE_LAYERS=VK_LAYER_QUIVER_sight` alone already puts Sight below every implicit
//! layer.
//!
//! Robustness rules, since this runs inside the game: every entry point catches panics (the
//! first one disables drawing for the process; telemetry and pass-through stay); every
//! failure disables drawing and is reported in the status block's `draw_error`; the present
//! path doesn't allocate, block, sleep or make syscalls beyond the Vulkan calls it needs, and
//! its work is bounded; everything read from shared memory is validated (quiver-perf).

#![allow(clippy::missing_safety_doc)]

pub mod blur;
mod compositor;
mod ffi;
pub mod formats;
pub mod graph;
mod hooks;
pub mod keys;
pub mod latency;
mod pipelines;
mod present;
mod state;
mod telemetry;
pub mod wayland;

use std::ffi::{CStr, c_char};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use ash::vk;

pub use ffi::NegotiateLayerInterface;

/// The layer's name, as in the manifest.
pub const LAYER_NAME: &str = "VK_LAYER_QUIVER_sight";

/// The installed manifest's template (`manifest/VK_LAYER_QUIVER_sight.json.in`).
pub const MANIFEST_TEMPLATE: &str = include_str!("../manifest/VK_LAYER_QUIVER_sight.json.in");

/// The layer manifest for a library at `library_path` (absolute), 64- or 32-bit. The loader
/// skips a manifest whose `library_arch` doesn't match the process, so both can be installed
/// side by side under the same layer name.
pub fn manifest_json(library_path: &str, is_64bit: bool) -> String {
    let escaped: String = library_path.chars().flat_map(|c| if c == '"' || c == '\\' { vec!['\\', c] } else { vec![c] }).collect();
    MANIFEST_TEMPLATE.replace("@LIBRARY_PATH@", &escaped).replace("@LIBRARY_ARCH@", if is_64bit { "64" } else { "32" })
}

/// Environment the layer and its manifest use.
pub mod env {
    /// `1`: the loader enables the layer (the manifest's `enable_environment`).
    pub const ENABLE: &str = "QUIVER_SIGHT";
    /// `1`: the loader never enables it (`disable_environment`).
    pub const DISABLE: &str = "QUIVER_SIGHT_DISABLE";
    /// The session dir (quiver-perf): without it the layer passes everything through.
    pub use quiver_perf::session::ENV_PERF_DIR as PERF_DIR;
    /// `0`: telemetry only, never composite (debugging; the per-game kill switch doesn't load
    /// the layer at all).
    pub const DRAW: &str = "QUIVER_SIGHT_DRAW";
    /// `0`: never blur the game behind the Sight panel ([`crate::blur`]), and don't add
    /// `SAMPLED` usage to swapchains for it.
    pub const BLUR: &str = "QUIVER_SIGHT_BLUR";
    /// `1`: log events (not per-present work) to stderr.
    pub const LOG: &str = "QUIVER_SIGHT_LOG";
    /// `0`: don't read the HUD's keys in the game ([`crate::keys`]).
    pub const KEYS: &str = "QUIVER_SIGHT_KEYS";
    /// `1`: read Reflex timing reports with `vkGetLatencyTimingsNV` ([`crate::latency`]).
    pub use crate::latency::ENV_TIMINGS as REFLEX_TIMINGS;
}

struct Config {
    draw: bool,
    blur: bool,
    log: bool,
}

fn config() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| {
        let flag = |k: &str| std::env::var(k).ok();
        Config {
            draw: flag(env::DRAW).as_deref() != Some("0"),
            blur: flag(env::BLUR).as_deref() != Some("0"),
            log: flag(env::LOG).is_some_and(|v| !v.is_empty() && v != "0"),
        }
    })
}

/// The backdrop blur may be set up on new swapchains.
pub(crate) fn blur_enabled() -> bool {
    config().blur && draw_enabled()
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
            // Not eprintln!: a closed stderr would panic, inside the game.
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "quiver-sight-layer: {}", format_args!($($t)*));
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

/// Instance-level extension functions the layer intercepts: only returned when the next
/// layer has them (an unsupported extension's function must stay NULL).
fn instance_ext_hook(name: &CStr) -> vk::PFN_vkVoidFunction {
    match name.to_bytes() {
        b"vkCreateXlibSurfaceKHR" => hook!(hooks::create_xlib_surface_khr),
        b"vkCreateXcbSurfaceKHR" => hook!(hooks::create_xcb_surface_khr),
        b"vkCreateWaylandSurfaceKHR" => hook!(hooks::create_wayland_surface_khr),
        b"vkDestroySurfaceKHR" => hook!(hooks::destroy_surface_khr),
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
        // VK_NV_low_latency2 (latency.rs): only returned when the next layer has them.
        b"vkSetLatencySleepModeNV" => hook!(latency::set_latency_sleep_mode_nv),
        b"vkLatencySleepNV" => hook!(latency::latency_sleep_nv),
        b"vkSetLatencyMarkerNV" => hook!(latency::set_latency_marker_nv),
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
    // Extension and device functions through the instance: ours, if the chain has them.
    if next.is_some()
        && let Some(f) = instance_ext_hook(n).or_else(|| device_hook(n))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest() {
        let m: serde_json::Value = serde_json::from_str(&manifest_json("/opt/q \"x\"\\lib.so", false)).unwrap();
        let l = &m["layer"];
        assert_eq!(l["name"], LAYER_NAME);
        assert_eq!(l["library_path"], "/opt/q \"x\"\\lib.so");
        assert_eq!(l["library_arch"], "32");
        assert_eq!(l["enable_environment"][env::ENABLE], "1");
        assert_eq!(l["disable_environment"][env::DISABLE], "1");
        let m: serde_json::Value = serde_json::from_str(&manifest_json("/usr/lib/quiver/sight/x86_64/libquiver_sight_layer.so", true)).unwrap();
        assert_eq!(m["layer"]["library_arch"], "64");
        assert_eq!(m["file_format_version"], "1.2.1");
    }
}
