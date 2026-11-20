//! Sight's notices and settings in quiverd (design/SIGHT.md §16).
//!
//! **Notices** are toasts over the running game that don't come from an event quiver-sight
//! already watches: quiverd's own (saves uploaded to the store's cloud, a mod's hotkey flipping
//! one of its options), agents' and the CLI's (`SightRequest::Notify`, `quiver sight notify`)
//! and mods' (`{"t":"notice"}` over the SDK). quiverd numbers them, fills in a caption, limits
//! each poster's rate, makes sure quiver-sight runs while a game does, and broadcasts
//! `Event::SightNotice`. A chosen button (`SightRequest::NoticeAct`) goes out as
//! `Event::SightNoticeAction`, and to the mod's connection when a mod posted it.
//!
//! **Settings** live in `$XDG_DATA_HOME/quiver/sight.toml` (quiver-sight reads it at start and
//! on `Event::SightSettingsChanged`; `quiver sight pad-chord` writes its key there directly).
//! Every writer keeps keys it doesn't know.
//!
//! **Mods' own changes** ([`ModSink`], the registry's options sink): a `changed` that isn't
//! persisted (a hotkey flip) goes to [`Daemon::options_from_mod`] (recorded as what the game
//! runs, shown, not saved) and becomes a notice naming the option, its value and the mod's
//! key for it ("Frame Generation off · You pressed F9 · this session only"). Persisted ones (the
//! mod's own menu) are logged, as before.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use quiver_api::Event;
use quiver_api::mods::ModSdkConnection;
use quiver_api::options::{OptionDef, OptionType, SurfaceState};
use quiver_api::sight::{NoticeButton, NoticeKind, NoticeLevel, NoticeSpec, QuickPin, SightNotice, SightSettings};
use quiver_core::modsdk::{ChangedValues, ModOptionsSink};
use quiver_proto::modsdk::ModNotice;
use serde_json::Value;

use crate::Daemon;

/// Notices a poster may post in [`RATE_WINDOW`] (quiverd's own aren't limited).
pub const RATE: usize = 6;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Posted notices kept for their buttons' answers.
const KEEP: usize = 64;

/// Who posted a notice, for its answer.
#[derive(Debug, Clone)]
struct Posted {
    from: String,
    tag: Option<String>,
    /// The mod connection that posted it.
    conn: Option<u64>,
    buttons: Vec<String>,
}

#[derive(Default)]
pub struct SightState {
    seq: Mutex<u64>,
    posted: Mutex<VecDeque<(String, Posted)>>,
    rate: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// The settings file (tests point it elsewhere); `None`: [`quiver_api::sight::settings_path`].
    path: OnceLock<PathBuf>,
    settings: Mutex<()>,
}

impl SightState {
    pub fn with_path(path: PathBuf) -> SightState {
        let s = SightState::default();
        let _ = s.path.set(path);
        s
    }

    fn path(&self) -> PathBuf {
        self.path.get().cloned().unwrap_or_else(quiver_api::sight::settings_path)
    }

    /// The settings as the file says now.
    pub fn settings(&self) -> SightSettings {
        quiver_api::sight::load_settings_at(&self.path())
    }

    /// Reads, changes and writes the settings under one lock.
    pub fn update(&self, f: impl FnOnce(&mut SightSettings) -> Result<(), String>) -> Result<SightSettings> {
        let _g = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.path();
        let mut s = quiver_api::sight::load_settings_at(&path);
        f(&mut s).map_err(anyhow::Error::msg)?;
        quiver_api::sight::save_settings_at(&path, &s)?;
        Ok(s)
    }

    /// Whether `from` may post another notice now (and counts it).
    pub fn allow(&self, from: &str, now: Instant) -> bool {
        if from == "quiverd" {
            return true;
        }
        let mut r = self.rate.lock().unwrap();
        let q = r.entry(from.to_owned()).or_default();
        while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= RATE_WINDOW) {
            q.pop_front();
        }
        if q.len() >= RATE {
            return false;
        }
        q.push_back(now);
        true
    }

    fn next_id(&self) -> String {
        let mut s = self.seq.lock().unwrap();
        *s += 1;
        format!("n-{}", *s)
    }

    fn remember(&self, id: &str, p: Posted) {
        let mut q = self.posted.lock().unwrap();
        q.push_back((id.to_owned(), p));
        while q.len() > KEEP {
            q.pop_front();
        }
    }

    fn find(&self, id: &str) -> Option<Posted> {
        self.posted.lock().unwrap().iter().rev().find(|(i, _)| i == id).map(|(_, p)| p.clone())
    }

    /// A notice as posted: checked, captioned, numbered (no session resolution).
    pub fn make(&self, from: &str, spec: NoticeSpec, conn: Option<u64>) -> Result<SightNotice> {
        let mut spec = spec.checked().map_err(anyhow::Error::msg)?;
        if spec.caption.is_none() {
            spec.caption = default_caption(from);
        }
        let n = SightNotice { id: self.next_id(), from: from.to_owned(), at: Utc::now(), spec };
        self.remember(&n.id, Posted { from: n.from.clone(), tag: n.spec.tag.clone(), conn, buttons: n.spec.buttons.iter().map(|b| b.id.clone()).collect() });
        Ok(n)
    }

    /// The answer to a notice's button: who to tell.
    pub fn answer(&self, id: &str, button: &str) -> Result<(String, Option<String>, Option<u64>)> {
        let Some(p) = self.find(id) else { bail!("no notice {id} (they're kept for the last {KEEP})") };
        if !p.buttons.iter().any(|b| b == button) {
            bail!("notice {id} has no button {button:?} (it has: {})", p.buttons.join(", "));
        }
        Ok((p.from, p.tag, p.conn))
    }
}

/// "Agent · claude-code" for an agent, the surface for a mod (quiverd's own say their own).
fn default_caption(from: &str) -> Option<String> {
    if let Some(a) = from.strip_prefix("agent:") {
        return Some(format!("Agent · {a}"));
    }
    if from == "agent" {
        return Some("Agent".into());
    }
    from.strip_prefix("mod:").map(str::to_owned)
}

/// A mod's notice as a spec (the mod's id is the tag; its first button is the filled one).
pub fn from_mod(m: &ModSdkConnection, n: &ModNotice) -> NoticeSpec {
    let level = match n.level.as_deref() {
        Some("success") => NoticeLevel::Success,
        Some("warning") | Some("warn") => NoticeLevel::Warning,
        Some("error") => NoticeLevel::Error,
        _ => NoticeLevel::Info,
    };
    NoticeSpec {
        kind: if level == NoticeLevel::Error { NoticeKind::Error } else { NoticeKind::Notice },
        level,
        caption: Some(m.surface.clone()),
        title: n.title.clone(),
        body: n.body.clone(),
        session: m.session.clone(),
        buttons: n.buttons.iter().enumerate().map(|(i, b)| NoticeButton { id: b.id.clone(), label: b.label.clone(), primary: i == 0 }).collect(),
        tag: Some(n.id.clone()).filter(|t| !t.is_empty()),
        ..Default::default()
    }
}

/// An option's value in words ("off", "Balanced", "1.6").
fn value_words(d: &OptionDef, v: &Value) -> String {
    match d.ty {
        OptionType::Bool => if v.as_bool().unwrap_or(false) { "on" } else { "off" }.into(),
        _ => {
            if let Some(l) = d.values.iter().find(|e| quiver_api::options::same_value(&e.value, v)).and_then(|e| e.label.clone()) {
                return l;
            }
            if let Some(s) = d.special_of(v) {
                return s.label.clone();
            }
            v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())
        }
    }
}

/// The key the mod flips `id` with: an action whose `hotkey_option` names a key option, the
/// action and the option sharing their first id segment (`fg.toggle` ↔ `fg.enabled`), its
/// current value (else its default).
fn hotkey_for(surface: &SurfaceState, id: &str) -> Option<String> {
    let head = id.split('.').next()?;
    let schema_actions = surface.actions.iter().filter(|(aid, _)| aid.split('.').next() == Some(head));
    for (_, a) in schema_actions {
        let Some(k) = &a.hotkey else { continue };
        if !k.is_empty() {
            return Some(k.clone());
        }
    }
    None
}

/// The notice for a mod's hotkey flip, from its surface's schema (labels, the hotkey).
pub fn hotkey_notice(surface: &SurfaceState, values: &BTreeMap<String, Value>, session: &str) -> Option<NoticeSpec> {
    let (id, v) = values.iter().next()?;
    let st = surface.options.get(id)?;
    let label = st.def.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| id.clone());
    let mut title = format!("{label} {}", value_words(&st.def, v));
    if values.len() > 1 {
        title = format!("{title} (and {} more)", values.len() - 1);
    }
    let key = hotkey_for(surface, id);
    Some(NoticeSpec {
        kind: NoticeKind::ModHotkey,
        level: NoticeLevel::Info,
        caption: Some(surface.title.clone()).filter(|t| !t.is_empty()),
        title,
        body: Some(if key.is_some() { "You pressed {key} · this session only".to_owned() } else { "Changed in the game · this session only".to_owned() }),
        key,
        session: Some(session.to_owned()),
        tag: Some(format!("hotkey:{}:{id}", surface.key)),
        ..Default::default()
    })
}

impl Daemon {
    /// Posts a notice (`SightRequest::Notify`, the SDK, quiverd itself).
    pub(crate) fn notify(&self, from: &str, mut spec: NoticeSpec, conn: Option<u64>) -> Result<SightNotice> {
        if !self.sight.allow(from, Instant::now()) {
            bail!("too many notices from {from}: at most {RATE} a minute");
        }
        // The session: the one named (it must be running), else the focused or only game.
        let sessions = self.perf.sessions();
        if sessions.is_empty() {
            bail!("no game is running: notices show over a game");
        }
        match &spec.session {
            Some(s) if !sessions.iter().any(|x| &x.session == s) => bail!("no running session {s}"),
            Some(_) => {}
            None => spec.session = self.perf.pick_session(None).ok(),
        }
        let n = self.sight.make(from, spec, conn)?;
        self.perf.sight.ensure(self.perf.data_dir(), "a notice");
        if from != "quiverd" {
            let game = sessions.iter().find(|s| Some(&s.session) == n.spec.session.as_ref()).map(|s| s.game_ref());
            self.log_activity_game(from, format!("Sight notice {}: {}", n.id, n.spec.title), game);
        } else {
            tracing::info!(id = %n.id, kind = ?n.spec.kind, title = %n.spec.title, "Sight notice");
        }
        self.emit(Event::SightNotice(n.clone()));
        Ok(n)
    }

    /// A notice's button was chosen.
    pub(crate) fn notice_act(&self, id: &str, button: &str, by: &str) -> Result<()> {
        let (from, tag, conn) = self.sight.answer(id, button)?;
        if let Some(c) = conn {
            let mod_id = tag.as_deref().unwrap_or(id);
            if !self.mod_sdk.notice_action(c, mod_id, button) {
                tracing::info!(id, button, "the mod that posted the notice has gone");
            }
        }
        tracing::info!(id, button, %from, by, "Sight notice answered");
        self.emit(Event::SightNoticeAction { id: id.to_owned(), button: button.to_owned(), from, tag, by: by.to_owned() });
        Ok(())
    }

    pub(crate) fn sight_settings_set(&self, key: &str, value: &Value, source: &str) -> Result<SightSettings> {
        let s = self.sight.update(|s| s.set(key, value))?;
        self.log_activity_game(source, format!("Sight setting {key} = {}", s.get(key).map(|v| v.to_string()).unwrap_or_default()), None);
        self.emit(Event::SightSettingsChanged(s.clone()));
        Ok(s)
    }

    pub(crate) fn sight_pin(&self, scope: &str, pin: QuickPin, on: bool, source: &str) -> Result<SightSettings> {
        let what = format!("{}{}", if pin.surface.is_empty() { String::new() } else { format!("{} ", pin.surface) }, pin.id);
        let s = self.sight.update(|s| s.pin(scope, pin, on))?;
        self.log_activity_game(source, format!("{} {what} {} the quick menu ({scope})", if on { "pinned" } else { "unpinned" }, if on { "to" } else { "from" }), None);
        self.emit(Event::SightSettingsChanged(s.clone()));
        Ok(s)
    }

    /// Saves went up to the store's cloud after a session: a notice (shown on the desktop, the
    /// game has gone).
    pub(crate) fn notice_cloud_saved(&self, session: &str, what: String) {
        let spec = NoticeSpec { kind: NoticeKind::CloudSaved, level: NoticeLevel::Success, title: "Saved to the cloud".into(), body: Some(what), session: Some(session.to_owned()), ..Default::default() };
        if let Err(e) = self.notify("quiverd", spec, None) {
            tracing::debug!("cloud-save notice: {e:#}");
        }
    }
}

/// The registry's options sink in quiverd (see the module docs).
pub struct ModSink {
    daemon: OnceLock<Weak<Daemon>>,
}

impl ModSink {
    pub fn new() -> Arc<ModSink> {
        Arc::new(ModSink { daemon: OnceLock::new() })
    }

    pub fn attach(&self, d: &Arc<Daemon>) {
        let _ = self.daemon.set(Arc::downgrade(d));
    }

    fn daemon(&self) -> Option<Arc<Daemon>> {
        self.daemon.get()?.upgrade()
    }
}

impl ModOptionsSink for ModSink {
    fn changed(&self, m: &ModSdkConnection, values: &ChangedValues, persist: bool) {
        tracing::info!(product_id = m.product_id, surface = %m.surface, persist, ?values, "mod changed its own options");
        if persist {
            return;
        }
        let (Some(d), Some(session)) = (self.daemon(), m.session.clone()) else { return };
        let (surface, values) = (m.surface.clone(), values.clone());
        tokio::spawn(async move {
            if let Err(e) = d.options_from_mod(&session, &surface, values.clone(), false).await {
                tracing::debug!(%session, %surface, "a mod's change: {e:#}");
                return;
            }
            let Some((st, typed)) = d.options_surface_values(&session, &surface, &values) else { return };
            if let Some(spec) = hotkey_notice(&st, &typed, &session)
                && let Err(e) = d.notify("quiverd", spec, None)
            {
                tracing::debug!("hotkey notice: {e:#}");
            }
        });
    }

    fn handles(&self, m: &ModSdkConnection) {
        tracing::info!(product_id = m.product_id, surface = %m.surface, options = ?m.options, "mod applies these options live");
    }

    fn notice(&self, m: &ModSdkConnection, n: &ModNotice) {
        let Some(d) = self.daemon() else { return };
        let from = format!("mod:{}", m.surface);
        if let Err(e) = d.notify(&from, from_mod(m, n), Some(m.conn)) {
            tracing::info!(surface = %m.surface, "mod notice not shown: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::options::{ActionInfo, OptionState};
    use quiver_api::sight::PinKind;
    use serde_json::json;

    #[test]
    fn rate_per_poster() {
        let s = SightState::default();
        let t = Instant::now();
        for _ in 0..RATE {
            assert!(s.allow("agent:a", t));
        }
        assert!(!s.allow("agent:a", t), "over the rate");
        assert!(s.allow("agent:b", t), "per poster");
        assert!(s.allow("quiverd", t) && s.allow("quiverd", t), "quiverd's own aren't limited");
        assert!(s.allow("agent:a", t + RATE_WINDOW), "the window moves on");
    }

    #[test]
    fn notices_are_checked_captioned_and_answered() {
        let s = SightState::default();
        let spec = NoticeSpec {
            title: "DLSS v4 is ready".into(),
            body: Some("Your DLSS profile picks it up next launch.".into()),
            buttons: vec![NoticeButton { id: "keep_v3".into(), label: "Keep v3 for now".into(), primary: true }, NoticeButton { id: "notes".into(), label: "Notes".into(), primary: false }],
            ..Default::default()
        };
        let n = s.make("agent:aco-dlss", spec.clone(), None).unwrap();
        assert_eq!((n.id.as_str(), n.spec.caption.as_deref()), ("n-1", Some("Agent · aco-dlss")));
        assert_eq!(n.spec.secs(), quiver_api::sight::NOTICE_ACTION_SECS);
        assert_eq!(s.answer("n-1", "notes").unwrap().0, "agent:aco-dlss");
        assert!(s.answer("n-1", "delete").unwrap_err().to_string().contains("no button"));
        assert!(s.answer("n-9", "notes").is_err());
        // No title, too many buttons.
        assert!(s.make("cli", NoticeSpec::default(), None).is_err());
        let mut many = spec.clone();
        many.buttons = (0..4).map(|i| NoticeButton { id: format!("b{i}"), label: "x".into(), primary: false }).collect();
        assert!(s.make("cli", many, None).is_err());
        // A mod's: its surface is the caption, its id the tag, the first button filled.
        let m = ModSdkConnection { conn: 7, surface: "aco-dlss".into(), session: Some("s-1".into()), ..Default::default() };
        let mn = ModNotice { id: "dlss4".into(), title: "DLSS v4 is ready".into(), body: None, level: Some("warning".into()), buttons: vec![quiver_proto::modsdk::ModNoticeButton { id: "keep_v3".into(), label: "Keep v3".into() }] };
        let n = s.make("mod:aco-dlss", from_mod(&m, &mn), Some(m.conn)).unwrap();
        assert_eq!((n.spec.caption.as_deref(), n.spec.tag.as_deref(), n.spec.level, n.spec.buttons[0].primary), (Some("aco-dlss"), Some("dlss4"), NoticeLevel::Warning, true));
        assert_eq!(s.answer(&n.id, "keep_v3").unwrap(), ("mod:aco-dlss".into(), Some("dlss4".into()), Some(7)));
    }

    #[test]
    fn hotkey_flips_read_like_the_mock() {
        let mut opts = BTreeMap::new();
        opts.insert("fg.enabled".to_owned(), OptionState { def: OptionDef { ty: OptionType::Bool, label: Some("Frame Generation".into()), ..Default::default() }, ..Default::default() });
        let mut actions = BTreeMap::new();
        actions.insert("fg.toggle".to_owned(), ActionInfo { label: "Toggle Frame Generation".into(), hotkey: Some("F9".into()), ..Default::default() });
        let st = SurfaceState { key: "m1/aco-dlss".into(), title: "DLSS".into(), options: opts, actions, ..Default::default() };
        let vals: BTreeMap<String, Value> = [("fg.enabled".to_owned(), json!(false))].into_iter().collect();
        let n = hotkey_notice(&st, &vals, "s-1").unwrap();
        assert_eq!((n.caption.as_deref(), n.title.as_str(), n.key.as_deref()), (Some("DLSS"), "Frame Generation off", Some("F9")));
        assert_eq!(n.body.as_deref(), Some("You pressed {key} · this session only"));
        assert_eq!(n.kind, NoticeKind::ModHotkey);
        // No action for it: still said, without a key.
        let st2 = SurfaceState { actions: BTreeMap::new(), ..st.clone() };
        let n = hotkey_notice(&st2, &vals, "s-1").unwrap();
        assert_eq!((n.key, n.body.as_deref()), (None, Some("Changed in the game · this session only")));
        // An option the schema doesn't know: nothing to say.
        let other: BTreeMap<String, Value> = [("x.y".to_owned(), json!(1))].into_iter().collect();
        assert!(hotkey_notice(&st, &other, "s-1").is_none());
    }

    #[test]
    fn settings_round_trip_and_keep_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sight.toml");
        std::fs::write(&p, "pad_chord = true\nfuture_key = \"kept\"\n").unwrap();
        let s = SightState::with_path(p.clone());
        assert!(s.settings().pad_chord);
        s.update(|x| x.set("toasts.dnd", &json!(true))).unwrap();
        s.update(|x| x.set("achievements.on_unlock", &json!("toast_screenshot"))).unwrap();
        let pin = QuickPin { kind: PinKind::Option, surface: "m1/aco-dlss".into(), id: "world.npc_density".into(), label: "Crowd density".into() };
        s.update(|x| x.pin("ubisoft:3539/dlss", pin.clone(), true)).unwrap();
        let back = s.settings();
        assert!(back.toasts.dnd && back.pad_chord);
        assert_eq!(back.achievements.on_unlock, quiver_api::sight::OnUnlock::ToastScreenshot);
        assert_eq!(back.pins_for(&quiver_api::GameRef::ubisoft(3539), Some("dlss")), vec![pin]);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("future_key = \"kept\""), "{text}");
        assert!(s.update(|x| x.set("toasts.loud", &json!(true))).is_err());
    }
}
