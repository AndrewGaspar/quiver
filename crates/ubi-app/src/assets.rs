//! Assets compiled into the binary: Lucide icons (ISC) and the UI fonts (Geist, Geist Mono,
//! Archivo SemiExpanded; all SIL OFL 1.1). Licenses are next to the files in `assets/`.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/*.svg"]
#[include = "fonts/*.ttf"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Self::get(path).map(|f| f.data))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter().filter(|p| p.starts_with(path)).map(|p| SharedString::from(p.to_string())).collect())
    }
}

pub const UI_FONT: &str = "Geist";
pub const MONO_FONT: &str = "Geist Mono";
pub const DISPLAY_FONT: &str = "Archivo SemiExpanded";

pub fn register_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> =
        Assets::iter().filter(|p| p.ends_with(".ttf")).filter_map(|p| Assets::get(&p).map(|f| f.data)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("could not register fonts: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_the_ui_names_is_embedded() {
        let mut missing = vec![];
        for name in crate::ui::Icon::ALL.iter().map(|i| i.path()) {
            if Assets::get(name).is_none() {
                missing.push(name);
            }
        }
        assert!(missing.is_empty(), "missing icons: {missing:?}");
        assert!(Assets::iter().any(|p| p.ends_with("Geist-Regular.ttf")));
    }
}
