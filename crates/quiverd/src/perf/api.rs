//! `Request::Perf` and the launch hooks, on the daemon.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use quiver_api::perf::{HudScopeRef, PerfRequest, SessionSummary};
use quiver_api::sight::{PanelAction, SightRequest};
use quiver_core::launcher;

use super::ConnCtx;
use crate::Daemon;

impl Daemon {
    /// Perf requests. Connection-bound ones (subscriptions, `HudAttach`) need `conn`.
    pub(crate) async fn handle_perf(self: &Arc<Self>, source: &str, conn: Option<ConnCtx>, req: PerfRequest) -> Result<Value> {
        let hub = &self.perf;
        let need_conn = || conn.clone().context("this request needs its own connection (subscribe on the socket)");
        Ok(match req {
            PerfRequest::Sessions => json!(hub.sessions()),
            PerfRequest::Catalog { session, game } => json!(hub.catalog(session.as_deref(), game)?),
            PerfRequest::Subscribe { session, game, select, every_ms, window_ms, stats } => {
                json!(hub.subscribe(&need_conn()?, session.as_deref(), game, select, every_ms, window_ms, stats)?)
            }
            PerfRequest::Unsubscribe { sub } => {
                hub.unsubscribe(need_conn()?.id, sub)?;
                Value::Null
            }
            PerfRequest::HudGet { scope } => {
                let scope = self.perf_scope(scope)?;
                json!(self.with_anti_cheat(hub.hud.view(&scope)?))
            }
            PerfRequest::HudSet { scope, set, unset, message } => {
                let scope = self.perf_scope(scope)?;
                if let Some(l) = &set.layout {
                    super::layout::load(hub.hud.dir(), l).map_err(|e| anyhow::anyhow!(e))?;
                }
                let v = hub.hud.apply(&scope, &set, &unset, source, message.as_deref())?;
                let what = serde_json::to_string(&set).unwrap_or_default();
                let unset_s = if unset.is_empty() { String::new() } else { format!(", unset {}", unset.join(",")) };
                self.log_activity_game(source, format!("HUD settings r{} ({}): {what}{unset_s}", v.revision, scope.label()), scope.game);
                hub.broadcast.send(quiver_api::perf::PerfEvent::HudChanged { product_id: scope.product_id(), session: None }).ok();
                // Running games take the new look now, not at their next launch.
                hub.settings_changed(&scope, &set);
                json!(self.with_anti_cheat(v))
            }
            PerfRequest::HudToggle { session, game } => json!(hub.toggle(session.as_deref(), game)?),
            PerfRequest::HudNext { session, game } => json!(hub.next(session.as_deref(), game)?),
            PerfRequest::HudAttach { session } => json!(hub.attach(&need_conn()?, &session)?),
            PerfRequest::HudStatus { session, game } => json!(hub.status(session.as_deref(), game)?),
            PerfRequest::HudLayouts => json!(super::layout::list(hub.hud.dir())),
            PerfRequest::HudLayout { id } => json!(super::layout::load(hub.hud.dir(), &id).map_err(|e| anyhow::anyhow!(e))?.to_api()),
            PerfRequest::HudKeybinds => json!(hub.keybinds()),
            PerfRequest::LaunchEnv { game, profile } => {
                let game = *game;
                let profile = match profile {
                    Some(p) => profile_id(game, &p, crate::mods::installed)?,
                    None => None,
                };
                // A dry run: the session dir a launch would get, without creating it.
                let dir = super::shm_root().join(format!("quiver-{}", quiver_perf::session::current_uid())).join("s-example");
                let env = [("QUIVER_SESSION", "s-example".to_owned()), ("QUIVER_PERF_DIR", dir.display().to_string())];
                let s = quiver_core::settings::current(game).map(|(s, _)| s).unwrap_or_default();
                let install = crate::mods::installed(game).ok().map(|g| g.install_dir);
                let (cfg, off, _) = hub.launch_config(&game, profile.as_deref(), install.as_deref(), &s.env, true);
                let mut plan = super::launch::plan(&cfg, &env, &s.env, &s.wrappers, super::launch::installed_layer_dir(hub.data_dir()).as_deref());
                super::launch::add_input_preload(&mut plan, &s.env, super::launch::installed_input_preload().as_deref());
                if let Some(why) = off {
                    plan.notes = vec![format!("no Sight layer: {why}; the launch is unchanged")];
                    plan.layer_off = Some(why);
                }
                json!(plan)
            }
            PerfRequest::Summaries { game, limit } => json!(summaries(&game, limit)),
            PerfRequest::FrameTimes { session, game, span_ms, buckets } => json!(hub.frame_times(session.as_deref(), game, span_ms, buckets)?),
            PerfRequest::SaveLog { session, game, seconds } => {
                let (saved, g) = hub.save_log(session.as_deref(), game, seconds)?;
                self.log_activity_game(source, format!("performance log ({} frames): {}", saved.frames, saved.path.display()), Some(g));
                json!(saved)
            }
            PerfRequest::HudPreview { session, layout } => json!(hub.preview(&session, &layout)?),
            PerfRequest::HudLayoutSave { layout } => {
                let info = super::layout::save(hub.hud.dir(), &layout).map_err(|e| anyhow::anyhow!(e))?;
                self.log_activity_game(source, format!("HUD layout {} saved ({})", info.id, info.name), None);
                json!(info)
            }
            PerfRequest::DevSession { product_id, stop, sight } => {
                if !matches!(source, "cli" | "dev" | "test") {
                    bail!("dev sessions are for the CLI and tests");
                }
                if stop {
                    let id = hub.sessions().into_iter().find(|s| s.dev && s.product_id == product_id).map(|s| s.session).context("no dev session for that product")?;
                    let summary = hub.end(&id);
                    json!({ "session": id, "summary": summary })
                } else {
                    if hub.product_running(product_id) {
                        bail!("product {product_id} already has a perf session");
                    }
                    let id = quiver_core::modsdk::new_session_id();
                    let began = hub.begin(&id, quiver_api::GameRef::ubisoft(product_id), None, None, None, &Default::default(), &[], true)?;
                    let mut info = hub.sessions().into_iter().find(|s| s.session == id).context("session vanished")?;
                    info.dev = true;
                    self.log_activity(source, format!("perf dev session {id} for {product_id}"), Some(product_id));
                    if sight {
                        hub.sight.ensure(hub.data_dir(), "dev session");
                    }
                    json!({ "info": info, "env": began.plan.env })
                }
            }
        })
    }

    /// `Request::Sight`: the in-game panel (`panel.rs`).
    pub(crate) fn handle_sight(&self, source: &str, req: SightRequest) -> Result<Value> {
        let hub = &self.perf;
        Ok(match req {
            SightRequest::Toggle { session, section } => json!(hub.panel_command(PanelAction::Toggle, session.as_deref(), section, source)?),
            SightRequest::Open { session, section } => json!(hub.panel_command(PanelAction::Open, session.as_deref(), section, source)?),
            SightRequest::Close => json!(hub.panel_command(PanelAction::Close, None, None, source)?),
            SightRequest::Status => json!(hub.panel_status()),
            SightRequest::Report { report } => {
                if source != "sight" {
                    bail!("only quiver-sight reports the panel's state");
                }
                hub.panel_report(report);
                Value::Null
            }
        })
    }

    /// A game scope's view with the anti-cheat found in the game's install folder (looked
    /// at again at most once a minute).
    fn with_anti_cheat(&self, mut v: quiver_api::perf::HudSettingsView) -> quiver_api::perf::HudSettingsView {
        if let Some(g) = v.scope.game
            && let Ok(game) = crate::mods::installed(g)
        {
            v.anti_cheat = self.perf.anti_cheat_in(&game.install_dir, false).map(|d| d.describe());
        }
        v
    }

    /// A scope with its profile name resolved to an id.
    fn perf_scope(&self, scope: HudScopeRef) -> Result<HudScopeRef> {
        resolve_scope(scope, crate::mods::installed)
    }

    /// The perf part of a launch: the session's registry and, when the launch gets the Sight
    /// layer (by default; not with anti-cheat in the install folder), its session dir and the
    /// launch's environment (added to `env`). Returns lines for the job's detail. A failure
    /// leaves the launch as it was.
    pub(crate) fn perf_launch(&self, session_id: &str, game: &launcher::Game, profile: Option<&str>, settings: &mut quiver_api::GameSettings) -> Vec<String> {
        if std::env::var("QUIVER_PERF").is_ok_and(|v| v == "0") {
            return Vec::new();
        }
        match self.perf.begin(session_id, game.game_ref(), profile, game.prefix(), Some(&game.install_dir), &settings.env, &settings.wrappers, false) {
            Ok(b) => {
                let enabled = b.plan.hud.as_ref().is_some_and(|h| h.enabled);
                settings.env.extend(b.plan.env);
                // Toasts are on for Quiver launches; the HUD renders in the same process
                // (nothing while it's hidden).
                self.perf.sight.ensure(self.perf.data_dir(), if enabled { "launch with the Sight layer" } else { "launch (toasts)" });
                b.plan.notes.into_iter().map(|n| format!("hud: {n}")).collect()
            }
            Err(e) => {
                tracing::warn!("perf: {e:#}");
                vec![format!("hud: not set up ({e:#})")]
            }
        }
    }
}

impl Daemon {
    /// The perf part of a `quiver steam-run` launch (`steam.rs`): its session and the
    /// environment for Steam's command, in `plan`. A failure leaves the plan as it was.
    pub(crate) fn perf_steam_launch(&self, plan: &mut quiver_api::steamlib::SteamLaunchPlan, steam_env: &std::collections::BTreeMap<String, String>) {
        if std::env::var("QUIVER_PERF").is_ok_and(|v| v == "0") {
            return;
        }
        match self.perf.begin_steam(plan, steam_env) {
            // Sight is on for Steam copies as for Quiver installs (SIGHT steer 15).
            Ok(()) => self.perf.sight.ensure(self.perf.data_dir(), "Steam launch"),
            Err(e) => {
                tracing::warn!(game = %plan.game_ref(), "perf for a Steam launch: {e:#}");
                plan.perf_notes.push(format!("hud: not set up ({e:#})"));
            }
        }
    }

    /// Every minute: end Steam launches' perf sessions whose game never started.
    pub(crate) fn start_perf_steam_sweep(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                let Some(d) = weak.upgrade() else { return };
                let playing: std::collections::HashSet<quiver_api::GameRef> = d.all_sessions().iter().map(|s| s.game_ref()).collect();
                d.perf.sweep_steam(|g| playing.contains(g), std::time::Instant::now());
            }
        });
    }
}

/// A profile name or id of `game`, as the id its HUD settings are stored under (`None`:
/// vanilla). Any store's game with profiles (Ubisoft, Epic, Steam: what mods see through
/// `installed`, as Tweaks' scopes do); a game without them says so.
fn profile_id(game: quiver_api::GameRef, name: &str, installed: impl Fn(quiver_api::GameRef) -> Result<launcher::Game>) -> Result<Option<String>> {
    profile_id_with(game, name, installed, crate::mods::resolve_default)
}

fn profile_id_with(
    game: quiver_api::GameRef,
    name: &str,
    installed: impl Fn(quiver_api::GameRef) -> Result<launcher::Game>,
    resolve: impl Fn(quiver_api::GameRef, &str) -> Result<String>,
) -> Result<Option<String>> {
    let v = name.trim();
    if v.is_empty() || v.eq_ignore_ascii_case(quiver_api::mods::VANILLA) {
        return Ok(None);
    }
    installed(game).with_context(|| format!("{game}: profile HUD settings need an installed game with profiles; set its HUD for the game"))?;
    let id = resolve(game, v)?;
    Ok(Some(id).filter(|id| !id.is_empty() && !id.eq_ignore_ascii_case(quiver_api::mods::VANILLA)))
}

/// [`Daemon::perf_scope`]: a profile needs a game; its name becomes its id.
fn resolve_scope(scope: HudScopeRef, installed: impl Fn(quiver_api::GameRef) -> Result<launcher::Game>) -> Result<HudScopeRef> {
    resolve_scope_with(scope, installed, crate::mods::resolve_default)
}

fn resolve_scope_with(
    mut scope: HudScopeRef,
    installed: impl Fn(quiver_api::GameRef) -> Result<launcher::Game>,
    resolve: impl Fn(quiver_api::GameRef, &str) -> Result<String>,
) -> Result<HudScopeRef> {
    let Some(prof) = scope.profile.clone() else { return Ok(scope) };
    let game = scope.game.context("a profile scope needs a game")?;
    scope.profile = profile_id_with(game, &prof, installed, resolve)?;
    Ok(scope)
}

/// A game's recorded sessions that carry a perf summary, newest first.
pub fn summaries(game: &quiver_api::GameRef, limit: usize) -> Vec<SessionSummary> {
    let path = quiver_core::paths::game_dir(game).join("playtime.jsonl");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<SessionSummary> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let perf = serde_json::from_value(v.get("perf")?.clone()).ok()?;
            Some(SessionSummary {
                started: serde_json::from_value(v.get("started")?.clone()).ok()?,
                seconds: v.get("seconds").and_then(Value::as_u64).unwrap_or(0),
                profile: v.get("profile").and_then(Value::as_str).map(str::to_owned),
                perf,
            })
        })
        .collect();
    out.reverse();
    out.truncate(limit.max(1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::GameRef;

    /// Ubisoft 3539, Epic's Alan Wake Remastered and a Steam game are installed; Steam 1 isn't.
    fn installed(g: GameRef) -> Result<launcher::Game> {
        if g == GameRef::steam(1) {
            bail!("not installed");
        }
        Ok(launcher::Game { product_id: 0, name: "x".into(), install_dir: "/games/x".into(), prefix: "/p".into(), exe: "x.exe".into(), args: None, language: None, game: Some(g) })
    }

    fn resolve(_: GameRef, v: &str) -> Result<String> {
        match v {
            "DLSS" | "p-dlss" => Ok("p-dlss".into()),
            other => bail!("no profile {other:?}"),
        }
    }

    #[test]
    fn profile_scopes_work_for_every_store_with_profiles() {
        let epic = GameRef::parse_ref("epic:7315f614b0504081bcdceef138075c9c").unwrap();
        for g in [GameRef::ubisoft(3539), epic, GameRef::steam(870780)] {
            let s = resolve_scope_with(HudScopeRef { game: Some(g), profile: Some("DLSS".into()) }, installed, resolve).unwrap();
            assert_eq!(s.profile.as_deref(), Some("p-dlss"), "{g}: a name becomes its id");
            assert_eq!(s.game, Some(g));
            // Vanilla is the game's own scope.
            let v = resolve_scope_with(HudScopeRef { game: Some(g), profile: Some("vanilla".into()) }, installed, resolve).unwrap();
            assert_eq!(v.profile, None, "{g}");
        }
        // No profile: as asked; a profile needs a game, an installed one, and to exist.
        let plain = HudScopeRef::of(GameRef::steam(1));
        assert_eq!(resolve_scope_with(plain.clone(), installed, resolve).unwrap(), plain);
        assert!(resolve_scope_with(HudScopeRef { game: None, profile: Some("DLSS".into()) }, installed, resolve).is_err());
        let e = resolve_scope_with(HudScopeRef { game: Some(GameRef::steam(1)), profile: Some("DLSS".into()) }, installed, resolve).unwrap_err();
        assert!(format!("{e:#}").contains("installed game with profiles"), "{e:#}");
        assert!(resolve_scope_with(HudScopeRef { game: Some(epic), profile: Some("Nope".into()) }, installed, resolve).is_err());
        // The launch env's profile too.
        assert_eq!(profile_id_with(epic, "DLSS", installed, resolve).unwrap().as_deref(), Some("p-dlss"));
        assert_eq!(profile_id_with(epic, "", installed, resolve).unwrap(), None);
    }
}
