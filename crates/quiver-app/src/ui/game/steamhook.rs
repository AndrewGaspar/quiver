//! A Steam game's hook on its page (design/STORE-STEAM.md §9, steer 3b): whether Steam starts
//! it through `quiver steam-run`, so its tweaks, HUD and launch settings reach it, with the
//! switch; and the offer when the person changes a tweak or the HUD for a game that isn't
//! hooked. Opt-in per game: nothing turns the hook on unasked.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::Request;
use quiver_api::exports::{PatchState, SteamHook};
use quiver_api::perf::{HudScope, HudScopeRef};

use super::options::form::banner;
use super::options::logic::Tone;
use super::page::Load;
use crate::app::Quiver;
use crate::err_text;
use crate::model::{GameRow, Gid};
use crate::theme::Theme;
use crate::ui::{Icon, Kind, SEMIBOLD, Size, button, card, card_header, icon, mono};

impl Quiver {
    /// The hook of the Steam game on the page, from its detail (`GameDetail.steam_hook`).
    pub(in crate::ui) fn steam_hook(&self, id: Gid) -> Option<&SteamHook> {
        self.gp.store.ready().filter(|d| self.gp.id == id && d.game == id.0).and_then(|d| d.steam_hook.as_ref())
    }

    /// The game's HUD at launch: (the Sight layer is in its launches, the HUD shown at
    /// launch), from its own scope or the global one; anti-cheat in its folder keeps the
    /// layer out.
    fn steam_hud(&mut self, id: Gid, cx: &mut Context<Self>) -> Option<(bool, bool)> {
        let scope = HudScopeRef::of(id.0);
        if !self.perf.hud.contains_key(&scope) {
            self.load_hud(scope.clone(), cx);
        }
        let blocked = self.hud_anti_cheat_block(&scope).is_some();
        self.hud_effective(&scope).map(|c| (c.enabled && !blocked, c.on))
    }

    /// Turn the hook on or off (staged by quiverd while Steam runs).
    pub(in crate::ui) fn set_steam_hook(&mut self, id: Gid, on: bool, cx: &mut Context<Self>) {
        let Some(app_id) = id.0.as_steam() else { return };
        self.call::<SteamHook, _>(Request::SteamHook { app_id, on, steam_user: None }, cx, move |this, r, cx| match r {
            Ok(h) => {
                let msg = match (on, h.staged, h.unchanged) {
                    (_, _, true) if h.dropped_staged => format!("{}: the change waiting for Steam was dropped", h.name),
                    (_, _, true) => format!("{}: {}", h.name, h.describe()),
                    (true, true, _) => format!("{} starts through Quiver once Steam closes or restarts", h.name),
                    (true, false, _) => format!("{} now starts through Quiver: its tweaks, HUD and launch settings apply when Steam launches it", h.name),
                    (false, true, _) => format!("{} stops starting through Quiver once Steam closes or restarts", h.name),
                    (false, false, _) => format!("{} starts without Quiver again: its launch options are back as they were", h.name),
                };
                if let Load::Ready(d) = &mut this.gp.store
                    && d.game == id.0
                {
                    d.steam_hook = Some(h);
                }
                this.show_toast(msg, cx);
            }
            Err(e) => this.show_toast(format!("Couldn't change how Steam starts it: {}", err_text(&e)), cx),
        });
    }

    /// After a tweak or HUD change for a Steam game: when it isn't hooked, say so (the page
    /// has the switch).
    pub(in crate::ui) fn steam_hook_offer(&mut self, id: Gid, what: &str, cx: &mut Context<Self>) {
        let Some(h) = self.steam_hook(id) else { return };
        if h.hooked() {
            return;
        }
        let msg = if h.hookable {
            format!("Steam starts {} without Quiver, so {what} doesn't reach it. \"Start through Quiver\" on its page changes that.", h.name)
        } else {
            format!("Steam starts {} without Quiver, so {what} doesn't reach it ({}).", h.name, h.note.as_deref().unwrap_or("it can't be hooked from Quiver"))
        };
        self.show_toast(msg, cx);
    }

    fn hook_button(&self, th: &Theme, id: Gid, h: &SteamHook, cx: &mut Context<Self>) -> AnyElement {
        let on = !matches!(h.state, PatchState::On | PatchState::ByHand | PatchState::Waiting);
        let (label, kind, ic) = if on { ("Start through Quiver", Kind::Primary, Icon::Gamepad) } else { ("Stop starting through Quiver", Kind::Secondary, Icon::RefreshCw) };
        button(("steam-hook", id.ix()), th, kind, Size::Small, Some(ic), label)
            .when(!h.hookable, |b| b.opacity(0.5))
            .when(h.hookable, |b| b.on_click(cx.listener(move |this, _, _, cx| this.set_steam_hook(id, on, cx))))
            .into_any_element()
    }

    /// The Overview's "Quiver launch" card for a Steam game.
    pub(in crate::ui) fn render_steam_hook_card(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = g.id();
        id.0.as_steam()?;
        let h = self.steam_hook(id)?.clone();
        let hud = self.steam_hud(id, cx);
        let (ic, color, title) = match h.state {
            PatchState::On | PatchState::ByHand => (Icon::CircleCheck, th.ok, "Starts through Quiver"),
            PatchState::Waiting => (Icon::RefreshCw, th.warn, "Starts through Quiver once Steam closes or restarts"),
            PatchState::WaitingUndo => (Icon::RefreshCw, th.warn, "Stops starting through Quiver once Steam closes or restarts"),
            PatchState::Off => (Icon::CircleDot, th.tx3, "Steam starts it without Quiver"),
            PatchState::Unknown | PatchState::Other => (Icon::TriangleAlert, th.warn, "Unknown: several Steam users, none chosen"),
        };
        let what = if h.hooked() {
            "Its tweaks, HUD and Quiver launch settings apply when Steam launches it. Steam stays in charge: its Proton, overlay, Steam Input and Stop work as before."
        } else {
            "Its tweaks, HUD and Quiver launch settings reach it only when it starts through Quiver (quiver steam-run in its Steam launch options). Steam stays in charge of the launch."
        };
        // The everyday switch is whether the HUD shows at launch; the layer itself is in
        // every launch (the Performance tab turns it off for a game it troubles).
        let hud_line = hud.map(|(layer, shown)| {
            let scope = HudScopeRef::of(id.0);
            let label = if shown { "Hide at launch" } else { "Show at launch" };
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(icon(Icon::Monitor, 14., if layer && h.hooked() { th.ok } else { th.tx3 }))
                .child(div().flex_1().text_size(px(12.5)).child(match (layer, h.hooked(), shown) {
                    (false, _, _) => "Quiver's HUD: no Sight layer for this game (see Performance)",
                    (true, true, false) => "Quiver's HUD: in its launches, hidden until Shift+F3 (Shift+F2 opens Sight)",
                    (true, true, true) => "Quiver's HUD: in its launches, shown at launch (Shift+F3 hides it)",
                    (true, false, _) => "Quiver's HUD: doesn't reach the game until it starts through Quiver",
                }))
                .when(layer, |d| {
                    d.child(button(("steam-hud", id.ix()), th, Kind::Ghost, Size::Small, None, label).on_click(cx.listener(move |this, _, _, cx| {
                        this.hud_apply(scope.clone(), HudScope { on: Some(!shown), ..Default::default() }, vec![], cx);
                        if !shown {
                            this.steam_hook_offer(id, "the HUD", cx);
                        }
                    })))
                })
        });
        Some(
            card(th)
                .child(card_header(th, "Quiver launch").child(div().flex_1()).child(self.hook_button(th, id, &h, cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(16.))
                        .child(div().flex().items_center().gap(px(8.)).child(icon(ic, 14., color)).child(div().text_size(px(13.)).font_weight(SEMIBOLD).child(title)))
                        .child(div().text_size(px(12.)).text_color(th.tx3).child(what))
                        .when(!h.launch_options.is_empty(), |c| c.child(mono(11.5, th.tx2).child(format!("Launch options: {}", h.launch_options))))
                        .when_some(h.note.clone(), |c, n| c.child(div().text_size(px(12.)).text_color(th.warn).child(n)))
                        .when(h.steam_running && !h.hooked() && h.hookable, |c| {
                            c.child(div().text_size(px(11.5)).text_color(th.tx4).child("Steam is running: the change waits until Steam closes (Steam rewrites its settings when it exits)."))
                        })
                        .children(hud_line),
                )
                .into_any_element(),
        )
    }

    /// The Mods tab's line for a Steam game: Play in Steam runs the profile marked "Steam
    /// runs", once the game starts through Quiver (with the switch when it doesn't).
    pub(in crate::ui) fn render_steam_mods_banner(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = g.id();
        let name = crate::model::clean_name(&g.entry.name);
        let h = self.steam_hook(id)?.clone();
        let (tone, ic, text) = match h.state {
            PatchState::On | PatchState::ByHand => (Tone::Ok, Icon::CircleCheck, format!("{name} starts through Quiver: Play in Steam runs the profile marked \"Steam runs\", with its mods and options.")),
            PatchState::Waiting => (Tone::Warn, Icon::RefreshCw, format!("{name} starts through Quiver once Steam closes or restarts; its mods apply from then.")),
            _ => (Tone::Warn, Icon::TriangleAlert, format!("Steam starts {name} without Quiver, so these mods don't reach it yet: they apply once it starts through Quiver (quiver steam-run in its Steam launch options; undone exactly).")),
        };
        let show_button = !h.hooked() && h.hookable;
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(banner(th, tone, ic, text))
                .when(show_button, |d| d.child(div().flex().child(self.hook_button(th, id, &h, cx))))
                .when_some(h.note.clone().filter(|_| !h.hookable), |d, n| d.child(div().text_size(px(12.)).text_color(th.tx3).child(n)))
                .into_any_element(),
        )
    }

    /// A Steam game's profile chip next to Play: which profile Play in Steam runs (its
    /// `steam_profile`), opening the menu that changes it; and, while the game doesn't start
    /// through Quiver, that its mods apply only once it does.
    pub(in crate::ui) fn render_steam_profile_chip(&mut self, th: &Theme, g: &GameRow, slim: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = g.id();
        if !g.steam_game() || !g.mods_installed() || self.gp.id != id {
            return None;
        }
        let profiles = self.gp.profiles();
        if profiles.is_empty() {
            return None;
        }
        let cur = self.default_profile(id).and_then(|p| profiles.iter().find(|x| x.id == p).map(|x| x.name.clone()));
        let on = cur.is_some();
        let label = format!("Steam runs: {}", cur.unwrap_or_else(|| "Vanilla".into()));
        let unhooked = self.steam_hook(id).is_some_and(|h| !h.hooked());
        let (bg, fg, line) = if slim {
            (crate::theme::rgba(0x0e1016, 0.62), crate::theme::on_art::tx1(), crate::theme::on_art::line())
        } else {
            (th.ink2, th.tx1, th.line2)
        };
        let chip = div()
            .id("act-steam-profile")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(if slim { 30. } else { 36. }))
            .px(px(12.))
            .rounded(px(8.))
            .bg(bg)
            .border_1()
            .border_color(if on { th.acc_line } else { line })
            .text_color(fg)
            .font_weight(crate::ui::MEDIUM)
            .cursor_pointer()
            .child(icon(Icon::Layers, 14., if on { th.acc_hi } else { fg }))
            .child(label)
            .child(icon(Icon::ChevronDown, 12., fg))
            .on_click(cx.listener(|this, _, _, cx| {
                this.gp.default_menu = true;
                this.click(crate::nav::Target::Action(crate::app::ACTION_OPTIONS), cx)
            }));
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .child(chip)
                .when(unhooked && !slim, |d| {
                    d.child(div().text_size(px(11.5)).text_color(th.warn).child("Mods apply once it starts through Quiver (Quiver launch, on Overview)"))
                })
                .into_any_element(),
        )
    }

    /// The Tweaks tab's line for a Steam game: whether its tweaks reach it, with the switch.
    pub(in crate::ui) fn render_steam_hook_banner(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let name = crate::model::clean_name(&g.entry.name);
        self.load_store_detail(id, false, cx);
        let Some(h) = self.steam_hook(id).cloned() else {
            return banner(th, Tone::Accent, Icon::Info, format!("{name}'s tweaks apply when it starts through Quiver (quiver steam-run in its Steam launch options). Steam games have one scope: the game.")).into_any_element();
        };
        let (tone, ic, text) = match h.state {
            PatchState::On | PatchState::ByHand => (Tone::Ok, Icon::CircleCheck, format!("{name} starts through Quiver: these tweaks apply when Steam launches it (Steam picks the Proton build). Steam games have one scope: the game.")),
            PatchState::Waiting => (Tone::Warn, Icon::RefreshCw, format!("{name} starts through Quiver once Steam closes or restarts; these tweaks apply from then.")),
            _ => (Tone::Warn, Icon::TriangleAlert, format!("Steam starts {name} without Quiver, so these tweaks don't reach it. Start it through Quiver to apply them (its Steam launch options get quiver steam-run; undone exactly).")),
        };
        let show_button = !h.hooked() && h.hookable;
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(banner(th, tone, ic, text))
            .when(show_button, |d| d.child(div().flex().child(self.hook_button(th, id, &h, cx))))
            .when_some(h.note.clone().filter(|_| !h.hookable), |d, n| d.child(div().text_size(px(12.)).text_color(th.tx3).child(n)))
            .into_any_element()
    }
}
