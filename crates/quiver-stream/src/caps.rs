//! `quiver-stream caps --json`: what this machine can decode and show, as the
//! [`ClientCaps`] a `StartStream` (and the stream's Hello) carries, so the host can start the
//! game at the right size, rate and codec before the stream window opens.
//!
//! The decoders come from the presenter's Vulkan device and libav ([`VideoDecoder::probe`]);
//! the display is SDL's primary display (its size in pixels, refresh, scale and whether HDR is
//! on); the pads are the controllers SDL sees now. No window is opened.

use std::ffi::c_int;
use std::sync::Arc;

use anyhow::{Result, bail};
use quiver_stream_proto::{ClientCaps, DisplayCaps, PadKind};
use sdl3_sys::everything as sdl;
use sdl3_sys::gamepad::*;

use crate::decode::VideoDecoder;
use crate::gpu::Gpu;

/// Probe this machine.
pub fn probe() -> Result<ClientCaps> {
    let (entry, instance, exts) = Gpu::instance(&[])?;
    let gpu = Arc::new(Gpu::new(entry, instance, exts, None)?);
    let decoders = VideoDecoder::probe(&gpu);
    drop(gpu);
    let (display, pads) = sdl_probe()?;
    Ok(ClientCaps { decoders, display, audio_channels: 2, pads, microphone: false })
}

fn sdl_probe() -> Result<(DisplayCaps, Vec<PadKind>)> {
    unsafe {
        if !sdl::SDL_Init(sdl::SDL_INIT_VIDEO | sdl::SDL_INIT_GAMEPAD) {
            bail!("SDL_Init: {}", std::ffi::CStr::from_ptr(sdl::SDL_GetError()).to_string_lossy());
        }
    }
    let display = unsafe { display_caps() };
    let pads = unsafe { pad_kinds() };
    unsafe { sdl::SDL_Quit() };
    Ok((display, pads))
}

unsafe fn display_caps() -> DisplayCaps {
    let id = unsafe { sdl::SDL_GetPrimaryDisplay() };
    let mode = unsafe { sdl::SDL_GetCurrentDisplayMode(id) };
    let (w, h, refresh, density) = if mode.is_null() {
        (1920, 1080, 60.0, 1.0)
    } else {
        let m = unsafe { &*mode };
        (m.w, m.h, m.refresh_rate, m.pixel_density)
    };
    let props = unsafe { sdl::SDL_GetDisplayProperties(id) };
    let hdr = props != 0 && unsafe { sdl::SDL_GetBooleanProperty(props, sdl::SDL_PROP_DISPLAY_HDR_ENABLED_BOOLEAN, false) };
    let scale = unsafe { sdl::SDL_GetDisplayContentScale(id) };
    // A mode's size is in points on a scaled display; the stream wants pixels.
    let px = |v: c_int| ((v as f32 * density.max(1.0)).round() as u32).max(1);
    DisplayCaps {
        width: px(w),
        height: px(h),
        refresh_mhz: ((if refresh > 0.0 { refresh } else { 60.0 }) * 1000.0).round() as u32,
        hdr,
        vrr: false,
        stereo_sbs: false,
        scale: if scale > 0.0 { scale } else { 1.0 },
    }
}

unsafe fn pad_kinds() -> Vec<PadKind> {
    let mut n: c_int = 0;
    let ids = unsafe { SDL_GetGamepads(&mut n) };
    if ids.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..n.max(0) as usize {
        let id = unsafe { *ids.add(i) };
        let kind = unsafe { crate::input::pad_kind(SDL_GetGamepadTypeForID(id), SDL_GetGamepadVendorForID(id), SDL_GetGamepadProductForID(id)) };
        out.push(kind);
    }
    unsafe { sdl::SDL_free(ids as *mut _) };
    out.truncate(4);
    out
}
