//! Add-ons: everything associated with the game, owned or not; ones you don't own link to
//! a Ubisoft Store search for their name.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use ubi_api::info::ProductKind;

use super::page::{AddonFilter, load_note, note, small};
use crate::app::Quiver;
use crate::art::{ArtSource, Variant};
use crate::model::GameRow;
use crate::nav::Target;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, MEDIUM, PillKind, art_fill, card, card_header, icon, pill};

fn kind_label(k: ProductKind) -> &'static str {
    match k {
        ProductKind::SeasonPass => "Season pass",
        ProductKind::AddOn | ProductKind::PreOrderAddOn => "Add-on",
        ProductKind::Bundle | ProductKind::BundlePackage => "Bundle",
        ProductKind::Consumable | ProductKind::ConsumablePacks => "In-game currency",
        ProductKind::PreOrderGame | ProductKind::Preorder => "Pre-order",
        ProductKind::Game => "Game",
        ProductKind::Trial => "Trial",
        _ => "Product",
    }
}

impl Quiver {
    pub(super) fn render_addons(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let Some(info) = self.gp.info.ready().filter(|_| self.gp.id == id).cloned() else {
            return card(th).children(load_note(th, &self.gp.info, "add-ons")).into_any_element();
        };
        let all: Vec<_> = info.addons.iter().filter(|a| a.visible || a.owned).collect();
        let owned = all.iter().filter(|a| a.owned).count();
        let shown = self.addons_shown();
        let filter = self.gp.addon_filter;
        let seg_el = div()
            .flex()
            .children([(AddonFilter::All, "All"), (AddonFilter::Owned, "Owned"), (AddonFilter::NotOwned, "Not owned")].into_iter().map(|(f, l)| {
                let on = f == filter;
                div()
                    .id(l)
                    .px(px(10.))
                    .py(px(3.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(th.ink5).text_color(th.tx1).font_weight(MEDIUM))
                    .when(!on, |d| d.text_color(th.tx2))
                    .child(l)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.gp.addon_filter = f;
                        cx.notify();
                    }))
            }))
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(8.))
            .bg(th.ink1)
            .border_1()
            .border_color(th.line2);
        let rows: Vec<AnyElement> = shown
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let focused = self.focused(Target::Body(i));
                let name = a.name.clone().unwrap_or_else(|| format!("Add-on {}", a.product_id));
                let art = a.thumb.clone().map(|t| self.art_state(Some(ArtSource::Cdn(t)), Variant::Original, cx));
                let url = a.store_url.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(9.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .when(focused, |d| d.bg(th.acc_soft))
                    .child(match art {
                        Some(s) => div().w(px(30.)).h(px(35.)).flex_none().child(art_fill(th, &s, "", 4., !a.owned)).into_any_element(),
                        None => div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .w(px(30.))
                            .h(px(35.))
                            .rounded(px(5.))
                            .bg(if a.owned { th.ok_soft } else { th.ink3 })
                            .child(icon(if a.owned { Icon::CircleCheck } else { Icon::Package }, 15., if a.owned { th.ok } else { th.tx3 }))
                            .into_any_element(),
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(div().font_weight(MEDIUM).text_color(if a.owned { th.tx1 } else { th.tx2 }).truncate().child(name))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(format!("{} · product {}", kind_label(a.kind), a.product_id))),
                    )
                    .child(if a.owned {
                        pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Owned").into_any_element()
                    } else {
                        pill(th, PillKind::Muted, None, "Not owned").into_any_element()
                    })
                    .child(div().w(px(196.)).flex().justify_end().when_some(url, |d, url| {
                        d.child(
                            small(("addon-store", i), th, None, "View in Ubisoft Store")
                                .child(icon(Icon::ExternalLink, 12., th.tx2))
                                .focus_ring(focused, th, 6.)
                                .on_click(cx.listener(move |this, _, _, cx| this.open_external(&url, cx))),
                        )
                    }))
                    .into_any_element()
            })
            .collect();
        let owners: Vec<String> = info.account.iter().map(|a| self.account_name(a)).collect();
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                card(th)
                    .child(
                        card_header(th, "Add-ons")
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{owned} owned · {} more available", all.len() - owned)))
                            .child(div().flex_1())
                            .child(seg_el),
                    )
                    .when(rows.is_empty(), |d| d.child(note(th, if all.is_empty() { "Ubisoft lists no add-ons for this game." } else { "Nothing in this filter." })))
                    .children(rows),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.5))
                    .text_color(th.tx3)
                    .child(icon(Icon::Info, 12., th.tx3))
                    .child(format!(
                        "Ownership of {}. \"View in Ubisoft Store\" opens a store search for the add-on's name in your browser; there's no store inside Quiver.",
                        if owners.is_empty() { "the launch account".to_string() } else { owners.join(", ") }
                    )),
            )
            .child(self.cli_line("addons-cli", th, format!("quiver info {id} --all"), cx).w(px(360.)))
            .into_any_element()
    }
}
