//! The Sight panel drawn in the game (design/SIGHT.md §14, "In the game"): the layer's side of
//! `panel.input` and `panel.image` (quiver-perf `panel`).
//!
//! - The heartbeat thread ([`tick`]) maps `panel.input` when it appears and writes the layer's
//!   word there: whether the target swapchain can show the panel ([`PANEL_LAYER_CAN_DRAW`]:
//!   the HUD's formats, drawing on) and its extent, which quiver-sight sizes the panel's image
//!   by. While the panel is open it maps `panel.image` (re-mapping it when quiver-sight
//!   replaces it for a new extent) and drops it once the panel closes.
//! - The present path ([`open_now`], one atomic load while the panel is closed) composites the
//!   image over the game's frame (after the HUD) with the panel's cursor on top
//!   ([`cursor_vertices`]), and then says so ([`mark_drawn`]): the input preload releases input
//!   to the game when the panel stops showing up in frames.

use std::path::Path;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use quiver_perf::hud::HudReader;
use quiver_perf::layout::HUD_HEADER_BYTES;
use quiver_perf::panel::{PANEL_IMAGE_FILE, PANEL_INPUT_FILE, PANEL_LAYER_CAN_DRAW, PanelInput, open_panel_image_reader, open_panel_input};
use quiver_perf::shm::Mapping;

use crate::graph::Vertex;
use crate::log;

static INPUT: AtomicPtr<PanelInput<Mapping>> = AtomicPtr::new(std::ptr::null_mut());
/// `PANEL_LAYER_*` for the target swapchain, and its extent (w | h << 32), from `retarget`.
pub static CAN: AtomicU32 = AtomicU32::new(0);
pub static EXTENT: AtomicU64 = AtomicU64::new(0);

/// The current `panel.image` reader; `generation` bumps when it's (re)opened or dropped.
pub struct Image {
    pub reader: Option<HudReader<Mapping>>,
    pub generation: u64,
}

pub static IMAGE: Mutex<Image> = Mutex::new(Image { reader: None, generation: 0 });

fn input() -> Option<&'static PanelInput<Mapping>> {
    let p = INPUT.load(Ordering::Acquire);
    // SAFETY: set once from a leaked Box, never freed.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// The open panel now: its epoch and, when it shows one, the cursor (swapchain px).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Open {
    pub epoch: u64,
    pub cursor: Option<(i32, i32)>,
}

/// Whether the panel is open (one atomic load when it isn't).
#[inline]
pub fn open_now() -> Option<Open> {
    let p = input()?;
    let c = p.control();
    if !c.open() {
        return None;
    }
    Some(Open { epoch: c.epoch(), cursor: p.cursor().map(|(xy, _)| xy) })
}

/// The panel of `epoch` was composited into a frame.
#[inline]
pub fn mark_drawn(epoch: u64, now: i64) {
    if let Some(p) = input() {
        p.mark_drawn(epoch, now);
    }
}

/// The heartbeat thread's part (every 250 ms).
pub fn tick(dir: &Path, now: i64) {
    if input().is_none() {
        match open_panel_input(&dir.join(PANEL_INPUT_FILE), true) {
            Ok(p) => {
                log!("panel.input mapped");
                INPUT.store(Box::into_raw(Box::new(p)), Ordering::Release);
            }
            Err(_) => return,
        }
    }
    let Some(p) = input() else { return };
    let flags = if crate::draw_enabled() { CAN.load(Ordering::Relaxed) } else { 0 };
    let e = EXTENT.load(Ordering::Relaxed);
    p.set_layer(std::process::id(), flags, (e as u32, (e >> 32) as u32), now);
    refresh_image(&dir.join(PANEL_IMAGE_FILE), p.control().open());
}

/// Maps `panel.image` while the panel is open (again when quiver-sight replaces it), drops it
/// when closed (a 32-bit game shouldn't keep tens of MiB of address space for nothing).
fn refresh_image(path: &Path, open: bool) {
    let mut img = match IMAGE.lock() {
        Ok(h) => h,
        Err(p) => p.into_inner(),
    };
    if !open {
        if img.reader.take().is_some() {
            img.generation += 1;
            log!("panel.image dropped (panel closed)");
        }
        return;
    }
    let keep = img.reader.as_ref().is_some_and(|r| {
        let region = r.region();
        let want = HUD_HEADER_BYTES as u64 + 2 * r.info().buffer_bytes as u64;
        region.is_file_at(path) && region.size_now().is_ok_and(|n| n >= want)
    });
    if keep {
        return;
    }
    let had = img.reader.take().is_some();
    match open_panel_image_reader(path) {
        Ok(r) => {
            log!("panel.image opened ({} bytes per buffer)", r.info().buffer_bytes);
            img.reader = Some(r);
            img.generation += 1;
        }
        Err(_) if had => img.generation += 1,
        Err(_) => {}
    }
}

/// `retarget`'s word on the target swapchain: whether the panel can be drawn on it.
pub fn set_target(can: bool, extent: (u32, u32)) {
    CAN.store(if can { PANEL_LAYER_CAN_DRAW } else { 0 }, Ordering::Relaxed);
    EXTENT.store(extent.0 as u64 | (extent.1 as u64) << 32, Ordering::Relaxed);
}

/// Vertices of the panel's cursor: an arrow (a dark triangle under a light one) with its tip
/// at `tip`, `scale` swapchain px per logical px.
pub const CURSOR_VERTS: usize = 6;

pub fn cursor_vertices(tip: (i32, i32), scale: f32, out: &mut [Vertex; CURSOR_VERTS]) {
    let s = scale.clamp(0.5, 8.0);
    let (x, y) = (tip.0 as f32, tip.1 as f32);
    let p = |dx: f32, dy: f32| [x + dx * s, y + dy * s];
    let dark = u32::from_le_bytes([0, 0, 0, 235]);
    let light = u32::from_le_bytes([250, 250, 250, 255]);
    // Outline, then fill (logical px; the tip is the hotspot).
    let tri = [(p(-1.0, -2.0), dark), (p(-1.0, 19.5), dark), (p(14.0, 13.5), dark), (p(0.5, 1.5), light), (p(0.5, 16.0), light), (p(10.5, 12.3), light)];
    for (o, (pos, color)) in out.iter_mut().zip(tri) {
        *o = Vertex { pos, color };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_shape() {
        let mut v = [Vertex::default(); CURSOR_VERTS];
        cursor_vertices((100, 50), 2.0, &mut v);
        // The fill is inside the outline, the tip at the hotspot.
        assert!(v[3].pos[0] > v[0].pos[0] && v[3].pos[1] > v[0].pos[1]);
        assert!(v[0].pos[0] <= 100.0 && v[0].pos[1] <= 50.0);
        assert!((v[3].pos[0] - 101.0).abs() < 1e-3);
        assert_eq!(v[0].color.to_le_bytes()[3], 235);
        assert_eq!(v[5].color.to_le_bytes()[0], 250);
    }
}
