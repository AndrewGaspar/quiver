//! How the toast lane, the overlay drawn in the game, the quick menu and Sight's settings fit
//! into [`Sight`] (design/SIGHT.md §16). `lib.rs` routes messages here; the pieces are
//! `toasts`, `overlay`, `quick`, `screenshot`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{App, AppContext};
use quiver_api::options::OptionOp;
use quiver_api::perf::{HudScope, Stat};
use quiver_api::sight::{OnUnlock, PanelAction, PanelCommand, SightNotice, SightSettings};
use quiver_perf::panel::{EV_KEY, PanelEvent};

use crate::link::{Loaded, LinkReq, Msg};
use crate::panel::ingame::{self, InGame};
use crate::quick::{self, Act, Effect, Key, QuickView, TileKind};
use crate::toasts::{self, ToastData, ToastKind};
use crate::{Sight, overlay};

/// The quick menu drawn in the game.
pub struct QuickShown {
    game: InGame,
    view: gpui::Entity<QuickView>,
}

impl Sight {
    pub(crate) fn send_link(&self, r: LinkReq) {
        if let Some(l) = &self.link {
            let _ = l.send(r);
        }
    }

    /// The session something is for when it doesn't say: the quick menu's, the panel's, the
    /// overlay's, else the first with a HUD model.
    fn some_session(&self) -> Option<String> {
        self.quick.session.clone().filter(|_| self.quick.active()).or_else(|| self.panel.session().map(str::to_owned)).or_else(|| self.overlay.session().map(str::to_owned)).or_else(|| {
            let mut v: Vec<&String> = self.huds.keys().collect();
            v.sort();
            v.first().map(|s| (*s).clone())
        })
    }

    // ---- Settings ------------------------------------------------------------------------

    pub(crate) fn settings_changed(&mut self, s: SightSettings, cx: &mut App) {
        if s.toasts.dnd != self.settings.toasts.dnd {
            tracing::info!(dnd = s.toasts.dnd, "do not disturb");
        }
        self.panel.settings_changed(&s, cx);
        self.settings = s;
        self.panel_pins(cx);
    }

    // ---- Toasts --------------------------------------------------------------------------

    pub(crate) fn notice(&mut self, n: SightNotice, cx: &mut App) {
        self.toast(toasts::from_notice(&n), cx);
    }

    /// A toast for the lane: do-not-disturb, the unlock screenshot, then the game or the
    /// desktop.
    pub(crate) fn toast(&mut self, mut t: ToastData, cx: &mut App) {
        if !toasts::allowed(&t, self.settings.toasts.dnd) {
            tracing::info!(title = %t.title, "toast held back: do not disturb");
            return;
        }
        if t.kind == ToastKind::Achievement {
            match self.settings.achievements.on_unlock {
                OnUnlock::Off => {
                    tracing::info!(title = %t.title, "achievement: no toast (achievements.on_unlock = off)");
                    return;
                }
                // Half a second after (SIGHT §3.7: the moment in the frame); the layer's capture
                // is under Sight's drawing, so the toast isn't in it.
                OnUnlock::ToastScreenshot if !t.shot => {
                    t.shot = true;
                    let session = t.session.clone().or_else(|| self.some_session());
                    if let Some(s) = session {
                        self.screenshot(&s, Duration::from_millis(500), false);
                    }
                }
                _ => {}
            }
        }
        if t.session.is_none() {
            t.session = self.some_session();
        }
        self.place_toasts();
        let target = self.overlay_target(t.session.as_deref());
        self.toasts.set_in_game(target.is_some(), cx);
        self.toasts.push(t, cx);
        self.sync_overlay(cx);
    }

    /// Where the overlay can be drawn now: a session whose layer shows images without
    /// capture, and that the panel or the quick menu doesn't hold.
    fn overlay_target(&self, session: Option<&str>) -> Option<overlay::Target> {
        let s = session.map(str::to_owned).or_else(|| self.toasts.session()).or_else(|| self.some_session())?;
        if self.panel.in_game() && self.panel.session() == Some(s.as_str()) {
            return None;
        }
        if self.quick_shown.is_some() && self.quick.session.as_deref() == Some(s.as_str()) {
            return None;
        }
        if self.overlay_failed.get(&s).is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
            return None;
        }
        self.panel.game_target(&s, false).ok()
    }

    /// Puts the lane, the chip and the hint where they go now.
    pub(crate) fn sync_overlay(&mut self, cx: &mut App) {
        if self.overlay.failed()
            && let Some(s) = self.overlay.session().map(str::to_owned)
        {
            tracing::info!(session = %s, "the game didn't show the overlay: toasts on the desktop for a while");
            self.overlay_failed.insert(s, Instant::now());
            self.overlay.close(cx);
        }
        let target = self.overlay_target(None);
        let in_game = target.is_some();
        self.toasts.set_in_game(in_game, cx);
        let list = if in_game { self.toasts.list() } else { Vec::new() };
        if let Err(e) = self.overlay.sync(target, list, self.toasts.corner, cx) {
            tracing::warn!("overlay in the game: {e:#}");
            if let Some(s) = self.toasts.session() {
                self.overlay_failed.insert(s, Instant::now());
            }
            self.toasts.set_in_game(false, cx);
        }
    }

    /// Every tick: expiry, the image's heartbeat, the first-launch hint.
    pub(crate) fn tick_overlay(&mut self, cx: &mut App) {
        self.toasts.tick(cx);
        self.overlay.renew();
        self.first_launch_hint(cx);
        self.sync_overlay(cx);
    }

    /// "Open Sight: Shift+F2 or Guide+Start" once per install, when the first game can show it.
    fn first_launch_hint(&mut self, cx: &mut App) {
        if self.settings.hint.shown || self.hint_done {
            return;
        }
        let Some(s) = self.some_session() else { return };
        // In the game once it draws; else (no layer drawing after a while) as a toast.
        let in_game = self.panel.game_target(&s, false).is_ok();
        let waited = self.started_at.entry(s.clone()).or_insert_with(Instant::now).elapsed();
        let Some(in_game) = overlay::hint_due(&self.settings, self.hint_done, in_game, waited) else { return };
        self.hint_done = true;
        tracing::info!(session = %s, in_game, "first-launch hint");
        if in_game {
            self.overlay.show_hint();
        } else if self.layer_shell {
            self.toast(toasts::hint_toast(), cx);
        }
        self.send_link(LinkReq::SettingsSet { key: "hint.shown".into(), value: serde_json::Value::Bool(true) });
    }

    /// The status chip ("Sight · 118 fps · 21:14 · 1 h 12 m"): when `forced` (`quiver sight
    /// chip`), or the setting is on.
    pub(crate) fn chip(&mut self, session: Option<&str>, forced: bool, cx: &mut App) {
        if !forced && !self.settings.status_chip.enabled {
            return;
        }
        let Some(s) = session.map(str::to_owned).or_else(|| self.some_session()) else { return };
        let c = overlay::Chip { fps: self.fps.get(&s).copied(), started: self.started.get(&s).copied() };
        self.overlay.show_chip(c);
        self.sync_overlay(cx);
    }

    /// Live metrics (the panel's subscription): the chip's FPS.
    pub(crate) fn metrics_seen(&mut self, u: &quiver_api::perf::PerfUpdate) {
        if let Some(f) = u.values.get("fps").and_then(|m| m.get(&Stat::Mean)).and_then(|v| v.value) {
            self.fps.insert(u.session.clone(), f);
        }
    }

    /// Session data came (for the panel or the quick menu): the quick menu's tiles, the chip's
    /// play time.
    pub(crate) fn data_seen(&mut self, session: &str, data: &Loaded, cx: &mut App) {
        let Loaded::All(d) = data else { return };
        if let Some(st) = d.session.ready().and_then(|v| v.started) {
            self.started.insert(session.to_owned(), st);
        }
        // Kept for the quick menu, and what's pinned for the panel's Y.
        self.quick_data.insert(session.to_owned(), (**d).clone());
        if self.quick.session.as_deref() == Some(session) {
            self.quick_tiles(cx);
        }
        self.panel_pins(cx);
    }

    /// The open panel's game's pins, for Y (pin or unpin).
    pub(crate) fn panel_pins(&mut self, cx: &mut App) {
        let Some(s) = self.panel.session().map(str::to_owned) else { return };
        let Some((game, profile)) = self.quick_data.get(&s).and_then(|d| d.session.ready()).map(|v| (v.game, v.profile().map(str::to_owned))) else { return };
        let pins = [None, profile.as_deref()].into_iter().filter_map(|p| {
            let scope = quiver_api::sight::pin_scope(&game, p);
            self.settings.pins.get(&scope).map(|pins| (scope, pins.clone()))
        }).collect();
        self.panel.set_pinned(pins, cx);
    }

    // ---- Screenshots ---------------------------------------------------------------------

    /// A screenshot of the session's game after `after` (`link::LinkReq::Shot`: the layer's
    /// capture through quiverd, else grim); `say`: a toast with where it went.
    fn screenshot(&mut self, session: &str, after: Duration, say: bool) {
        let monitor = self.huds.get(session).and_then(|h| h.model.as_ref()?.monitor.clone());
        let game = self.quick_data.get(session).and_then(|d| d.session.ready().map(|v| v.name.clone())).filter(|n| !n.is_empty()).unwrap_or_else(|| "Game".into());
        self.send_link(LinkReq::Shot { session: session.to_owned(), after, monitor, game, say });
    }

    // ---- The quick menu ------------------------------------------------------------------

    /// A panel command the quick menu (or the chip) takes; false: the panel's.
    pub(crate) fn quick_command(&mut self, c: &PanelCommand, cx: &mut App) -> bool {
        match c.action {
            PanelAction::Chip => {
                self.chip(c.session.as_deref(), true, cx);
                true
            }
            PanelAction::Quick => {
                let Some(s) = c.session.clone() else { return true };
                if self.quick.shown() {
                    self.quick_close(cx);
                    return true;
                }
                self.quick.latch(&s, Instant::now());
                self.quick_load(&s);
                self.quick_effect(Effect::Show, cx);
                true
            }
            // Shift+F2 (or the chord through hyprpad) in the game: a tap or a hold.
            PanelAction::Toggle if c.by == "keys" && !self.panel.is_open() => {
                if self.quick.active() {
                    // Pressed again while it's up (latched): close it; a bounce while waiting.
                    if self.quick.shown() {
                        self.quick_close(cx);
                    }
                    return true;
                }
                let Some(s) = c.session.clone() else { return false };
                let now_ns = quiver_perf::clock::mono_raw_ns();
                let Some(input) = self.panel.input_of(&s).filter(|p| p.layer().reports_chord(now_ns)) else { return false };
                if let Err(why) = self.panel.game_target(&s, true) {
                    tracing::info!(session = %s, %why, "no quick menu here: the panel");
                    return false;
                }
                // When the press was, from the layer's word (else now).
                let ch = input.chord();
                let ago = (now_ns.max(0) as u64).saturating_sub(ch.at_ns);
                let at = if ch.at_ns != 0 && ago < 2_000_000_000 { Instant::now() - Duration::from_nanos(ago) } else { Instant::now() };
                self.quick.press(&s, at);
                self.quick_cmd = Some(c.clone());
                self.quick_load(&s);
                self.quick_poll_start(cx);
                true
            }
            _ => {
                if self.quick.active() {
                    self.quick_close(cx);
                }
                false
            }
        }
    }

    fn quick_load(&self, session: &str) {
        self.send_link(LinkReq::Load { session: session.to_owned(), part: crate::panel::data::Part::All, view: None });
    }

    /// Polls the chord every `quick::POLL` while the menu waits or is up.
    fn quick_poll_start(&mut self, cx: &mut App) {
        if self.quick_poll.as_ref().is_some_and(|f| !f.load(Ordering::Relaxed)) {
            return;
        }
        let Some(tx) = self.tx.clone() else { return };
        let stop = Arc::new(AtomicBool::new(false));
        self.quick_poll = Some(stop.clone());
        cx.spawn(async move |cx| {
            while !stop.load(Ordering::Relaxed) {
                cx.background_executor().timer(quick::POLL).await;
                if tx.unbounded_send(Msg::QuickPoll).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn quick_poll_stop(&mut self) {
        if let Some(f) = self.quick_poll.take() {
            f.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn quick_polled(&mut self, cx: &mut App) {
        let now = Instant::now();
        let held = self.quick.session.as_deref().and_then(|s| self.panel.input_of(s)).and_then(|p| {
            let now_ns = quiver_perf::clock::mono_raw_ns();
            p.layer().reports_chord(now_ns).then(|| p.chord().held)
        });
        let mut e = self.quick.chord(held, now);
        if e == Effect::None {
            e = self.quick.tick(now);
        }
        // Held for a minute, or the game stopped showing it: give the game its input back.
        let shown_but_not_drawn = self.quick_shown.as_ref().is_some_and(|q| !q.game.confirmed && q.game.opened.elapsed() > ingame::CONFIRM_WITHIN);
        let too_long = self.quick.phase == quick::Phase::Held && self.quick.since.is_some_and(|t| now.saturating_duration_since(t) > Duration::from_secs(60));
        if e == Effect::None && (shown_but_not_drawn || too_long) {
            tracing::info!(shown_but_not_drawn, too_long, "quick menu: closing");
            self.quick.phase = quick::Phase::Closed;
            e = Effect::Close;
        }
        if e == Effect::None && self.quick.enter_since.is_some() {
            e = Effect::Redraw; // the Quit bar fills
        }
        self.quick_effect(e, cx);
        if !self.quick.active() {
            self.quick_poll_stop();
        }
    }

    /// Forwarded game input while the quick menu holds it: the arrows, Enter, Escape.
    pub(crate) fn quick_input(&mut self, session: &str, events: &[PanelEvent], cx: &mut App) -> bool {
        if self.quick_shown.is_none() || self.quick.session.as_deref() != Some(session) {
            return false;
        }
        let now = Instant::now();
        for e in events.iter().filter(|e| e.kind == EV_KEY) {
            if let Some(k) = Key::from_evdev(e.code) {
                let eff = self.quick.key(k, e.down(), now);
                self.quick_effect(eff, cx);
            }
        }
        true
    }

    fn quick_tiles(&mut self, cx: &mut App) {
        let Some(s) = self.quick.session.clone() else { return };
        let data = self.quick_data.get(&s).cloned().unwrap_or_default();
        let game = data.session.ready().map(|v| (v.game, v.profile().map(str::to_owned)));
        let pins = game.map(|(g, p)| self.settings.pins_for(&g, p.as_deref())).unwrap_or_default();
        let replay = quick::replay_buffer().map(|(pid, _)| pid);
        self.quick.set_tiles(quick::tiles(&data, &pins, &self.toasts.answerable(), replay));
        self.quick_redraw(cx);
    }

    fn quick_redraw(&mut self, cx: &mut App) {
        if let Some(q) = &self.quick_shown {
            let (quick, now) = (&self.quick, Instant::now());
            q.view.update(cx, |v, cx| {
                v.set(quick, now);
                cx.notify();
            });
        }
    }

    fn quick_close(&mut self, cx: &mut App) {
        if self.quick.active() {
            // Through Back: closes and remembers the focus.
            let _ = self.quick.key(Key::Back, true, Instant::now());
            self.quick.phase = quick::Phase::Closed;
        }
        self.quick_hide(cx);
        self.quick_poll_stop();
    }

    fn quick_hide(&mut self, cx: &mut App) {
        if let Some(q) = self.quick_shown.take() {
            let session = q.game.session.clone();
            q.game.close(cx);
            if !self.panel.is_open() {
                crate::panel::write_state(None);
            }
            tracing::info!(%session, "quick menu closed");
        }
    }

    /// Draws it in the game (input captured: the arrows mustn't reach the game).
    fn quick_show(&mut self, cx: &mut App) -> anyhow::Result<()> {
        let Some(s) = self.quick.session.clone() else { anyhow::bail!("no session") };
        let t = self.panel.game_target(&s, true).map_err(anyhow::Error::msg)?;
        let Some(ui) = self.tx.clone() else { anyhow::bail!("no UI channel") };
        self.quick_tiles(cx);
        // It takes the image over from the overlay.
        self.overlay.close(cx);
        let (quick, now) = (&self.quick, Instant::now());
        let view = cx.new(|cx| QuickView::from(quick, now, cx));
        let stereo = if t.stereo.fits(t.extent) { t.stereo } else { Default::default() };
        let factor = ingame::factor(stereo.logical_extent(t.extent), t.monitor);
        let game = InGame::open(&s, t.input.clone(), &t.dir, t.extent, stereo, factor, view.clone(), ui, cx)?;
        crate::panel::write_state(Some((&s, "game")));
        tracing::info!(session = %s, tiles = self.quick.tiles.len(), latched = self.quick.phase == quick::Phase::Latched, "quick menu shown");
        self.quick_shown = Some(QuickShown { game, view });
        Ok(())
    }

    fn quick_effect(&mut self, e: Effect, cx: &mut App) {
        let session = self.quick.session.clone();
        match e {
            Effect::None => {}
            Effect::Redraw => self.quick_redraw(cx),
            Effect::OpenPanel => {
                self.quick_hide(cx);
                if let Some(c) = self.quick_cmd.take() {
                    self.panel_command(c, cx);
                }
            }
            Effect::Show => {
                if let Err(e) = self.quick_show(cx) {
                    tracing::info!("quick menu: {e:#}; the panel instead");
                    self.quick.phase = quick::Phase::Closed;
                    self.quick_effect(Effect::OpenPanel, cx);
                }
            }
            Effect::Apply(i) => {
                let tile = self.quick.tiles.get(i).cloned();
                self.quick_hide(cx);
                if let (Some(s), Some(t)) = (session.as_deref(), tile) {
                    self.quick_apply(s, &t, cx);
                }
                self.chip(session.as_deref(), false, cx);
            }
            Effect::Close => {
                self.quick_hide(cx);
                self.chip(session.as_deref(), false, cx);
            }
            Effect::Quit => {
                self.quick_hide(cx);
                if let Some(s) = session.as_deref() {
                    let game = self.quick_data.get(s).and_then(|d| d.session.ready().map(|v| v.game));
                    match self.panel.quit_game(s, game) {
                        Ok(()) => tracing::info!(session = s, "quick menu: quit the game"),
                        Err(e) => self.toast(ToastData { kind: ToastKind::Error, caption: "Quick menu".into(), title: "Couldn't quit the game".into(), body: Some(e), urgent: true, ..Default::default() }, cx),
                    }
                }
            }
        }
        if !self.quick.active() {
            self.quick_cmd = None;
        }
    }

    fn quick_apply(&mut self, session: &str, t: &quick::Tile, cx: &mut App) {
        let Some(data) = self.quick_data.get(session).cloned() else {
            self.toast(ToastData { caption: "Quick menu".into(), title: format!("{}: not loaded yet", t.label), ..Default::default() }, cx);
            return;
        };
        let Some(act) = quick::act(t, &data) else { return };
        let view = data.session.ready().cloned();
        let said = |title: String| ToastData { kind: ToastKind::Options, caption: "Quick menu".into(), title, duration: Duration::from_secs(3), session: Some(session.to_owned()), ..Default::default() };
        tracing::info!(session, tile = %t.label, ?act, "quick menu: apply");
        match act {
            Act::SetOption(surface, id, value) => {
                let Some(view) = view else { return };
                let words = data.options.ready().and_then(|l| l.iter().find(|s| s.key == surface)).and_then(|s| s.options.get(&id)).map(|st| {
                    let mut st = st.clone();
                    st.value = value.clone();
                    quick::value_words(&st)
                });
                self.send_link(LinkReq::SetOption { view, op: OptionOp::Set { surface: surface.clone(), id: id.clone(), value }, key: (surface, id), live: quiver_api::options::Live::IfRunning });
                self.toast(said(format!("{} {}", t.label, words.unwrap_or_default().to_lowercase())), cx);
            }
            Act::SetTweak(id, value) => {
                let (Some(view), Some(tw)) = (view, data.tweaks.ready()) else { return };
                let words = if id == "fps.cap" { quick::cap_words(&value) } else { value.to_string() };
                self.send_link(LinkReq::SetTweak { view, op: OptionOp::Set { surface: tw.key.clone(), id: id.clone(), value }, id });
                self.toast(said(format!("{} · {words}", t.label)), cx);
            }
            Act::HudPreset(next) => {
                let Some(view) = view else { return };
                let name = crate::panel::state::PRESETS.iter().find(|(i, _)| *i == next).map(|(_, n)| (*n).to_owned()).unwrap_or_else(|| next.clone());
                self.send_link(LinkReq::HudSet { view, set: HudScope { layout: Some(next), on: Some(true), ..Default::default() } });
                self.toast(said(format!("HUD · {name}")), cx);
            }
            Act::HudShow => self.send_link(LinkReq::HudToggle { session: session.to_owned() }),
            Act::Notice { id, button } => {
                self.toasts.dismiss_notice(&id, cx);
                self.send_link(LinkReq::NoticeAct { id, button });
            }
            Act::Screenshot => self.screenshot(session, Duration::from_millis(150), true),
            Act::Replay { pid } => {
                // gpu-screen-recorder saves its replay buffer on SIGUSR1.
                let ok = unsafe { libc::kill(pid as i32, libc::SIGUSR1) } == 0;
                self.toast(if ok { said("Replay saved".into()) } else { ToastData { kind: ToastKind::Error, title: "The replay buffer has stopped".into(), urgent: true, ..Default::default() } }, cx);
            }
            Act::Quit => {}
        }
        if matches!(t.kind, TileKind::Notice { .. }) {
            self.sync_overlay(cx);
        }
    }

    /// Every tick: the quick menu's capture deadline and image heartbeat.
    pub(crate) fn quick_renew(&mut self) {
        if let Some(q) = &mut self.quick_shown {
            q.game.renew();
        }
    }

    pub(crate) fn quick_session_ended(&mut self, session: &str, cx: &mut App) {
        if self.quick.session.as_deref() == Some(session) {
            self.quick_close(cx);
            self.quick.session = None;
        }
        self.quick_data.remove(session);
        self.fps.remove(session);
        self.started.remove(session);
        if self.overlay.session() == Some(session) {
            self.overlay.close(cx);
        }
    }
}
