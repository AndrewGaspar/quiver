//! Settings: Accounts, Library and storage, Compatibility, Steam and shortcuts, Appearance,
//! Automation and CLI, About. A section list on the left (Zone::Side), the section on the
//! right; its buttons are the focusable body rows, in [`Quiver::settings_actions`] order.

use quiver_ui::depth::DepthExt;
use crate::model::Gid;
use std::collections::BTreeMap;

use gpui::{AnyElement, ClipboardItem, Context, Div, Hsla, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use quiver_api::exports::{ExportKind, ExportState};

use super::global::GlobalCmd;
use super::{
    BOLD, FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, art_fill, avatar, button, caption, card, chip, command_line,
    display, icon, mono, pill,
};
use crate::app::Quiver;
use crate::art::{ArtSlot, Variant};
use crate::model::{GameRow, ago, clean_name, initials, tilde};
use crate::nav::{Page, SettingsSection, Target, Zone};
use crate::theme::{Theme, rgba};

pub mod epic;

/// The repository, for About.
const SOURCE_URL: &str = "https://github.com/AndrewGaspar/quiver";

/// A focusable control in a Settings section.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsAction {
    Cmd(GlobalCmd),
    Copy(String),
    Url(String),
    /// The welcome flow again (`welcome.rs`).
    Welcome,
    /// Library and storage › Advanced: show or hide the compatibility folders.
    Advanced,
    /// Performance HUD: step a setting for every game (`ui/perf.rs`).
    Hud(crate::ui::perf::HudField),
    /// Tweaks: the global scope's focusable controls, in order (`ui/game/tweaks/`).
    Tweak(usize),
    /// Accounts › Epic Games (`settings/epic.rs`).
    Epic(epic::EpicAction),
    /// Display: "3D on stereo displays" (`stereo_ui.rs`).
    Stereo(crate::stereo_ui::Setting),
    /// Display: the 3D depth strength, percent.
    StereoDepth(u32),
}

/// Avatar gradients, by sign-in order.
pub fn account_colors(i: usize) -> (Hsla, Hsla) {
    const G: [(u32, u32); 4] = [(0x3b4a7a, 0x6a5aa8), (0x2f6b5e, 0x4a9a83), (0x7a5a3b, 0xa8845a), (0x6a3b5a, 0xa85a8a)];
    let (a, b) = G[i % G.len()];
    (rgba(a, 1.), rgba(b, 1.))
}

pub fn section_icon(s: SettingsSection) -> Icon {
    match s {
        SettingsSection::Accounts => Icon::Users,
        SettingsSection::Library => Icon::HardDrive,
        SettingsSection::Compatibility => Icon::Cpu,
        SettingsSection::Performance => Icon::Activity,
        SettingsSection::Tweaks => Icon::SlidersHorizontal,
        SettingsSection::Steam => Icon::Gamepad,
        SettingsSection::Appearance => Icon::Palette,
        SettingsSection::Display => Icon::Monitor,
        SettingsSection::Automation => Icon::Bot,
        SettingsSection::About => Icon::Info,
    }
}

/// The agent policy from PROPOSAL.md ("What agents may do"): action, CLI, agents. Game
/// changes are Allow for agents too (2026-11-18); only Steam restarts and accounts ask.
pub const POLICY: [(&str, &str, &str); 8] = [
    ("Read library and status", "Allow", "Allow"),
    ("Change launch options, mods, options, tweaks, exports", "Allow", "Allow"),
    ("Launch and stop games", "Allow", "Allow"),
    ("Install and update", "Allow", "Allow"),
    ("Restore saves or settings", "Allow", "Allow"),
    ("Uninstall, delete compatibility folders, profiles or saves", "Allow", "Allow"),
    ("Restart Steam", "Ask", "Ask"),
    ("Sign in, sign out, add or remove accounts", "Ask", "Deny"),
];

impl Quiver {
    /// The focusable controls of the open section, in focus order.
    pub fn settings_actions(&self) -> Vec<SettingsAction> {
        let mut v = vec![];
        match self.nav.settings {
            SettingsSection::Accounts => {
                v.push(SettingsAction::Cmd(GlobalCmd::AddAccount));
                if let Some(s) = &self.status {
                    for (i, a) in s.accounts.iter().enumerate() {
                        if !a.default {
                            v.push(SettingsAction::Cmd(GlobalCmd::MakeDefault(i)));
                        }
                        v.push(SettingsAction::Cmd(GlobalCmd::SignOut(i)));
                    }
                }
                v.extend(self.epic_actions());
            }
            SettingsSection::Library => v.extend([SettingsAction::Welcome, SettingsAction::Advanced]),
            SettingsSection::Compatibility => v.push(SettingsAction::Cmd(GlobalCmd::RescanProtons)),
            SettingsSection::Performance => {
                if self.hud_effective(&quiver_api::perf::HudScopeRef::global()).is_some() {
                    v.extend(crate::ui::perf::HUD_FIELDS.map(SettingsAction::Hud));
                }
            }
            SettingsSection::Tweaks => v.extend((0..self.tweaks_targets(None).len()).map(SettingsAction::Tweak)),
            SettingsSection::Steam => {
                if matches!(&self.ui.steam, Some(Ok(s)) if s.root.is_some()) {
                    for g in self.steam_copies() {
                        match super::steamgames::control(&g) {
                            Some(true) => {
                                if g.offer {
                                    v.push(SettingsAction::Cmd(GlobalCmd::SteamDismiss(g.app_id)));
                                }
                                v.push(SettingsAction::Cmd(GlobalCmd::SteamPatch(g.app_id)));
                            }
                            Some(false) => v.push(SettingsAction::Cmd(GlobalCmd::SteamUnpatch(g.app_id))),
                            None => {}
                        }
                    }
                    v.push(SettingsAction::Cmd(GlobalCmd::SteamOffers));
                }
                if let Some(Ok(s)) = &self.ui.steam {
                    if !s.running && !s.staged.is_empty() {
                        v.push(SettingsAction::Cmd(GlobalCmd::SteamApply));
                    }
                    if s.running && !s.staged.is_empty() {
                        v.push(SettingsAction::Cmd(GlobalCmd::SteamRestart));
                    }
                }
                if let Some(Ok(list)) = &self.ui.exports {
                    v.extend((0..list.len()).map(|i| SettingsAction::Cmd(GlobalCmd::RemoveExport(i))));
                }
            }
            SettingsSection::Appearance => {}
            SettingsSection::Display => {
                v.extend([crate::stereo_ui::Setting::Auto, crate::stereo_ui::Setting::Off].map(SettingsAction::Stereo));
                v.extend(crate::stereo_ui::STRENGTHS.map(SettingsAction::StereoDepth));
            }
            SettingsSection::Automation => {
                v.push(SettingsAction::Copy(quiver_api::socket_path().display().to_string()));
            }
            SettingsSection::About => {
                v.push(SettingsAction::Url(SOURCE_URL.into()));
                v.push(SettingsAction::Url("https://account.ubisoft.com".into()));
                v.push(SettingsAction::Url("https://store.ubisoft.com".into()));
            }
        }
        v
    }

    pub fn settings_activate(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(a) = self.settings_actions().get(i).cloned() else { return };
        self.run_settings_action(a, cx);
    }

    fn run_settings_action(&mut self, a: SettingsAction, cx: &mut Context<Self>) {
        match a {
            SettingsAction::Cmd(c) => self.run_global(c, cx),
            SettingsAction::Copy(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                self.show_toast(format!("Copied: {text}"), cx);
            }
            SettingsAction::Url(url) => {
                cx.open_url(&url);
                self.show_toast(format!("Opening {url}"), cx);
            }
            SettingsAction::Welcome => self.run_dl(crate::ui::downloads::DlCmd::Welcome, cx),
            SettingsAction::Advanced => self.ui.lib_advanced = !self.ui.lib_advanced,
            SettingsAction::Hud(f) => self.hud_cycle(quiver_api::perf::HudScopeRef::global(), f, cx),
            SettingsAction::Tweak(i) => self.tweaks_activate(None, i, cx),
            SettingsAction::Epic(a) => self.epic_action(a, cx),
            SettingsAction::Stereo(s) => self.set_stereo_setting(s, cx),
            SettingsAction::StereoDepth(p) => self.set_stereo_strength(p, cx),
        }
    }

    /// Whether this control has keyboard/controller focus.
    fn action_focused(&self, actions: &[SettingsAction], a: &SettingsAction) -> bool {
        self.nav.page == Page::Settings
            && self.nav.zone == Zone::Body
            && actions.iter().position(|x| x == a).is_some_and(|i| self.focused(Target::Body(i)))
    }

    /// A button that runs a settings action, with the focus ring when it has focus.
    fn action_button(
        &self,
        th: &Theme,
        actions: &[SettingsAction],
        a: SettingsAction,
        kind: Kind,
        icon_: Option<Icon>,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.action_focused(actions, &a);
        let id = actions.iter().position(|x| *x == a).unwrap_or(usize::MAX);
        button(("set-act", id), th, kind, Size::Small, icon_, label)
            .focus_ring(focused, th, 6.)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.focus(&this.layout, Target::Body(id));
                this.run_settings_action(a.clone(), cx);
                cx.notify();
            }))
            .into_any_element()
    }

    /// Width of the section's column (tables drop columns to fit it).
    fn settings_w(&self) -> f32 {
        let bp = self.bp();
        if bp.two_columns() { (self.viewport.0 - 250. - 72.).clamp(480., 920.) } else { self.viewport.0 - 2. * bp.pad() }
    }

    pub fn render_settings(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let section = self.nav.settings;
        let actions = self.settings_actions();
        let body = match section {
            SettingsSection::Accounts => {
                let mut v = self.settings_accounts(th, &actions, cx);
                v.extend(self.settings_epic(th, &actions, cx));
                v
            }
            SettingsSection::Library => {
                let mut v = self.settings_library(th, &actions, cx);
                let f = self.action_focused(&actions, &SettingsAction::Welcome);
                v.insert(1, self.settings_setup_group(th, f, cx));
                v
            }
            SettingsSection::Compatibility => self.settings_compat(th, &actions, cx),
            SettingsSection::Performance => {
                let focused: Vec<crate::ui::perf::HudField> =
                    crate::ui::perf::HUD_FIELDS.into_iter().filter(|f| self.action_focused(&actions, &SettingsAction::Hud(*f))).collect();
                self.settings_hud(th, &focused, cx)
            }
            SettingsSection::Tweaks => self.settings_tweaks(th, cx),
            SettingsSection::Steam => self.settings_steam(th, &actions, cx),
            SettingsSection::Appearance => self.settings_appearance(th, cx),
            SettingsSection::Display => self.settings_display(th, &actions, cx),
            SettingsSection::Automation => self.settings_automation(th, &actions, cx),
            SettingsSection::About => self.settings_about(th, &actions, cx),
        };
        // Below Medium the section list becomes a row of chips over the section.
        let bp = self.bp();
        let stacked = !bp.two_columns();
        let pad = if stacked { bp.pad() } else { 36. };
        let width = self.settings_w();
        let nav = self.render_settings_nav(th, stacked, cx);
        // Tweaks' unsaved bar floats over the bottom of the section, as on a game page.
        let bar = if section == SettingsSection::Tweaks { self.render_tweaks_bar(th, cx) } else { None };
        div()
            .flex()
            .when(stacked, |d| d.flex_col())
            .flex_1()
            .min_h_0()
            .relative()
            .children(bar)
            .child(nav)
            .child(
                div()
                    .id("settings-pane")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.body_scroll)
                    .px(px(pad))
                    .pt(px(if stacked { 20. } else { 28. }))
                    .pb(px(48.))
                    // A definite width, so wrapped text is measured at the width it is drawn at.
                    .child(div().flex().flex_col().gap(px(18.)).w(px(width)).children(body)),
            )
    }

    fn render_settings_nav(&mut self, th: &Theme, stacked: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let version = self.status.as_ref().map(|s| s.version.clone()).unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
        let rows: Vec<AnyElement> = SettingsSection::ALL
            .iter()
            .map(|&s| {
                let on = s == self.nav.settings;
                let focused = self.focused(Target::SettingsNav(s));
                let fg = if on { th.tx1 } else { th.tx2 };
                let (hover, hover_fg) = (th.ink3, th.tx1);
                let badge: Option<AnyElement> = match s {
                    SettingsSection::Accounts => self
                        .status
                        .as_ref()
                        .map(|st| st.accounts.len())
                        .filter(|n| *n > 1)
                        .map(|n| super::badge(th, n.to_string(), false).into_any_element()),
                    SettingsSection::Steam => match &self.ui.steam {
                        Some(Ok(st)) if !st.staged.is_empty() => Some(div().size(px(7.)).rounded_full().bg(th.warn).into_any_element()),
                        _ => None,
                    },
                    // Unsaved global tweaks (kept while elsewhere).
                    SettingsSection::Tweaks if self.tweaks_unsaved(&quiver_api::tweaks::Scope::Global) => {
                        Some(div().size(px(7.)).rounded_full().bg(th.warn).into_any_element())
                    }
                    _ => None,
                };
                div()
                    .id(("set-nav", s.index()))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(if stacked { 7. } else { 10. }))
                    .h(px(if stacked { 30. } else { 34. }))
                    .px(px(10.))
                    .rounded(px(7.))
                    .when(stacked, |d| d.border_1().border_color(if on { th.acc_line } else { th.line1 }))
                    .text_size(px(13.))
                    .text_color(fg)
                    .cursor_pointer()
                    .when(on, |d| d.bg(th.sel).font_weight(MEDIUM))
                    .when(!on, |d| d.hover(move |st| st.bg(hover).text_color(hover_fg)))
                    .focus_ring(focused, th, 7.)
                    .when(on && !stacked, |d| d.child(div().absolute().left(px(-8.)).top(px(8.)).bottom(px(8.)).w(px(3.)).rounded(px(2.)).bg(th.acc)))
                    .child(icon(section_icon(s), 15., if on { th.acc_hi } else { th.tx3 }))
                    .child(div().when(!stacked, |d| d.flex_1()).whitespace_nowrap().child(s.label()))
                    .when_some(badge, |d, b| d.child(b))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.nav.focus(&this.layout, Target::SettingsNav(s));
                        this.body_scroll.set_offset(gpui::point(px(0.), px(0.)));
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        if stacked {
            let pad = self.bp().pad();
            return div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(px(12.))
                .px(px(pad))
                .pt(px(18.))
                .pb(px(14.))
                .bg(th.ink0)
                .border_b_1()
                .border_color(th.line1)
                .depth_surface()
                .child(display(20., BOLD, th.tx1).child("Settings"))
                .child(div().flex().flex_wrap().gap(px(6.)).children(rows))
                .into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(250.))
            .h_full()
            .px(px(10.))
            .pt(px(22.))
            .pb(px(16.))
            .depth_surface()
            .bg(th.ink0)
            .border_r_1()
            .border_color(th.line1)
            .child(div().px(px(10.)).pb(px(14.)).child(display(20., BOLD, th.tx1).child("Settings")))
            .child(div().flex().flex_col().gap(px(2.)).pl(px(8.)).children(rows))
            .child(div().flex_1())
            .child(
                div()
                    .px(px(10.))
                    .pt(px(12.))
                    .text_size(px(11.))
                    .line_height(px(16.))
                    .text_color(th.tx3)
                    .child(format!(
                        "Quiver {version} · a third-party launcher for your Ubisoft library. Not affiliated with or endorsed by Ubisoft."
                    )),
            )
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Accounts

    fn settings_accounts(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let accounts = self.status.as_ref().map(|s| s.accounts.clone()).unwrap_or_default();
        let default_id = accounts.iter().find(|a| a.default).map(|a| a.user_id.clone());
        let mut out = vec![head_row(
            th,
            "Accounts",
            "Games from every signed-in account show up in one library.",
            Some(self.action_button(
                th,
                actions,
                SettingsAction::Cmd(GlobalCmd::AddAccount),
                if accounts.is_empty() { Kind::Primary } else { Kind::Secondary },
                Some(Icon::Plus),
                if accounts.is_empty() { "Sign in to Ubisoft" } else { "Add account" },
                cx,
            )),
        )];
        if accounts.is_empty() {
            out.push(
                card(th)
                    .p(px(18.))
                    .text_color(th.tx2)
                    .child(match self.status.as_ref().and_then(|s| s.signed_in.clone()) {
                        // A daemon from before multiple accounts reports only the name.
                        Some(name) => format!("Signed in as {name}. This quiverd doesn't list accounts; update it to manage several."),
                        None => "No Ubisoft account is signed in. Your other stores' games are in the library either way; sign in to add your Ubisoft games.".to_owned(),
                    })
                    .into_any_element(),
            );
        }
        for (i, a) in accounts.iter().enumerate() {
            let owned = self.games.iter().filter(|g| g.entry.owned_by.contains(&a.user_id)).count();
            let only = self.games.iter().filter(|g| g.entry.owned_by.len() == 1 && g.entry.owned_by[0] == a.user_id).count();
            let playing: Vec<String> =
                self.ui.sessions.iter().filter(|s| s.account == a.user_id).map(|s| self.game_name(Gid::from(s.product_id))).collect();
            let (from, to) = account_colors(i);
            let games = if accounts.len() > 1 && only > 0 {
                format!("{owned} games, {only} only on this account")
            } else {
                format!("{owned} game{}", if owned == 1 { "" } else { "s" })
            };
            let (dot, status) = if !playing.is_empty() {
                (th.ok, format!("Playing {} now · its session is live in quiverd", playing.join(", ")))
            } else if a.default {
                (th.ok, "Default · launches games every account owns, unless a game picks another".to_owned())
            } else {
                (th.tx4, "Signed in · connects when one of its games starts".to_owned())
            };
            let mut buttons = div().flex().flex_none().items_center().gap(px(6.));
            if !a.default {
                buttons = buttons.child(self.action_button(
                    th,
                    actions,
                    SettingsAction::Cmd(GlobalCmd::MakeDefault(i)),
                    Kind::Ghost,
                    Some(Icon::Star),
                    "Make default",
                    cx,
                ));
            }
            buttons = buttons.child(self.action_button(
                th,
                actions,
                SettingsAction::Cmd(GlobalCmd::SignOut(i)),
                Kind::Ghost,
                Some(Icon::LogOut),
                "Sign out",
                cx,
            ));
            out.push(
                card(th)
                    .flex_row()
                    .flex_wrap()
                    .items_start()
                    .gap(px(14.))
                    .p(px(16.))
                    .child(avatar(&initials(&a.name), from, to, th.ink2).size(px(40.)).text_size(px(14.)))
                    .child(
                        // Narrow: the buttons go under the account instead of squeezing it.
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(200.))
                            .gap(px(3.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().font_weight(SEMIBOLD).text_size(px(14.5)).child(a.name.clone()))
                                    .when(a.default, |d| d.child(pill(th, PillKind::Accent, None, "Default"))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(6.))
                                    .text_size(px(12.))
                                    .text_color(th.tx3)
                                    .child(mono(11.5, th.tx3).child(short_id(&a.user_id)))
                                    .child("·")
                                    .child(games),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.))
                                    .mt(px(4.))
                                    .text_size(px(12.))
                                    .child(div().size(px(7.)).rounded_full().bg(dot))
                                    .child(div().flex_1().min_w_0().text_color(th.tx2).child(status)),
                            ),
                    )
                    .child(buttons)
                    .into_any_element(),
            );
        }
        // No Ubisoft account: nothing to own or share yet.
        if accounts.is_empty() {
            return out;
        }
        out.push(
            // Block layout (icon placed absolutely) so the wrapped text is measured at its width.
            div()
                .relative()
                .pl(px(38.))
                .pr(px(14.))
                .py(px(12.))
                .rounded(px(10.))
                .bg(th.warn_soft)
                .border_1()
                .border_color(th.warn.opacity(0.25))
                .text_size(px(12.5))
                .line_height(px(18.))
                .child(div().absolute().left(px(14.)).top(px(14.)).child(icon(Icon::TriangleAlert, 14., th.warn)))
                .child(div().text_color(th.warn).child(
                    "Ubisoft allows one live session per account. Signing in to Ubisoft Connect elsewhere with the same account ends Quiver's session for it, and its games won't start here until you sign in again.",
                ))
                .into_any_element(),
        );
        out.push(self.owners_table(th, &accounts, default_id.as_deref(), cx));
        out
    }

    fn owners_table(&mut self, th: &Theme, accounts: &[quiver_api::Account], default_id: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let name_of = |id: &str| accounts.iter().find(|a| a.user_id == id).map(|a| a.name.clone()).unwrap_or_else(|| short_id(id));
        let default_name = default_id.map(name_of).unwrap_or_default();
        let mut games: Vec<GameRow> = self
            .games
            .iter()
            .filter(|g| g.entry.game.is_ubisoft())
            .filter(|g| g.entry.owned_by.len() > 1 || g.entry.owned_by.first().map(String::as_str) != default_id)
            .cloned()
            .collect();
        games.sort_by(|a, b| (b.entry.owned_by.len(), &a.entry.name).cmp(&(a.entry.owned_by.len(), &b.entry.name)));
        let shared = self.games.iter().filter(|g| g.entry.owned_by.len() > 1).count();
        let total = self.games.len();
        let sub = if accounts.len() > 1 {
            format!("Games owned by more than one account launch with the default account ({default_name}) unless a game overrides it")
        } else {
            format!("All {total} games belong to {default_name}")
        };
        // Narrower sections drop "Cloud saves from", then "Owned by".
        let w = self.settings_w();
        let (show_owned, show_cloud) = (w >= 640., w >= 820.);
        let launch_w = if w >= 520. { 240. } else { 150. };
        let mut cols = vec![("Game", None)];
        if show_owned {
            cols.push(("Owned by", Some(170.)));
        }
        cols.push(("Launch account", Some(launch_w)));
        if show_cloud {
            cols.push(("Cloud saves from", Some(150.)));
        }
        let (names, widths): (Vec<&str>, Vec<Option<f32>>) = cols.into_iter().unzip();
        let mut rows: Vec<AnyElement> = vec![table_head(th, &names, &widths)];
        for g in &games {
            let art = self.art_for(g, ArtSlot::Tile, Variant::Tile, cx);
            let owners: Vec<String> = g.entry.owned_by.iter().map(|o| name_of(o)).collect();
            let over = g.settings.as_ref().and_then(|s| s.account.clone());
            let launch_id = over.clone().or_else(|| {
                if g.entry.owned_by.iter().any(|o| Some(o.as_str()) == default_id) {
                    default_id.map(str::to_owned)
                } else {
                    g.entry.owned_by.first().cloned()
                }
            });
            let launch = match (&over, &launch_id) {
                (Some(_), Some(id)) => name_of(id),
                (None, Some(id)) if Some(id.as_str()) == default_id => format!("Default ({})", name_of(id)),
                (None, Some(id)) => format!("{} · only owner", name_of(id)),
                _ => "—".into(),
            };
            let id = g.id();
            rows.push(
                div()
                    .id(("own", id.ix()))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .h(px(46.))
                    .border_t_1()
                    .border_color(th.line1)
                    .cursor_pointer()
                    .hover(|s| s.bg(th.ink3))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_game(id, cx)))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap(px(10.))
                            .child(div().w(px(22.)).h(px(26.)).flex_none().child(art_fill(th, &art, "", 4., false)))
                            .child(div().truncate().child(clean_name(&g.entry.name))),
                    )
                    .when(show_owned, |d| d.child(div().w(px(170.)).flex_none().text_color(th.tx2).truncate().child(owners.join(", "))))
                    .child(
                        div()
                            .w(px(launch_w))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().min_w_0().truncate().text_color(if g.entry.owned_by.len() > 1 { th.tx1 } else { th.tx3 }).child(launch))
                            .when(over.is_some(), |d| d.child(pill(th, PillKind::Warn, None, "Override"))),
                    )
                    .when(show_cloud, |d| {
                        d.child(
                            div()
                                .w(px(150.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_color(th.tx2)
                                .child(icon(Icon::Cloud, 12., th.tx3))
                                .child(launch_id.as_deref().map(name_of).unwrap_or_default()),
                        )
                    })
                    .into_any_element(),
            );
        }
        if games.is_empty() {
            rows.push(
                div()
                    .px(px(16.))
                    .py(px(14.))
                    .border_t_1()
                    .border_color(th.line1)
                    .text_color(th.tx3)
                    .child(if accounts.len() > 1 {
                        "Every game is owned by the default account alone."
                    } else {
                        "Sign in another account to share its games here; each game shows who owns it."
                    })
                    .into_any_element(),
            );
        }
        card(th)
            .child(card_head(
                th,
                "Who owns what",
                &sub,
                Some(
                    div()
                        .text_size(px(12.))
                        .text_color(th.tx3)
                        .child(format!("{total} games · {shared} shared"))
                        .into_any_element(),
                ),
            ))
            .children(rows)
            .child(foot_cli(th, "quiver set <game> account=<name>", "per-game launch account"))
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Library and storage

    fn settings_library(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let installed: Vec<&quiver_api::Game> = self.games.iter().filter_map(|g| g.entry.installed.as_ref()).collect();
        let parents = |f: &dyn Fn(&quiver_api::Game) -> &std::path::Path| {
            let mut m: BTreeMap<String, usize> = BTreeMap::new();
            for g in &installed {
                if let Some(p) = f(g).parent() {
                    *m.entry(tilde(p)).or_default() += 1;
                }
            }
            m
        };
        let games_dirs = parents(&|g| g.install_dir.as_path());
        let prefix_dirs = parents(&|g| g.prefix.as_path());
        let data = quiver_api::client::daemon_log().parent().map(tilde).unwrap_or_default();
        let dir_rows = |m: &BTreeMap<String, usize>, what: &str| -> AnyElement {
            if m.is_empty() {
                return div().text_color(th.tx3).child(format!("No {what} yet")).into_any_element();
            }
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .children(m.iter().map(|(p, n)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(path_field(th, p))
                        .child(div().flex_none().text_size(px(12.)).text_color(th.tx3).child(format!("{n} game{}", if *n == 1 { "" } else { "s" })))
                }))
                .into_any_element()
        };
        let custom = self.custom_art.len();
        vec![
            head_row(th, "Library and storage", "Where games, Quiver's own state and art live.", None),
            group(
                th,
                "Library",
                "",
                vec![
                    frow(
                        th,
                        "Owned games",
                        None,
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(format!("{} games, {} installed", self.games.len(), installed.len()))
                            .when(self.status.as_ref().is_some_and(|s| s.accounts.len() > 1), |d| {
                                d.child(div().text_color(th.tx3).text_size(px(12.)).child("merged from every signed-in account"))
                            })
                            .into_any_element(),
                    ),
                    frow(th, "Game folders", Some("Each game installs into its own folder"), dir_rows(&games_dirs, "installs")),
                ],
            ),
            {
                // The per-game Wine prefixes: a detail most people never need.
                let open = self.ui.lib_advanced;
                let toggle = self.action_button(
                    th,
                    actions,
                    SettingsAction::Advanced,
                    Kind::Ghost,
                    Some(if open { Icon::ChevronDown } else { Icon::ChevronRight }),
                    if open { "Hide" } else { "Show" },
                    cx,
                );
                let mut c = card(th).child(card_head(
                    th,
                    "Advanced",
                    "Windows compatibility folders: each game's own small Windows setup",
                    Some(toggle),
                ));
                if open {
                    c = c.child(div().px(px(16.)).child(frow(
                        th,
                        "Compatibility folders",
                        Some("One per game (Proton prefixes), so games never affect each other"),
                        dir_rows(&prefix_dirs, "folders"),
                    )));
                }
                c.into_any_element()
            },
            group(
                th,
                "Quiver's files",
                "",
                vec![
                    frow(th, "State", Some("Accounts, settings history, activity log, save backups"), path_field(th, &data).into_any_element()),
                    frow(th, "Daemon log", None, path_field(th, &tilde(&quiver_api::client::daemon_log())).into_any_element()),
                    frow(th, "Art cache", Some("Ubisoft art, downloaded once"), path_field(th, &tilde(&crate::art::default_cache_dir())).into_any_element()),
                    frow(
                        th,
                        "Custom art",
                        Some("Your own images per game"),
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(path_field(th, &tilde(&crate::art::custom_art_path())))
                            .child(div().flex_none().text_size(px(12.)).text_color(th.tx3).child(format!("{custom} game{}", if custom == 1 { "" } else { "s" })))
                            .into_any_element(),
                    ),
                ],
            ),
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(pill(th, PillKind::Future, None, "Not wired yet"))
                .child(div().text_size(px(12.)).text_color(th.tx3).child("Moving installs between folders isn't wired yet. Free space and download rules are on the Downloads page."))
                .into_any_element(),
        ]
    }

    // -----------------------------------------------------------------------------------------
    // Compatibility

    fn settings_compat(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let default = self.status.as_ref().and_then(|s| s.proton_default.clone());
        // Games pinned to a build (as far as their settings are loaded).
        let mut pinned: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for g in &self.games {
            if let Some(p) = g.settings.as_ref().and_then(|s| s.proton.clone()) {
                pinned.entry(p).or_default().push(crate::model::short_name(&g.entry.name));
            }
        }
        let rescan = self.action_button(th, actions, SettingsAction::Cmd(GlobalCmd::RescanProtons), Kind::Secondary, Some(Icon::RefreshCw), "Rescan", cx);
        let mut rows: Vec<AnyElement> = vec![];
        match &self.ui.protons {
            None => rows.push(note_row(th, "Looking for Proton builds…")),
            Some(Err(e)) => rows.push(note_row(th, &format!("Could not list Proton builds: {e}"))),
            Some(Ok(list)) if list.is_empty() => {
                rows.push(note_row(th, "No Proton builds found in Steam's compatibilitytools.d. Install GE-Proton there, then Rescan."))
            }
            Some(Ok(list)) => {
                // Narrower sections drop "Found in" (always Steam's folder), then narrow "Pinned by".
                let w = self.settings_w();
                let show_found = w >= 720.;
                let pinned_w = if w >= 560. { 260. } else { 110. };
                if show_found {
                    rows.push(table_head(th, &["Build", "Found in", "Pinned by"], &[None, Some(220.), Some(pinned_w)]));
                } else {
                    rows.push(table_head(th, &["Build", "Pinned by"], &[None, Some(pinned_w)]));
                }
                // Newest first, like the daemon's pick.
                for b in list.iter().rev() {
                    let is_default = default.as_deref() == Some(b.as_str());
                    let by = pinned.get(b).map(|v| v.join(", ")).unwrap_or_default();
                    rows.push(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .px(px(16.))
                            .h(px(42.))
                            .border_t_1()
                            .border_color(th.line1)
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(mono(12.5, th.tx1).font_weight(SEMIBOLD).child(b.clone()))
                                    .when(is_default, |d| d.child(pill(th, PillKind::Accent, None, "Default"))),
                            )
                            .when(show_found, |d| d.child(div().w(px(220.)).flex_none().text_color(th.tx2).child("Steam compatibilitytools.d")))
                            .child(
                                div()
                                    .w(px(pinned_w))
                                    .flex_none()
                                    .truncate()
                                    .text_color(if by.is_empty() { th.tx4 } else { th.tx2 })
                                    .child(if by.is_empty() { "—".into() } else { by }),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
        let n_pinned: usize = pinned.values().map(Vec::len).sum();
        vec![
            head_row(th, "Compatibility", "Proton builds, and what games run with unless they pick their own.", None),
            group(
                th,
                "Defaults for games",
                "",
                vec![
                    frow(
                        th,
                        "Default build",
                        Some("Games without their own build use this"),
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .h(px(30.))
                                    .px(px(10.))
                                    .rounded(px(8.))
                                    .bg(th.ink1)
                                    .border_1()
                                    .border_color(th.line2)
                                    .child(icon(Icon::Cpu, 13., th.tx3))
                                    .child(mono(12., th.tx1).child(default.clone().unwrap_or_else(|| "None found".into()))),
                            )
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(if n_pinned > 0 {
                                format!("{n_pinned} game{} pick their own", if n_pinned == 1 { "" } else { "s" })
                            } else {
                                "quiverd uses the newest GE-Proton it finds".into()
                            }))
                            .into_any_element(),
                    ),
                    frow(
                        th,
                        "Per game",
                        Some("Launch options › Proton, revisioned like every setting"),
                        command_line(th, "quiver set <game> proton=GE-Proton10-26").max_w(px(420.)).into_any_element(),
                    ),
                ],
            ),
            card(th)
                .child(card_head(th, "Installed Proton builds", "", Some(rescan)))
                .children(rows)
                .child(foot_cli(th, "quiver protons", "the same list"))
                .into_any_element(),
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(pill(th, PillKind::Future, None, "Not wired yet"))
                .child(div().text_size(px(12.)).text_color(th.tx3).child("Downloading GE-Proton releases and compatibility folder snapshots before a build change."))
                .into_any_element(),
        ]
    }

    // -----------------------------------------------------------------------------------------
    // Steam and shortcuts

    fn settings_steam(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = vec![head_row(
            th,
            "Steam and shortcuts",
            "Games in Steam and your app launcher. Quiver never restarts Steam on its own.",
            None,
        )];
        // Steam itself.
        match self.ui.steam.clone() {
            None => out.push(card(th).p(px(16.)).text_color(th.tx3).child("Looking for Steam…").into_any_element()),
            Some(Err(e)) => out.push(card(th).p(px(16.)).text_color(th.bad).child(format!("Could not read Steam's users: {e}")).into_any_element()),
            Some(Ok(steam)) => {
                let status = match (&steam.root, steam.running) {
                    (None, _) => pill(th, PillKind::Muted, None, "Not found"),
                    (Some(_), true) => pill(th, PillKind::Warn, None, "Running"),
                    (Some(_), false) => pill(th, PillKind::Ok, None, "Closed"),
                };
                let mut rows: Vec<AnyElement> = vec![];
                rows.push(frow(
                    th,
                    "Steam",
                    None,
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(10.))
                        .child(status)
                        .child(match &steam.root {
                            Some(r) => mono(12., th.tx2).child(tilde(r)).into_any_element(),
                            None => div().text_color(th.tx3).child("No Steam install found; shortcuts can't be added").into_any_element(),
                        })
                        .into_any_element(),
                ));
                let users: Vec<AnyElement> = steam
                    .users
                    .iter()
                    .map(|u| {
                        let selected = steam.selected.as_deref() == Some(u.id.as_str());
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.))
                            .child(icon(Icon::User, 13., if selected { th.acc_hi } else { th.tx3 }))
                            .child(div().font_weight(if selected { SEMIBOLD } else { MEDIUM }).child(u.persona_name.clone().unwrap_or_else(|| u.id.clone())))
                            .when_some(u.account_name.clone(), |d, a| d.child(div().text_color(th.tx3).text_size(px(12.)).child(a)))
                            .child(mono(11., th.tx4).child(u.id.clone()))
                            .when(selected, |d| d.child(pill(th, PillKind::Accent, None, "Gets shortcuts")))
                            .when(u.most_recent, |d| d.child(pill(th, PillKind::Muted, None, "Last signed in")))
                            .into_any_element()
                    })
                    .collect();
                let users_help = match (steam.users.len(), &steam.selected) {
                    (0, _) => "No Steam users on this machine yet".to_owned(),
                    (1, _) => "The only Steam user, so Quiver doesn't ask".to_owned(),
                    (_, Some(_)) => "Chosen on the first export; quiver export steam --user changes it".to_owned(),
                    (n, None) => format!("{n} users: the first Steam export asks which one"),
                };
                rows.push(frow(
                    th,
                    "Steam user",
                    Some(&users_help),
                    div().flex().flex_col().gap(px(8.)).children(users).into_any_element(),
                ));
                out.push(group(th, "Steam", "", rows));
                if steam.root.is_some() {
                    out.push(self.settings_steam_copies(th, actions, cx));
                }

                if !steam.staged.is_empty() {
                    let n = steam.staged.len();
                    let mut banner = div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .px(px(16.))
                        .py(px(14.))
                        .rounded(px(12.))
                        .bg(th.warn_soft)
                        .border_1()
                        .border_color(th.warn.opacity(0.3))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .child(icon(Icon::ClockAlert, 16., th.warn))
                                .child(div().font_weight(SEMIBOLD).text_color(th.warn).child(format!(
                                    "{n} change{} waiting for Steam to restart",
                                    if n == 1 { "" } else { "s" }
                                ))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .pl(px(26.))
                                .children(steam.staged.iter().map(|s| mono(11.5, th.tx2).child(s.clone()))),
                        );
                    if steam.running {
                        banner = banner.child(
                            div()
                                .pl(px(26.))
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .child(div().text_size(px(12.5)).text_color(th.tx2).child(
                                    "Steam rewrites its settings files when it exits, so Quiver waits. Restart Steam when it suits you; Quiver asks before it does.",
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap(px(10.))
                                        .child(self.action_button(th, actions, SettingsAction::Cmd(GlobalCmd::SteamRestart), Kind::Secondary, Some(Icon::RefreshCw), "Restart Steam…", cx))
                                        .child(command_line(th, "quiver steam restart").w(px(260.)).flex_none()),
                                ),
                        );
                    } else {
                        banner = banner.child(
                            div()
                                .pl(px(26.))
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(10.))
                                .child(div().text_size(px(12.5)).text_color(th.tx2).child("Steam is closed, so they can be written now."))
                                .child(self.action_button(th, actions, SettingsAction::Cmd(GlobalCmd::SteamApply), Kind::Primary, None, "Write them now", cx)),
                        );
                    }
                    out.push(banner.into_any_element());
                }
            }
        }
        // What Quiver wrote.
        let mut rows: Vec<AnyElement> = vec![];
        match &self.ui.exports {
            None => rows.push(note_row(th, "Loading…")),
            Some(Err(e)) => rows.push(note_row(th, &format!("Could not list exports: {e}"))),
            Some(Ok(list)) if list.is_empty() => rows.push(note_row(
                th,
                "No shortcuts yet. Add a game to Steam or your app launcher from its page.",
            )),
            Some(Ok(list)) => {
                // Narrower sections drop "Where", then "Written"; Remove becomes an icon.
                let w = self.settings_w();
                let (show_where, show_written, wide_btn) = (w >= 860., w >= 700., w >= 560.);
                let state_w = if w >= 560. { 170. } else { 130. };
                let btn_w = if wide_btn { 86. } else { 32. };
                let mut cols: Vec<(&str, Option<f32>)> = vec![("Game", None)];
                if show_where {
                    cols.push(("Where", Some(250.)));
                }
                if show_written {
                    cols.push(("Written", Some(90.)));
                }
                cols.push(("State", Some(state_w)));
                cols.push(("", Some(btn_w)));
                let (names, widths): (Vec<&str>, Vec<Option<f32>>) = cols.into_iter().unzip();
                rows.push(table_head(th, &names, &widths));
                for (i, x) in list.clone().iter().enumerate() {
                    let r = &x.record;
                    let g = self.game(Gid(r.game)).cloned();
                    let art = match &g {
                        Some(g) => self.art_for(g, ArtSlot::Tile, Variant::Tile, cx),
                        None => crate::art::ArtState::Failed,
                    };
                    let (kind_icon, kind) = match r.kind {
                        ExportKind::Desktop => (Icon::Monitor, ".desktop"),
                        ExportKind::Steam => (Icon::Gamepad, "Steam"),
                    };
                    let state = match x.state {
                        ExportState::Ok => pill(th, PillKind::Ok, Some(Icon::CircleCheck), "As written"),
                        ExportState::WaitingForSteam => pill(th, PillKind::Warn, Some(Icon::Clock), "Waiting for Steam restart"),
                        ExportState::Missing => pill(th, PillKind::Bad, Some(Icon::CircleX), "Missing"),
                        ExportState::Changed => pill(th, PillKind::Warn, Some(Icon::TriangleAlert), "Changed outside Quiver"),
                    };
                    let place = match (r.kind, &r.steam_user) {
                        (ExportKind::Steam, Some(u)) => format!("Steam user {u}"),
                        _ => tilde(&r.path),
                    };
                    let id = Gid(r.game);
                    rows.push(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .px(px(16.))
                            .h(px(48.))
                            .border_t_1()
                            .border_color(th.line1)
                            .child(
                                div()
                                    .id(("exp", i))
                                    .flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .gap(px(10.))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| this.open_game(id, cx)))
                                    .child(div().w(px(22.)).h(px(26.)).flex_none().child(art_fill(th, &art, "", 4., false)))
                                    .child(div().truncate().child(crate::model::short_name(&r.name)))
                                    .child(chip(th, Some(kind_icon), kind, false).h(px(22.)).text_size(px(11.))),
                            )
                            .when(show_where, |d| d.child(mono(11.5, th.tx3).w(px(250.)).flex_none().truncate().child(place)))
                            .when(show_written, |d| d.child(div().w(px(90.)).flex_none().text_size(px(12.)).text_color(th.tx3).child(ago(r.written_at))))
                            .child(div().w(px(state_w)).flex_none().flex().overflow_hidden().child(state))
                            .child(
                                div().w(px(btn_w)).flex_none().flex().justify_end().child(self.action_button(
                                    th,
                                    actions,
                                    SettingsAction::Cmd(GlobalCmd::RemoveExport(i)),
                                    Kind::Ghost,
                                    Some(Icon::Trash),
                                    if wide_btn { "Remove" } else { "" },
                                    cx,
                                )),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
        out.push(
            card(th)
                .child(card_head(th, "Shortcuts Quiver made", "Each is checked against what Quiver wrote", None))
                .children(rows)
                .child(foot_cli(th, "quiver export list", "the same list, with paths"))
                .into_any_element(),
        );
        out
    }

    /// Ubisoft games installed through Steam: start each through Quiver, or undo it.
    fn settings_steam_copies(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = vec![];
        match &self.ui.sg.list {
            None => rows.push(note_row(th, "Looking for Ubisoft games in Steam…")),
            Some(Err(e)) => rows.push(note_row(th, &format!("Could not look for Ubisoft games in Steam: {e}"))),
            Some(Ok(list)) if list.games.is_empty() => rows.push(note_row(th, "No Ubisoft games are installed through Steam.")),
            Some(Ok(list)) => {
                for g in list.games.clone() {
                    let busy = self.ui.sg.busy.contains(&g.app_id);
                    let lib = g.product_id.and_then(|p| self.game(Gid::from(p))).cloned();
                    let art = match &lib {
                        Some(l) => self.art_for(l, ArtSlot::Tile, Variant::Tile, cx),
                        None => self.art_for_steam_game(&g, ArtSlot::Tile, Variant::Tile, cx),
                    };
                    let mut buttons = div().flex().flex_none().items_center().gap(px(6.));
                    match super::steamgames::control(&g) {
                        Some(true) => {
                            if g.offer {
                                buttons = buttons.child(self.action_button(th, actions, SettingsAction::Cmd(GlobalCmd::SteamDismiss(g.app_id)), Kind::Ghost, None, "Not now", cx));
                            }
                            buttons = buttons.child(self.action_button(th, actions, SettingsAction::Cmd(GlobalCmd::SteamPatch(g.app_id)), Kind::Primary, Some(Icon::Check), "Use Quiver", cx));
                        }
                        Some(false) => {
                            buttons = buttons.child(self.action_button(th, actions, SettingsAction::Cmd(GlobalCmd::SteamUnpatch(g.app_id)), Kind::Ghost, Some(Icon::Undo), "Undo", cx));
                        }
                        None => {}
                    }
                    let id = g.product_id;
                    rows.push(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_x(px(12.))
                            .gap_y(px(6.))
                            .px(px(16.))
                            .py(px(9.))
                            .border_t_1()
                            .border_color(th.line1)
                            .child(div().w(px(22.)).h(px(26.)).flex_none().child(art_fill(th, &art, "", 4., lib.is_none())))
                            .child(
                                div()
                                    .id(("sg-row", g.app_id as usize))
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w(px(160.))
                                    .gap(px(1.))
                                    .when(lib.is_some(), |d| d.cursor_pointer())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(id) = id.filter(|id| this.game(Gid::from(*id)).is_some()) {
                                            this.open_game(Gid::from(id), cx);
                                        }
                                    }))
                                    .child(div().flex().items_center().gap(px(8.)).child(div().truncate().child(clean_name(&g.name))).when(g.offer, |d| d.child(pill(th, PillKind::Accent, None, "New"))))
                                    .child(div().text_size(px(12.)).text_color(th.tx3).truncate().child(super::steamgames::state_line(&g))),
                            )
                            .when(busy || g.state != quiver_api::exports::PatchState::Off || !g.patchable, |d| {
                                d.child(div().flex_none().child(super::steamgames::state_pill(th, &g, busy)))
                            })
                            .child(buttons)
                            .into_any_element(),
                    );
                }
            }
        }
        let offers = self.steam_offers_enabled();
        let f = self.action_focused(actions, &SettingsAction::Cmd(GlobalCmd::SteamOffers));
        rows.insert(
            0,
            div()
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(th.line1)
                .text_size(px(12.5))
                .line_height(px(18.))
                .text_color(th.tx2)
                .child(super::steamgames::EXPLAIN)
                .into_any_element(),
        );
        rows.push(div().px(px(16.)).child(frow(
            th,
            "Ask about new installs",
            Some("When a Ubisoft game is installed through Steam, Quiver offers this once"),
            div()
                .id("sg-offers")
                .flex()
                .items_center()
                .gap(px(10.))
                .cursor_pointer()
                .child(div().rounded(px(9.)).child(super::game::toggle(th, offers)).focus_ring(f, th, 9.))
                .child(div().text_color(th.tx2).child(if offers { "On" } else { "Off" }))
                .on_click(cx.listener(|this, _, _, cx| {
                    let on = !this.steam_offers_enabled();
                    this.set_steam_offers(on, cx);
                }))
                .into_any_element(),
        )).into_any_element());
        card(th)
            .child(card_head(th, "Ubisoft games in Steam", "Steam copies can start through Quiver instead of Ubisoft Connect", None))
            .children(rows)
            .child(foot_cli(th, "quiver steam games", "the same list; quiver steam patch / unpatch <game>"))
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Appearance

    /// Display: 3D on stereo displays, and its depth (`stereo_ui.rs`, design/STEREO-UI.md).
    fn settings_display(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::stereo_ui::{STRENGTHS, Setting};
        let setting = self.stereo.prefs.stereo_ui;
        let percent = self.stereo.prefs.stereo_depth;
        // A segmented control whose segments are focusable settings actions.
        let seg = |this: &Self, items: Vec<(SettingsAction, String, bool)>, cx: &mut Context<Self>| {
            div().flex().flex_none().p(px(2.)).gap(px(2.)).rounded(px(8.)).bg(th.ink1).border_1().border_color(th.line2).children(
                items.into_iter().map(|(a, label, on)| {
                    let focused = this.action_focused(actions, &a);
                    let id = actions.iter().position(|x| *x == a).unwrap_or(usize::MAX);
                    div()
                        .id(("display-seg", id))
                        .px(px(12.))
                        .py(px(5.))
                        .rounded(px(6.))
                        .text_size(px(12.5))
                        .text_color(if on { th.tx1 } else { th.tx2 })
                        .when(on, |d| d.bg(th.ink5).font_weight(MEDIUM))
                        .when(!on, |d| d.hover(|s| s.bg(th.ink3)))
                        .cursor_pointer()
                        .focus_ring(focused, th, 6.)
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.nav.focus(&this.layout, Target::Body(id));
                            this.run_settings_action(a.clone(), cx);
                            cx.notify();
                        }))
                        .into_any_element()
                }),
            )
        };
        let mode = seg(
            self,
            [Setting::Auto, Setting::Off].into_iter().map(|s| (SettingsAction::Stereo(s), s.label().to_owned(), s == setting)).collect(),
            cx,
        );
        let strength = seg(
            self,
            STRENGTHS.into_iter().map(|p| (SettingsAction::StereoDepth(p), format!("{p}%"), p == percent)).collect(),
            cx,
        )
        .when(setting == Setting::Off, |d| d.opacity(0.55));
        let packed = self.stereo.packed;
        let status = div()
            .flex()
            .items_start()
            .gap(px(8.))
            .py(px(10.))
            .border_t_1()
            .border_color(th.line1)
            .text_size(px(12.))
            .text_color(if packed { th.tx1 } else { th.tx3 })
            .child(icon(Icon::Monitor, 14., if packed { th.acc_hi } else { th.tx3 }))
            .child(div().flex_1().min_w_0().child(self.stereo_status()));
        vec![
            head_row(th, "Display", "How Quiver draws itself on the display it's on.", None),
            group(
                th,
                "3D on stereo displays",
                "XReal glasses in 3D mode, a HypXRland XR monitor",
                vec![
                    frow(
                        th,
                        "3D",
                        Some("Auto draws each eye itself whenever the window is on a side-by-side stereo display: art at the back, cards a step up, controls a step more, focus and menus nearest. Flat everywhere else."),
                        div().flex().child(mode).into_any_element(),
                    ),
                    frow(
                        th,
                        "Depth",
                        Some("How far things float in front of the window. The same steps as Sight's depth in games."),
                        div().flex().child(strength).into_any_element(),
                    ),
                    status.into_any_element(),
                ],
            ),
        ]
    }

    fn settings_appearance(&mut self, th: &Theme, _cx: &mut Context<Self>) -> Vec<AnyElement> {
        let builtin = th.name.as_ref() == "Quiver dark";
        let hyprpad = self.hyprpad_running();
        let source = if std::env::var("QUIVER_THEME").is_ok_and(|v| v == "builtin") {
            "QUIVER_THEME=builtin".to_owned()
        } else {
            tilde(&crate::theme::omarchy_state_dir().join("theme/colors.toml"))
        };
        let seg = |items: &[(&str, bool)]| {
            div().flex().flex_none().p(px(2.)).gap(px(2.)).rounded(px(8.)).bg(th.ink2).border_1().border_color(th.line2).children(
                items.iter().map(|(l, on)| {
                    div()
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(px(6.))
                        .text_size(px(12.))
                        .text_color(if *on { th.tx1 } else { th.tx3 })
                        .when(*on, |d| d.bg(th.ink5))
                        .child(l.to_string())
                }),
            )
        };
        let swatches = div().flex().flex_none().rounded(px(6.)).overflow_hidden().border_1().border_color(th.line2).children(
            [th.ink0, th.ink1, th.ink3, th.ink5, th.acc, th.tx1, th.ok, th.warn, th.bad].into_iter().map(|c| div().w(px(18.)).h(px(22.)).bg(c)),
        );
        let markers = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .children(["tag:quiver-game", "QUIVER_* env", "systemd scope", "sessions socket"].map(|t| {
                div()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded_full()
                    .bg(th.ink3)
                    .border_1()
                    .border_color(th.line2)
                    .child(mono(11., th.tx2).child(t))
            }))
            .child(pill(th, PillKind::Future, None, "Not wired yet"));
        vec![
            head_row(th, "Appearance", "Colors follow your Omarchy theme, live. Controller hints and hyprpad.", None),
            group(
                th,
                "Theme",
                "",
                vec![
                    frow(
                        th,
                        "Colors",
                        None,
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.))
                            .child(seg(&[("Follow Omarchy", !builtin), ("Quiver dark", builtin)]))
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(if builtin {
                                "No Omarchy theme found, so the built-in palette is used"
                            } else {
                                "Restyles the moment you switch themes"
                            }))
                            .into_any_element(),
                    ),
                    frow(
                        th,
                        "Current theme",
                        Some("Re-read whenever theme.generation changes"),
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.))
                            .child(swatches)
                            .child(div().font_weight(SEMIBOLD).child(th.name.clone()))
                            .child(pill(th, PillKind::Muted, None, if th.dark { "dark" } else { "light" }))
                            .into_any_element(),
                    ),
                    frow(th, "Source", None, path_field(th, &source).into_any_element()),
                ],
            ),
            self.theme_preview(th),
            group(
                th,
                "Controller",
                "",
                vec![
                    frow(
                        th,
                        "Gamepad",
                        Some("Read directly; never grabbed exclusively"),
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.))
                            .child(match &self.pad {
                                Some(_) => pill(th, PillKind::Ok, Some(Icon::Gamepad), "Connected").into_any_element(),
                                None => pill(th, PillKind::Muted, None, "None connected").into_any_element(),
                            })
                            .when_some(self.pad.clone(), |d, n| d.child(div().text_color(th.tx2).child(n)))
                            .into_any_element(),
                    ),
                    frow(
                        th,
                        "hyprpad",
                        Some("Forwards the pad to Quiver while its window has focus"),
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.))
                            .child(if hyprpad {
                                pill(th, PillKind::Ok, None, "Running")
                            } else {
                                pill(th, PillKind::Muted, None, "Not running")
                            })
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(if hyprpad {
                                "Its desktop bindings aren't needed; Quiver reads the pad itself"
                            } else {
                                "Quiver reads the pad itself either way"
                            }))
                            .into_any_element(),
                    ),
                    frow(
                        th,
                        "Button hints",
                        None,
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.))
                            .child(seg(&[("When I use a gamepad", true), ("Always", false), ("Never", false)]))
                            .child(div().text_size(px(12.)).text_color(th.tx3).child("Mouse or keyboard input hides them again"))
                            .into_any_element(),
                    ),
                    frow(th, "Guide button", None, div().text_color(th.tx2).child("Not used by Quiver. hyprpad owns guide-button chords.").into_any_element()),
                    frow(th, "Mark launched games", Some("One generic hyprpad rule for every game Quiver starts"), markers.into_any_element()),
                ],
            ),
        ]
    }

    /// A small sample of the components in the current theme.
    fn theme_preview(&self, th: &Theme) -> AnyElement {
        let actor = |c: Hsla, i: Icon, l: &str| {
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .px(px(6.))
                .py(px(2.))
                .rounded(px(6.))
                .bg(c.opacity(0.13))
                .text_size(px(11.5))
                .font_weight(SEMIBOLD)
                .text_color(c)
                .child(icon(i, 12., c))
                .child(l.to_owned())
        };
        let ink = |c: Hsla, l: &str| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().w(px(46.)).h(px(30.)).rounded(px(6.)).bg(c).border_1().border_color(th.line2))
                .child(mono(10., th.tx3).child(l.to_owned()))
        };
        // The ink swatches go under the sample below 640 px.
        let stack = self.settings_w() < 640.;
        card(th)
            .child(card_head(th, "Preview", "How the current theme draws Quiver's parts", None))
            .child(
                div()
                    .flex()
                    .when(stack, |d| d.flex_col())
                    .gap(px(24.))
                    .p(px(16.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .flex_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(button("pv-play", th, Kind::Primary, Size::Medium, Some(Icon::Play), "Play"))
                                    .child(button("pv-sec", th, Kind::Secondary, Size::Medium, None, "Game details"))
                                    .child(
                                        div()
                                            .relative()
                                            .child(button("pv-focus", th, Kind::Secondary, Size::Medium, Some(Icon::Gamepad), "Focused"))
                                            .focus_ring(true, th, 8.),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(chip(th, None, "Installed", true))
                                    .child(chip(th, None, "All accounts", false))
                                    .child(pill(th, PillKind::Ok, None, "In sync"))
                                    .child(pill(th, PillKind::Warn, None, "Update"))
                                    .child(pill(th, PillKind::Bad, None, "Failed"))
                                    .child(pill(th, PillKind::Future, None, "Not yet")),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(6.))
                                    .child(actor(th.who_gui, Icon::MousePointer, "You · app"))
                                    .child(actor(th.who_cli, Icon::SquareTerminal, "You · CLI"))
                                    .child(actor(th.who_agent, Icon::Bot, "claude-code"))
                                    .child(actor(th.who_sys, Icon::Cog, "System")),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(12.))
                                    .child(div().text_color(th.tx1).child("Primary"))
                                    .child(div().text_color(th.tx2).child("Secondary"))
                                    .child(div().text_color(th.tx3).child("Muted"))
                                    .child(div().text_color(th.tx4).child("Disabled"))
                                    .child(div().text_color(th.acc_hi).child("Accent")),
                            )
                            .child(super::progress(th, 0.64, Some(280.), 6., None)),
                    )
                    .child(
                        // Two explicit rows: a wrapping flex box is measured unwrapped here.
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(ink(th.ink0, "ink-0"))
                                    .child(ink(th.ink1, "ink-1"))
                                    .child(ink(th.ink2, "ink-2"))
                                    .child(ink(th.ink3, "ink-3")),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(ink(th.ink4, "ink-4"))
                                    .child(ink(th.ink5, "ink-5"))
                                    .child(ink(th.sel, "sel"))
                                    .child(ink(th.acc, "acc")),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Whether hyprpad is running (a /proc scan, at most every 10 s).
    pub fn hyprpad_running(&mut self) -> bool {
        if let Some((v, at)) = self.ui.hyprpad {
            if at.elapsed().as_secs() < 10 {
                return v;
            }
        }
        let running = std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
            .any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim().starts_with("hyprpad")));
        self.ui.hyprpad = Some((running, std::time::Instant::now()));
        running
    }

    // -----------------------------------------------------------------------------------------
    // Automation and CLI

    fn settings_automation(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let socket = quiver_api::socket_path().display().to_string();
        let socket_short = socket.replace(&std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "\u{0}".into()), "$XDG_RUNTIME_DIR");
        let version = self.status.as_ref().map(|s| s.version.clone()).unwrap_or_default();
        let broker = self.status.as_ref().map(|s| s.broker.clone()).filter(|b| !b.is_empty());
        let up = self.daemon_up() && self.source.is_some();
        let (dot, state) = if self.demo {
            (th.acc, "Demo data · no daemon".to_owned())
        } else if up {
            (th.ok, "quiverd is running".to_owned())
        } else {
            (th.warn, "quiverd is not reachable".to_owned())
        };
        let kv = |k: &str, v: AnyElement| {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.5))
                .child(div().w(px(170.)).flex_none().text_color(th.tx2).child(k.to_owned()))
                .child(div().flex_1().min_w(px(160.)).child(v))
        };
        let copy = self.action_button(th, actions, SettingsAction::Copy(socket.clone()), Kind::Ghost, Some(Icon::Copy), "Copy", cx);
        let daemon = card(th)
            .gap(px(10.))
            .p(px(16.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .child(div().size(px(8.)).rounded_full().bg(dot))
                    .child(div().font_weight(SEMIBOLD).child(state))
                    .child(mono(11.5, th.tx3).child(format!("v{version}"))),
            )
            .child(kv("Socket", div().flex().flex_wrap().items_center().gap(px(8.)).child(mono(12., th.tx2).child(socket_short)).child(copy).into_any_element()))
            .child(kv("This window's source", mono(12., th.who_gui).child("gui").into_any_element()))
            .when_some(broker, |d, b| d.child(kv("Ticket broker for bridges", mono(12., th.tx2).child(b).into_any_element())))
            .child(kv(
                "Protocol",
                div().text_color(th.tx3).child("JSON lines; every request carries a source, kept in Activity").into_any_element(),
            ));
        let cell_w = if self.settings_w() >= 560. { 150. } else { 80. };
        let policy_cell = |v: &str| {
            let (fg, bg) = match v {
                "Ask" => (th.warn, th.warn_soft),
                "Deny" => (th.bad, th.bad_soft),
                _ => (th.ok, th.ok_soft),
            };
            div()
                .w(px(cell_w))
                .flex_none()
                .child(div().flex_none().w(px(70.)).py(px(2.)).rounded(px(6.)).bg(bg).text_color(fg).text_size(px(12.)).font_weight(SEMIBOLD).flex().justify_center().child(v.to_owned()))
        };
        let mut policy: Vec<AnyElement> = vec![table_head(th, &["Action", if cell_w < 150. { "CLI" } else { "quiver CLI (you)" }, "Agents"], &[None, Some(cell_w), Some(cell_w)])];
        for (a, c, g) in POLICY {
            policy.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .h(px(40.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(div().flex_1().min_w_0().child(a))
                    .child(policy_cell(c))
                    .child(policy_cell(g))
                    .into_any_element(),
            );
        }
        let cli = [
            ("quiver status", "accounts, running games, jobs"),
            ("quiver library --json", "the library, for scripts"),
            ("quiver play origins", "what Play does"),
            ("quiver --source my-agent set origins proton=GE-Proton10-26", "an agent's change, labelled in Activity"),
            ("quiver activity", "this log"),
            ("quiver watch", "live daemon events as JSON"),
        ];
        vec![
            head_row(th, "Automation and CLI", "The app, the quiver command and agents all drive the same daemon.", None),
            daemon.into_any_element(),
            card(th)
                .child(card_head(
                    th,
                    "What each client may do",
                    "The app asks you directly",
                    Some(pill(th, PillKind::Future, None, "Display only for now").into_any_element()),
                ))
                .children(policy)
                .child(
                    div()
                        .px(px(16.))
                        .py(px(10.))
                        .border_t_1()
                        .border_color(th.line1)
                        .text_size(px(12.))
                        .text_color(th.tx3)
                        .child("Agents are any client whose --source isn't gui or cli. Restoring saves already waits for a person's OK; tokens and approvals come next."),
                )
                .into_any_element(),
            card(th)
                .child(card_head(th, "The same things from a terminal", "Every button in the app is one of these", None))
                .child(
                    div().flex().flex_col().gap(px(8.)).p(px(16.)).children(cli.map(|(c, what)| {
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(12.))
                            .child(command_line(th, c).w(px(470.)).max_w(gpui::relative(1.)))
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(what))
                    })),
                )
                .into_any_element(),
        ]
    }

    // -----------------------------------------------------------------------------------------
    // About

    fn settings_about(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let version = self.status.as_ref().map(|s| s.version.clone()).unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
        let link = |this: &Self, url: &str, i: Icon, label: &str, cx: &mut Context<Self>| {
            this.action_button(th, actions, SettingsAction::Url(url.into()), Kind::Secondary, Some(i), label.to_owned(), cx)
        };
        let credits = [
            ("Protobuf definitions", "from YoobieRE/ubisoft-demux-node, refreshed from the client's descriptors"),
            ("Text input", "adapted from GPUI's input example (Apache-2.0, Zed Industries)"),
            ("Fonts", "Geist, Geist Mono and Archivo (SIL OFL 1.1)"),
            ("Icons", "Lucide (ISC)"),
            ("Game art", "Ubisoft's, from its asset CDN; trademarks belong to their owners"),
        ];
        vec![
            card(th)
                .flex_row()
                .items_center()
                .gap(px(18.))
                .p(px(22.))
                .child(brand_mark(th, 44.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap(px(10.))
                                .child(display(26., gpui::FontWeight::EXTRA_BOLD, th.tx1).child("Quiver"))
                                .child(mono(13., th.tx3).child(format!("v{version}"))),
                        )
                        .child(div().text_color(th.tx2).child("A third-party launcher for your Ubisoft library on Linux."))
                        .child(div().text_color(th.tx3).text_size(px(12.5)).child(
                            "Not affiliated with or endorsed by Ubisoft. You need your own Ubisoft account, and Quiver only runs games you own.",
                        )),
                )
                .into_any_element(),
            group(
                th,
                "Links",
                "",
                vec![div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(link(self, SOURCE_URL, Icon::ExternalLink, "Source code", cx))
                    .child(link(self, "https://account.ubisoft.com", Icon::ShieldCheck, "Ubisoft account and security", cx))
                    .child(link(self, "https://store.ubisoft.com", Icon::Globe, "Ubisoft Store", cx))
                    .into_any_element()],
            ),
            group(
                th,
                "License and credits",
                "",
                std::iter::once(frow(th, "License", None, div().child("MIT").into_any_element()))
                    .chain(credits.into_iter().map(|(k, v)| frow(th, k, None, div().text_color(th.tx2).child(v).into_any_element())))
                    .collect(),
            ),
        ]
    }
}

// ---------------------------------------------------------------------------------------------
// Building blocks

fn short_id(id: &str) -> String {
    if id.len() > 13 { format!("{}…", &id[..8]) } else { id.to_owned() }
}

/// Section title, subtitle and an optional control on the right.
fn head_row(th: &Theme, title: &str, sub: &str, right: Option<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .items_end()
        .gap(px(16.))
        .pb(px(2.))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(220.))
                .gap(px(3.))
                .child(div().text_size(px(20.)).font_weight(BOLD).child(title.to_owned()))
                .child(div().text_color(th.tx3).child(sub.to_owned())),
        )
        .when_some(right, |d, r| d.child(r))
        .into_any_element()
}

/// A card of setting rows with a title (the mock-up's `section()`).
fn group(th: &Theme, title: &str, sub: &str, rows: Vec<AnyElement>) -> AnyElement {
    card(th)
        .px(px(16.))
        .pt(px(4.))
        .pb(px(6.))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_x(px(8.))
                .gap_y(px(2.))
                .pt(px(10.))
                .pb(px(8.))
                .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child(title.to_owned()))
                .when(!sub.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(th.tx3).child(sub.to_owned()))),
        )
        .children(rows)
        .into_any_element()
}

/// One setting: label (and help) on the left, the value or control on the right.
fn frow(th: &Theme, label: &str, help: Option<&str>, control: AnyElement) -> AnyElement {
    // The control goes under the label when it would get narrower than 220 px.
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(16.))
        .gap_y(px(8.))
        .py(px(10.))
        .border_t_1()
        .border_color(th.line1)
        .child(
            div()
                .w(px(200.))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().font_weight(MEDIUM).text_size(px(12.5)).child(label.to_owned()))
                .when_some(help, |d, h| d.child(div().text_size(px(11.5)).line_height(px(15.)).text_color(th.tx3).child(h.to_owned()))),
        )
        .child(div().flex_1().min_w(px(220.)).flex().flex_col().child(control))
        .into_any_element()
}

fn card_head(th: &Theme, title: &str, sub: &str, right: Option<AnyElement>) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(10.))
        .gap_y(px(4.))
        .px(px(16.))
        .py(px(12.))
        .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child(title.to_owned()))
        .when(!sub.is_empty(), |d| d.child(div().min_w_0().text_size(px(12.)).text_color(th.tx3).truncate().child(sub.to_owned())))
        .child(div().flex_1())
        .when_some(right, |d, r| d.child(r))
}

/// Uppercase column headers; `widths[i]` is a fixed width, `None` takes the rest.
fn table_head(th: &Theme, cols: &[&str], widths: &[Option<f32>]) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(16.))
        .h(px(32.))
        .border_t_1()
        .border_color(th.line1)
        .children(cols.iter().zip(widths).map(|(c, w)| {
            let d = div().child(caption(c, th.tx3));
            match w {
                Some(w) => d.w(px(*w)).flex_none(),
                None => d.flex_1(),
            }
        }))
        .into_any_element()
}

fn note_row(th: &Theme, text: &str) -> AnyElement {
    div().px(px(16.)).py(px(14.)).border_t_1().border_color(th.line1).text_color(th.tx3).child(text.to_owned()).into_any_element()
}

/// The CLI line at the foot of a card.
fn foot_cli(th: &Theme, cmd: &str, what: &str) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(10.))
        .px(px(16.))
        .py(px(10.))
        .border_t_1()
        .border_color(th.line1)
        .child(command_line(th, cmd.to_owned()).flex_1().min_w(px(200.)).max_w(px(360.)))
        .child(div().text_size(px(12.)).text_color(th.tx3).child(what.to_owned()))
        .into_any_element()
}

/// A read-only path in a field-like box.
fn path_field(th: &Theme, path: &str) -> Div {
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .h(px(30.))
        .px(px(10.))
        .rounded(px(8.))
        .bg(th.ink1)
        .border_1()
        .border_color(th.line2)
        .child(mono(12., th.tx1).flex_1().min_w_0().truncate().child(path.to_owned()))
}

/// The Quiver mark (three stacked squares), at `size`.
pub fn brand_mark(th: &Theme, size: f32) -> Div {
    let s = size / 24.;
    div()
        .relative()
        .size(px(size))
        .flex_none()
        .child(div().absolute().left(px(0.)).top(px(8. * s)).size(px(16. * s)).rounded(px(5. * s)).bg(th.mark1))
        .child(div().absolute().left(px(4. * s)).top(px(4. * s)).size(px(16. * s)).rounded(px(5. * s)).bg(th.mark2))
        .child(div().absolute().left(px(8. * s)).top(px(0.)).size(px(16. * s)).rounded(px(5. * s)).bg(gpui::linear_gradient(
            135.,
            gpui::linear_color_stop(th.acc_hi, 0.),
            gpui::linear_color_stop(th.acc, 1.),
        )))
}
