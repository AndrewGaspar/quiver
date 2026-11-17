//! Components shared by the screens. They take the resolved [`Theme`] and never mix colors
//! themselves (PROPOSAL.md, "Design system for GPUI").

pub mod activity;
pub mod game;
pub mod global;
pub mod hero;
pub mod library;
pub mod overlay;
pub mod downloads;
pub mod palette;
pub mod responsive;
pub mod settings;
pub mod signin;
pub mod steamgames;
pub mod topbar;
pub mod welcome;

pub use quiver_ui::components::*;

use gpui::{AnyElement, Div, IntoElement, ObjectFit, ParentElement, Pixels, Styled, StyledImage, div, img, prelude::*, px};

use crate::art::{ArtImage, ArtState};
use crate::theme::Theme;

// ---------------------------------------------------------------------------------------------
// Art

/// An image drawn to cover a `w`×`h` box, anchored at `pos` (0..1 on each axis, like CSS
/// `object-position`). The parent must clip (`overflow_hidden`).
pub fn cover(image: &ArtImage, w: f32, h: f32, pos: (f32, f32)) -> AnyElement {
    let (iw, ih) = (image.width.max(1) as f32, image.height.max(1) as f32);
    let s = (w / iw).max(h / ih);
    let (dw, dh) = (iw * s, ih * s);
    img(image.path.clone())
        .absolute()
        .left(px((w - dw) * pos.0))
        .top(px((h - dh) * pos.1))
        .w(px(dw))
        .h(px(dh))
        .object_fit(ObjectFit::Fill)
        .into_any_element()
}

/// Shown while art loads, and in its place if it fails.
pub fn art_placeholder(th: &Theme, state: &ArtState, label: &str, radius: Pixels) -> Div {
    let failed = matches!(state, ArtState::Failed);
    div()
        .size_full()
        .rounded(radius)
        .bg(gpui::linear_gradient(
            160.,
            gpui::linear_color_stop(th.ink4, 0.),
            gpui::linear_color_stop(th.ink2, 1.),
        ))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .p(px(10.))
        .when(failed, |d| d.child(icon(Icon::ImageOff, 18., th.tx4)))
        .when(!label.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(12.))
                    .font_weight(SEMIBOLD)
                    .text_color(th.tx3)
                    .text_center()
                    .child(label.to_owned()),
            )
        })
}

/// A rounded art image filling its parent, with the placeholder until it's ready.
pub fn art_fill(th: &Theme, state: &ArtState, label: &str, radius: f32, dim: bool) -> AnyElement {
    match state {
        ArtState::Ready(image) => {
            let (th1, th2) = (th.clone(), th.clone());
            let label = label.to_owned();
            img(image.path.clone())
                .size_full()
                .rounded(px(radius))
                .object_fit(ObjectFit::Cover)
                .grayscale(dim)
                .with_loading(move || art_placeholder(&th1, &ArtState::Loading, "", px(radius)).into_any_element())
                .with_fallback(move || art_placeholder(&th2, &ArtState::Failed, &label, px(radius)).into_any_element())
                .into_any_element()
        }
        _ => art_placeholder(th, state, label, px(radius)).into_any_element(),
    }
}
