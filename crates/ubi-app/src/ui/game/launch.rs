//! Launch options: an editable form over the current revision (Proton, launch account,
//! wrappers, environment, arguments, DLL overrides), the equivalent `quiver set`, and the
//! revision history with per-revision diffs and restore.

use gpui::{AnyElement, Context, Entity, IntoElement, MouseDownEvent, ParentElement, SharedString, Styled, div, prelude::*, px};
use ubi_api::{GameSettings, Revision, SettingOp};

use super::page::{DiffLine, Dialog, Dropdown, KvRow, checkbox, diff, field_box, form_row, section, set_command, sh_quote, small, split_wrappers};
use crate::app::{InputMode, Quiver};
use crate::model::{Actor, GameRow, actor, actor_label, ago, tilde};
use crate::nav::Target;
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, badge, button, card, card_header, icon, icon_button, mono, pill, soft_shadow};

/// Wrappers offered as one-click toggles.
const COMMON_WRAPPERS: &[(&str, &str)] = &[("gamemoderun", "GameMode"), ("mangohud", "MangoHud"), ("gamescope", "gamescope")];

impl Quiver {
    pub(super) fn render_launch(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if !g.installed() {
            return card(th)
                .p(px(24.))
                .text_color(th.tx3)
                .child("Launch options belong to an installed game. Install it, then set its Proton build, wrappers and environment here.")
                .into_any_element();
        }
        if g.settings.is_none() {
            return card(th).p(px(24.)).text_color(th.tx3).child("Loading launch options…").into_any_element();
        }
        self.ensure_launch_form(id, cx);
        let staged = self.launch_staged(cx).unwrap_or_default();
        let ops = self.launch_ops(cx);
        let changed: Vec<String> = ops
            .iter()
            .map(|o| match o {
                SettingOp::Set { key, .. } | SettingOp::Unset { key } => key.clone(),
            })
            .collect();
        let is_changed = |prefix: &str| changed.iter().any(|k| k == prefix || k.starts_with(&format!("{prefix}.")));

        let form = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(px(14.))
            .pb(px(if ops.is_empty() { 0. } else { 70. }))
            .child(self.render_runtime_section(th, g, &staged, is_changed("proton"), is_changed("account"), cx))
            .child(self.render_wrappers_section(th, g, &staged, is_changed("wrappers"), cx))
            .child(self.render_kv_section(th, false, is_changed("env"), cx))
            .child(self.render_game_section(th, g, is_changed("args"), cx))
            .child(self.render_kv_section(th, true, is_changed("dll"), cx))
            .child(self.render_resolved(th, g, &staged, &ops, cx));

        div()
            .flex()
            .gap(px(20.))
            .items_start()
            .child(form)
            .child(self.render_history(th, g, cx))
            .into_any_element()
    }

    /// A dropdown button and, when open, its options under it.
    fn dropdown(&mut self, th: &Theme, dd: Dropdown, label: String, detail: String, width: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let open = self.gp.dropdown == Some(dd);
        let opts = self.dropdown_options(dd);
        let current = match dd {
            Dropdown::Proton => self.gp.launch.as_ref().and_then(|f| f.proton.clone()),
            Dropdown::Account => self.gp.launch.as_ref().and_then(|f| f.account.clone()),
        };
        let pad = self.mode != InputMode::Pointer;
        let sel = self.gp.dropdown_sel;
        let btn = div()
            .id(("dd", dd as usize))
            .flex()
            .items_center()
            .gap(px(8.))
            .w(px(width))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(8.))
            .bg(th.ink1)
            .border_1()
            .border_color(if open { th.acc } else { th.line2 })
            .cursor_pointer()
            .hover(|s| s.border_color(th.acc_line))
            .child(icon(if dd == Dropdown::Proton { Icon::Cpu } else { Icon::User }, 14., th.tx2))
            .child(div().font_weight(MEDIUM).truncate().child(label))
            .child(div().flex_1())
            .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(detail))
            .child(icon(Icon::ChevronDown, 14., th.tx2))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.gp.dropdown == Some(dd) {
                    this.gp.dropdown = None;
                } else {
                    let cur = match dd {
                        Dropdown::Proton => this.gp.launch.as_ref().and_then(|f| f.proton.clone()),
                        Dropdown::Account => this.gp.launch.as_ref().and_then(|f| f.account.clone()),
                    };
                    this.gp.dropdown_sel = this.dropdown_options(dd).iter().position(|o| o.0 == cur).unwrap_or(0);
                    this.gp.dropdown = Some(dd);
                }
                cx.notify();
            }));
        let list = open.then(|| {
            let items: Vec<AnyElement> = opts
                .into_iter()
                .enumerate()
                .map(|(i, (value, label, detail))| {
                    let on = value == current;
                    let shown = if label.is_empty() { value.clone().unwrap_or_default() } else { label };
                    let v = value.clone();
                    div()
                        .id(("dd-opt", i))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(7.))
                        .cursor_pointer()
                        .when(pad && i == sel, |d| d.bg(th.acc_soft))
                        .hover(|s| s.bg(th.ink3))
                        .child(div().w(px(14.)).child(if on { icon(Icon::Check, 14., th.acc_hi).into_any_element() } else { div().into_any_element() }))
                        .child(div().font_weight(if on { SEMIBOLD } else { MEDIUM }).child(shown))
                        .child(div().flex_1())
                        .child(div().text_size(px(11.5)).text_color(th.tx3).child(detail))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dropdown_pick(dd, v.clone());
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect();
            gpui::deferred(
                gpui::anchored().snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id(("dd-list", dd as usize))
                        .occlude()
                        .mt(px(4.))
                        .w(px(width.max(300.)))
                        .p(px(4.))
                        .flex()
                        .flex_col()
                        .rounded(px(10.))
                        .bg(th.ink2)
                        .border_1()
                        .border_color(th.line2)
                        .shadow(vec![soft_shadow(crate::theme::rgba(0x000000, 0.55), 14., 40.)])
                        .text_size(px(12.5))
                        .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.gp.dropdown = None;
                            cx.notify();
                        }))
                        .children(items),
                ),
            )
            .with_priority(1)
        });
        div().flex().flex_col().child(btn).children(list)
    }

    fn render_runtime_section(
        &mut self,
        th: &Theme,
        g: &GameRow,
        staged: &GameSettings,
        proton_changed: bool,
        account_changed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let def = self.status.as_ref().and_then(|s| s.proton_default.clone()).unwrap_or_else(|| "newest installed".into());
        let (plabel, pdetail) = match &staged.proton {
            Some(p) => (p.clone(), "this game".to_string()),
            None => (def.clone(), "global default".to_string()),
        };
        let proton = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(self.dropdown(th, Dropdown::Proton, plabel, pdetail, 280., cx))
            .when(staged.proton.is_some(), |d| d.child(pill(th, PillKind::Accent, Some(Icon::Pin), "Pinned")))
            .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("global default is {def}")));
        let owners = g.entry.owned_by.len();
        let (alabel, adetail) = match &staged.account {
            Some(a) => (self.account_name(a), "this game".to_string()),
            None => {
                let (n, _) = self.launch_account(g.id());
                (format!("Default ({n})"), "default".to_string())
            }
        };
        let account = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(self.dropdown(th, Dropdown::Account, alabel, adetail, 280., cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(12.))
                    .text_color(th.tx3)
                    .child(icon(Icon::Cloud, 12., th.tx3))
                    .child(if owners > 1 { format!("{owners} accounts own it; this one's cloud saves are used") } else { "Also decides whose cloud saves are used".into() }),
            );
        section(th, "Runtime", "Proton build and the account that launches the game", None)
            .child(form_row(th, "Compatibility tool", Some("Changing it upgrades the prefix; older builds may not run it again"), proton_changed, proton))
            .child(form_row(th, "Launch account", Some("Who plays, reports play time and syncs saves"), account_changed, account))
    }

    fn render_wrappers_section(&mut self, th: &Theme, g: &GameRow, staged: &GameSettings, changed: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let exe = g.entry.installed.as_ref().map(|i| i.exe.rsplit('/').next().unwrap_or(&i.exe).to_owned()).unwrap_or_default();
        let chain: Vec<String> = staged.wrappers.iter().cloned().chain(["proton".to_string(), exe]).collect();
        let n = chain.len();
        let chain_el = div().flex().flex_wrap().items_center().gap(px(6.)).pb(px(10.)).children(chain.into_iter().enumerate().map(|(i, w)| {
            let last = i + 1 == n;
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(pill(th, if last { PillKind::Accent } else { PillKind::Muted }, None, w).font_family(crate::assets::MONO_FONT).font_weight(gpui::FontWeight::NORMAL))
                .when(!last, |d| d.child(icon(Icon::ChevronRight, 12., th.tx3)))
        }));
        let input = self.gp.launch.as_ref().map(|f| f.wrappers.clone());
        let toggles = div().flex().gap(px(6.)).children(COMMON_WRAPPERS.iter().map(|(cmd, label)| {
            let on = staged.wrappers.iter().any(|w| w == cmd);
            let cmd = cmd.to_string();
            div()
                .id(SharedString::from(format!("wrap-{cmd}")))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(28.))
                .px(px(10.))
                .rounded(px(8.))
                .bg(if on { th.acc_soft } else { th.ink3 })
                .border_1()
                .border_color(if on { th.acc_line } else { th.line2 })
                .text_size(px(12.))
                .text_color(if on { th.acc_hi } else { th.tx2 })
                .cursor_pointer()
                .child(checkbox(th, on).size(px(14.)))
                .child(label.to_string())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = &this.gp.launch {
                        let mut list = split_wrappers(f.wrappers.read(cx).text());
                        if let Some(i) = list.iter().position(|w| *w == cmd) {
                            list.remove(i);
                        } else {
                            list.push(cmd.clone());
                        }
                        let text = list.join(", ");
                        f.wrappers.clone().update(cx, |t, cx| t.set_text(text, cx));
                    }
                    cx.notify();
                }))
        }));
        section(th, "Wrappers", "Commands the game runs inside, outermost first", None)
            .child(chain_el)
            .child(form_row(
                th,
                "Wrapper commands",
                Some("Comma or space separated"),
                changed,
                div().flex().flex_col().gap(px(8.)).child(div().w_full().children(input)).child(toggles),
            ))
    }

    fn kv_row_el(&self, th: &Theme, dll: bool, i: usize, row: &KvRow, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(220.)).flex_none().child(row.key.clone()))
            .child(div().text_color(th.tx3).child("="))
            .child(div().flex_1().min_w_0().child(row.value.clone()))
            .child(
                icon_button(SharedString::from(format!("kv-rm-{dll}-{i}")), th, Icon::X, false)
                    .size(px(28.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = &mut this.gp.launch {
                            let rows = if dll { &mut f.dll } else { &mut f.env };
                            if i < rows.len() {
                                rows.remove(i);
                            }
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_kv_section(&mut self, th: &Theme, dll: bool, changed: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let rows: Vec<(Entity<TextInput>, Entity<TextInput>)> = self
            .gp
            .launch
            .as_ref()
            .map(|f| (if dll { &f.dll } else { &f.env }).iter().map(|r| (r.key.clone(), r.value.clone())).collect())
            .unwrap_or_default();
        let n = rows.iter().filter(|(k, _)| !k.read(cx).text().trim().is_empty()).count();
        let (title, sub, add, empty) = if dll {
            (
                "DLL overrides",
                if n == 0 { "WINEDLLOVERRIDES: native or built-in per DLL".to_string() } else { format!("{n} override{}", if n == 1 { "" } else { "s" }) },
                "Add override",
                "No overrides. A ReShade layer usually needs dxgi = n,b.",
            )
        } else {
            (
                "Environment",
                if n == 0 { "Extra variables for the game's process".to_string() } else { format!("{n} variable{}", if n == 1 { "" } else { "s" }) },
                "Add variable",
                "No extra variables. Examples: DXVK_HUD=fps, PROTON_ENABLE_WAYLAND=1.",
            )
        };
        let add_btn = small(if dll { "add-dll" } else { "add-env" }, th, Some(Icon::Plus), add)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.launch_add_row(dll, cx);
                cx.notify();
            }))
            .into_any_element();
        let mut s = section(th, title, sub, Some(add_btn)).when(changed, |d| {
            d.child(div().flex().items_center().gap(px(6.)).pb(px(6.)).text_size(px(11.5)).text_color(th.warn).child(div().size(px(6.)).rounded_full().bg(th.warn)).child("Changed"))
        });
        let mut list = div().flex().flex_col().gap(px(6.)).pt(px(8.)).pb(px(6.)).border_t_1().border_color(th.line1);
        if rows.is_empty() {
            list = list.child(div().py(px(4.)).text_size(px(12.)).text_color(th.tx3).child(empty));
        }
        let form_rows: Vec<AnyElement> = self
            .gp
            .launch
            .as_ref()
            .map(|f| {
                let v = if dll { &f.dll } else { &f.env };
                v.iter().enumerate().map(|(i, r)| self.kv_row_el(th, dll, i, r, cx)).collect()
            })
            .unwrap_or_default();
        list = list.children(form_rows);
        s = s.child(list);
        s
    }

    fn render_game_section(&mut self, th: &Theme, g: &GameRow, changed: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let _ = cx;
        let game = g.entry.installed.clone();
        let exe = game.as_ref().map(|i| i.exe.clone()).unwrap_or_default();
        let own = game.as_ref().and_then(|i| i.args.clone()).unwrap_or_default();
        let args = self.gp.launch.as_ref().map(|f| f.args.clone());
        section(th, "Game", "From the Ubisoft product config, plus your additions", None)
            .child(form_row(th, "Executable", None, false, field_box(th, exe, false).font_family(crate::assets::MONO_FONT).max_w(px(420.))))
            .child(form_row(
                th,
                "Arguments",
                Some("Ubisoft's own arguments are locked; yours go after them"),
                changed,
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .when(!own.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap(px(5.))
                                .h(px(28.))
                                .px(px(9.))
                                .rounded(px(8.))
                                .bg(th.ink3)
                                .border_1()
                                .border_color(th.line2)
                                .text_size(px(12.))
                                .text_color(th.tx2)
                                .font_family(crate::assets::MONO_FONT)
                                .child(icon(Icon::Lock, 12., th.tx3))
                                .child(own),
                        )
                    })
                    .child(div().flex_1().min_w_0().children(args)),
            ))
    }

    fn render_resolved(&mut self, th: &Theme, g: &GameRow, staged: &GameSettings, ops: &[SettingOp], cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let message = self.gp.launch.as_ref().map(|f| f.message.read(cx).text().to_owned());
        let set = if ops.is_empty() { format!("quiver set {id} proton=GE-Proton10-26 env.DXVK_HUD=fps -m \"why\"") } else { set_command(id, ops, message.as_deref()) };
        let game = g.entry.installed.clone();
        let def = self.status.as_ref().and_then(|s| s.proton_default.clone()).unwrap_or_else(|| "<newest>".into());
        let proton = staged.proton.clone().unwrap_or(def);
        let mut env: Vec<String> = staged.env.iter().map(|(k, v)| format!("{k}={}", sh_quote(v))).collect();
        if !staged.dll_overrides.is_empty() {
            let o: Vec<String> = staged.dll_overrides.iter().map(|(k, v)| format!("{k}={v}")).collect();
            env.push(format!("WINEDLLOVERRIDES={}", sh_quote(&o.join(";"))));
        }
        if let Some(gm) = &game {
            env.push(format!("WINEPREFIX={}", tilde(&gm.prefix)));
        }
        env.push(format!("UPLAY_ARGUMENTS=\"-upc_uplay_id {id} …\""));
        let mut cmd: Vec<String> = staged.wrappers.clone();
        cmd.push(format!("~/.local/share/Steam/compatibilitytools.d/{proton}/proton run"));
        if let Some(gm) = &game {
            cmd.push(gm.exe.clone());
            if let Some(a) = &gm.args {
                cmd.push(a.clone());
            }
        }
        cmd.extend(staged.args.iter().cloned());
        let line = |s: String, color: gpui::Hsla| mono(11.5, color).child(s);
        let set_copy = set.clone();
        card(th)
            .p(px(14.))
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().font_weight(SEMIBOLD).child("Resolved launch"))
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(if ops.is_empty() { "what quiverd runs" } else { "with your unsaved changes" }))
                    .child(div().flex_1())
                    .child(small("copy-set", th, Some(Icon::SquareTerminal), "Copy as quiver command").on_click(cx.listener(move |this, _, _, cx| this.copy_text(set_copy.clone(), "", cx)))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(th.ink0)
                    .border_1()
                    .border_color(th.line1)
                    .child(line(env.join(" "), th.who_cli))
                    .child(line(cmd.join(" "), th.tx2)),
            )
            .child(self.cli_line("launch-cli", th, set, cx))
    }

    /// The floating bar with unsaved changes (drawn over the pane's bottom edge).
    pub(super) fn render_pending(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = g.id();
        self.gp.launch.as_ref().filter(|f| f.game == id)?;
        let ops = self.launch_ops(cx);
        if ops.is_empty() {
            return None;
        }
        let keys: Vec<String> = ops
            .iter()
            .map(|o| match o {
                SettingOp::Set { key, .. } | SettingOp::Unset { key } => key.clone(),
            })
            .collect();
        let next = g.current_revision().unwrap_or(0) + 1;
        let msg = self.gp.launch.as_ref().map(|f| f.message.clone())?;
        let busy = self.gp.busy;
        Some(
            div()
                .absolute()
                .left(px(24.))
                .right(px(424.))
                .bottom(px(16.))
                .flex()
                .items_center()
                .gap(px(10.))
                .pl(px(16.))
                .pr(px(12.))
                .py(px(10.))
                .rounded(px(12.))
                .bg(th.ink3)
                .border_1()
                .border_color(th.line2)
                .shadow(vec![soft_shadow(crate::theme::rgba(0x000000, 0.55), 14., 40.)])
                .child(div().size(px(7.)).rounded_full().bg(th.warn))
                .child(div().flex_none().font_weight(SEMIBOLD).child(format!("{} unsaved change{}", ops.len(), if ops.len() == 1 { "" } else { "s" })))
                .child(mono(11.5, th.tx3).max_w(px(180.)).truncate().child(keys.join(", ")))
                .child(div().flex_1().min_w(px(160.)).child(msg))
                .child(
                    button("launch-discard", th, Kind::Ghost, Size::Small, None, "Discard").on_click(cx.listener(|this, _, _, cx| {
                        this.launch_discard();
                        cx.notify();
                    })),
                )
                .child(
                    button("launch-save", th, Kind::Primary, Size::Small, Some(Icon::Check), if busy { "Saving…".to_string() } else { format!("Save as r{next}") })
                        .on_click(cx.listener(move |this, _, _, cx| this.launch_save(id, cx))),
                )
                .into_any_element(),
        )
    }

    fn render_history(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let mut history: Vec<Revision> = g.history.clone().unwrap_or_default();
        history.sort_by(|a, b| b.revision.cmp(&a.revision));
        let latest = history.first().map(|r| r.revision);
        let sel = self.gp.rev_sel;
        let items: Vec<AnyElement> = history
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let is_cur = Some(r.revision) == latest;
                let open = sel == Some(r.revision);
                let focused = self.focused(Target::Body(i));
                let who = actor(&r.source);
                let who_color = match who {
                    Actor::App => th.who_gui,
                    Actor::Cli => th.who_cli,
                    Actor::Agent => th.who_agent,
                    Actor::System => th.who_sys,
                };
                let who_icon = match who {
                    Actor::App => Icon::MousePointer,
                    Actor::Cli => Icon::SquareTerminal,
                    Actor::Agent => Icon::Bot,
                    Actor::System => Icon::Cog,
                };
                let last = i + 1 == history.len();
                let prev = history.get(i + 1).map(|p| p.settings.clone()).unwrap_or_default();
                let changes = diff(&prev, &r.settings);
                let rev = r.revision;
                let mut entry = div()
                    .id(("rev", i))
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .mb(px(12.))
                    .px(px(10.))
                    .py(px(if open { 10. } else { 4. }))
                    .rounded(px(10.))
                    .cursor_pointer()
                    .when(open, |d| d.bg(th.ink3).border_1().border_color(th.acc_line))
                    .when(!open, |d| d.hover(|s| s.bg(th.ink3)))
                    .focus_ring(focused, th, 10.)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.gp.rev_sel = if this.gp.rev_sel == Some(rev) { None } else { Some(rev) };
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(mono(12., if is_cur { th.acc_hi } else { th.tx1 }).font_weight(SEMIBOLD).child(format!("r{}", r.revision)))
                            .when(is_cur, |d| d.child(pill(th, PillKind::Accent, None, "Current")))
                            .child(div().flex_1())
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(ago(r.at))),
                    )
                    .child(div().text_size(px(12.5)).child(r.message.clone().unwrap_or_else(|| format!("{} change{}", changes.len(), if changes.len() == 1 { "" } else { "s" }))))
                    .child(
                        div().flex().items_center().gap(px(6.)).child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(5.))
                                .px(px(6.))
                                .py(px(2.))
                                .rounded(px(6.))
                                .bg(who_color.opacity(0.13))
                                .text_size(px(11.5))
                                .font_weight(SEMIBOLD)
                                .text_color(who_color)
                                .child(icon(who_icon, 12., who_color))
                                .child(actor_label(&r.source)),
                        ),
                    );
                if open {
                    let prev_label = history.get(i + 1).map(|p| format!("r{}", p.revision)).unwrap_or_else(|| "defaults".into());
                    let lines: Vec<AnyElement> = if changes.is_empty() {
                        vec![div().px(px(10.)).py(px(4.)).text_color(th.tx3).child("No changes").into_any_element()]
                    } else {
                        changes
                            .iter()
                            .map(|l| {
                                let (sign, k, v, col, bg) = match l {
                                    DiffLine::Removed(k, v) => ("-", k, v, th.bad, th.bad_soft),
                                    DiffLine::Added(k, v) => ("+", k, v, th.ok, th.ok_soft),
                                };
                                div().px(px(10.)).py(px(2.)).bg(bg).text_color(col).child(format!("{sign} {k} = {v:?}")).into_any_element()
                            })
                            .collect()
                    };
                    entry = entry.child(
                        div()
                            .mt(px(8.))
                            .flex()
                            .flex_col()
                            .rounded(px(8.))
                            .overflow_hidden()
                            .border_1()
                            .border_color(th.line1)
                            .bg(th.ink0)
                            .font_family(crate::assets::MONO_FONT)
                            .text_size(px(11.5))
                            .child(div().px(px(10.)).py(px(5.)).text_color(th.tx3).border_b_1().border_color(th.line1).child(format!("{prev_label} → r{rev}")))
                            .children(lines),
                    );
                    let restore_cmd = format!("quiver restore {id} {rev}");
                    entry = entry.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .mt(px(8.))
                            .when(!is_cur, |d| {
                                d.child(button(("rev-restore", i), th, Kind::Primary, Size::Small, Some(Icon::RotateCcw), format!("Restore r{rev}")).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.open_dialog(Dialog::RestoreSettings { game: id, revision: rev });
                                        cx.notify();
                                    },
                                )))
                            })
                            .when(is_cur, |d| d.child(div().text_size(px(11.5)).text_color(th.tx3).child("This is what the next launch uses.")))
                            .child(div().flex_1())
                            .child(
                                icon_button(("rev-copy", i), th, Icon::Copy, true)
                                    .size(px(26.))
                                    .on_click(cx.listener(move |this, _, _, cx| this.copy_text(restore_cmd.clone(), "", cx))),
                            ),
                    );
                }
                div()
                    .relative()
                    .flex()
                    .gap(px(8.))
                    .when(!last, |d| d.child(div().absolute().left(px(10.)).top(px(24.)).bottom(px(0.)).w(px(1.)).bg(th.line2)))
                    .child(
                        div().w(px(22.)).flex_none().pt(px(4.)).child(
                            div()
                                .size(px(21.))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(if is_cur { th.acc } else { th.ink3 })
                                .border_1()
                                .border_color(if is_cur { th.acc } else { th.line2 })
                                .child(icon(if is_cur { Icon::Check } else if r.message.as_deref().is_some_and(|m| m.starts_with("Restore")) { Icon::RotateCcw } else { Icon::History }, 11., if is_cur { th.acc_ink } else { th.tx2 })),
                        ),
                    )
                    .child(entry)
                    .into_any_element()
            })
            .collect();
        card(th)
            .w(px(380.))
            .flex_none()
            .child(
                card_header(th, "History")
                    .child(badge(th, format!("{} revision{}", history.len(), if history.len() == 1 { "" } else { "s" }), false))
                    .child(div().flex_1())
                    .child(div().text_size(px(11.5)).text_color(th.tx3).child("click one to compare")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .p(px(14.))
                    .pb(px(4.))
                    .when(items.is_empty(), |d| d.child(div().pb(px(10.)).text_color(th.tx3).child("No revisions yet: the first save makes r1.")))
                    .children(items),
            )
    }
}
