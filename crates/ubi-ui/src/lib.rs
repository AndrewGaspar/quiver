//! Quiver's GPUI design system, shared by the desktop app (`ubi-app`), Sight's panel and toasts,
//! and the performance HUD that `quiver-sight` renders off-screen (SIGHT.md §4.3,
//! SIGHT-HUD.md §2.9): theme tokens derived from the current Omarchy theme ([`palette`],
//! [`theme`]), the embedded fonts and icons ([`assets`]), the shared components
//! ([`components`]) and a single-line text input ([`text_input`]).
//!
//! GPUI comes from Zed's repository at a pinned revision (the workspace `Cargo.toml`): the
//! newest one that builds with the toolchain in use (Rust 1.96) and has wlr-layer-shell
//! windows and the wgpu headless renderer.

pub mod assets;
pub mod components;
pub mod palette;
pub mod text_input;
pub mod theme;

/// Call once at startup: registers the fonts and the text input's key bindings, and installs
/// `theme` as the global [`theme::Theme`].
pub fn init(cx: &mut gpui::App, theme: theme::Theme) {
    assets::register_fonts(cx);
    cx.set_global(theme);
    text_input::bind_keys(cx);
}
