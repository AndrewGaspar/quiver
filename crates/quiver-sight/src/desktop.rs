//! The desktop presenter (SIGHT-HUD §2.11): the same [`HudView`] on a click-through
//! wlr-layer-shell surface (namespace `quiver-sight-hud`, overlay layer, no keyboard focus,
//! an empty input region) on the game's monitor, anchored per the model. Used when the layer
//! can't draw (OpenGL, anti-cheat, layer missing or dead). It draws the frame-time graph
//! itself from the model's points. Commits follow the model (≤ the HUD's rate), which keeps
//! the extra compositor flips of §2.1 to that rate.

use anyhow::Result;
use gpui::layer_shell::{KeyboardInteractivity, Layer, LayerShellOptions};
use gpui::{
    App, AppContext, Bounds, DisplayId, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, point,
    px, size,
};
use quiver_api::perf::{Anchor, HudModel, Presenter};

use crate::hud_view::{HudView, Mode};
use crate::place::layer_anchor;

pub const NAMESPACE: &str = "quiver-sight-hud";

#[derive(Debug, Clone, PartialEq)]
struct Key {
    anchor: Anchor,
    offset: [i32; 2],
    monitor: Option<String>,
}

#[derive(Default)]
pub struct DesktopHud {
    window: Option<(WindowHandle<HudView>, Key)>,
}

/// The display GPUI knows for an output name (`eDP-1`): Wayland outputs' UUIDs are v5 UUIDs
/// of their names.
pub fn display_named(cx: &App, name: &str) -> Option<DisplayId> {
    let want = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes());
    cx.displays().into_iter().find(|d| d.uuid().is_ok_and(|u| u == want)).map(|d| d.id())
}

/// Opens a click-through layer-shell surface that never takes focus.
pub fn open_layer<V: gpui::Render + 'static>(
    cx: &mut App,
    namespace: &str,
    anchor: gpui::layer_shell::Anchor,
    margin: (f32, f32, f32, f32),
    first_size: (f32, f32),
    monitor: Option<&str>,
    view: V,
) -> Result<WindowHandle<V>> {
    let display_id = monitor.and_then(|m| display_named(cx, m));
    if monitor.is_some() && display_id.is_none() {
        tracing::debug!(?monitor, "monitor not among GPUI's displays: the compositor picks one");
    }
    let opts = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: size(px(first_size.0), px(first_size.1)) })),
        app_id: Some("quiver-sight".into()),
        focus: false,
        window_background: WindowBackgroundAppearance::Transparent,
        display_id,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: namespace.to_owned(),
            layer: Layer::Overlay,
            anchor,
            margin: Some((px(margin.0), px(margin.1), px(margin.2), px(margin.3))),
            keyboard_interactivity: KeyboardInteractivity::None,
            ..Default::default()
        }),
        ..Default::default()
    };
    cx.open_window(opts, move |window, cx| {
        // Click-through: an empty input region.
        window.set_input_region(Some(&[]));
        cx.new(|_| view)
    })
}

impl DesktopHud {
    /// Shown while the model is on and its presenter is the desktop; `layer_shell` is false
    /// when the platform has none (headless, X11): then nothing is shown.
    pub fn update(&mut self, m: &HudModel, layer_shell: bool, cx: &mut App) -> Result<()> {
        if !(m.on && m.presenter == Presenter::Desktop && layer_shell) {
            self.close(cx);
            return Ok(());
        }
        let key = Key { anchor: m.anchor, offset: [m.offset[0].round() as i32, m.offset[1].round() as i32], monitor: m.monitor.clone() };
        // The game's monitor became known and the compositor already put the surface there.
        if let Some((h, k)) = &mut self.window
            && k.monitor.is_none()
            && (Key { monitor: None, ..key.clone() }) == *k
            && let Some(want) = key.monitor.as_deref().and_then(|n| display_named(cx, n))
            && h.update(cx, |_, w, cx| w.display(cx).map(|d| d.id())).ok().flatten() == Some(want)
        {
            k.monitor = key.monitor.clone();
        }
        if let Some((h, k)) = &self.window
            && *k == key
        {
            let model = m.clone();
            let unit = m.scale.clamp(0.5, 4.0);
            if h.update(cx, |v, _, cx| {
                v.model = model;
                v.unit = unit;
                cx.notify();
            })
            .is_ok()
            {
                return Ok(());
            }
        }
        self.close(cx);
        let unit = m.scale.clamp(0.5, 4.0);
        let (edges, margin) = layer_anchor(m.anchor, m.offset);
        let h = open_layer(cx, NAMESPACE, edges, margin, (300.0 * unit, 40.0 * unit), m.monitor.as_deref(), HudView::new(m.clone(), unit, Mode::Desktop))?;
        tracing::info!(monitor = ?m.monitor, anchor = ?m.anchor, reason = %m.presenter_reason, "desktop HUD shown");
        self.window = Some((h, key));
        Ok(())
    }

    pub fn close(&mut self, cx: &mut App) {
        if let Some((h, _)) = self.window.take() {
            let _ = h.update(cx, |_, w, _| w.remove_window());
            tracing::info!("desktop HUD closed");
        }
    }
}
