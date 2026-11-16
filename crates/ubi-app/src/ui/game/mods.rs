//! Mods: profiles (Vanilla plus named stacks of layers), the selected profile's layer
//! stack, its switches and history, deleted profiles, and the layer store with "add a
//! layer from a folder".

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use ubi_api::SettingOp;
use ubi_api::mods::{Layer, ModsState, ProfileInfo, ProfileRevision, VANILLA};

use super::page::{Dialog, Load, checkbox, danger, human_size, kv, load_note, note, small, toggle};
use crate::app::Quiver;
use crate::model::{GameRow, actor_label, ago, day, gb, tilde};
use crate::nav::Target;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, badge, button, caption, card, card_header, icon, icon_button, mono, pill};

/// Stable colors for layers (by position in the store).
fn layer_color(th: &Theme, i: usize) -> gpui::Hsla {
    [th.who_agent, th.acc, th.who_cli, th.warn, th.ok, th.bad][i % 6]
}

impl Quiver {
    pub(super) fn render_mods(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if !g.installed() {
            return card(th)
                .p(px(24.))
                .text_color(th.tx3)
                .child("Mods are overlays on an installed game. Install it first; the base install is never modified.")
                .into_any_element();
        }
        let Some(m) = self.gp.mods.ready().filter(|_| self.gp.id == id).cloned() else {
            return card(th).children(load_note(th, &self.gp.mods, "mods")).into_any_element();
        };
        // Default selection: the first profile, else Vanilla.
        if self.gp.prof_sel.is_none() {
            let first = m.profiles.first().map(|p| p.profile.id.clone());
            self.select_profile(id, first, cx);
        }
        let sel = self.gp.prof_sel.clone().unwrap_or_else(|| VANILLA.into());
        let selected = m.profiles.iter().find(|p| p.profile.id == sel).cloned();
        if selected.is_none() && sel != VANILLA {
            self.gp.prof_sel = Some(VANILLA.into());
        }
        if selected.is_some() && self.gp.prof_hist.is_idle() {
            self.load_profile_history(id, &sel, cx);
        }
        self.ensure_mods_inputs(cx);

        let left = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(px(16.))
            .child(match &selected {
                Some(p) => self.render_stack(th, g, &m, p, cx).into_any_element(),
                None => self.render_vanilla_stack(th, g, &m).into_any_element(),
            })
            .child(self.render_layer_store(th, g, &m, cx));
        let right = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(match &selected {
                Some(p) => self.render_profile_card(th, g, p, cx).into_any_element(),
                None => self.render_vanilla_card(th, g, cx).into_any_element(),
            })
            .when(selected.is_some(), |d| d.child(self.render_profile_history(th, g, cx)))
            .when(!m.deleted.is_empty(), |d| d.child(self.render_deleted(th, g, &m, cx)));
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(self.render_profile_cards(th, g, &m, &sel, cx))
            .child(crate::ui::responsive::columns(self.pane_bp(), 20., 380., left, right))
            .into_any_element()
    }

    fn render_profile_cards(&mut self, th: &Theme, g: &GameRow, m: &ModsState, sel: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let playing = self.default_profile(id);
        let mut cards: Vec<AnyElement> = Vec::new();
        let entry = |i: usize, key: Option<String>, name: String, sub: String, saves: (Icon, &str), ic: Icon, pinned: bool, this: &mut Self, cx: &mut Context<Self>| {
            let on = key.as_deref().unwrap_or(VANILLA) == sel;
            let focused = this.focused(Target::Body(i));
            let is_play = key == playing;
            let k = key.clone();
            div()
                .id(("prof-card", i))
                .flex_1()
                .min_w(px(260.))
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(12.))
                .rounded(px(12.))
                .border_1()
                .border_color(if on { th.acc_line } else { th.line2 })
                .bg(if on { th.acc_soft } else { th.ink2 })
                .cursor_pointer()
                .hover(|s| s.border_color(th.acc_line))
                .focus_ring(focused, th, 12.)
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(34.))
                        .rounded(px(9.))
                        .bg(if on { th.acc } else { th.ink4 })
                        .child(icon(ic, 16., if on { th.acc_ink } else { th.tx2 })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .font_weight(SEMIBOLD)
                                .child(div().truncate().child(name))
                                .when(is_play, |d| d.child(pill(th, PillKind::Accent, Some(Icon::CircleCheck), "Default"))),
                        )
                        .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(sub)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(px(4.))
                        .when(pinned, |d| d.child(icon(Icon::Pin, 12., th.tx3)))
                        .child(div().flex().items_center().gap(px(4.)).text_size(px(11.)).text_color(th.tx3).child(icon(saves.0, 11., th.tx3)).child(saves.1.to_string())),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_profile(id, k.clone(), cx);
                    cx.notify();
                }))
                .into_any_element()
        };
        cards.push(entry(0, None, "Vanilla".into(), "Base game only, never mounted".into(), (Icon::Cloud, "Cloud saves"), Icon::ShieldCheck, false, self, cx));
        for (i, p) in m.profiles.iter().enumerate() {
            let pr = &p.profile;
            let n = pr.layers.len();
            let sub = format!(
                "{n} layer{}{}",
                if n == 1 { "" } else { "s" },
                if p.conflicts.is_empty() { String::new() } else { format!(" · {} conflicts", p.conflicts.len()) }
            );
            let saves = if pr.cloud_saves { (Icon::Cloud, "Own saves, synced") } else { (Icon::HardDrive, "Own saves") };
            cards.push(entry(i + 1, Some(pr.id.clone()), pr.name.clone(), sub, saves, Icon::Layers, pr.pinned, self, cx));
        }
        let new_i = m.profiles.len() + 1;
        let focused = self.focused(Target::Body(new_i));
        cards.push(
            div()
                .id("prof-new")
                .w(px(130.))
                .min_h(px(60.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .rounded(px(12.))
                .border_1()
                .border_dashed()
                .border_color(th.line2)
                .text_color(th.tx3)
                .cursor_pointer()
                .hover(|s| s.text_color(th.tx1).border_color(th.acc_line))
                .focus_ring(focused, th, 12.)
                .child(icon(Icon::Plus, 14., th.tx3))
                .child("New profile")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.new_profile_dialog(id, cx);
                    cx.notify();
                }))
                .into_any_element(),
        );
        div().flex().flex_wrap().gap(px(12.)).children(cards)
    }

    fn base_rows(&self, th: &Theme, g: &GameRow) -> Vec<AnyElement> {
        let info = self.gp.info.ready().filter(|_| self.gp.id == g.id());
        let owned: Vec<String> = info.map(|i| i.addons.iter().filter(|a| a.owned && a.kind.is_dlc()).filter_map(|a| a.name.clone()).collect()).unwrap_or_default();
        let ver = info.and_then(|i| i.install.as_ref()).and_then(|i| i.version.clone());
        let size = info.and_then(|i| i.install.as_ref()).map(|i| i.size_bytes).filter(|s| *s > 0);
        let dir = g.entry.installed.as_ref().map(|i| tilde(&i.install_dir)).unwrap_or_default();
        let mut v = vec![];
        if !owned.is_empty() {
            v.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .border_1()
                    .border_dashed()
                    .border_color(th.line2)
                    .child(icon(Icon::Package, 14., th.tx3))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().font_weight(SEMIBOLD).child("Ubisoft add-ons"))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(format!("{} · part of the base install, owned", owned.join(", ")))),
                    )
                    .child(pill(th, PillKind::Muted, None, "Always on"))
                    .into_any_element(),
            );
        }
        v.push(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(12.))
                .rounded(px(10.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(icon(Icon::Lock, 14., th.tx3))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().font_weight(SEMIBOLD).child(match &ver {
                            Some(v) => format!("Base install · {v}"),
                            None => "Base install".into(),
                        }))
                        .child(mono(11.5, th.tx3).truncate().child(format!("{dir}{} · never modified", size.map(|s| format!(" · {}", gb(s))).unwrap_or_default()))),
                )
                .child(pill(th, PillKind::Muted, None, "Read-only"))
                .into_any_element(),
        );
        v
    }

    fn render_vanilla_stack(&self, th: &Theme, g: &GameRow, m: &ModsState) -> impl IntoElement + use<> {
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().items_center().child(caption("Vanilla · nothing mounted", th.tx3)))
            .child(note(th, if m.layers.is_empty() {
                "Vanilla runs the base install directly. Add a layer from a folder below, then create a profile that stacks it on top."
            } else {
                "Vanilla runs the base install directly with the game's own saves. Pick a profile above to see its layers."
            }).px(px(0.)).py(px(4.)))
            .children(self.base_rows(th, g))
    }

    fn render_stack(&mut self, th: &Theme, g: &GameRow, m: &ModsState, p: &ProfileInfo, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let prof = p.profile.clone();
        let pid = prof.id.clone();
        // Profile layers top first, then the store's other layers (off).
        let mut order: Vec<(Layer, Option<usize>)> = prof
            .layers
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(pos, lid)| m.layers.iter().find(|l| &l.id == lid).map(|l| (l.clone(), Some(pos))))
            .collect();
        for l in &m.layers {
            if !prof.layers.contains(&l.id) {
                order.push((l.clone(), None));
            }
        }
        let n_on = prof.layers.len();
        let rows: Vec<AnyElement> = order
            .into_iter()
            .enumerate()
            .map(|(i, (l, pos))| {
                let on = pos.is_some();
                let color = layer_color(th, m.layers.iter().position(|x| x.id == l.id).unwrap_or(0));
                let over: Vec<&ubi_api::mods::Conflict> = p.conflicts.iter().filter(|c| c.layers.last() == Some(&l.id)).collect();
                let under: Vec<&ubi_api::mods::Conflict> = p.conflicts.iter().filter(|c| c.layers.contains(&l.id) && c.layers.last() != Some(&l.id)).collect();
                let name_of = |lid: &str| m.layers.iter().find(|x| x.id == lid).map(|x| x.name.clone()).unwrap_or_else(|| lid.to_owned());
                let warn = if !over.is_empty() {
                    let others: Vec<String> = over[0].layers.iter().filter(|x| **x != l.id).map(|x| name_of(x)).collect();
                    Some(format!("Overrides {} file{} from {}", over.len(), if over.len() == 1 { "" } else { "s" }, others.join(", ")))
                } else if !under.is_empty() {
                    Some(format!("{} file{} overridden by a higher layer", under.len(), if under.len() == 1 { "" } else { "s" }))
                } else {
                    None
                };
                let lid = l.id.clone();
                let toggle_pid = pid.clone();
                let layers_now = prof.layers.clone();
                let move_btn = |dir: i32, this: &mut Self, cx: &mut Context<Self>| {
                    let lid = l.id.clone();
                    let pid = pid.clone();
                    let layers_now = prof.layers.clone();
                    let can = match pos {
                        Some(p) if dir > 0 => p + 1 < n_on,
                        Some(p) => p > 0,
                        None => false,
                    };
                    let _ = this;
                    icon_button(SharedString::from(format!("mv-{dir}-{}", l.id)), th, if dir > 0 { Icon::ChevronUp } else { Icon::ChevronDown }, false)
                        .size(px(24.))
                        .when(!can, |d| d.opacity(0.3))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !can {
                                return;
                            }
                            let mut v = layers_now.clone();
                            if let Some(i) = v.iter().position(|x| *x == lid) {
                                let j = if dir > 0 { i + 1 } else { i - 1 };
                                v.swap(i, j);
                                this.profile_set(id, &pid, vec![SettingOp::Set { key: "layers".into(), value: v.join(",") }], cx);
                            }
                        }))
                };
                let up = move_btn(1, self, cx);
                let down = move_btn(-1, self, cx);
                div()
                    .id(("stack", i))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(11.))
                    .rounded(px(10.))
                    .bg(th.ink2)
                    .border_1()
                    .border_color(th.line1)
                    .when(!on, |d| d.opacity(0.62))
                    .child(
                        div()
                            .id(("stack-toggle", i))
                            .cursor_pointer()
                            .child(checkbox(th, on))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let mut v = layers_now.clone();
                                if let Some(i) = v.iter().position(|x| *x == lid) {
                                    v.remove(i);
                                } else {
                                    v.push(lid.clone()); // on top
                                }
                                this.profile_set(id, &toggle_pid, vec![SettingOp::Set { key: "layers".into(), value: v.join(",") }], cx);
                            })),
                    )
                    .child(div().w(px(4.)).h(px(30.)).rounded(px(2.)).flex_none().bg(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(div().font_weight(SEMIBOLD).truncate().child(l.name.clone()))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .text_size(px(11.5))
                                    .text_color(th.tx3)
                                    .child(format!("{} files · {}", l.files, human_size(l.bytes)))
                                    .when_some(warn, |d, w| d.child(div().flex().items_center().gap(px(4.)).text_color(th.warn).child(icon(Icon::TriangleAlert, 11., th.warn)).child(w))),
                            ),
                    )
                    .when(on, |d| d.child(up).child(down))
                    .child(mono(11., th.tx3).w(px(22.)).child(match pos {
                        Some(p) => format!("#{}", p + 1),
                        None => "off".into(),
                    }))
                    .into_any_element()
            })
            .collect();
        let missing = p.missing_layers.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(caption("Layer stack · top wins", th.tx3))
                    .child(badge(th, format!("{n_on} on"), true))
                    .child(div().flex_1())
                    .child(div().text_size(px(11.5)).text_color(th.tx3).child("Tick a layer to add it on top; arrows reorder")),
            )
            .when(!missing.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(8.))
                        .bg(th.warn_soft)
                        .text_color(th.warn)
                        .text_size(px(12.))
                        .child(icon(Icon::TriangleAlert, 14., th.warn))
                        .child(div().flex_1().min_w_0().child(format!("Uses {} layer(s) that were removed ({}); launching it fails until you update its layers.", missing.len(), missing.join(", ")))),
                )
            })
            .when(rows.is_empty(), |d| d.child(note(th, "No layers in the store yet. Add one from a folder below.").px(px(0.))))
            .children(rows)
            .children(self.base_rows(th, g))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(10.))
                    .mt(px(4.))
                    .rounded(px(10.))
                    .bg(th.ink0)
                    .border_1()
                    .border_color(th.line1)
                    .text_size(px(12.))
                    .child(icon(Icon::Pencil, 14., th.tx2))
                    .child(div().flex_1().min_w(px(180.)).text_color(th.tx2).child("Files the game writes go to this profile's own writable layer"))
                    .child(mono(11., th.tx3).child(format!("profiles/{}/upper", p.profile.id))),
            )
    }

    fn render_layer_store(&mut self, th: &Theme, g: &GameRow, m: &ModsState, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let used: Vec<String> = m.profiles.iter().flat_map(|p| p.profile.layers.clone()).collect();
        let rows: Vec<AnyElement> = m
            .layers
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let in_use = used.contains(&l.id);
                let layer = l.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(9.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(div().w(px(4.)).h(px(26.)).rounded(px(2.)).flex_none().bg(layer_color(th, i)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().flex().items_center().gap(px(8.)).child(div().font_weight(MEDIUM).truncate().child(l.name.clone())).child(mono(11., th.tx3).child(l.id.clone())))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(format!(
                                "{} files · {} · {} · added {} by {}",
                                l.files,
                                human_size(l.bytes),
                                l.origin.as_ref().map(|o| tilde(o)).unwrap_or_else(|| "?".into()),
                                day(l.added),
                                actor_label(&l.added_by)
                            ))),
                    )
                    .when(in_use, |d| d.child(pill(th, PillKind::Muted, None, "In use")))
                    .when(!in_use, |d| {
                        d.child(
                            icon_button(("layer-rm", i), th, Icon::Trash, false).size(px(26.)).on_click(cx.listener(move |this, _, _, cx| {
                                this.open_dialog(Dialog::RemoveLayer { game: id, layer: layer.clone() });
                                cx.notify();
                            })),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let name = self.gp.layer_name.clone();
        let path = self.gp.layer_path.clone();
        card(th)
            .child(
                card_header(th, "Layer store")
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{} layer{} · copied into Quiver, reflinked where possible", m.layers.len(), if m.layers.len() == 1 { "" } else { "s" })))
                    .child(div().flex_1()),
            )
            .children(rows)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(th.line1)
                    .bg(th.ink1.opacity(0.5))
                    .rounded_b(px(12.))
                    .child(div().flex().flex_wrap().items_center().gap(px(8.)).child(icon(Icon::FolderPlus, 14., th.tx2)).child(div().font_weight(SEMIBOLD).child("Add layer from folder")).child(div().min_w_0().text_size(px(11.5)).text_color(th.tx3).child("laid out like the install folder; names are matched to the install's case")))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(200.)).flex_shrink().min_w(px(110.)).children(name))
                            .child(div().flex_1().min_w_0().children(path))
                            .child(button("layer-add", th, Kind::Primary, Size::Small, Some(Icon::Plus), "Add layer").on_click(cx.listener(move |this, _, _, cx| this.add_layer(id, cx)))),
                    ),
            )
    }

    fn render_profile_card(&mut self, th: &Theme, g: &GameRow, p: &ProfileInfo, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let prof = p.profile.clone();
        let pid = prof.id.clone();
        let overrides = super::page::flatten(&prof.settings);
        let row = |label: &str, help: Option<&str>, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.5))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_color(th.tx2).child(label.to_owned()))
                        .when_some(help, |d, h| d.child(div().text_size(px(11.)).text_color(th.tx3).child(h.to_owned()))),
                )
                .child(control)
        };
        let switch = |key: &'static str, on: bool, this: &mut Self, cx: &mut Context<Self>| {
            let pid = pid.clone();
            let _ = this;
            div()
                .id(key)
                .cursor_pointer()
                .child(toggle(th, on))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.profile_set(id, &pid, vec![SettingOp::Set { key: key.into(), value: if on { "off".into() } else { "on".into() } }], cx);
                }))
                .into_any_element()
        };
        let cloud = switch("cloud_saves", prof.cloud_saves, self, cx);
        let pinned = switch("pinned", prof.pinned, self, cx);
        let del_prof = prof.clone();
        let play_i = self.gp.profiles().iter().position(|x| x.id == prof.id).map(|i| i + 1).unwrap_or(0);
        let is_default = self.default_profile(id).as_deref() == Some(prof.id.as_str());
        card(th)
            .p(px(16.))
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().font_weight(SEMIBOLD).text_size(px(14.)).child(prof.name.clone()))
                    .child(mono(11., th.tx3).child(format!("id {}", prof.id)))
                    .child(div().flex_1())
                    .child(badge(th, format!("r{}", p.revision), false)),
            )
            .child(row("Saves", Some("Kept apart from Vanilla's"), pill(th, PillKind::Muted, Some(Icon::HardDrive), "Own saves").into_any_element()))
            .child(row("Sync these saves to the cloud", Some("Off by default, so a broken mod can't overwrite your cloud saves"), cloud))
            .child(row("Pin to library", Some("Its own tile and list row"), pinned))
            .child(row(
                "Launch overrides",
                None,
                if overrides.is_empty() {
                    div().text_color(th.tx3).child("None").into_any_element()
                } else {
                    div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(px(2.))
                        .children(overrides.iter().map(|(k, v)| mono(11.5, th.tx2).child(format!("{k}={v}"))))
                        .into_any_element()
                },
            ))
            .child(self.cli_line("prof-cli", th, format!("quiver play {id} --profile {}", super::page::sh_quote(&prof.name)), cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(button("prof-play", th, Kind::Primary, Size::Small, Some(Icon::Play), "Play this profile").on_click(cx.listener(move |this, _, _, cx| this.play_with(id, play_i, cx))))
                    .child(self.make_default_button(th, id, play_i, is_default, cx))
                    .child(div().flex_1())
                    .child(danger("prof-del", th, Some(Icon::Trash), "Delete profile…").on_click(cx.listener(move |this, _, _, cx| {
                        this.open_dialog(Dialog::DeleteProfile { game: id, profile: del_prof.clone() });
                        cx.notify();
                    }))),
            )
    }

    fn render_vanilla_card(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let cloud = self.gp.info.ready().map(|i| i.features.cloud_saves).unwrap_or(true);
        let is_default = self.default_profile(id).is_none();
        card(th)
            .p(px(16.))
            .gap(px(10.))
            .child(div().flex().items_center().gap(px(8.)).child(div().font_weight(SEMIBOLD).text_size(px(14.)).child("Vanilla")))
            .child(kv(th, "Runs", div().child("The base install, unmounted")))
            .child(kv(th, "Saves", div().child(if cloud { "The game's own, synced with the cloud" } else { "The game's own (no cloud saves)" })))
            .child(kv(th, "Settings", div().child("This game's launch options")))
            .child(self.cli_line("van-cli", th, format!("quiver play {id} --vanilla"), cx))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(button("van-play", th, Kind::Primary, Size::Small, Some(Icon::Play), "Play Vanilla").on_click(cx.listener(move |this, _, _, cx| this.play_with(id, 0, cx))))
                    .child(self.make_default_button(th, id, 0, is_default, cx)),
            )
    }

    /// "Make default" (profile entry `i`, 0: vanilla), or a "Default for Play" mark when it
    /// already is: the game's default profile, which Play and shortcuts run.
    fn make_default_button(&mut self, th: &Theme, id: u32, i: usize, is_default: bool, cx: &mut Context<Self>) -> AnyElement {
        if is_default {
            return div().flex().items_center().child(pill(th, PillKind::Accent, Some(Icon::CircleCheck), "Default for Play")).into_any_element();
        }
        small(("make-default", i), th, Some(Icon::CircleCheck), "Make default")
            .on_click(cx.listener(move |this, _, _, cx| this.set_default_profile(id, i, cx)))
            .into_any_element()
    }

    fn render_profile_history(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let sel = self.gp.prof_sel.clone().unwrap_or_default();
        let hist: Option<Vec<ProfileRevision>> = match &self.gp.prof_hist {
            Load::Ready((p, h)) if *p == sel => Some(h.clone()),
            _ => None,
        };
        let mut c = card(th).child(card_header(th, "Profile history").child(div().flex_1()).child(div().text_size(px(11.5)).text_color(th.tx3).child("every change is a revision")));
        let Some(mut h) = hist else {
            return c.children(load_note(th, &self.gp.prof_hist, "history").or_else(|| Some(note(th, "Loading history…"))));
        };
        h.sort_by(|a, b| b.revision.cmp(&a.revision));
        let cur = h.first().map(|r| r.revision);
        let rows: Vec<AnyElement> = h
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let is_cur = Some(r.revision) == cur;
                let prev = h.get(i + 1).map(|p| p.profile.clone());
                let what = r.message.clone().unwrap_or_else(|| describe_change(prev.as_ref(), r));
                let (pid, name, rev) = (r.profile.id.clone(), r.profile.name.clone(), r.revision);
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(8.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .child(mono(12., if is_cur { th.acc_hi } else { th.tx1 }).w(px(28.)).font_weight(SEMIBOLD).child(format!("r{}", r.revision)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(div().text_size(px(12.5)).truncate().when(r.deleted, |d| d.text_color(th.bad)).child(if r.deleted { "Deleted".to_string() } else { what }))
                            .child(div().text_size(px(11.)).text_color(th.tx3).child(format!("{} · {}", actor_label(&r.source), ago(r.at)))),
                    )
                    .when(is_cur, |d| d.child(pill(th, PillKind::Accent, None, "Current")))
                    .when(!is_cur && !r.deleted, |d| {
                        d.child(small(("prof-restore", i), th, Some(Icon::RotateCcw), "Restore").on_click(cx.listener(move |this, _, _, cx| {
                            this.open_dialog(Dialog::RestoreProfile { game: id, id: pid.clone(), name: name.clone(), revision: rev });
                            cx.notify();
                        })))
                    })
                    .into_any_element()
            })
            .collect();
        c = c.children(rows);
        c
    }

    fn render_deleted(&mut self, th: &Theme, g: &GameRow, m: &ModsState, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        card(th).child(card_header(th, "Deleted profiles").child(badge(th, m.deleted.len().to_string(), false))).children(m.deleted.iter().enumerate().map(|(i, p)| {
            let (pid, name, rev) = (p.profile.id.clone(), p.profile.name.clone(), p.revision.saturating_sub(1).max(1));
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(16.))
                .py(px(9.))
                .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                .child(icon(Icon::Layers, 14., th.tx3))
                .child(div().flex_1().min_w_0().flex().flex_col().child(div().text_color(th.tx2).child(name.clone())).child(mono(11., th.tx3).child(format!("{} · saves kept", p.profile.id))))
                .child(small(("deleted-restore", i), th, Some(Icon::Undo), "Restore").on_click(cx.listener(move |this, _, _, cx| {
                    this.open_dialog(Dialog::RestoreProfile { game: id, id: pid.clone(), name: name.clone(), revision: rev });
                    cx.notify();
                })))
        }))
    }
}

/// What a profile revision changed, when it has no message.
fn describe_change(prev: Option<&ubi_api::Profile>, r: &ProfileRevision) -> String {
    let Some(p) = prev else { return "Created".into() };
    let n = &r.profile;
    let mut parts = vec![];
    if p.name != n.name {
        parts.push(format!("renamed to {}", n.name));
    }
    if p.layers != n.layers {
        let added = n.layers.iter().filter(|l| !p.layers.contains(l)).count();
        let removed = p.layers.iter().filter(|l| !n.layers.contains(l)).count();
        parts.push(match (added, removed) {
            (0, 0) => "layers reordered".into(),
            (a, 0) => format!("{a} layer{} added", if a == 1 { "" } else { "s" }),
            (0, r) => format!("{r} layer{} removed", if r == 1 { "" } else { "s" }),
            (a, r) => format!("{a} added, {r} removed"),
        });
    }
    if p.cloud_saves != n.cloud_saves {
        parts.push(format!("cloud saves {}", if n.cloud_saves { "on" } else { "off" }));
    }
    if p.pinned != n.pinned {
        parts.push(if n.pinned { "pinned".into() } else { "unpinned".into() });
    }
    if p.settings != n.settings {
        parts.push("launch overrides".into());
    }
    if parts.is_empty() {
        "No changes".into()
    } else {
        let s = parts.join(", ");
        s[..1].to_uppercase() + &s[1..]
    }
}
