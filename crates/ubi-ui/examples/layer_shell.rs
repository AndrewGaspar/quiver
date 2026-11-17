//! Opens a small wlr-layer-shell surface the way Sight's toasts will: the overlay layer,
//! anchored to the top-right corner, no keyboard focus, an empty input region (clicks go
//! through), drawn with `ubi-ui` components. It stays up for `--seconds` (default 2.5), then
//! closes and the process exits.
//!
//! ```text
//! cargo run -p ubi-ui --example layer_shell -- [--seconds 2.5] [--namespace quiver-sight-test]
//! ```
//!
//! Needs a Wayland compositor with `zwlr_layer_shell_v1` (Hyprland, Sway, …).

use std::time::{Duration, Instant};

use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
use gpui::{
    App, AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, div, point, px, size,
};
use ubi_ui::assets::{self, UI_FONT};
use ubi_ui::components::{Icon, SEMIBOLD, caption, icon, progress, soft_shadow};
use ubi_ui::theme::{self, Theme};

struct Toast {
    opened: Instant,
    seconds: f32,
}

impl Render for Toast {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        let left = (1.0 - self.opened.elapsed().as_secs_f32() / self.seconds).clamp(0.0, 1.0);
        div().size_full().p(px(10.)).child(
            div()
                .size_full()
                .flex()
                .gap(px(12.))
                .items_center()
                .p(px(14.))
                .rounded(px(12.))
                .bg(th.ink2.opacity(0.94))
                .border_1()
                .border_color(th.line2)
                .shadow(vec![soft_shadow(th.scrim, 6., 18.)])
                .font_family(UI_FONT)
                .child(
                    div()
                        .flex_none()
                        .size(px(36.))
                        .rounded(px(9.))
                        .bg(th.acc_soft)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(Icon::Trophy, 18., th.acc_hi)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(caption("Quiver · Sight", th.tx3))
                        .child(div().text_size(px(14.)).font_weight(SEMIBOLD).text_color(th.tx1).child("Layer-shell test surface"))
                        .child(progress(&th, left, None, 3., None)),
                ),
        )
    }
}

fn main() {
    let mut seconds = 2.5f32;
    let mut namespace = "quiver-sight-test".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => seconds = it.next().and_then(|v| v.parse().ok()).expect("--seconds N"),
            "--namespace" => namespace = it.next().expect("--namespace NAME"),
            other => panic!("unknown argument {other}"),
        }
    }
    let started = Instant::now();
    gpui_platform::application().with_assets(assets::Assets).run(move |cx: &mut App| {
        ubi_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        let window = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(360.), px(92.)),
                    })),
                    app_id: Some("quiver-sight-test".into()),
                    focus: false,
                    window_background: WindowBackgroundAppearance::Transparent,
                    kind: WindowKind::LayerShell(LayerShellOptions {
                        namespace: namespace.clone(),
                        layer: Layer::Overlay,
                        anchor: Anchor::TOP | Anchor::RIGHT,
                        margin: Some((px(16.), px(16.), px(0.), px(0.))),
                        keyboard_interactivity: KeyboardInteractivity::None,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                move |window, cx| {
                    // Click-through: an empty input region.
                    window.set_input_region(Some(&[]));
                    cx.new(|_| Toast { opened: Instant::now(), seconds })
                },
            )
            .expect("open a layer-shell window (does the compositor support zwlr_layer_shell_v1?)");
        println!("layer surface '{namespace}' opened after {:?}", started.elapsed());

        cx.spawn(async move |cx| {
            let opened = Instant::now();
            // Redraw the countdown bar a few times a second.
            while opened.elapsed().as_secs_f32() < seconds {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let _ = window.update(cx, |_, _, cx| cx.notify());
            }
            let _ = window.update(cx, |_, window, _| window.remove_window());
            println!("closed after {:?}", opened.elapsed());
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
