//! Overlays: the command palette (Ctrl K / Start), the game menu (Y), the controller hint
//! bar, and toasts.

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};

use super::{Icon, MEDIUM, PillKind, SEMIBOLD, caption, glyph, icon, kbd, mono, pill, soft_shadow};
use crate::app::{Command, InputMode, Quiver};
use crate::model::short_name;
use crate::nav::{Overlay, Page, Target, Zone};
use crate::theme::{Theme, rgba};

impl Quiver {
    pub fn render_overlay(&mut self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let overlay = self.nav.overlay?;
        if let Overlay::Palette { selected } = overlay {
            return Some(self.render_palette(th, selected, cx));
        }
        let (title, context, commands, selected) = match overlay {
            Overlay::Palette { selected } => {
                let ctx = self.nav.focused_game(&self.layout).and_then(|id| self.game(id)).map(|g| short_name(&g.entry.name));
                ("palette", ctx, self.palette_commands(), selected)
            }
            Overlay::Menu { game, selected } => {
                let default_menu = self.gp.play_menu && self.gp.default_menu;
                let ctx = self.game(game).map(|g| if default_menu { format!("{} · {}", g.entry.name, Quiver::default_for(game)) } else { g.entry.name.clone() });
                ("menu", ctx, self.menu_commands(game), selected)
            }
        };
        let palette = title == "palette";
        let pad = self.mode == InputMode::Pad;
        let width = self.dialog_width(if palette { 720. } else { 400. });
        let left = ((self.viewport.0 - width) / 2.).max(16.);
        let items: Vec<AnyElement> = commands
            .iter()
            .enumerate()
            .map(|(i, c)| self.render_command(th, c, i, i == selected, palette, cx))
            .collect();

        let header = if palette {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(16.))
                .py(px(14.))
                .border_b_1()
                .border_color(th.line1)
                .child(icon(Icon::Search, 16., th.tx3))
                .child(div().w(px(2.)).h(px(18.)).bg(th.acc))
                .child(div().flex_1().text_size(px(15.)).text_color(th.tx3).child("Type a command or a game"))
                .when_some(context.clone(), |d, c| d.child(pill(th, PillKind::Muted, None, c)))
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(14.))
                .pt(px(10.))
                .pb(px(8.))
                .child(div().font_weight(SEMIBOLD).truncate().child(context.clone().unwrap_or_default()))
                .into_any_element()
        };
        let footer = div()
            .flex()
            .items_center()
            .gap(px(16.))
            .px(px(16.))
            .py(px(10.))
            .border_t_1()
            .border_color(th.line1)
            .bg(th.ink1)
            .rounded_b(px(14.))
            .text_size(px(11.5))
            .text_color(th.tx3)
            .when(!pad, |d| {
                d.child(div().flex().items_center().gap(px(6.)).child(kbd(th, "↑↓")).child("Move"))
                    .child(div().flex().items_center().gap(px(6.)).child(kbd(th, "↵")).child("Run"))
                    .child(div().flex().items_center().gap(px(6.)).child(kbd(th, "Esc")).child("Close"))
            })
            .when(pad, |d| {
                d.child(div().flex().items_center().gap(px(6.)).child(glyph(th, "A", false)).child("Run"))
                    .child(div().flex().items_center().gap(px(6.)).child(glyph(th, "B", false)).child("Close"))
            })
            .child(div().flex_1())
            .when(palette, |d| d.child("Every entry is a daemon call"));

        let panel = div()
            .id(title)
            .absolute()
            .top(px(if palette { 92. } else { 132. }))
            .left(px(left))
            .w(px(width))
            .flex()
            .flex_col()
            .rounded(px(14.))
            .bg(th.ink2)
            .border_1()
            .border_color(th.line2)
            .shadow(vec![soft_shadow(rgba(0x000000, 0.7), 30., 90.)])
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .py(px(8.))
                    .when(palette, |d| d.child(div().px(px(20.)).py(px(6.)).child(caption("Commands", th.tx3))))
                    .children(items),
            )
            .child(footer);

        Some(
            div()
                .id("scrim")
                .absolute()
                .inset_0()
                .bg(th.scrim)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.nav.close_overlay();
                    cx.notify();
                }))
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_command(&self, th: &Theme, c: &Command, i: usize, on: bool, palette: bool, cx: &mut Context<Self>) -> AnyElement {
        let fg = if on { th.acc_hi } else { th.tx3 };
        div()
            .id(("cmd", i))
            .flex()
            .items_center()
            .gap(px(12.))
            .mx(px(6.))
            .px(px(if palette { 14. } else { 10. }))
            .py(px(9.))
            .rounded(px(8.))
            .when(on, |d| d.bg(th.acc_soft))
            .hover(|s| s.bg(th.ink3))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let t = if matches!(this.nav.overlay, Some(Overlay::Palette { .. })) {
                    Target::PaletteItem(i)
                } else {
                    Target::MenuItem(i)
                };
                this.click(t, cx);
            }))
            .child(icon(c.icon, 14., fg))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(div().font_weight(if on { SEMIBOLD } else { MEDIUM }).truncate().child(c.label.clone()))
                    .when_some(c.detail.clone(), |d, s| d.child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(s))),
            )
            .when(c.future, |d| d.child(pill(th, PillKind::Future, None, "Not yet")))
            .when_some(c.cli.clone().filter(|_| palette || on), |d, cli| {
                d.child(
                    mono(11., if on { th.who_cli } else { th.tx4 })
                        .max_w(px(if palette { 280. } else { 150. }))
                        .truncate()
                        .child(cli),
                )
            })
            .when(on && palette, |d| d.child(kbd(th, "↵")))
            .into_any_element()
    }

    pub fn render_hintbar(&self, th: &Theme) -> impl IntoElement + use<> {
        let hint = |g: Vec<&'static str>, label: &str| {
            div()
                .flex()
                .items_center()
                .gap(px(7.))
                .children(g.into_iter().map(|g| glyph(th, g, false)))
                .child(label.to_owned())
        };
        let mut hints = vec![];
        if self.nav.overlay.is_some() {
            hints.push(hint(vec!["A"], "Select"));
            hints.push(hint(vec!["B"], "Close"));
        } else {
            match (self.nav.page, self.nav.zone) {
                (Page::Library, Zone::Hero) => {
                    hints.push(hint(vec!["A"], "Select"));
                    hints.push(hint(vec!["Y"], "Menu"));
                }
                (Page::Library, _) => {
                    hints.push(hint(vec!["A"], "Open"));
                    hints.push(hint(vec!["X"], "Play options"));
                    hints.push(hint(vec!["Y"], "Menu"));
                }
                (Page::Game(_), _) => {
                    hints.push(hint(vec!["A"], if self.nav.zone == Zone::Actions { "Select" } else { "Open" }));
                    hints.push(hint(vec!["X"], "Play options"));
                    hints.push(hint(vec!["Y"], "Menu"));
                    hints.push(hint(vec!["B"], "Back to library"));
                }
                (Page::Settings, Zone::Body) => {
                    hints.push(hint(vec!["A"], "Select"));
                    hints.push(hint(vec!["B"], "Sections"));
                }
                (Page::Settings, _) => {
                    hints.push(hint(vec!["A"], "Open section"));
                    hints.push(hint(vec!["B"], "Back to library"));
                }
                (Page::Downloads, Zone::Body) => {
                    hints.push(hint(vec!["A"], "Select"));
                    hints.push(hint(vec!["X"], "Pause / resume"));
                    hints.push(hint(vec!["LT", "RT"], "Move in queue"));
                    hints.push(hint(vec!["B"], "Back to library"));
                }
                (Page::Activity, _) => {
                    hints.push(hint(vec!["A"], "Open game"));
                    hints.push(hint(vec!["Y"], if self.ui.activity_game.is_some() { "All games" } else { "Only this game" }));
                    hints.push(hint(vec!["B"], "Back to library"));
                }
                _ => {
                    hints.push(hint(vec!["A"], "Open game"));
                    hints.push(hint(vec!["B"], "Back to library"));
                }
            }
            hints.push(hint(vec!["LB", "RB"], "Library · Downloads · Activity"));
            match self.nav.page {
                Page::Game(_) => hints.push(hint(vec!["LT", "RT"], "Tabs")),
                Page::Settings => hints.push(hint(vec!["LT", "RT"], "Sections")),
                Page::Activity => hints.push(hint(vec!["LT", "RT"], "Source")),
                _ => {}
            }
        }
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(18.))
            .h(px(44.))
            .px(px(24.))
            .bg(th.ink0)
            .border_t_1()
            .border_color(th.line1)
            .text_size(px(12.5))
            .text_color(th.tx2)
            .children(hints)
            .child(div().flex_1())
            .when_some(self.pad.clone(), |d, name| d.child(div().text_color(th.tx4).text_size(px(11.5)).child(name)))
            .child(hint(vec!["≡"], "Commands"))
    }
}

/// A short note at the bottom; `bad`: an error, tinted red with a red icon and border.
pub fn toast(th: &Theme, text: SharedString, bad: bool) -> impl IntoElement + use<> {
    let (bg, border, ic, ic_color) = if bad { (th.bad.opacity(0.22).blend(th.ink3), th.bad.opacity(0.7), Icon::CircleX, th.bad) } else { (th.ink3, th.line2, Icon::Info, th.acc_hi) };
    div().absolute().left_0().right_0().bottom(px(64.)).flex().justify_center().px(px(16.)).child(
        div()
            .flex()
            .min_w_0()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .py(px(9.))
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .shadow(vec![soft_shadow(rgba(0x000000, 0.5), 14., 40.)])
            .text_size(px(12.5))
            .when(bad, |d| d.text_color(th.tx1))
            .child(icon(ic, 14., ic_color))
            .child(div().min_w_0().child(text)),
    )
}
