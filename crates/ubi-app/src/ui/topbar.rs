//! Top bar: brand, Library · Downloads · Activity, search (opens the palette), daemon
//! status, account.

use gpui::{Context, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px};

use super::{FocusExt, Icon, avatar, badge, glyph, icon, icon_button, kbd};
use crate::app::{InputMode, Quiver};
use crate::assets::DISPLAY_FONT;
use crate::model::{initials, job_fraction};
use crate::nav::{Input, Section, Target};
use crate::theme::{Theme, rgba};

impl Quiver {
    pub fn render_topbar(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pad = self.mode == InputMode::Pad;
        let current = self.nav.page.section();
        let installing: Vec<f32> = self.games.iter().filter_map(|g| g.installing()).collect();
        let jobs = self.status.as_ref().map(|s| s.jobs.iter().filter(|j| !j.done).count()).unwrap_or(0);
        let overall = self
            .status
            .as_ref()
            .and_then(|s| s.jobs.iter().find(|j| !j.done && j.kind == ubi_api::JobKind::Install && j.status != "Queued"))
            .map(job_fraction);

        let nav = div().flex().h_full().gap(px(2.)).children(Section::ALL.iter().map(|&s| {
            let on = s == current;
            let focused = self.focused(Target::Section(s));
            let fg = if on { th.tx1 } else { th.tx2 };
            let i = match s {
                Section::Library => Icon::LibraryBig,
                Section::Downloads => Icon::Download,
                Section::Activity => Icon::Activity,
            };
            div()
                .id(("nav", s.index()))
                .relative()
                .flex()
                .items_center()
                .h_full()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .text_size(px(13.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(fg)
                        .focus_ring(focused, th, 8.)
                        .child(icon(i, 16., fg))
                        .child(s.label())
                        .when(s == Section::Downloads && overall.is_some(), |d| {
                            d.child(badge(th, format!("{:.0}%", (overall.unwrap_or(0.) * 100.).floor()), true))
                        })
                        .when(s == Section::Downloads && overall.is_none() && !installing.is_empty(), |d| {
                            d.child(badge(th, installing.len().to_string(), true))
                        }),
                )
                .when(on, |d| {
                    d.child(div().absolute().left(px(12.)).right(px(12.)).bottom(px(-1.)).h(px(2.)).rounded(px(2.)).bg(th.acc))
                })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.click(Target::Section(s), cx)))
        }));

        let mark = div()
            .relative()
            .size(px(24.))
            .flex_none()
            .child(div().absolute().left(px(0.)).top(px(8.)).size(px(16.)).rounded(px(5.)).bg(th.mark1))
            .child(div().absolute().left(px(4.)).top(px(4.)).size(px(16.)).rounded(px(5.)).bg(th.mark2))
            .child(
                div().absolute().left(px(8.)).top(px(0.)).size(px(16.)).rounded(px(5.)).bg(gpui::linear_gradient(
                    135.,
                    gpui::linear_color_stop(th.acc_hi, 0.),
                    gpui::linear_color_stop(th.acc, 1.),
                )),
            );
        let brand = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(9.))
            .child(mark)
            .child(div().font_family(DISPLAY_FONT).font_weight(FontWeight::EXTRA_BOLD).text_size(px(16.)).child("Quiver"));

        let search = div()
            .id("search")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .w(px(340.))
            .px(px(10.))
            .rounded(px(8.))
            .bg(th.ink2)
            .border_1()
            .border_color(th.line1)
            .text_color(th.tx3)
            .cursor_pointer()
            .hover(|s| s.border_color(th.line2))
            .on_click(cx.listener(|this, _, _, cx| this.input(Input::Palette, InputMode::Pointer, cx)))
            .child(icon(Icon::Search, 16., th.tx3))
            .child(div().flex_1().text_size(px(13.)).child("Search games, settings, commands"))
            .child(if pad { glyph(th, "≡", false) } else { kbd(th, "Ctrl K") });

        let (dot, label) = if self.demo {
            (th.acc, "Demo data".to_string())
        } else if jobs > 0 {
            (th.ok, format!("quiverd · {jobs} job{}", if jobs == 1 { "" } else { "s" }))
        } else {
            (th.ok, "quiverd".to_string())
        };
        let daemon = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.))
            .px(px(10.))
            .py(px(5.))
            .rounded_full()
            .bg(th.ink2)
            .border_1()
            .border_color(th.line1)
            .text_size(px(12.))
            .text_color(th.tx2)
            .child(div().size(px(7.)).rounded_full().bg(dot))
            .child(label);

        let account = self.status.as_ref().and_then(|s| s.signed_in.clone()).unwrap_or_default();
        let accounts = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .child(avatar(&initials(&account), rgba(0x3b4a7a, 1.), rgba(0x6a5aa8, 1.), th.ink0))
            .child(icon(Icon::ChevronDown, 14., th.tx3));

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(22.))
            .h(px(52.))
            .px(px(18.))
            .bg(th.ink0)
            .border_b_1()
            .border_color(th.line1)
            .child(brand)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_full()
                    .gap(px(10.))
                    .when(pad, |d| d.child(glyph(th, "LB", true)))
                    .child(nav)
                    .when(pad, |d| d.child(glyph(th, "RB", true))),
            )
            .child(div().flex_1())
            .child(search)
            .child(daemon)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        icon_button("bell", th, Icon::Bell, false)
                            .on_click(cx.listener(|this, _, _, cx| this.show_toast("Notifications: not wired yet", cx))),
                    )
                    .child(
                        icon_button("settings", th, Icon::Settings, false)
                            .on_click(cx.listener(|this, _, _, cx| this.show_toast("Settings: coming with the next screens", cx))),
                    ),
            )
            .child(accounts)
    }
}
