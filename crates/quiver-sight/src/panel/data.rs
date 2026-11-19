//! What the panel asks quiverd (on the link's tokio thread): the session and its game, the
//! running profile's mod options, the game's tweaks, the HUD, achievements; and the changes
//! the panel makes. Each part fails on its own ([`Load::Failed`]), so a missing piece
//! never blanks the panel.

use anyhow::{Context, Result};
use quiver_api::options::{Level, OptionOp, OptionsChange, OptionsRequest, SurfaceKind, SurfaceState};
use quiver_api::perf::{HudScope, HudScopeRef, HudSettingsView, HudStatus, PerfRequest, PerfSessionInfo};
use quiver_api::tweaks::{Scope, TweaksRequest};
use quiver_api::{ActiveSession, Client, GameRef, Request};

use super::state::{Ach, Achs, HudState, Load, PanelData, SessionView};

/// Which parts to (re)load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    All,
    Options,
    Tweaks,
    Hud,
}

fn load<T>(r: Result<T>) -> Load<T> {
    match r {
        Ok(t) => Load::Ready(t),
        Err(e) => Load::Failed(format!("{e:#}")),
    }
}

/// The session's identity: from `Sessions` (Quiver launches, Steam copies), else the perf
/// registry (dev sessions).
pub async fn session(client: &Client, id: &str) -> Result<SessionView> {
    let active: Vec<ActiveSession> = client.call(Request::Sessions).await.unwrap_or_default();
    let perf: Vec<PerfSessionInfo> = client.call(Request::Perf(PerfRequest::Sessions)).await.unwrap_or_default();
    let a = active.into_iter().find(|s| s.session.as_deref() == Some(id));
    let p = perf.into_iter().find(|s| s.session == id);
    let (game, profile, started, pids, reported, steam_app) = match (&a, &p) {
        (Some(a), _) => (a.game_ref(), a.profile.clone(), Some(a.started), a.pids.clone(), a.game.is_none().then_some(a.reported), a.steam_app_id),
        (None, Some(p)) => (p.game_ref(), p.profile.clone(), Some(p.started), Vec::new(), None, None),
        (None, None) => anyhow::bail!("session {id} isn't running"),
    };
    let mut v = SessionView { session: id.into(), game, profile_id: profile, started, pids, reported, copy: copy_label(game, steam_app), ..Default::default() };
    match client.call::<quiver_api::stores::GameDetail>(Request::GameDetail { game, refresh: false }).await {
        Ok(d) => {
            v.name = d.name;
            v.total_secs = d.playtime.map(|p| p.total_seconds);
        }
        Err(e) => {
            tracing::debug!("game detail: {e:#}");
            v.name = game.to_string();
        }
    }
    if let Some(pid) = v.profile().map(str::to_owned) {
        match client.call::<quiver_api::mods::ModsState>(Request::Mods(quiver_api::mods::ModsRequest::List { game: game.into() })).await {
            Ok(m) => {
                if let Some(p) = m.profiles.iter().find(|p| p.profile.id == pid) {
                    v.profile_name = Some(p.profile.name.clone());
                    v.mods = p.resolved.iter().map(|r| r.describe()).collect();
                    v.settings = Some(settings_words(p));
                }
            }
            Err(e) => tracing::debug!("mods: {e:#}"),
        }
        if v.profile_name.is_none() {
            v.profile_name = Some(pid);
        }
    }
    Ok(v)
}

/// Which copy of the game runs, for the header: a Ubisoft game Quiver installed or its Steam
/// copy, a Steam game (Steam's install), or another store's game Quiver installed.
pub fn copy_label(game: GameRef, steam_app: Option<u32>) -> String {
    if game.is_ubisoft() {
        return if steam_app.is_some() { "Steam copy".into() } else { "Quiver install".into() };
    }
    if game.as_steam().is_some() {
        return "Steam".into();
    }
    format!("{} · Quiver install", game.store.label())
}

/// A profile's game settings in words, for the Session section.
pub fn settings_words(p: &quiver_api::mods::ProfileInfo) -> String {
    if p.config_dirs.is_empty() {
        return "Vanilla's files (shared)".into();
    }
    let dirs: Vec<String> = p
        .config_dirs
        .iter()
        .map(|d| {
            let parts: Vec<&str> = d.dir.split('/').filter(|x| !x.is_empty()).collect();
            parts[parts.len().saturating_sub(2)..].join("/")
        })
        .collect();
    format!("its own ({})", dirs.join(", "))
}

/// The running profile's option surfaces, without Quiver's own (the Tweaks section has
/// those).
pub async fn options(client: &Client, s: &SessionView) -> Load<Vec<SurfaceState>> {
    let r: Result<Vec<SurfaceState>> =
        client.call(Request::Options(OptionsRequest::List { game: s.game.into(), profile: Some(s.profile_id.clone().unwrap_or_else(|| quiver_api::mods::VANILLA.into())), level: Some(Level::Advanced) })).await;
    load(r.map(|v| v.into_iter().filter(|x| x.kind != SurfaceKind::Platform).collect()))
}

/// The scope the Tweaks section edits: the running profile when there is one (a change
/// there always reaches this game), else the game.
pub fn tweak_scope(s: &SessionView) -> (Scope, String) {
    match s.profile() {
        Some(p) => (Scope::Profile { game: s.game, profile: Some(p.to_owned()) }, format!("the {} profile", s.profile_name.as_deref().unwrap_or(p))),
        _ => (Scope::Game { game: s.game }, "this game".into()),
    }
}

pub async fn tweaks(client: &Client, s: &SessionView) -> Load<SurfaceState> {
    let (scope, _) = tweak_scope(s);
    load(client.call(Request::Tweaks(TweaksRequest::List { scope, level: Some(Level::Advanced) })).await)
}

pub async fn hud(client: &Client, s: &SessionView) -> Load<HudState> {
    let status: Option<HudStatus> = client.call(Request::Perf(PerfRequest::HudStatus { session: Some(s.session.clone()), game: None })).await.ok();
    let settings: Result<HudSettingsView> = client.call(Request::Perf(PerfRequest::HudGet { scope: HudScopeRef::of(s.game) })).await;
    load(settings.map(|settings| HudState { status, settings }))
}

pub async fn achievements(client: &Client, game: GameRef) -> Load<Achs> {
    let d: Result<quiver_api::stores::GameDetail> = client.call(Request::GameDetail { game, refresh: false }).await;
    match d {
        Ok(d) => match d.achievements {
            Some(a) if !a.list.is_empty() => Load::Ready(Achs::sorted(
                a.list
                    .into_iter()
                    .map(|x| Ach { id: x.id, name: x.name, description: x.description, hidden: x.hidden, unlocked: x.unlocked, unlocked_at: x.unlocked_at })
                    .collect(),
            )),
            _ => Load::Unavailable("No achievements known for this game (Quiver hasn't read them from the store yet).".into()),
        },
        Err(e) => Load::Failed(format!("{e:#}")),
    }
}

/// Loads `part` into `d` (`All` also the session itself).
pub async fn refresh(client: &Client, session: &str, part: Part, d: &mut PanelData) {
    if part == Part::All || d.session.ready().is_none() {
        d.session = load(session_view(client, session).await);
    }
    let Some(s) = d.session.ready().cloned() else {
        let why = match &d.session {
            Load::Failed(e) => e.clone(),
            _ => "no session".into(),
        };
        for l in [&mut d.tweaks as &mut dyn SetFailed, &mut d.options, &mut d.hud, &mut d.achievements] {
            l.set_failed(&why);
        }
        return;
    };
    if matches!(part, Part::All | Part::Options) {
        d.options = options(client, &s).await;
    }
    if matches!(part, Part::All | Part::Tweaks) {
        d.tweak_scope_name = tweak_scope(&s).1;
        d.tweaks = tweaks(client, &s).await;
    }
    if matches!(part, Part::All | Part::Hud) {
        d.hud = hud(client, &s).await;
    }
    if part == Part::All {
        d.achievements = achievements(client, s.game).await;
    }
}

async fn session_view(client: &Client, id: &str) -> Result<SessionView> {
    session(client, id).await.with_context(|| format!("session {id}"))
}

trait SetFailed {
    fn set_failed(&mut self, why: &str);
}

impl<T> SetFailed for Load<T> {
    fn set_failed(&mut self, why: &str) {
        *self = Load::Failed(why.to_owned());
    }
}

// ---- Changes ---------------------------------------------------------------------------------

pub async fn set_option(client: &Client, s: &SessionView, op: OptionOp) -> Result<OptionsChange> {
    let profile = Some(s.profile_id.clone().unwrap_or_else(|| quiver_api::mods::VANILLA.into()));
    client
        .call(Request::Options(OptionsRequest::Set { game: s.game.into(), profile, ops: vec![op], live: Default::default(), message: Some("in game (Sight)".into()), confirmed: false }))
        .await
}

pub async fn set_tweak(client: &Client, s: &SessionView, op: OptionOp) -> Result<OptionsChange> {
    let (scope, _) = tweak_scope(s);
    client.call(Request::Tweaks(TweaksRequest::Set { scope, ops: vec![op], message: Some("in game (Sight)".into()), confirmed: false })).await
}

pub async fn hud_set(client: &Client, game: GameRef, set: HudScope) -> Result<HudSettingsView> {
    client.call(Request::Perf(PerfRequest::HudSet { scope: HudScopeRef::of(game), set, unset: vec![], message: Some("in game (Sight)".into()) })).await
}

pub async fn hud_toggle(client: &Client, session: &str) -> Result<HudStatus> {
    client.call(Request::Perf(PerfRequest::HudToggle { session: Some(session.into()), game: None })).await
}

/// Force quit: quiverd stops the game (Wine's `wineserver -k` for Quiver installs).
pub async fn stop(client: &Client, game: GameRef) -> Result<()> {
    client.call::<serde_json::Value>(Request::Stop { game: game.into() }).await.map(|_| ())
}

/// The key metrics, twice a second while the panel shows them.
pub const METRICS: [&str; 9] = ["fps", "fps.base", "frametime", "gpu.load", "gpu.temp", "gpu.power", "gpu.vram.used", "cpu.load", "mem.used"];

pub async fn subscribe(client: &Client, session: &str) -> Result<u64> {
    use quiver_api::perf::Stat;
    client
        .call(Request::Perf(PerfRequest::Subscribe {
            session: Some(session.into()),
            game: None,
            select: METRICS.iter().map(|s| s.to_string()).collect(),
            every_ms: 500,
            window_ms: 1000,
            stats: vec![Stat::Last, Stat::Mean, Stat::P1low],
        }))
        .await
}

pub async fn unsubscribe(client: &Client, sub: u64) {
    let _ = client.call::<serde_json::Value>(Request::Perf(PerfRequest::Unsubscribe { sub })).await;
}
