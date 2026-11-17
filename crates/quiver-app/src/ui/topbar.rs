//! Top bar: brand, Library · Downloads · Activity, now playing, search (opens the palette),
//! daemon status, settings, accounts.

use gpui::{Context, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px};

use super::responsive::{Bp, tip};
use super::{FocusExt, Icon, avatar, badge, glyph, icon, icon_button, kbd};
use crate::app::{InputMode, Link, Quiver};
use crate::assets::DISPLAY_FONT;
use crate::model::{initials, job_fraction};
use crate::nav::{Input, Page, Section, SettingsSection, Target};
use crate::theme::Theme;

impl Quiver {
    pub fn render_topbar(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pad = self.mode == InputMode::Pad;
        // Settings isn't one of the three sections: nothing is underlined there.
        let current = (self.nav.page != Page::Settings).then(|| self.nav.page.section());
        let installing: Vec<f32> = self.games.iter().filter_map(|g| g.installing()).collect();
        let jobs = self.status.as_ref().map(|s| s.jobs.iter().filter(|j| !j.done).count()).unwrap_or(0);
        let overall = self
            .status
            .as_ref()
            .and_then(|s| s.jobs.iter().find(|j| !j.done && j.kind == quiver_api::JobKind::Install && !j.status.starts_with("Queued")))
            .map(job_fraction);

        let bp = self.bp();
        // What collapses as the window narrows (see `responsive.rs`).
        let labels = bp >= Bp::Large;
        let two_rows = bp.narrow();
        let tab = |s: Section, this: &Self, with_icon: bool, with_label: bool| {
            let on = Some(s) == current;
            let focused = this.focused(Target::Section(s));
            let fg = if on { th.tx1 } else { th.tx2 };
            let i = match s {
                Section::Library => Icon::LibraryBig,
                Section::Downloads => Icon::Download,
                Section::Activity => Icon::Activity,
            };
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .px(px(if with_label { 12. } else { 9. }))
                .py(px(6.))
                .rounded(px(8.))
                .text_size(px(13.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(fg)
                .whitespace_nowrap()
                .focus_ring(focused, th, 8.)
                .when(with_icon, |d| d.child(icon(i, 16., fg)))
                .when(with_label, |d| d.child(s.label()))
                .when(s == Section::Downloads && overall.is_some(), |d| {
                    d.child(badge(th, format!("{:.0}%", (overall.unwrap_or(0.) * 100.).floor()), true))
                })
                .when(s == Section::Downloads && overall.is_none() && !installing.is_empty(), |d| {
                    d.child(badge(th, installing.len().to_string(), true))
                })
        };
        let underline =
            |inset: f32| div().absolute().left(px(inset)).right(px(inset)).bottom(px(-1.)).h(px(2.)).rounded(px(2.)).bg(th.acc);

        let nav = div().flex().flex_none().h_full().gap(px(2.)).children(Section::ALL.iter().map(|&s| {
            let on = Some(s) == current;
            div()
                .id(("nav", s.index()))
                .relative()
                .flex()
                .items_center()
                .h_full()
                .child(tab(s, self, true, labels))
                .when(on, |d| d.child(underline(if labels { 12. } else { 6. })))
                .when(!labels, |d| d.tooltip(tip(th, s.label())))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.click(Target::Section(s), cx)))
        }));
        // Narrow windows: the sections get a row of their own, full width, with labels.
        let nav_row = two_rows.then(|| {
            div()
                .flex()
                .flex_none()
                .h(px(40.))
                .px(px(8.))
                .bg(th.ink0)
                .border_b_1()
                .border_color(th.line1)
                .children(Section::ALL.iter().map(|&s| {
                    let on = Some(s) == current;
                    div()
                        .id(("nav-row", s.index()))
                        .relative()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .items_center()
                        .justify_center()
                        .child(tab(s, self, false, true))
                        .when(on, |d| d.child(underline(14.)))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.click(Target::Section(s), cx)))
                }))
        });

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
            .when(bp >= Bp::Medium, |d| {
                d.child(div().font_family(DISPLAY_FONT).font_weight(FontWeight::EXTRA_BOLD).text_size(px(16.)).child("Quiver"))
            });

        let open_palette = cx.listener(|this, _, _, cx| this.input(Input::Palette, InputMode::Pointer, cx));
        let search = if bp >= Bp::Medium {
            let wide = self.ui.sessions.is_empty() && bp == Bp::Wide;
            div()
                .id("search")
                .flex()
                .flex_shrink_1()
                .items_center()
                .gap(px(8.))
                .h(px(32.))
                .w(px(if self.ui.sessions.is_empty() { 320. } else { 262. }))
                .min_w(px(150.))
                .px(px(10.))
                .rounded(px(8.))
                .bg(th.ink2)
                .border_1()
                .border_color(th.line1)
                .text_color(th.tx3)
                .cursor_pointer()
                .hover(|s| s.border_color(th.line2))
                .on_click(open_palette)
                .child(icon(Icon::Search, 16., th.tx3))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .child(if wide { "Search games, settings, commands" } else { "Search games, commands" }),
                )
                .child(if pad { glyph(th, "≡", false) } else { kbd(th, "Ctrl K") })
        } else {
            icon_button("search", th, Icon::Search, true)
                .bg(th.ink2)
                .on_click(open_palette)
                .tooltip(tip(th, if pad { "Search (≡)" } else { "Search (Ctrl K)" }))
        };

        let (dot, label) = if self.link == Link::Reconnecting {
            (th.warn, "Reconnecting to quiverd…".to_string())
        } else if self.link == Link::Reconnected {
            (th.ok, "Reconnected to quiverd".to_string())
        } else if self.demo {
            (th.acc, "Demo data".to_string())
        } else if jobs > 0 {
            (th.ok, format!("quiverd · {jobs} job{}", if jobs == 1 { "" } else { "s" }))
        } else {
            (th.ok, "quiverd".to_string())
        };
        let daemon_full = bp == Bp::Wide;
        let daemon = div()
            .id("daemon")
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
            .when(!daemon_full, |d| d.px(px(11.)).py(px(11.)).tooltip(tip(th, label.clone())))
            .child(div().size(px(7.)).rounded_full().bg(dot))
            .when(daemon_full, |d| d.child(label));

        // One avatar per signed-in account (default first), overlapping.
        let mut names: Vec<(usize, String)> = self
            .status
            .as_ref()
            .map(|s| s.accounts.iter().enumerate().map(|(i, a)| (i, a.name.clone())).collect())
            .unwrap_or_default();
        if let Some(d) = self.status.as_ref().and_then(|s| s.accounts.iter().position(|a| a.default)) {
            names.sort_by_key(|(i, _)| *i != d);
        }
        // No Ubisoft account: a plain person icon (Settings › Accounts offers the sign-in).
        let signed_out = names.is_empty() && !self.ubisoft_signed_in();
        if names.is_empty() {
            let account = self.status.as_ref().and_then(|s| s.signed_in.clone()).unwrap_or_default();
            names.push((0, account));
        }
        let n = names.len();
        let shown = if bp >= Bp::Medium { 3 } else { 1 };
        let accounts = div()
            .id("accounts")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.nav.open_settings(SettingsSection::Accounts);
                cx.notify();
            }))
            .when(signed_out, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(30.))
                        .rounded_full()
                        .bg(th.ink3)
                        .border_1()
                        .border_color(th.line2)
                        .child(icon(Icon::User, 15., th.tx3)),
                )
            })
            .when(!signed_out, |d| {
                d.child(
                    // Overlapping by 9 px, drawn absolutely in a box of the stack's width.
                    div().relative().flex_none().h(px(30.)).w(px(30. + 21. * (n.min(shown) - 1) as f32)).children(
                        names.into_iter().take(shown).enumerate().map(|(k, (i, name))| {
                            let (from, to) = super::settings::account_colors(i);
                            avatar(&initials(&name), from, to, th.ink0).absolute().top_0().left(px(21. * k as f32))
                        }),
                    ),
                )
            })
            .when(n > shown, |d| d.child(div().text_size(px(11.)).text_color(th.tx3).child(format!("+{}", n - shown))))
            .when(!two_rows, |d| d.child(icon(Icon::ChevronDown, 14., th.tx3)));
        let playing = self.render_now_playing_pill(th, bp <= Bp::Medium, cx);
        let on_settings = self.nav.page == Page::Settings;

        let gap = match bp {
            Bp::Wide => 22.,
            Bp::Large | Bp::Medium => 16.,
            Bp::Compact => 12.,
            Bp::Narrow => 8.,
        };
        let bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(gap))
            .h(px(52.))
            .px(px(if two_rows { 12. } else { 18. }))
            .bg(th.ink0)
            .when(!two_rows, |d| d.border_b_1().border_color(th.line1))
            .child(brand)
            .when(!two_rows, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .h_full()
                        .gap(px(10.))
                        .when(pad && bp >= Bp::Medium, |d| d.child(glyph(th, "LB", true)))
                        .child(nav)
                        .when(pad && bp >= Bp::Medium, |d| d.child(glyph(th, "RB", true))),
                )
            })
            .child(div().flex_1().min_w_0())
            .when_some(playing, |d, p| d.child(p))
            .child(search)
            .child(daemon)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.))
                    .when(!two_rows, |d| {
                        d.child(
                            icon_button("bell", th, Icon::Bell, false)
                                .tooltip(tip(th, "Notifications"))
                                .on_click(cx.listener(|this, _, _, cx| this.show_toast("Notifications: not wired yet", cx))),
                        )
                    })
                    .child(
                        icon_button("settings", th, Icon::Settings, false)
                            .when(on_settings, |d| d.bg(th.ink3))
                            .tooltip(tip(th, "Settings"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.nav.open_settings(this.nav.settings);
                                cx.notify();
                            })),
                    ),
            )
            .child(accounts);
        div().flex().flex_col().flex_none().child(bar).children(nav_row)
    }
}
