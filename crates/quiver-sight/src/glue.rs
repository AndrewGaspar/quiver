//! How the toast lane, the overlay drawn in the game, the quick menu and Sight's settings fit
//! into [`Sight`] (design/SIGHT.md §16). `lib.rs` routes messages here; the pieces are
//! `toasts`, `overlay`, `quick`, `screenshot`.

use std::time::{Duration, Instant};

use gpui::App;
use quiver_api::options::OptionOp;
use quiver_api::perf::{HudScope, Stat};
use quiver_api::sight::{OnUnlock, SightNotice, SightSettings};

use crate::link::{Loaded, LinkReq, Msg};
use crate::replay::{self, Recorder};
use crate::quick::{self, Act, TileKind};
use crate::toasts::{self, ToastData, ToastKind};
use crate::{Sight, overlay};

impl Sight {
    pub(crate) fn send_link(&self, r: LinkReq) {
        if let Some(l) = &self.link {
            let _ = l.send(r);
        }
    }

    /// The session something is for when it doesn't say: the panel's, the
    /// overlay's, else the first with a HUD model.
    fn some_session(&self) -> Option<String> {
        self.panel.session().map(str::to_owned).or_else(|| self.overlay.session().map(str::to_owned)).or_else(|| {
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
        self.refresh_quick(cx);
        self.sync_overlay(cx);
    }

    /// Where the overlay can be drawn now: a session whose layer shows images without
    /// capture, and that the panel doesn't hold.
    fn overlay_target(&self, session: Option<&str>) -> Option<overlay::Target> {
        let s = session.map(str::to_owned).or_else(|| self.toasts.session()).or_else(|| self.some_session())?;
        if self.panel.in_game() && self.panel.session() == Some(s.as_str()) {
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
        if self.toasts.tick(cx) { self.refresh_quick(cx); }
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
        match data {
            Loaded::All(d) => {
                if let Some(st) = d.session.ready().and_then(|v| v.started) {
                    self.started.insert(session.to_owned(), st);
                }
                self.quick_data.insert(session.to_owned(), (**d).clone());
            }
            part => {
                let Some(d) = self.quick_data.get_mut(session) else { return };
                match part {
                    Loaded::Options(options, _) => d.options = options.clone(),
                    Loaded::Tweaks(tweaks) => d.tweaks = tweaks.clone(),
                    Loaded::Hud(hud) => d.hud = hud.clone(),
                    Loaded::All(_) => unreachable!(),
                }
            }
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
        self.refresh_quick(cx);
    }

    // ---- Screenshots ---------------------------------------------------------------------

    /// A screenshot of the session's game after `after` (`link::LinkReq::Shot`: the layer's
    /// capture through quiverd, else grim); `say`: a toast with where it went.
    fn screenshot(&mut self, session: &str, after: Duration, say: bool) {
        let monitor = self.huds.get(session).and_then(|h| h.model.as_ref()?.monitor.clone());
        let game = self.quick_data.get(session).and_then(|d| d.session.ready().map(|v| v.name.clone())).filter(|n| !n.is_empty()).unwrap_or_else(|| "Game".into());
        self.send_link(LinkReq::Shot { session: session.to_owned(), after, monitor, game, say });
    }

    /// Rebuild the open panel's quick actions from current session data.
    pub(crate) fn refresh_quick(&mut self, cx: &mut App) {
        let Some(session) = self.panel.session().map(str::to_owned) else { return };
        let fallback = Default::default();
        let data = self.quick_data.get(&session).unwrap_or(&fallback);
        let pins = data.session.ready().map(|v| self.settings.pins_for(&v.game, v.profile())).unwrap_or_default();
        let replay = self.replay_state(&session);
        let notices: Vec<_> = self.toasts.answerable().into_iter().filter(|n| n.session.as_deref().is_none_or(|s| s == session)).collect();
        self.panel.set_quick(quick::tiles(data, &pins, &notices, replay), replay, cx);
    }

    pub(crate) fn quick_apply(&mut self, session: &str, t: &quick::Tile, cx: &mut App) {
        if self.panel.session() != Some(session) || !self.panel.nav(cx).is_some_and(|n| n.section == quiver_api::sight::PanelSection::Quick) { return; }
        let data = self.quick_data.get(session).cloned().unwrap_or_default();
        let Some(act) = quick::act(t, &data) else { return };
        self.panel.close(cx);
        self.chip(Some(session), false, cx);
        self.sync_overlay(cx);
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
            Act::Replay(action) => self.apply_replay(session, action, cx),
        }
        if matches!(t.kind, TileKind::Notice { .. }) {
            self.sync_overlay(cx);
        }
    }

    pub(crate) fn replay_state(&self, session: &str) -> replay::State {
        if !self.replay_enabled { return replay::State::Hidden; }
        if let Some(r) = self.recorders.get(session) {
            return if r.stopping() { replay::State::Stopping } else if r.ready { replay::State::Active(r.seconds) } else { replay::State::Starting };
        }
        match quick::replay_buffer(|pid| self.recorders.values().any(|r| r.pid() == Some(pid))) {
            Some((pid, seconds)) => replay::State::External { pid, seconds },
            None => replay::State::Off,
        }
    }

    fn replay_changed(&mut self, session: &str, cx: &mut App) {
        if self.panel.session() == Some(session) { self.refresh_quick(cx); }
    }

    pub(crate) fn apply_replay(&mut self, session: &str, action: replay::Action, cx: &mut App) {
        if !self.replay_enabled { return; }
        let result = match action {
            replay::Action::Start(seconds) => {
                if !matches!(self.replay_state(session), replay::State::Off) {
                    self.replay_changed(session, cx);
                    return;
                }
                let Some(game) = self.quick_data.get(session).and_then(|d| d.session.ready()).map(|v| v.name.clone()) else {
                    self.replay_feedback(session, Err("Game session details are unavailable".into()), cx);
                    return;
                };
                let monitor = self.panel.game_monitor(session);
                Recorder::start(&game, monitor.as_deref(), seconds).map(|r| {
                    self.recorders.insert(session.to_owned(), r);
                    "Starting replay buffer…".to_owned()
                })
            }
            replay::Action::Stop => {
                let Some(r) = self.recorders.get_mut(session) else { return };
                r.stop();
                Ok("Stopping replay buffer…".into())
            }
            replay::Action::Save | replay::Action::SaveExternal(_) => {
                let socket = self.recorders.get(session).filter(|r| r.ready && !r.stopping()).map(Recorder::socket);
                let session = session.to_owned();
                let tx = self.tx.clone();
                cx.background_executor().spawn(async move {
                    let result = match action {
                        replay::Action::SaveExternal(pid) => replay::save_external(pid),
                        _ => socket.ok_or_else(|| anyhow::anyhow!("the replay buffer is not ready"))
                            .and_then(|socket| replay::save(&socket)).map(|p| format!("Saved {p}")),
                    };
                    if let Some(tx) = tx {
                        let _ = tx.unbounded_send(Msg::ReplayFeedback { session, result: result.map_err(|e| format!("{e:#}")) });
                    }
                }).detach();
                return;
            }
        };
        self.replay_feedback(session, result.map_err(|e| format!("{e:#}")), cx);
        self.replay_changed(session, cx);
    }

    pub(crate) fn replay_feedback(&mut self, session: &str, result: Result<String, String>, cx: &mut App) {
        let (kind, title, body, urgent) = match result {
            Ok(text) => (ToastKind::Options, text, None, false),
            Err(reason) => (ToastKind::Error, "Replay unavailable".into(), Some(reason), true),
        };
        if self.panel.session() == Some(session) {
            self.panel.replay_feedback(body.as_deref().unwrap_or(&title).to_owned(), urgent, cx);
        }
        self.toast(ToastData { kind, title, body, urgent, caption: "Instant replay".into(), session: Some(session.into()), ..Default::default() }, cx);
    }

    pub(crate) fn poll_replays(&mut self, cx: &mut App) {
        let mut updates = Vec::new();
        self.recorders.retain(|session, r| {
            let was_ready = r.ready;
            match r.poll() {
                Err(e) => { updates.push((session.clone(), Err(e))); false }
                Ok(false) => { updates.push((session.clone(), Ok("Replay buffer stopped".into()))); false }
                Ok(true) => {
                    if r.ready && !was_ready { updates.push((session.clone(), Ok(format!("Buffering the last {} seconds", r.seconds)))); }
                    true
                }
            }
        });
        for (session, result) in updates {
            self.replay_feedback(&session, result.map_err(|e| format!("{e:#}")), cx);
            self.replay_changed(&session, cx);
        }
    }

    pub(crate) fn quick_session_ended(&mut self, session: &str, cx: &mut App) {
        self.quick_data.remove(session);
        self.fps.remove(session);
        self.started.remove(session);
        if self.overlay.session() == Some(session) {
            self.overlay.close(cx);
        }
    }
}
