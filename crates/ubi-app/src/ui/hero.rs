//! A game's title over its hero art, shared by the library home's hero and the game page:
//! the trimmed logo from the product config (or the user's own logo), nothing when the
//! Ubisoft art already has the logo painted in, and the name in display type while the logo
//! loads or when there is none.

use gpui::{AnyElement, Context, FontWeight, IntoElement, ParentElement, Styled, div, img, px};

use super::display;
use crate::app::Quiver;
use crate::art::{ArtImage, ArtSlot, ArtState, Variant};
use crate::model::GameRow;
use crate::theme::on_art;

/// Heroes that already have the logo painted in, so Quiver doesn't draw it again.
pub const BAKED_LOGO: &[u32] = &[3539, 3600];

#[derive(Clone, Debug, PartialEq)]
pub enum HeroTitle {
    /// The hero art carries the logo itself.
    Baked,
    Logo(ArtImage),
    /// The name: the logo is loading, failed, or the game has none.
    Text,
}

/// How big a title may be.
#[derive(Clone, Copy, Debug)]
pub struct TitleBox {
    pub max_w: f32,
    pub max_h: f32,
    /// Upscaling cap for small logos.
    pub max_scale: f32,
    /// Display type size for the name, and how wide it may wrap.
    pub text: f32,
    pub text_w: f32,
}

impl Quiver {
    pub fn hero_title(&mut self, g: &GameRow, cx: &mut Context<Self>) -> HeroTitle {
        let id = g.id();
        // Custom art (any slot) means the hero may not be Ubisoft's any more.
        if BAKED_LOGO.contains(&id) && !self.custom_art.contains_key(&id) {
            return HeroTitle::Baked;
        }
        match self.art_for(g, ArtSlot::Logo, Variant::Trimmed, cx) {
            ArtState::Ready(l) => HeroTitle::Logo(l),
            _ => HeroTitle::Text,
        }
    }
}

/// The logo fitted inside the box (anchored top-left), or the name. `None` for baked art.
pub fn title_element(t: &HeroTitle, name: &str, b: TitleBox) -> Option<AnyElement> {
    match t {
        HeroTitle::Baked => None,
        HeroTitle::Logo(l) => {
            let s = (b.max_w / l.width.max(1) as f32).min(b.max_h / l.height.max(1) as f32).min(b.max_scale);
            Some(img(l.path.clone()).flex_none().w(px(l.width as f32 * s)).h(px(l.height as f32 * s)).into_any_element())
        }
        HeroTitle::Text => Some(
            display(b.text, FontWeight::EXTRA_BOLD, on_art::tx1())
                .line_height(px(b.text * 1.04))
                .max_w(px(b.text_w))
                .child(name.to_owned())
                .into_any_element(),
        ),
    }
}

/// [`title_element`] in a box of fixed height, bottom-aligned, so the content below doesn't
/// move when the logo replaces the name.
pub fn title_slot(t: &HeroTitle, name: &str, b: TitleBox) -> Option<AnyElement> {
    let el = title_element(t, name, b)?;
    Some(div().h(px(b.max_h)).flex().flex_col().justify_end().child(el).into_any_element())
}
