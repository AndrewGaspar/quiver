//! Ubisoft games bought on Steam, started through Quiver instead of Ubisoft Connect (the
//! daemon's `Request::SteamGames` / `SteamPatch` / `SteamUnpatch`; see
//! `quiver_core::exports::steamgames`). Shared by the welcome flow's Steam step, Settings ›
//! Steam, the game page and the offer card that appears when quiverd notices a new install.
//!
//! Steam only reads launch options when it starts, so while it runs a change waits. Quiver
//! never restarts Steam without asking: [`Quiver::ask_restart_steam`] opens the confirmation.

use quiver_ui::depth::DepthExt;
use std::collections::HashSet;

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::exports::{PatchState, SteamGame, SteamGames, SteamPatchResult};
use quiver_api::{Event, Request};

use super::global::{Confirm, ConfirmAction};
use super::{Icon, Kind, MEDIUM, PillKind, Size, button, icon, pill, soft_shadow};
use crate::app::Quiver;
use crate::err_text;
use crate::theme::{Theme, rgba};

/// What the user is told the switch does, everywhere it's offered.
pub const EXPLAIN: &str = "Steam copies of Ubisoft games normally open Ubisoft Connect first. Quiver can start them instead: no Connect window, no second sign-in, and your play time still counts. Nothing in the game changes, and you can undo it any time.";

#[derive(Default)]
pub struct SteamGamesUi {
    pub list: Option<Result<SteamGames, String>>,
    /// Requests in flight, by Steam app id.
    pub busy: HashSet<u32>,
    loading: bool,
}

impl Quiver {
    pub fn load_steam_games(&mut self, cx: &mut Context<Self>) {
        if self.ui.sg.loading || self.source.is_none() {
            return;
        }
        self.ui.sg.loading = true;
        self.call::<SteamGames, _>(Request::SteamGames { steam_user: None }, cx, |this, r, _| {
            this.ui.sg.loading = false;
            this.ui.sg.list = Some(r.map_err(|e| err_text(&e)));
        });
    }

    pub fn steam_games_event(&mut self, e: &Event, cx: &mut Context<Self>) {
        if let Event::SteamGamesChanged { .. } = e {
            self.load_steam_games(cx);
            // Staged changes written, or patched elsewhere: the Steam user list shows them.
            self.load_global_exports(cx);
        }
    }

    /// Ubisoft games installed through Steam (empty until loaded).
    pub fn steam_copies(&self) -> Vec<SteamGame> {
        match &self.ui.sg.list {
            Some(Ok(s)) => s.games.clone(),
            _ => vec![],
        }
    }

    pub fn steam_running(&self) -> bool {
        matches!(&self.ui.sg.list, Some(Ok(s)) if s.running) || matches!(&self.ui.steam, Some(Ok(s)) if s.running)
    }

    /// The Steam copy of a library game, if one is installed.
    pub fn steam_copy(&self, product_id: u32) -> Option<SteamGame> {
        self.steam_copies().into_iter().find(|g| g.product_id == Some(product_id))
    }

    /// Games that can be switched and aren't yet.
    pub fn steam_switchable(&self) -> Vec<u32> {
        self.steam_copies().into_iter().filter(|g| g.patchable && g.state == PatchState::Off).map(|g| g.app_id).collect()
    }

    /// Changes waiting for Steam to restart (shortcuts and launch options).
    pub fn steam_waiting(&self) -> bool {
        matches!(&self.ui.steam, Some(Ok(s)) if !s.staged.is_empty())
            || self.steam_copies().iter().any(|g| matches!(g.state, PatchState::Waiting | PatchState::WaitingUndo))
    }

    /// Switch a game to Quiver (`undo`: back to Ubisoft Connect).
    pub fn steam_patch(&mut self, app_id: u32, undo: bool, cx: &mut Context<Self>) {
        if !self.ui.sg.busy.insert(app_id) {
            return;
        }
        let req = if undo { Request::SteamUnpatch { app_id, steam_user: None } } else { Request::SteamPatch { app_id, steam_user: None } };
        self.call::<SteamPatchResult, _>(req, cx, move |this, r, cx| {
            this.ui.sg.busy.remove(&app_id);
            match r {
                Ok(r) if r.unchanged && r.dropped_staged => this.show_toast(format!("{}: the change waiting for Steam is dropped", r.name), cx),
                Ok(r) if r.unchanged => {}
                Ok(r) if r.staged => this.show_toast(
                    if undo {
                        format!("{} goes back to Ubisoft Connect when Steam restarts", r.name)
                    } else {
                        format!("{} starts through Quiver once Steam restarts", r.name)
                    },
                    cx,
                ),
                Ok(r) if undo => this.show_toast(format!("{} starts through Ubisoft Connect again", r.name), cx),
                Ok(r) => this.show_toast(format!("{} now starts through Quiver from Steam", r.name), cx),
                Err(e) => this.show_error(&e, cx),
            }
            this.load_steam_games(cx);
            this.load_global_exports(cx);
        });
    }

    pub fn steam_patch_all(&mut self, cx: &mut Context<Self>) {
        for id in self.steam_switchable() {
            self.steam_patch(id, false, cx);
        }
    }

    /// "Not now" on an offer: not offered again.
    pub fn steam_dismiss(&mut self, app_id: u32, cx: &mut Context<Self>) {
        if let Some(Ok(s)) = &mut self.ui.sg.list
            && let Some(g) = s.games.iter_mut().find(|g| g.app_id == app_id)
        {
            g.offer = false;
        }
        self.call::<serde_json::Value, _>(Request::SteamDismissOffer { app_id }, cx, |this, r, cx| {
            if let Err(e) = r {
                this.show_error(&e, cx);
            }
            this.load_steam_games(cx);
        });
    }

    pub fn steam_offers_enabled(&self) -> bool {
        match &self.ui.sg.list {
            Some(Ok(s)) => s.offers,
            _ => true,
        }
    }

    pub fn set_steam_offers(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Ok(s)) = &mut self.ui.sg.list {
            s.offers = on;
        }
        self.call::<serde_json::Value, _>(Request::SteamOffers { enabled: Some(on) }, cx, move |this, r, cx| {
            match r {
                Ok(_) if on => this.show_toast("Quiver offers this when a Ubisoft game is installed through Steam", cx),
                Ok(_) => this.show_toast("No more offers for new Steam installs", cx),
                Err(e) => this.show_error(&e, cx),
            }
            this.load_steam_games(cx);
        });
    }

    /// The confirmation for restarting Steam (the global dialog; the welcome flow asks inline).
    pub fn ask_restart_steam(&mut self, cx: &mut Context<Self>) {
        let waiting: Vec<String> = match &self.ui.steam {
            Some(Ok(s)) => s.staged.clone(),
            _ => vec![],
        };
        let body = if waiting.is_empty() {
            "Quiver closes Steam cleanly and starts it again. Close any game you started from Steam first.".to_owned()
        } else {
            format!(
                "Steam only reads these when it starts, so Quiver closes it cleanly, writes {} change{} and starts it again. Close any game you started from Steam first.",
                waiting.len(),
                if waiting.len() == 1 { "" } else { "s" }
            )
        };
        self.ui.confirm = Some(Confirm {
            title: "Restart Steam now?".into(),
            body: body.into(),
            confirm: "Restart Steam".into(),
            cli: Some("quiver steam restart".into()),
            action: ConfirmAction::RestartSteam,
            alt: None,
            selected: 0,
        });
        cx.notify();
    }

    /// The card for a game quiverd noticed was newly installed through Steam.
    pub fn render_steam_offer(&mut self, th: &Theme, above: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.welcome_open() {
            return None;
        }
        let g = self.steam_copies().into_iter().find(|g| g.offer && g.patchable && g.state == PatchState::Off)?;
        let id = g.app_id;
        let busy = self.ui.sg.busy.contains(&id);
        Some(
            div()
                .id("steam-offer")
                .depth_overlay()
                .absolute()
                .right(px(if self.bp().narrow() { 16. } else { 20. }))
                .bottom(px(if above { 150. } else { 20. }))
                .w(px(self.dialog_width(400.)))
                .flex()
                .flex_col()
                .gap(px(10.))
                .px(px(16.))
                .py(px(14.))
                .rounded(px(12.))
                .bg(th.ink3)
                .border_1()
                .border_color(th.line2)
                .shadow(vec![soft_shadow(rgba(0x000000, 0.5), 14., 40.)])
                .text_size(px(12.5))
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(10.))
                        .child(div().pt(px(1.)).child(icon(Icon::Gamepad, 15., th.acc_hi)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .flex_1()
                                .min_w_0()
                                .child(div().font_weight(MEDIUM).child(format!("{} was installed through Steam", g.name)))
                                .child(div().text_size(px(12.)).line_height(px(17.)).text_color(th.tx2).child(
                                    "Start it through Quiver instead of Ubisoft Connect? No Connect window or second sign-in, and you can undo it in Settings › Steam.",
                                )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(6.))
                        .child(
                            button("offer-no", th, Kind::Ghost, Size::Small, None, "Not now")
                                .on_click(cx.listener(move |this, _, _, cx| this.steam_dismiss(id, cx))),
                        )
                        .child(
                            button("offer-yes", th, Kind::Primary, Size::Small, Some(Icon::Check), if busy { "Switching…" } else { "Use Quiver" })
                                .on_click(cx.listener(move |this, _, _, cx| this.steam_patch(id, false, cx))),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// The action a game offers: `Some(true)` switch to Quiver, `Some(false)` undo, `None`
/// nothing (can't be switched).
pub fn control(g: &SteamGame) -> Option<bool> {
    match g.state {
        PatchState::On | PatchState::ByHand | PatchState::Waiting => Some(false),
        PatchState::Off | PatchState::WaitingUndo if g.patchable => Some(true),
        _ => None,
    }
}

/// The game's state as a pill.
pub fn state_pill(th: &Theme, g: &SteamGame, busy: bool) -> AnyElement {
    if busy {
        return pill(th, PillKind::Muted, Some(Icon::LoaderCircle), "Switching…").into_any_element();
    }
    match g.state {
        PatchState::On => pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Starts through Quiver"),
        PatchState::ByHand => pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Starts through Quiver (set by hand)"),
        PatchState::Waiting => pill(th, PillKind::Warn, Some(Icon::Clock), "Quiver after Steam restarts"),
        PatchState::WaitingUndo => pill(th, PillKind::Warn, Some(Icon::Clock), "Connect after Steam restarts"),
        PatchState::Off if g.patchable => pill(th, PillKind::Muted, None, "Starts through Ubisoft Connect"),
        PatchState::Off | PatchState::Unknown | PatchState::Other => pill(th, PillKind::Muted, None, "Can't switch"),
    }
    .into_any_element()
}

/// The line under a game's name, next to its state pill.
pub fn state_line(g: &SteamGame) -> String {
    let base = match g.state {
        PatchState::On => "Play it from Steam as usual",
        PatchState::ByHand => "Its launch options were set by hand; play it from Steam as usual",
        PatchState::Waiting => "Steam picks this up when it restarts",
        PatchState::WaitingUndo => "Goes back to Ubisoft Connect when Steam restarts",
        _ => "Opens Ubisoft Connect first",
    };
    match &g.note {
        Some(n) => format!("{base} · {n}"),
        None => base.to_owned(),
    }
}
