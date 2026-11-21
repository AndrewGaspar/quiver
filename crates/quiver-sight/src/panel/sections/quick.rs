//! The Quick tab shares the panel's tabs, focus, input capture and confirmation dialog.
use gpui::{Context, Div, ParentElement, SharedString, Styled, div, prelude::*, px};
use quiver_ui::components::{FocusExt, SEMIBOLD, icon};
use quiver_ui::input::Input;
use quiver_ui::theme::Theme;

use crate::link::Msg;
use crate::panel::state::{Button, Confirm};
use crate::panel::view::{PanelView, focus_mark};
use crate::quick::TileKind;

impl PanelView {
    pub(crate) fn quick_input(&mut self, input: Input, cx: &mut Context<Self>) -> bool {
        let i = self.nav.quick.focus(&self.quick_tiles);
        let Some(tile) = self.quick_tiles.get(i) else { return false };
        match input {
            Input::Up | Input::Down if matches!(tile.kind, TileKind::StartReplay { .. }) => {
                self.nav.quick.step_length(input == Input::Up);
            }
            Input::Left | Input::Up | Input::Right | Input::Down => {
                let next = if matches!(input, Input::Right | Input::Down) {
                    (i + 1).min(self.quick_tiles.len() - 1)
                } else {
                    i.saturating_sub(1)
                };
                self.nav.quick.select(&self.quick_tiles[next]);
            }
            Input::Activate => self.apply_quick_tile(i, cx),
            _ => return false,
        }
        cx.notify();
        true
    }

    fn apply_quick_tile(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(mut tile) = self.quick_tiles.get(index).cloned() else { return };
        self.nav.quick.select(&tile);
        if matches!(tile.kind, TileKind::Quit) {
            self.nav.confirm = Some(Confirm {
                what: Button::Quit,
                yes: false,
            });
        } else if let Some(ui) = &self.wiring.ui {
            if matches!(tile.kind, TileKind::StartReplay { .. }) {
                tile.kind = TileKind::StartReplay {
                    seconds: self.nav.quick.length(),
                };
            }
            let _ = ui.unbounded_send(Msg::QuickAction {
                session: self.session.clone(),
                tile,
            });
        }
        cx.notify();
    }

    pub(crate) fn quick_body(&self, th: &Theme, docked: bool, cx: &mut Context<Self>) -> Div {
        let focus = self.nav.quick.focus(&self.quick_tiles);
        let visible = (((self.room.w - 112.) / 152.).floor() as usize).max(1);
        let start = focus.saturating_sub(visible / 2).min(self.quick_tiles.len().saturating_sub(visible));
        let tiles = self.quick_tiles.iter().enumerate().skip(start).take(visible).map(|(i, t)| {
            let on = i == focus;
            let kind = t.kind.clone();
            let start_replay = matches!(t.kind, TileKind::StartReplay { .. });
            let length_button = |up: bool| {
                div()
                    .id(if up { "longer-replay" } else { "shorter-replay" })
                    .px(px(5.))
                    .cursor_pointer()
                    .text_color(th.acc_hi)
                    .hover(|d| d.bg(th.ink3))
                    .child(if up { "▲" } else { "▼" })
                    .on_click(cx.listener(move |v, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(t) = v.quick_tiles.iter().find(|t| matches!(t.kind, TileKind::StartReplay { .. })) {
                            v.nav.quick.select(t);
                            v.nav.quick.step_length(up);
                        }
                        cx.notify();
                    }))
            };
            let value = div().text_size(px(12.)).text_color(if t.on == Some(true) { th.ok } else { th.tx2 });
            let value = if start_replay {
                value
                    .flex()
                    .items_center()
                    .child(length_button(true))
                    .child(length_button(false))
                    .child(format!("{} s", self.nav.quick.length()))
            } else {
                value.truncate().child(t.value.clone())
            };
            div()
                .id(SharedString::from(format!("quick-{i}")))
                .relative()
                .flex_1()
                .min_w_0()
                .max_w(px(190.))
                .h(px(100.))
                .p(px(10.))
                .flex()
                .flex_col()
                .justify_between()
                .rounded(px(10.))
                .bg(if on { th.ink4 } else { th.ink2 })
                .border_1()
                .border_color(th.line2)
                .focus_ring(on, th, 10.)
                .when(on, |d| d.child(focus_mark(&self.marks)))
                .child(icon(t.icon, 18., th.acc_hi))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(14.)).font_weight(SEMIBOLD).line_clamp(2).child(t.label.clone()))
                        .child(value),
                )
                .cursor_pointer()
                .hover(|d| d.bg(th.ink4))
                .on_click(cx.listener(move |v, _, _, cx| {
                    if let Some(index) = v.quick_tiles.iter().position(|t| t.kind == kind) {
                        v.apply_quick_tile(index, cx);
                    }
                }))
        });
        let arrow = |next: bool| {
            div()
                .id(if next { "quick-next" } else { "quick-prev" })
                .cursor_pointer()
                .px(px(6.))
                .py(px(12.))
                .text_color(th.acc_hi)
                .child(if next { "▶" } else { "◀" })
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.input(if next { Input::Right } else { Input::Left }, cx);
                }))
        };
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .p(px(if docked { 12. } else { 4. }))
            .when(start > 0, |d| d.child(arrow(false)))
            .children(tiles)
            .when(start + visible < self.quick_tiles.len(), |d| d.child(arrow(true)))
    }
}
