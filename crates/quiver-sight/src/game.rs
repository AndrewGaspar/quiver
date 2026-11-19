//! The game presenter's side (SIGHT-HUD §2.9): an off-screen GPUI window per session renders
//! the [`HudView`] at the scale the swapchain implies, and the frame goes into `hud.image`
//! as premultiplied RGBA with its descriptor (position from the anchor, opacity, the graph
//! rectangles the layer fills). Rendering happens only when the model changes; between
//! models only the heartbeat moves.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use gpui::{App, AppContext, Hsla, OffscreenWindowOptions, Rgba, WindowHandle, px, size};
use quiver_api::perf::{HudModel, Presenter};
use quiver_perf::hud::{HudWriter, rgba_premul};
use quiver_perf::layout::{GRAPH_SERIES_PRESENTED, HUD_FORMAT_RGBA8_PREMUL_SRGB, HUD_MAX_DIM, HUD_MAX_GRAPHS, HUD_PIXELS_OFFSET};
use quiver_perf::shm::Mapping;
use quiver_perf::{HudBufferHeader, HudGraph};
use quiver_ui::theme::Theme;

use crate::hud_view::{BASE_TEXT, HudView, Measure, Mode, card_extent, model_size};
use crate::place::place;

/// The window for a model's card (logical px): its fixed size, plus a pixel for the
/// border's anti-aliasing.
pub fn window_size(m: &HudModel) -> (f32, f32) {
    let (w, h) = model_size(m);
    (w.ceil() + 1.0, h.ceil() + 1.0)
}

#[derive(Default)]
struct Frame {
    width: u32,
    height: u32,
    stride: u32,
    data: Vec<u8>,
    render: Duration,
}

struct Offscreen {
    handle: WindowHandle<HudView>,
    factor: f32,
    frame: Rc<RefCell<Frame>>,
    measure: Rc<RefCell<Measure>>,
    size: (f32, f32),
}

/// Timings of the last images, for the log.
#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub published: u64,
    /// update + layout + paint + GPU + readback, per published image (all passes); the
    /// last [`Stats::KEEP`].
    pub render: std::collections::VecDeque<Duration>,
    /// Copying into `hud.image`.
    pub publish: std::collections::VecDeque<Duration>,
    pub resizes: u64,
    pub windows: u64,
}

impl Stats {
    const KEEP: usize = 240;

    fn note(&mut self, render: Duration, publish: Duration) {
        for (q, v) in [(&mut self.render, render), (&mut self.publish, publish)] {
            if q.len() == Self::KEEP {
                q.pop_front();
            }
            q.push_back(v);
        }
        self.published += 1;
    }

    /// p50 / p99 / max of a series.
    pub fn summary(q: &std::collections::VecDeque<Duration>) -> String {
        let mut v: Vec<Duration> = q.iter().copied().collect();
        if v.is_empty() {
            return "-".into();
        }
        v.sort();
        let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        format!("p50 {:.2?} p99 {:.2?} max {:.2?}", at(0.5), at(0.99), v[v.len() - 1])
    }

    pub fn line(&self) -> String {
        format!(
            "{} images (last {}): render {}; publish {}; {} resizes, {} windows",
            self.published,
            self.render.len(),
            Self::summary(&self.render),
            Self::summary(&self.publish),
            self.resizes,
            self.windows
        )
    }
}

/// VmRSS and VmHWM of this process, MiB.
pub fn rss_mib() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let f = |k: &str| s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) / 1024;
    (f("VmRSS:"), f("VmHWM:"))
}

#[derive(Default)]
pub struct GameHud {
    window: Option<Offscreen>,
    writer: Option<(PathBuf, HudWriter<Mapping>)>,
    /// The last image published (descriptor and pixels), to hide it again without rendering.
    last: Option<(HudBufferHeader, Vec<u8>)>,
    warned_crop: bool,
    /// Since when the desktop presents this HUD (the off-screen window closes after a while).
    desktop_since: Option<Instant>,
    pub stats: Stats,
}

/// The desktop presenter has drawn the HUD this long: free the off-screen window's GPU
/// device. Not at once, so a presenter that flips back soon doesn't pay for a new device.
const IDLE_WINDOW: Duration = Duration::from_secs(15);

fn premul(c: Hsla) -> u32 {
    let c = Rgba::from(c);
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    rgba_premul(b(c.r), b(c.g), b(c.b), b(c.a))
}

/// How the model's frames pack their eyes, if that layout fits `extent` (else mono).
pub fn stereo(m: &HudModel, extent: [u32; 2]) -> quiver_perf::stereo::Stereo {
    let s = m.stereo.as_ref().map(|s| s.stereo()).unwrap_or_default();
    if s.fits((extent[0], extent[1])) { s } else { quiver_perf::stereo::Stereo::Mono }
}

/// Device px per logical px for a model: its text size (1.6% of the swapchain height) times
/// its scale, over the view's base text size.
pub fn factor(m: &HudModel) -> f32 {
    let f = m.text_px * m.scale / BASE_TEXT;
    if f.is_finite() { f.clamp(0.5, 8.0) } else { 1.0 }
}

/// [`factor`], reduced so the card's image stays within what the layer takes (2048 px a
/// side) and what `hud.image`'s buffer holds (`buffer_bytes`; 0: unknown): a wide card at a
/// high resolution or scale comes out a little smaller rather than cropped or refused.
pub fn fit_factor(m: &HudModel, buffer_bytes: u32) -> f32 {
    let f = factor(m);
    let (w, h) = model_size(m);
    // The image is the card rounded up plus a pixel.
    let side = (HUD_MAX_DIM - 2) as f32;
    let mut fit = f.min(side / w).min(side / h);
    let px = buffer_bytes.saturating_sub(HUD_PIXELS_OFFSET as u32) as f32 / 4.0;
    if px > 0.0 {
        // Some slack for the row stride's alignment and the rounding.
        fit = fit.min((px / ((w + 2.0) * (h + 2.0))).sqrt() * 0.97);
    }
    fit.max(0.25)
}

impl GameHud {
    pub fn heartbeat(&self, now_ns: i64) {
        if let Some((_, w)) = &self.writer {
            w.heartbeat(now_ns);
        }
    }

    fn writer(&mut self, path: &Path) -> Result<&mut HudWriter<Mapping>> {
        if self.writer.as_ref().is_none_or(|(p, _)| p != path) {
            let w = quiver_perf::session::open_hud_writer(path).with_context(|| format!("opening {}", path.display()))?;
            w.set_writer_pid(std::process::id());
            w.heartbeat(quiver_perf::clock::mono_raw_ns());
            tracing::info!(image = %path.display(), published = w.published(), "attached to hud.image");
            self.writer = Some((path.to_owned(), w));
            self.last = None;
        }
        Ok(&mut self.writer.as_mut().unwrap().1)
    }

    /// Follows a model: render and publish (shown, game presenter), or hide.
    pub fn update(&mut self, m: &HudModel, cx: &mut App) -> Result<()> {
        if let Some(p) = &m.image {
            self.writer(p)?;
        }
        if m.presenter != Presenter::Game {
            // The desktop draws it: make sure the layer doesn't too (the window goes later).
            self.hide(m);
            self.desktop_since.get_or_insert_with(Instant::now);
            return Ok(());
        }
        self.desktop_since = None;
        if !m.on {
            self.hide(m);
            return Ok(());
        }
        let (Some(extent), Some(_)) = (m.extent, self.writer.as_ref()) else {
            // The layer hasn't reported a swapchain yet (or no image): nothing to lay out for.
            return Ok(());
        };
        let started = Instant::now();
        let cap = self.writer.as_ref().map(|(_, w)| w.info().buffer_bytes).unwrap_or(0);
        let f = fit_factor(m, cap);
        if f < factor(m) - 0.001 && self.window.as_ref().is_none_or(|w| (w.factor - f).abs() > 0.001) {
            tracing::info!(asked = factor(m), fits = f, size = ?model_size(m), "HUD card too large for hud.image at this scale: drawn smaller");
        }
        if self.window.as_ref().is_none_or(|w| (w.factor - f).abs() > 0.001) {
            self.open_window(m, f, cx)?;
        }
        let w = self.window.as_mut().unwrap();
        let model = m.clone();
        w.handle.update(cx, |v, _, cx| {
            v.model = model;
            cx.notify();
        })?;
        // The card's size is the layout's, not its contents': the window changes only with
        // the layout (or the scale, which opens a new window).
        let want = window_size(m);
        let resize = (want.0 - w.size.0).abs() > 0.5 || (want.1 - w.size.1).abs() > 0.5;
        if resize {
            w.size = want;
            self.stats.resizes += 1;
        }
        cx.update_window(w.handle.into(), |_, win, cx| {
            if resize {
                win.resize(size(px(want.0), px(want.1)));
                // The platform's resize callback can't reach a window that is being updated
                // (this one): take the new size now.
                win.bounds_changed(cx);
            }
            win.render_now(cx);
        })?;
        tracing::debug!(card = ?w.measure.borrow().card, window = ?w.size, "rendered");
        let render = started.elapsed();
        let th = cx.global::<Theme>().clone();
        let (desc, pixels) = self.image(m, extent, &th);
        let t = Instant::now();
        let n = {
            let (_, writer) = self.writer.as_mut().unwrap();
            writer.publish(&desc, &pixels)?
        };
        self.stats.note(render, t.elapsed());
        if self.stats.published == 1 || self.stats.published.is_multiple_of(120) {
            let (rss, hwm) = rss_mib();
            tracing::info!(rss_mib = rss, peak_mib = hwm, "HUD {}", self.stats.line());
        }
        tracing::debug!(rev = m.rev, published = n, w = desc.width, h = desc.height, x = desc.x, y = desc.y, ?render, gpu = ?self.window.as_ref().map(|w| w.frame.borrow().render), "published");
        self.last = Some((desc, pixels));
        Ok(())
    }

    /// The descriptor and pixels for the frame just rendered.
    fn image(&mut self, m: &HudModel, extent: [u32; 2], th: &Theme) -> (HudBufferHeader, Vec<u8>) {
        let w = self.window.as_ref().unwrap();
        let fr = w.frame.borrow();
        let measure = w.measure.borrow();
        let f = w.factor;
        // The card's fixed size, rounded up, plus a pixel for the border's anti-aliasing (the
        // layout's size, never the contents': the measured card only checks it).
        let (mw, mh) = model_size(m);
        let card = ((mw * f).ceil() as u32 + 1, (mh * f).ceil() as u32 + 1);
        if let Some((w, h)) = card_extent(&measure)
            && ((w - mw).abs() > 0.5 || (h - mh).abs() > 0.5)
        {
            tracing::debug!(measured = ?(w, h), size = ?(mw, mh), "the card laid out at another size than the model's");
        }
        let (cw, mut ch) = (card.0.clamp(1, fr.width.max(1)), card.1.clamp(1, fr.height.max(1)));
        // Within the buffer (4 MiB at most): crop rows rather than fail.
        let cap = self.writer.as_ref().map(|(_, w)| w.info().buffer_bytes).unwrap_or(0) as usize;
        let max_rows = (cap.saturating_sub(HUD_PIXELS_OFFSET) / fr.stride.max(1) as usize) as u32;
        if ch > max_rows {
            if !self.warned_crop {
                tracing::warn!(height = ch, max_rows, "HUD image larger than hud.image's buffer: cropped");
                self.warned_crop = true;
            }
            ch = max_rows.max(1);
        }
        // A stereo frame: placed in one eye's logical space; the layer draws it in each eye
        // (design/SIGHT.md §15).
        let st = stereo(m, extent);
        let pl = place(m.anchor, [m.offset[0] * f, m.offset[1] * f], m.safe_area, st.logical_extent((extent[0], extent[1])), (cw, ch));
        let mut d = HudBufferHeader::ZERO;
        d.stereo = st.code();
        d.width = pl.w;
        d.height = pl.h;
        d.stride = fr.stride;
        d.format = HUD_FORMAT_RGBA8_PREMUL_SRGB;
        d.scale = f;
        (d.extent_w, d.extent_h) = (extent[0], extent[1]);
        (d.x, d.y) = (pl.x, pl.y);
        d.opacity = 1.0;
        d.model_rev = m.rev;
        let mut n = 0;
        for g in measure.graphs.iter().take(HUD_MAX_GRAPHS) {
            let gx = (f32::from(g.bounds.origin.x) * f).round().max(0.0) as i32;
            let gy = (f32::from(g.bounds.origin.y) * f).round().max(0.0) as i32;
            let gw = ((f32::from(g.bounds.size.width) * f).round() as i32).min(pl.w as i32 - gx);
            let gh = ((f32::from(g.bounds.size.height) * f).round() as i32).min(pl.h as i32 - gy);
            if gw <= 0 || gh <= 0 {
                continue;
            }
            let max_ms = if g.max_ms > g.min_ms.max(0.0) { g.max_ms } else { 0.0 };
            d.graph[n] = HudGraph {
                x: gx,
                y: gy,
                w: gw,
                h: gh,
                span_ms: g.span_ms.clamp(1000, 10_000),
                min_ms: g.min_ms.max(0.0),
                max_ms,
                line: premul(th.acc),
                fill: premul(th.acc.opacity(0.16)),
                muted: premul(th.tx4),
                warn_above_ms: g.warn_above_ms.max(0.0).round() as u32,
                bad_above_ms: g.bad_above_ms.max(0.0).round() as u32,
                warn: premul(th.warn),
                bad: premul(th.bad),
                series: GRAPH_SERIES_PRESENTED,
            };
            n += 1;
        }
        d.ngraphs = n as u32;
        let len = fr.stride as usize * pl.h as usize;
        let mut px = fr.data[..len.min(fr.data.len())].to_vec();
        px.resize(len, 0);
        (d, px)
    }

    /// Hides the HUD in the game: the last image again at opacity 0, which the layer treats as
    /// "toggled off" (no re-render, no 3 s staleness gap).
    pub fn hide(&mut self, m: &HudModel) {
        let Some((_, w)) = self.writer.as_mut() else { return };
        let (desc, pixels) = match &self.last {
            Some((d, _)) if d.opacity == 0.0 => return,
            Some((d, p)) => (HudBufferHeader { opacity: 0.0, ..*d }, p.clone()),
            None => {
                // Nothing shown yet; still tell a layer that might have an older image.
                let Some([ew, eh]) = m.extent else { return };
                let mut d = HudBufferHeader::ZERO;
                (d.width, d.height, d.stride, d.extent_w, d.extent_h, d.opacity, d.model_rev) = (1, 1, 4, ew, eh, 0.0, m.rev);
                (d, vec![0; 4])
            }
        };
        match w.publish(&desc, &pixels) {
            Ok(_) => self.last = Some((desc, pixels)),
            Err(e) => tracing::warn!("hiding the HUD image: {e}"),
        }
    }

    fn open_window(&mut self, m: &HudModel, f: f32, cx: &mut App) -> Result<()> {
        self.close_window(cx);
        let frame: Rc<RefCell<Frame>> = Rc::default();
        let view = HudView::new(m.clone(), 1.0, Mode::Image);
        let measure = view.measure.clone();
        let sink = frame.clone();
        let t = Instant::now();
        let first = window_size(m);
        let handle = cx.open_offscreen_window(
            OffscreenWindowOptions {
                size: size(px(first.0), px(first.1)),
                scale_factor: f,
                // Rendering stays off the game's (discrete) GPU and doesn't wake it.
                prefer_low_power_gpu: true,
                // Only render_now draws; a stray notify shouldn't render at 1 kHz.
                min_frame_interval: Some(Duration::from_millis(100)),
                ..Default::default()
            },
            move |fr| {
                let mut s = sink.borrow_mut();
                (s.width, s.height, s.stride, s.render) = (fr.width, fr.height, fr.stride, fr.render_duration);
                s.data.clear();
                s.data.extend_from_slice(fr.data);
            },
            move |_, cx| cx.new(|_| view),
        )?;
        if let Ok(Some(gpu)) = handle.update(cx, |_, w, _| w.gpu_specs()) {
            tracing::info!(factor = f, took = ?t.elapsed(), "off-screen HUD window on {} ({})", gpu.device_name, gpu.driver_name);
        }
        self.stats.windows += 1;
        self.window = Some(Offscreen { handle, factor: f, frame, measure, size: first });
        Ok(())
    }

    /// Frees the off-screen window once the desktop has presented for a while.
    pub fn tick(&mut self, cx: &mut App) {
        if self.desktop_since.is_some_and(|t| t.elapsed() >= IDLE_WINDOW) && self.window.is_some() {
            tracing::info!("desktop presenter for {IDLE_WINDOW:?}: closing the off-screen HUD window");
            self.close_window(cx);
        }
    }

    fn close_window(&mut self, cx: &mut App) {
        if let Some(w) = self.window.take() {
            let _ = w.handle.update(cx, |_, win, _| win.remove_window());
        }
    }

    /// The session ended: no more heartbeats, no window.
    pub fn close(&mut self, cx: &mut App) {
        self.close_window(cx);
        self.writer = None;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_follows_the_swapchain_and_scale() {
        let mut m: HudModel = serde_json::from_value(serde_json::json!({
            "session": "s-1", "product_id": 1, "rev": 1, "on": true, "layout": "compact", "name": "Compact",
            "presenter": "game", "presenter_reason": "", "anchor": "top-left", "offset": [12.0, 12.0], "scale": 1.0,
            "text_px": 25.6, "background": 0.45, "text_opacity": 1.0, "level_colors": true, "orientation": "row",
            "safe_area": 0.0, "captures": "shown", "rows": []
        }))
        .unwrap();
        assert!((factor(&m) - 25.6 / 12.0).abs() < 1e-4);
        m.scale = 2.5;
        m.text_px = 1600.0 * 0.016;
        assert!((factor(&m) - 2.5 * 25.6 / 12.0).abs() < 1e-4);
        m.text_px = f32::NAN;
        assert_eq!(factor(&m), 1.0);
    }

    #[test]
    fn a_large_card_is_drawn_smaller_rather_than_cropped() {
        use crate::demo::sample_model;
        let cap = quiver_perf::hud::buffer_bytes_for(1024, 1000).unwrap();
        let image = |m: &HudModel, f: f32| {
            let (w, h) = model_size(m);
            ((w * f).ceil() as u32 + 1, (h * f).ceil() as u32 + 1)
        };
        // Standard at 1600p, scale 1: as asked.
        let m = sample_model("standard", 0, Presenter::Game, Some([2560, 1600]));
        assert_eq!(fit_factor(&m, cap), factor(&m));
        // Compact at 4K is wider than the layer's 2048 px; Full at scale 2.5 more than the buffer.
        let m = sample_model("compact", 0, Presenter::Game, Some([3840, 2160]));
        let f = fit_factor(&m, cap);
        assert!(f < factor(&m) && image(&m, f).0 <= HUD_MAX_DIM, "{f}");
        let mut m = sample_model("full", 0, Presenter::Game, Some([2560, 1600]));
        m.scale = 2.5;
        let f = fit_factor(&m, cap);
        let (w, h) = image(&m, f);
        let stride = (w * 4).div_ceil(256) * 256;
        assert!(f < factor(&m) && (stride * h) as usize + HUD_PIXELS_OFFSET <= cap as usize, "{f}: {w}x{h}");
    }
}
