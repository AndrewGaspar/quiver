//! HDR (design/LAB.md §13): what the game tells the layer about HDR, and captures of the
//! swapchain image in its own format, for judging HDR numerically rather than from a
//! tonemapped screenshot.
//!
//! - **Metadata**: `vkSetHdrMetadataEXT` (VK_EXT_hdr_metadata) is intercepted and passed on
//!   unchanged; the last metadata the app set on the target swapchain goes into the status
//!   block (`hdr_*`, in 1/10000 nit and 1/100000 xy).
//! - **Captures**: writing a number `n` into the session dir's `capture.req` asks for one. The
//!   heartbeat thread sees it ([`tick`]); the next present on the target swapchain copies the
//!   presented image (the game's pixels, before the HUD or the panel) into a host buffer in
//!   the same submit; a later present takes the bytes once that submit is done (one copy of
//!   the image in the present path, only for a requested capture); the heartbeat thread
//!   writes `capture-<n>.raw` (tightly packed rows, the swapchain's own format) and
//!   `capture-<n>.json` (format, colour space, size, metadata), then `capture_done = n` in the
//!   status block. `TRANSFER_SRC` usage is added at swapchain creation when the surface allows
//!   it (`HDR_CAPTURE_READY`).
//! - **Holding** (design/LAB.md §8.2): while `capture.hold` exists in the session dir (`quiver
//!   lab clock` writes it when it pauses or steps the game's virtual clock), every present on
//!   the target swapchain copies its image and waits for the copy, keeping the latest
//!   ([`RETAINED`], `HDR_CAPTURE_HELD`). A held game presents nothing, so requests are served
//!   from that copy once no newer frame has come for 100 ms (the frame in flight when the hold
//!   began is in by then).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ash::vk;
use quiver_perf::LayerStatus;
use quiver_perf::layout::{HDR_CAPTURE_HELD, HDR_CAPTURE_READY, HDR_COLOR_SPACE, HDR_METADATA_SET};

use crate::{log, state};

/// The latest capture request seen in `capture.req`.
pub static REQUEST: AtomicU64 = AtomicU64::new(0);
/// The latest one a present recorded.
pub static TAKEN: AtomicU64 = AtomicU64::new(0);
/// The latest one written out.
pub static DONE: AtomicU64 = AtomicU64::new(0);
/// `HDR_CAPTURE_READY` | `HDR_COLOR_SPACE` for the target swapchain (from `retarget`).
pub static TARGET_FLAGS: AtomicU32 = AtomicU32::new(0);

pub const REQUEST_FILE: &str = "capture.req";
/// Present while the game's clock is held: keep every presented frame (module docs).
pub const HOLD_FILE: &str = "capture.hold";

/// Holding: `HOLD_FILE` exists.
pub static HOLD: AtomicBool = AtomicBool::new(false);
/// The latest frame kept while holding, and when it came.
pub static RETAINED: Mutex<Option<(Captured, Instant)>> = Mutex::new(None);
/// When the pending request was first seen (a held request waits for the frames to settle,
/// but not for ever).
static REQUEST_SEEN: Mutex<Option<Instant>> = Mutex::new(None);

/// The last metadata set on a swapchain.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metadata {
    pub swapchain: u64,
    pub calls: u32,
    pub max_cll: f32,
    pub max_fall: f32,
    pub max_luminance: f32,
    pub min_luminance: f32,
    /// r, g, b, white xy.
    pub primaries: [f32; 8],
}

pub static METADATA: Mutex<Option<Metadata>> = Mutex::new(None);

/// A capture taken off the GPU, for the heartbeat thread to write.
pub struct Captured {
    pub id: u64,
    pub format: vk::Format,
    pub color_space: vk::ColorSpaceKHR,
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    pub swapchain: u32,
    pub data: Vec<u8>,
}

pub static CAPTURED: Mutex<Option<Captured>> = Mutex::new(None);

/// Bytes per pixel of the formats a capture can take (tightly packed), else `None`.
pub fn bytes_per_pixel(f: vk::Format) -> Option<u32> {
    use vk::Format as F;
    Some(match f {
        F::B8G8R8A8_UNORM | F::B8G8R8A8_SRGB | F::R8G8B8A8_UNORM | F::R8G8B8A8_SRGB | F::A8B8G8R8_UNORM_PACK32 | F::A8B8G8R8_SRGB_PACK32 => 4,
        F::A2B10G10R10_UNORM_PACK32 | F::A2R10G10B10_UNORM_PACK32 => 4,
        F::R16G16B16A16_SFLOAT | F::R16G16B16A16_UNORM => 8,
        _ => return None,
    })
}

pub fn is_hdr_color_space(cs: vk::ColorSpaceKHR) -> bool {
    use vk::ColorSpaceKHR as C;
    matches!(cs, C::HDR10_ST2084_EXT | C::HDR10_HLG_EXT | C::EXTENDED_SRGB_LINEAR_EXT | C::EXTENDED_SRGB_NONLINEAR_EXT | C::BT2020_LINEAR_EXT | C::DOLBYVISION_EXT | C::DISPLAY_P3_LINEAR_EXT)
}

/// `vkSetHdrMetadataEXT`: noted for the status block, then passed on.
///
/// # Safety
/// The Vulkan contract for `vkSetHdrMetadataEXT`.
pub unsafe extern "system" fn set_hdr_metadata_ext(device: vk::Device, count: u32, swapchains: *const vk::SwapchainKHR, metadata: *const vk::HdrMetadataEXT<'_>) {
    let Some(d) = state::device(device) else { return };
    // SAFETY: the next layer's function of this name (it exists: device_hook only answers for
    // functions the chain has).
    let next = unsafe { (d.next_gdpa)(device, c"vkSetHdrMetadataEXT".as_ptr()) };
    crate::guard("vkSetHdrMetadataEXT", || {
        if count == 0 || swapchains.is_null() || metadata.is_null() {
            return;
        }
        // SAFETY: the app's arrays of `count`.
        let (scs, mds) = unsafe { (std::slice::from_raw_parts(swapchains, count as usize), std::slice::from_raw_parts(metadata, count as usize)) };
        let mut g = METADATA.lock().unwrap_or_else(|p| p.into_inner());
        for (sc, m) in scs.iter().zip(mds) {
            let calls = g.as_ref().map_or(0, |x| x.calls) + 1;
            let xy = |p: vk::XYColorEXT| [p.x, p.y];
            let p = [xy(m.display_primary_red), xy(m.display_primary_green), xy(m.display_primary_blue), xy(m.white_point)].concat();
            let mut primaries = [0f32; 8];
            primaries.copy_from_slice(&p);
            *g = Some(Metadata {
                swapchain: vk::Handle::as_raw(*sc),
                calls,
                max_cll: m.max_content_light_level,
                max_fall: m.max_frame_average_light_level,
                max_luminance: m.max_luminance,
                min_luminance: m.min_luminance,
                primaries,
            });
            log!("HDR metadata: max CLL {} FALL {} mastering {}..{} nits", m.max_content_light_level, m.max_frame_average_light_level, m.min_luminance, m.max_luminance);
        }
    });
    if let Some(f) = next {
        type Pfn = unsafe extern "system" fn(vk::Device, u32, *const vk::SwapchainKHR, *const vk::HdrMetadataEXT<'_>);
        // SAFETY: the next layer's vkSetHdrMetadataEXT, with the app's arguments.
        unsafe { std::mem::transmute::<unsafe extern "system" fn(), Pfn>(f)(device, count, swapchains, metadata) };
    }
}

/// Fills the status block's HDR fields.
pub fn fill_status(s: &mut LayerStatus) {
    let to = |v: f32, scale: f32| if v.is_finite() && v > 0.0 { (v * scale).min(u32::MAX as f32) as u32 } else { 0 };
    let mut flags = TARGET_FLAGS.load(Ordering::Relaxed);
    if let Some(m) = *METADATA.lock().unwrap_or_else(|p| p.into_inner()) {
        flags |= HDR_METADATA_SET;
        s.hdr_metadata_calls = m.calls;
        s.hdr_max_cll = to(m.max_cll, 10_000.0);
        s.hdr_max_fall = to(m.max_fall, 10_000.0);
        s.hdr_max_luminance = to(m.max_luminance, 10_000.0);
        s.hdr_min_luminance = to(m.min_luminance, 10_000.0);
        for (o, v) in s.hdr_primaries.iter_mut().zip(m.primaries) {
            *o = to(v, 100_000.0);
        }
    }
    if HOLD.load(Ordering::Relaxed) && RETAINED.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
        flags |= HDR_CAPTURE_HELD;
    }
    s.hdr_flags = flags;
    s.capture_done = DONE.load(Ordering::Relaxed);
}

fn write_capture(dir: &Path, c: &Captured, id: u64) {
    let md = *METADATA.lock().unwrap_or_else(|p| p.into_inner());
    let meta = sidecar(c, id, md.as_ref());
    let raw = dir.join(format!("capture-{id}.raw"));
    let json = dir.join(format!("capture-{id}.json"));
    match std::fs::write(&raw, &c.data).and_then(|_| std::fs::write(&json, meta.as_bytes())) {
        Ok(()) => {
            log!("capture {id} written: {}x{} {:?} {:?}", c.width, c.height, c.format, c.color_space);
            DONE.store(id, Ordering::Relaxed);
        }
        Err(e) => log!("capture {id}: {e}"),
    }
}

/// The target swapchain's capture readiness and colour space (from `retarget`).
pub fn set_target(capture: bool, cs: vk::ColorSpaceKHR) {
    let f = if capture { HDR_CAPTURE_READY } else { 0 } | if is_hdr_color_space(cs) { HDR_COLOR_SPACE } else { 0 };
    TARGET_FLAGS.store(f, Ordering::Relaxed);
}

/// The heartbeat thread's part: pick up a request, write a finished capture out.
pub fn tick(dir: &Path) {
    let hold = dir.join(HOLD_FILE).exists();
    if HOLD.swap(hold, Ordering::Relaxed) != hold {
        log!("capture hold {}", if hold { "on: keeping every presented frame (the game's clock is held or stepped)" } else { "off" });
        if !hold {
            *RETAINED.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join(REQUEST_FILE))
        && let Ok(n) = text.trim().parse::<u64>()
        && n > REQUEST.load(Ordering::Relaxed)
    {
        log!("capture {n} requested");
        REQUEST.store(n, Ordering::Relaxed);
        *REQUEST_SEEN.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    }
    if hold {
        // Served from the kept frame once it has settled (or after 2 s, whatever is newest).
        let n = REQUEST.load(Ordering::Relaxed);
        if n > DONE.load(Ordering::Relaxed) {
            let seen = REQUEST_SEEN.lock().unwrap_or_else(|p| p.into_inner()).unwrap_or_else(Instant::now);
            let kept = RETAINED.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((c, at)) = kept.as_ref()
                && (at.elapsed() >= Duration::from_millis(100) || seen.elapsed() >= Duration::from_secs(2))
            {
                log!("capture {n} from the kept frame (held)");
                TAKEN.fetch_max(n, Ordering::Relaxed);
                write_capture(dir, c, n);
            }
        }
    }
    let Some(c) = CAPTURED.lock().unwrap_or_else(|p| p.into_inner()).take() else { return };
    write_capture(dir, &c, c.id);
}

/// The capture's JSON sidecar (by hand: the layer has no JSON dependency; every value is a
/// number or a Vulkan enum's Rust name).
fn sidecar(c: &Captured, id: u64, md: Option<&Metadata>) -> String {
    let num = |v: f32| if v.is_finite() { format!("{v}") } else { "null".into() };
    let md = md.map_or("null".to_owned(), |m| {
        let p: Vec<String> = m.primaries.iter().map(|&v| num(v)).collect();
        format!(
            "{{\"max_cll\": {}, \"max_fall\": {}, \"max_luminance\": {}, \"min_luminance\": {}, \"primaries\": [{}], \"calls\": {}}}",
            num(m.max_cll),
            num(m.max_fall),
            num(m.max_luminance),
            num(m.min_luminance),
            p.join(", "),
            m.calls
        )
    });
    format!(
        "{{\n  \"id\": {},\n  \"format\": {},\n  \"format_name\": \"{:?}\",\n  \"color_space\": {},\n  \"color_space_name\": \"{:?}\",\n  \"width\": {},\n  \"height\": {},\n  \"bytes_per_pixel\": {},\n  \"row_bytes\": {},\n  \"swapchain\": {},\n  \"hdr_metadata\": {}\n}}\n",
        id,
        c.format.as_raw(),
        c.format,
        c.color_space.as_raw(),
        c.color_space,
        c.width,
        c.height,
        c.bytes_per_pixel,
        c.width * c.bytes_per_pixel,
        c.swapchain,
        md
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_spaces() {
        assert_eq!(bytes_per_pixel(vk::Format::A2B10G10R10_UNORM_PACK32), Some(4));
        assert_eq!(bytes_per_pixel(vk::Format::R16G16B16A16_SFLOAT), Some(8));
        assert_eq!(bytes_per_pixel(vk::Format::R5G6B5_UNORM_PACK16), None);
        assert!(is_hdr_color_space(vk::ColorSpaceKHR::HDR10_ST2084_EXT));
        assert!(is_hdr_color_space(vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT));
        assert!(!is_hdr_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR));
    }

    #[test]
    fn sidecar_is_json() {
        let c = Captured { id: 3, format: vk::Format::A2B10G10R10_UNORM_PACK32, color_space: vk::ColorSpaceKHR::HDR10_ST2084_EXT, width: 4, height: 2, bytes_per_pixel: 4, swapchain: 1, data: vec![] };
        let m = Metadata { max_cll: 4000.0, max_fall: f32::NAN, ..Default::default() };
        let v: serde_json::Value = serde_json::from_str(&sidecar(&c, 3, Some(&m))).unwrap();
        assert_eq!(v["format"], 64);
        assert_eq!(v["color_space_name"], "HDR10_ST2084_EXT");
        assert_eq!(v["row_bytes"], 16);
        assert_eq!(v["hdr_metadata"]["max_cll"], 4000.0);
        assert!(v["hdr_metadata"]["max_fall"].is_null());
        let v: serde_json::Value = serde_json::from_str(&sidecar(&c, 3, None)).unwrap();
        assert!(v["hdr_metadata"].is_null());
    }

    #[test]
    fn metadata_into_the_status_block() {
        *METADATA.lock().unwrap() = Some(Metadata { swapchain: 1, calls: 2, max_cll: 4000.0, max_fall: 600.0, max_luminance: 1000.0, min_luminance: 0.005, primaries: [0.708, 0.292, 0.17, 0.797, 0.131, 0.046, 0.3127, 0.329] });
        let mut s = LayerStatus::ZERO;
        fill_status(&mut s);
        assert!(s.hdr_flags & HDR_METADATA_SET != 0);
        assert_eq!((s.hdr_max_cll, s.hdr_max_fall, s.hdr_max_luminance, s.hdr_min_luminance), (40_000_000, 6_000_000, 10_000_000, 50));
        assert_eq!(s.hdr_primaries[0], 70800);
        assert_eq!(s.hdr_metadata_calls, 2);
        *METADATA.lock().unwrap() = None;
    }
}
