//! Mods: profiles (Vanilla plus named stacks of mods), the selected profile's mod stack
//! (each mod tracking its current version or pinned to one), its switches and history,
//! deleted profiles, and the mod store with each mod's versions ("Make current", "Retire")
//! and "add a mod from a folder".

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use quiver_api::mods::{ModRecord, ModsRequest, ModsState, ProfileInfo, ProfileRevision, ResolvedMod, VANILLA, VersionChange};
use quiver_api::{Request, SettingOp};

use super::page::{Dialog, Load, checkbox, danger, human_size, kv, load_note, note, small, toggle};
use crate::app::Quiver;
use crate::model::{Gid, GameRow, actor_label, ago, day, gb, tilde};
use crate::nav::Target;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, badge, button, caption, card, card_header, chip, icon, icon_button, mono, pill};

/// Stable colors for layers (by position in the store).
fn layer_color(th: &Theme, i: usize) -> gpui::Hsla {
    [th.who_agent, th.acc, th.who_cli, th.warn, th.ok, th.bad][i % 6]
}

impl Quiver {
    pub(super) fn render_mods(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if !g.mods_installed() {
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
        // A Steam game: its mods reach it only when it starts through Quiver.
        let hook = g.steam_game().then(|| self.render_steam_mods_banner(th, g, cx)).flatten();

        // Options, history, keys and sources have views of their own (`options/`).
        if self.gp.opt.view != super::options::ModsView::Stack {
            let view = self.render_options_view(th, g, cx);
            return div()
                .flex()
                .flex_col()
                .gap(px(18.))
                .children(hook)
                .child(self.render_profile_cards(th, g, &m, &sel, cx))
                .child(self.render_mods_views(th, id, cx))
                .child(view)
                .into_any_element();
        }

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
                Some(p) => self.render_profile_card(th, g, &m, p, cx).into_any_element(),
                None => self.render_vanilla_card(th, g, cx).into_any_element(),
            })
            .when(selected.is_some(), |d| d.child(self.render_profile_history(th, g, cx)))
            .when(!m.deleted.is_empty(), |d| d.child(self.render_deleted(th, g, &m, cx)));
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .children(hook)
            .child(self.render_profile_cards(th, g, &m, &sel, cx))
            .child(self.render_mods_views(th, id, cx))
            .child(crate::ui::responsive::columns(self.pane_bp(), 20., 380., left, right))
            .into_any_element()
    }

    fn render_profile_cards(&mut self, th: &Theme, g: &GameRow, m: &ModsState, sel: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let playing = self.default_profile(id);
        let steam = g.steam_game();
        let default_pill = if steam { "Steam runs" } else { "Default" };
        let mut cards: Vec<AnyElement> = Vec::new();
        let entry = |i: usize, key: Option<String>, name: String, sub: String, saves: (Icon, String), ic: Icon, pinned: bool, this: &mut Self, cx: &mut Context<Self>| {
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
                                .when(is_play, |d| d.child(pill(th, PillKind::Accent, Some(Icon::CircleCheck), default_pill))),
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
                        .child(div().flex().items_center().gap(px(4.)).text_size(px(11.)).text_color(th.tx3).child(icon(saves.0, 11., th.tx3)).child(saves.1)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_profile(id, k.clone(), cx);
                    cx.notify();
                }))
                .into_any_element()
        };
        // Which save store each uses (cloud icon: the store that syncs). Steam games: every
        // profile uses the real saves.
        let store_label = |s: Option<&quiver_api::mods::SavesPlan>| match s {
            _ if steam => (Icon::Cloud, "Real saves".to_owned()),
            Some(s) => (if s.cloud { Icon::Cloud } else { Icon::HardDrive }, s.store_name.clone().unwrap_or_else(|| "Own saves".into())),
            None => (Icon::Cloud, "Default".to_owned()),
        };
        cards.push(entry(0, None, "Vanilla".into(), "Base game only, never mounted".into(), store_label(m.vanilla_saves.as_ref()), Icon::ShieldCheck, false, self, cx));
        for (i, p) in m.profiles.iter().enumerate() {
            let pr = &p.profile;
            let n = pr.layers.len();
            let pins = p.resolved.iter().filter(|r| r.pinned).count();
            let sub = format!(
                "{n} mod{}{}{}",
                if n == 1 { "" } else { "s" },
                if pins == 0 { String::new() } else { format!(" · {pins} pinned") },
                if p.conflicts.is_empty() { String::new() } else { format!(" · {} conflicts", p.conflicts.len()) }
            );
            let saves = store_label(p.saves.as_ref());
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
        // A Steam game: Steam's install folder.
        let size = info.and_then(|i| i.install.as_ref()).map(|i| i.size_bytes).or_else(|| g.client().map(|c| c.size_bytes)).filter(|s| *s > 0);
        let dir = g.entry.installed.as_ref().map(|i| tilde(&i.install_dir)).or_else(|| g.client().map(|c| tilde(&c.dir))).unwrap_or_default();
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
                "Vanilla runs the base install directly. Add a mod from a folder below, then create a profile that stacks it on top."
            } else {
                "Vanilla runs the base install directly with the game's own saves. Pick a profile above to see its mods."
            }).px(px(0.)).py(px(4.)))
            .children(self.base_rows(th, g))
    }

    fn render_stack(&mut self, th: &Theme, g: &GameRow, m: &ModsState, p: &ProfileInfo, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let prof = p.profile.clone();
        let pid = prof.id.clone();
        let all = m.all_mods();
        // The profile's mods top first (with their entry's position), then the store's
        // other mods (off).
        let mut order: Vec<(ModRecord, Option<usize>, Option<ResolvedMod>)> = prof
            .layers
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(pos, e)| {
                let r = m.resolve(e)?;
                let rec = all.iter().find(|x| x.id == r.mod_id)?.clone();
                Some((rec, Some(pos), Some(r)))
            })
            .collect();
        for rec in &all {
            if !order.iter().any(|o| o.0.id == rec.id) {
                order.push((rec.clone(), None, None));
            }
        }
        let n_on = prof.layers.len();
        let rows: Vec<AnyElement> = order
            .into_iter()
            .enumerate()
            .map(|(i, (rec, pos, res))| {
                let on = pos.is_some();
                // The version this row runs (or would run, tracking current).
                let vid = res.as_ref().map_or(rec.current.clone(), |r| r.version.clone());
                let l = m.layer(&vid).cloned().unwrap_or_default();
                let color = layer_color(th, all.iter().position(|x| x.id == rec.id).unwrap_or(0));
                let over: Vec<&quiver_api::mods::Conflict> = p.conflicts.iter().filter(|c| c.layers.last() == Some(&vid)).collect();
                let under: Vec<&quiver_api::mods::Conflict> = p.conflicts.iter().filter(|c| c.layers.contains(&vid) && c.layers.last() != Some(&vid)).collect();
                let warn = if !over.is_empty() {
                    let others: Vec<String> = over[0].layers.iter().filter(|x| **x != vid).map(|x| m.display(x)).collect();
                    Some(format!("Overrides {} file{} from {}", over.len(), if over.len() == 1 { "" } else { "s" }, others.join(", ")))
                } else if !under.is_empty() {
                    Some(format!("{} file{} overridden by a higher mod", under.len(), if under.len() == 1 { "" } else { "s" }))
                } else {
                    None
                };
                let mid = rec.id.clone();
                let toggle_pid = pid.clone();
                let layers_now = prof.layers.clone();
                let move_btn = |dir: i32, this: &mut Self, cx: &mut Context<Self>| {
                    let pid = pid.clone();
                    let layers_now = prof.layers.clone();
                    let can = match pos {
                        Some(p) if dir > 0 => p + 1 < n_on,
                        Some(p) => p > 0,
                        None => false,
                    };
                    let _ = this;
                    icon_button(SharedString::from(format!("mv-{dir}-{}", rec.id)), th, if dir > 0 { Icon::ChevronUp } else { Icon::ChevronDown }, false)
                        .size(px(24.))
                        .when(!can, |d| d.opacity(0.3))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let Some(i) = pos.filter(|_| can) else { return };
                            let mut v = layers_now.clone();
                            let j = if dir > 0 { i + 1 } else { i - 1 };
                            v.swap(i, j);
                            this.profile_set(id, &pid, vec![SettingOp::Set { key: "mods".into(), value: v.join(",") }], cx);
                        }))
                };
                let up = move_btn(1, self, cx);
                let down = move_btn(-1, self, cx);
                let versions = match (&res, pos) {
                    (Some(r), Some(at)) if rec.versions.len() > 1 => Some(self.version_chips(th, id, &pid, &prof.layers, at, &rec, r, cx)),
                    _ => None,
                };
                let version_note = match &res {
                    Some(r) if rec.versions.len() > 1 || r.pinned => {
                        if r.pinned { format!("{} · pinned", r.label) } else { format!("{} · tracks current", r.label) }
                    }
                    None if rec.versions.len() > 1 => format!("{} · current of {}", rec.label_of(&rec.current), rec.versions.len()),
                    _ => String::new(),
                };
                div()
                    .id(("stack", i))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(11.))
                    .rounded(px(10.))
                    .bg(th.ink2)
                    .border_1()
                    .border_color(th.line1)
                    .when(!on, |d| d.opacity(0.62))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .id(("stack-toggle", i))
                                    .cursor_pointer()
                                    .child(checkbox(th, on))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let mut v = layers_now.clone();
                                        match pos {
                                            Some(i) => {
                                                v.remove(i);
                                            }
                                            None => v.push(mid.clone()), // on top, tracking current
                                        }
                                        this.profile_set(id, &toggle_pid, vec![SettingOp::Set { key: "mods".into(), value: v.join(",") }], cx);
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
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.))
                                            .child(div().font_weight(SEMIBOLD).truncate().child(rec.name.clone()))
                                            .when(!version_note.is_empty(), |d| d.child(mono(11., th.tx3).flex_none().child(version_note))),
                                    )
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
                            })),
                    )
                    .children(versions)
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
                    .child(caption("Mod stack · top wins", th.tx3))
                    .child(badge(th, format!("{n_on} on"), true))
                    .child(div().flex_1())
                    .child(div().text_size(px(11.5)).text_color(th.tx3).child("Tick a mod to add it on top; arrows reorder")),
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
                        .child(div().flex_1().min_w_0().child(format!("Uses {} mod(s) or version(s) that were removed ({}); launching it fails until you update its mods.", missing.len(), missing.join(", ")))),
                )
            })
            .when(rows.is_empty(), |d| d.child(note(th, "No mods in the store yet. Add one from a folder below.").px(px(0.))))
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

    /// A stacked mod's version picker: track the current version, or pin one (live versions
    /// newest first, plus a retired one the entry is pinned to).
    #[allow(clippy::too_many_arguments)]
    fn version_chips(
        &mut self,
        th: &Theme,
        id: Gid,
        pid: &str,
        entries: &[String],
        at: usize,
        rec: &ModRecord,
        r: &ResolvedMod,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut items: Vec<(String, String, Option<Icon>, bool)> =
            vec![(rec.id.clone(), format!("Track current · {}", rec.label_of(&rec.current)), Some(Icon::RefreshCw), !r.pinned)];
        for v in rec.versions.iter().rev().filter(|v| !v.retired || (r.pinned && r.version == v.id)) {
            let label = if v.retired { format!("{} (retired)", v.label) } else if v.prerelease { format!("{} (pre-release)", v.label) } else { v.label.clone() };
            items.push((quiver_api::mods::stack_entry(&rec.id, Some(&v.id)), label, Some(Icon::Pin), r.pinned && r.version == v.id));
        }
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .pl(px(46.))
            .children(items.into_iter().enumerate().map(|(j, (entry, label, ic, on))| {
                let v = entries.to_vec();
                let pid = pid.to_owned();
                chip(th, ic, label, on)
                    .id(SharedString::from(format!("ver-{}-{j}", rec.id)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if on {
                            return;
                        }
                        let mut v = v.clone();
                        v[at] = entry.clone();
                        this.profile_set(id, &pid, vec![SettingOp::Set { key: "mods".into(), value: v.join(",") }], cx);
                    }))
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// Make a version current, or retire one (or bring it back), then reload.
    fn version_op(&mut self, id: Gid, req: ModsRequest, cx: &mut Context<Self>) {
        self.call::<VersionChange, _>(Request::Mods(req), cx, move |this, r, cx| match r {
            Ok(c) => {
                let label = c.record.label_of(&c.version.id);
                let msg = if c.record.current == c.version.id {
                    match c.tracking.len() {
                        0 => format!("{} {label} is current", c.record.name),
                        n => format!("{} {label} is current: {n} profile{} run it next", c.record.name, if n == 1 { "" } else { "s" }),
                    }
                } else if c.record.versions.iter().any(|v| v.id == c.version.id && v.retired) {
                    format!("Retired {} {label}", c.record.name)
                } else {
                    format!("{} {label} is back", c.record.name)
                };
                this.show_toast(c.warnings.first().cloned().unwrap_or(msg), cx);
                this.load_mods(id, cx);
            }
            Err(e) => this.show_toast(crate::err_text(&e), cx),
        });
    }

    fn render_layer_store(&mut self, th: &Theme, g: &GameRow, m: &ModsState, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let all = m.all_mods();
        let open = self.gp.mod_open.clone();
        let rows: Vec<AnyElement> = all
            .iter()
            .enumerate()
            .map(|(i, rec)| {
                let users = m.users_of(&rec.id);
                let in_use = !users.is_empty();
                let l = m.layer(&rec.current).cloned().unwrap_or_default();
                let mut shown = l.clone();
                shown.name = rec.name.clone();
                let multi = rec.versions.len() > 1;
                let is_open = open.as_deref() == Some(rec.id.as_str());
                let mid = rec.id.clone();
                let head = div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(div().w(px(4.)).h(px(26.)).rounded(px(2.)).flex_none().bg(layer_color(th, i)))
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
                                    .gap(px(8.))
                                    .child(div().font_weight(MEDIUM).truncate().child(rec.name.clone()))
                                    .child(mono(11., th.tx3).child(rec.id.clone()))
                                    .when(multi, |d| d.child(badge(th, rec.label_of(&rec.current), true))),
                            )
                            .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(format!(
                                "{}{} files · {} · {} · added {} by {}",
                                if multi { format!("{} versions · ", rec.versions.len()) } else { String::new() },
                                l.files,
                                human_size(l.bytes),
                                l.origin.as_ref().map(|o| tilde(o)).unwrap_or_else(|| "?".into()),
                                day(l.added),
                                actor_label(&l.added_by)
                            ))),
                    )
                    .when(multi, |d| {
                        d.child(small(("mod-versions", i), th, Some(if is_open { Icon::ChevronUp } else { Icon::History }), "Versions").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.gp.mod_open = if this.gp.mod_open.as_deref() == Some(mid.as_str()) { None } else { Some(mid.clone()) };
                                cx.notify();
                            },
                        )))
                    })
                    .when(in_use, |d| d.child(pill(th, PillKind::Muted, None, "In use")))
                    .when(!in_use, |d| {
                        d.child(
                            icon_button(("layer-rm", i), th, Icon::Trash, false).size(px(26.)).on_click(cx.listener(move |this, _, _, cx| {
                                this.open_dialog(Dialog::RemoveLayer { game: id, layer: shown.clone() });
                                cx.notify();
                            })),
                        )
                    });
                let versions = (is_open && multi).then(|| self.render_versions(th, id, m, rec, cx));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(9.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(head)
                    .children(versions)
                    .into_any_element()
            })
            .collect();
        let name = self.gp.layer_name.clone();
        let path = self.gp.layer_path.clone();
        card(th)
            .child(
                card_header(th, "Mod store")
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{} mod{} · copied into Quiver, reflinked where possible", all.len(), if all.len() == 1 { "" } else { "s" })))
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
                    .child(div().flex().flex_wrap().items_center().gap(px(8.)).child(icon(Icon::FolderPlus, 14., th.tx2)).child(div().font_weight(SEMIBOLD).child("Add mod from folder")).child(div().min_w_0().text_size(px(11.5)).text_color(th.tx3).child("laid out like the install folder; names are matched to the install's case")))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(200.)).flex_shrink_1().min_w(px(110.)).children(name))
                            .child(div().flex_1().min_w_0().children(path))
                            .child(button("layer-add", th, Kind::Primary, Size::Small, Some(Icon::Plus), "Add mod").on_click(cx.listener(move |this, _, _, cx| this.add_layer(id, cx)))),
                    )
                    .child(div().text_size(px(11.)).text_color(th.tx3).child(format!("A new build of a mod is a version of it: quiver mods {id} version add MOD DIR"))),
            )
    }

    /// A mod's versions, newest first: which is current, who pins which, and the buttons to
    /// make one current (for every profile that tracks the mod) or retire it.
    fn render_versions(&mut self, th: &Theme, id: Gid, m: &ModsState, rec: &ModRecord, cx: &mut Context<Self>) -> AnyElement {
        let users = m.users_of(&rec.id);
        let tracking: Vec<String> = users.iter().filter(|(_, r)| !r.pinned).map(|(p, _)| p.profile.name.clone()).collect();
        let rows: Vec<AnyElement> = rec
            .versions
            .iter()
            .rev()
            .enumerate()
            .map(|(j, v)| {
                let is_cur = v.id == rec.current;
                let pins: Vec<String> = users.iter().filter(|(_, r)| r.pinned && r.version == v.id).map(|(p, _)| p.profile.name.clone()).collect();
                let l = m.layer(&v.id);
                let key = |what: &str| SharedString::from(format!("ver-{what}-{}-{j}", rec.id));
                let (mid, vid) = (rec.id.clone(), v.id.clone());
                let (mid2, vid2) = (rec.id.clone(), v.id.clone());
                let mut sub = format!("{} · added {} by {}", v.id, day(v.added), actor_label(&v.added_by));
                if let Some(l) = l {
                    sub.push_str(&format!(" · {} files · {}", l.files, human_size(l.bytes)));
                    if l.manifest.as_ref().and_then(|x| x.save_compatible) != Some(true) {
                        sub.push_str(" · may change saves");
                    }
                }
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .bg(if is_cur { th.acc_soft } else { th.ink1 })
                    .when(v.retired, |d| d.opacity(0.6))
                    .child(icon(if is_cur { Icon::CircleCheck } else { Icon::CircleDot }, 13., if is_cur { th.acc } else { th.tx3 }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(div().font_weight(SEMIBOLD).child(v.label.clone()))
                                    .when(v.retired, |d| d.child(pill(th, PillKind::Muted, None, "Retired")))
                                    .when(v.prerelease, |d| d.child(pill(th, PillKind::Warn, None, "Pre-release")))
                                    .when(!pins.is_empty(), |d| d.child(pill(th, PillKind::Muted, Some(Icon::Pin), format!("Pinned by {}", pins.join(", "))))),
                            )
                            .child(mono(11., th.tx3).truncate().child(sub))
                            .when_some(v.notes.clone(), |d, n| d.child(div().text_size(px(11.5)).text_color(th.tx2).truncate().child(n))),
                    )
                    .when(is_cur, |d| d.child(pill(th, PillKind::Accent, None, "Current")))
                    .when(!is_cur && !v.retired, |d| {
                        d.child(small(key("use"), th, Some(Icon::CircleCheck), "Make current").on_click(cx.listener(move |this, _, _, cx| {
                            this.version_op(id, ModsRequest::VersionUse { game: id.0.into(), mod_id: mid.clone(), version: vid.clone(), message: None }, cx);
                        })))
                    })
                    .when(!is_cur, |d| {
                        let retired = v.retired;
                        d.child(
                            small(key("retire"), th, Some(if retired { Icon::ArchiveRestore } else { Icon::Minus }), if retired { "Bring back" } else { "Retire" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let req = ModsRequest::VersionRetire { game: id.0.into(), mod_id: mid2.clone(), version: vid2.clone(), retired: !retired, message: None };
                                    this.version_op(id, req, cx);
                                })),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .pl(px(16.))
            .child(div().text_size(px(11.5)).text_color(th.tx3).child(if tracking.is_empty() {
                "No profile tracks the current version. Making a version current updates every profile that does.".to_owned()
            } else {
                format!("Tracking the current version: {}. Making another version current updates them; pinned profiles keep theirs.", tracking.join(", "))
            }))
            .children(rows)
            .into_any_element()
    }
    fn render_profile_card(&mut self, th: &Theme, g: &GameRow, m: &ModsState, p: &ProfileInfo, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
        let stores = self.render_store_picker(th, g, m, p, cx);
        let pinned = switch("pinned", prof.pinned, self, cx);
        let del_prof = prof.clone();
        let play_i = self.gp.profiles().iter().position(|x| x.id == prof.id).map(|i| i + 1).unwrap_or(0);
        let is_default = self.default_profile(id).as_deref() == Some(prof.id.as_str());
        // A Steam game plays from Steam (its `steam_profile`): no Play here, no library tile.
        let steam = g.steam_game();
        let cli = if steam {
            format!("quiver set {id} steam_profile={}", super::page::sh_quote(&prof.name))
        } else {
            format!("quiver play {id} --profile {}", super::page::sh_quote(&prof.name))
        };
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
            .child(stores)
            .when(!steam, |c| c.child(row("Pin to library", Some("Its own tile and list row"), pinned)))
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
            .child(self.cli_line("prof-cli", th, cli, cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .when(!steam, |d| {
                        d.child(button("prof-play", th, Kind::Primary, Size::Small, Some(Icon::Play), "Play this profile").on_click(cx.listener(move |this, _, _, cx| this.play_with(id, play_i, cx))))
                    })
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
        let steam = g.steam_game();
        let cloud = self.gp.info.ready().map(|i| i.features.cloud_saves).unwrap_or(true);
        let is_default = self.default_profile(id).is_none();
        let store = self.gp.mods.ready().and_then(|m| m.vanilla_saves.clone());
        let saves = match (&store, cloud) {
            _ if steam => "The game's real saves (Steam Cloud syncs them)".to_owned(),
            (_, false) => "The game's own (no cloud saves)".to_owned(),
            (Some(s), _) if s.cloud => format!("{} store, synced with the cloud", s.store_name.as_deref().unwrap_or("Default")),
            (Some(s), _) => format!("{} store, on this PC only", s.store_name.as_deref().unwrap_or("Default")),
            (None, _) => "The game's own, synced with the cloud".to_owned(),
        };
        card(th)
            .p(px(16.))
            .gap(px(10.))
            .child(div().flex().items_center().gap(px(8.)).child(div().font_weight(SEMIBOLD).text_size(px(14.)).child("Vanilla")))
            .child(kv(th, "Runs", div().child("The base install, unmounted")))
            .child(kv(th, "Saves", div().child(saves)))
            .child(kv(th, "Settings", div().child(if steam { "Steam's launch, with this game's Quiver settings" } else { "This game's launch options" })))
            .child(self.cli_line("van-cli", th, if steam { format!("quiver set {id} steam_profile=vanilla") } else { format!("quiver play {id} --vanilla") }, cx))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .when(!steam, |d| {
                        d.child(button("van-play", th, Kind::Primary, Size::Small, Some(Icon::Play), "Play Vanilla").on_click(cx.listener(move |this, _, _, cx| this.play_with(id, 0, cx))))
                    })
                    .child(self.make_default_button(th, id, 0, is_default, cx)),
            )
    }

    /// "Make default" (profile entry `i`, 0: vanilla), or a "Default for Play" mark when it
    /// already is: the game's default profile, which Play and shortcuts run. A Steam game's:
    /// "Use in Steam" / "Steam runs this" (its `steam_profile`).
    fn make_default_button(&mut self, th: &Theme, id: Gid, i: usize, is_default: bool, cx: &mut Context<Self>) -> AnyElement {
        let steam = id.0.as_steam().is_some();
        if is_default {
            let mark = if steam { "Steam runs this" } else { "Default for Play" };
            return div().flex().items_center().child(pill(th, PillKind::Accent, Some(Icon::CircleCheck), mark)).into_any_element();
        }
        small(("make-default", i), th, Some(Icon::CircleCheck), if steam { "Use in Steam" } else { "Make default" })
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
fn describe_change(prev: Option<&quiver_api::Profile>, r: &ProfileRevision) -> String {
    let Some(p) = prev else { return "Created".into() };
    let n = &r.profile;
    let mut parts = vec![];
    if p.name != n.name {
        parts.push(format!("renamed to {}", n.name));
    }
    if p.layers != n.layers {
        // Entries of the same mod (`MOD`, `MOD@VERSION`) are a version change, not a swap.
        let mod_of = |e: &String| quiver_api::mods::split_entry(e).0.to_owned();
        let (pm, nm): (Vec<String>, Vec<String>) = (p.layers.iter().map(mod_of).collect(), n.layers.iter().map(mod_of).collect());
        let added = nm.iter().filter(|l| !pm.contains(l)).count();
        let removed = pm.iter().filter(|l| !nm.contains(l)).count();
        let versions = n.layers.iter().filter(|e| !p.layers.contains(e) && pm.contains(&mod_of(e))).count();
        parts.push(match (added, removed) {
            (0, 0) if versions > 0 => format!("{versions} mod version{} changed", if versions == 1 { "" } else { "s" }),
            (0, 0) => "mods reordered".into(),
            (a, 0) => format!("{a} mod{} added", if a == 1 { "" } else { "s" }),
            (0, r) => format!("{r} mod{} removed", if r == 1 { "" } else { "s" }),
            (a, r) => format!("{a} added, {r} removed"),
        });
    }
    if p.saves != n.saves {
        parts.push("save store changed".into());
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
