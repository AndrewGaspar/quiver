//! `Request::Tweaks`: Quiver Tweaks (design/TWEAKS.md; `quiver_core::tweaks` does the
//! resolution, the mechanisms and the evidence). Launches get the compiled environment and,
//! when a tweak is set, a session log dir that is tailed for evidence while the game runs;
//! each tweak's effect is recorded with the session, reported as `Event::TweaksEffect` and as
//! `tweaks.*` metrics in the session's perf catalog.
//!
//! **Agent policy** (steer 12): game and profile tweaks are Allow; global tweaks, `prefer` at
//! the global scope, restores and `danger` tweaks are Ask (refused without `confirmed`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use quiver_api::mods::{VANILLA, is_human_source};
use quiver_api::options::{OpOutcome, OptionsChange, OptionsRevision, Outcome};
use quiver_api::tweaks::{DisplayCaps, Effect, Planned, Scope, ScopeKind, SessionTweaks, TweakEffect, TweakRevisions, TweaksPlan, TweaksRequest};
use quiver_api::{Event, GameRef};
use quiver_core::launcher::{self, Games};
use quiver_core::tweaks::{self as tw, At, Scopes};

use crate::Daemon;
use crate::mods::{launch_profile, locked, store_for, with_store_lock};

/// A session with tweaks, as quiverd tracks it.
#[derive(Debug, Clone)]
struct Live {
    record: SessionTweaks,
    plan: TweaksPlan,
    dir: PathBuf,
    prefix: Option<PathBuf>,
}

#[derive(Default)]
pub struct Tweaks {
    sessions: Mutex<HashMap<String, Live>>,
}

impl Tweaks {
    fn get(&self, s: &str) -> Option<Live> {
        self.sessions.lock().unwrap().get(s).cloned()
    }

    fn running(&self, game: &GameRef) -> bool {
        self.sessions.lock().unwrap().values().any(|l| l.record.game == *game)
    }
}

fn data() -> PathBuf {
    launcher::data_dir()
}

fn ask(source: &str, confirmed: bool, what: &str) -> Result<()> {
    if is_human_source(source) || confirmed {
        return Ok(());
    }
    bail!("{what} needs your confirmation when requested by '{source}' (agent policy: Ask). Run it in a terminal, where quiver asks, or without --source")
}

fn installed(product_id: u32) -> Option<launcher::Game> {
    Games::load().ok()?.get(product_id).cloned()
}

/// The profile id a request means for an installed Ubisoft game (`None`: its default).
fn profile_id(game: &launcher::Game, profile: Option<&str>) -> Result<String> {
    let store = store_for(game);
    let (p, _) = launch_profile(&store, game.product_id, profile)?;
    Ok(match p {
        Some(q) if !q.eq_ignore_ascii_case(VANILLA) => store.profile(&q)?.0.id,
        _ => VANILLA.to_owned(),
    })
}

/// The game's display right now (Hyprland, read-only), for `fps.cap = vrr`.
fn refresh_display() -> Option<DisplayCaps> {
    let sock = crate::perf::display::socket()?;
    let st = crate::perf::display::DisplayState::poll(&sock)?;
    let m = st.game_monitor(false)?;
    let d = DisplayCaps { name: m.name.clone(), refresh_hz: m.refresh_rate, vrr: m.vrr, hdr: None };
    tw::set_display(Some(d.clone()));
    Some(d)
}

/// A scope addressed by a request, resolved: where it's stored, the scopes it sees, the
/// game and profile.
struct Resolved {
    kind: ScopeKind,
    game: Option<GameRef>,
    installed: Option<launcher::Game>,
    profile: Option<String>,
    scopes: Scopes,
}

fn resolve_scope(scope: &Scope) -> Result<Resolved> {
    let data = data();
    match scope {
        Scope::Global => Ok(Resolved { kind: ScopeKind::Global, game: None, installed: None, profile: None, scopes: Scopes::load(&data, None, None)? }),
        Scope::Game { game } => {
            let installed = game.as_ubisoft().and_then(installed);
            Ok(Resolved { kind: ScopeKind::Game, game: Some(*game), installed, profile: None, scopes: Scopes::load(&data, Some(game), None)? })
        }
        Scope::Profile { game, profile } => {
            let product = game.as_ubisoft().with_context(|| format!("{game} has no mod profiles: set its tweaks for the game (--game) or globally"))?;
            let g = installed(product).context("not installed (profile tweaks need an installed game)")?;
            let pid = profile_id(&g, profile.as_deref())?;
            let store = store_for(&g);
            let scopes = Scopes::load(&data, Some(game), Some((&store, &pid)))?;
            Ok(Resolved { kind: ScopeKind::Profile, game: Some(*game), installed: Some(g), profile: Some(pid), scopes })
        }
    }
}

impl Resolved {
    fn at<'a>(&self, data: &'a Path, store: &'a Option<quiver_core::mods::ModStore>) -> At<'a> {
        match (self.kind, self.game, store) {
            (ScopeKind::Profile, _, Some(s)) => At::Profile(s, self.profile.clone().unwrap_or_else(|| VANILLA.into())),
            (ScopeKind::Game, Some(g), _) | (ScopeKind::Profile, Some(g), None) => At::Game(data, g),
            _ => At::Global(data),
        }
    }

    /// The values below this scope (what a reset falls back to).
    fn inherited(&self) -> BTreeMap<String, Value> {
        let below = match self.kind {
            ScopeKind::Global => return BTreeMap::new(),
            ScopeKind::Game => ScopeKind::Global,
            ScopeKind::Profile => ScopeKind::Game,
        };
        tw::effective(&tw::resolve(tw::schema(), &self.scopes, below))
    }
}

/// The launch environment a preview starts from: Quiver's `env`, then the game's settings
/// resolved through the profile's stack.
fn preview_env(cfg: &launcher::Config, g: Option<&launcher::Game>, profile: Option<&str>) -> (BTreeMap<String, String>, Option<String>) {
    let mut env: BTreeMap<String, String> = cfg.env.iter().cloned().collect();
    let mut proton = None;
    if let Some(g) = g {
        let store = store_for(g);
        let prof = profile.filter(|p| *p != VANILLA);
        if let Ok(plan) = store.plan(prof) {
            env.extend(plan.api.settings.env.clone());
            proton = plan.api.settings.proton.clone();
        }
    }
    (env, proton)
}

fn build_dir(cfg: &launcher::Config, proton: Option<&str>) -> PathBuf {
    proton.and_then(|p| launcher::resolve_proton(p).ok()).unwrap_or_else(|| cfg.proton.clone())
}

impl Daemon {
    pub(crate) async fn handle_tweaks(self: &Arc<Self>, source: &str, req: TweaksRequest) -> Result<Value> {
        let src = source.to_owned();
        let cfg = self.cfg.clone();
        Ok(match req {
            TweaksRequest::List { scope, level } => {
                let this = self.clone();
                json!(
                    with_store_lock(move || {
                        let r = resolve_scope(&scope)?;
                        let plan = match (&r.game, r.kind) {
                            (Some(g), ScopeKind::Game | ScopeKind::Profile) => this.preview(&cfg, g, r.installed.as_ref(), r.profile.as_deref()).ok(),
                            _ => None,
                        };
                        let last = r.game.and_then(|g| tw::last_session(&data(), &g));
                        let mut st = tw::state(tw::schema(), &r.scopes, r.kind, level, plan.as_ref(), last.as_ref());
                        if r.kind == ScopeKind::Global {
                            st.schema_note = format!("{} · {}", st.schema_note, tw::capabilities(&data(), Some(&cfg.proton), "quiver").summary());
                        }
                        Ok(st)
                    })
                    .await?
                )
            }
            TweaksRequest::Set { scope, ops, message, confirmed } => json!(self.tweaks_set(&src, scope, Change::Ops(ops), message, confirmed).await?),
            TweaksRequest::Prefer { scope, id, prefer, message, confirmed } => json!(self.tweaks_set(&src, scope, Change::Prefer(id, prefer), message, confirmed).await?),
            TweaksRequest::Restore { scope, revision, confirmed } => {
                ask(&src, confirmed, &format!("restoring tweaks r{revision}"))?;
                json!(self.tweaks_set(&src, scope, Change::Restore(revision), None, true).await?)
            }
            TweaksRequest::History { scope } => json!(
                with_store_lock(move || {
                    let r = resolve_scope(&scope)?;
                    let store = r.installed.as_ref().map(store_for);
                    let d = data();
                    r.at(&d, &store).history()
                })
                .await?
            ),
            TweaksRequest::Plan { game, profile } => {
                let this = self.clone();
                json!(
                    with_store_lock(move || {
                        let g = game.as_ubisoft().and_then(installed);
                        let pid = match &g {
                            Some(g) => Some(profile_id(g, profile.as_deref())?),
                            None => None,
                        };
                        this.preview(&cfg, &game, g.as_ref(), pid.as_deref())
                    })
                    .await?
                )
            }
            TweaksRequest::Capabilities { game } => {
                let (env, proton) = match game.and_then(|g| g.as_ubisoft()).and_then(installed) {
                    Some(g) => preview_env(&cfg, Some(&g), None),
                    None => (BTreeMap::new(), None),
                };
                let _ = env;
                refresh_display();
                let build = build_dir(&cfg, proton.as_deref());
                json!(tokio::task::spawn_blocking(move || tw::capabilities(&data(), Some(&build), "quiver")).await?)
            }
            TweaksRequest::Status => json!(self.tweaks.sessions.lock().unwrap().values().map(|l| l.record.clone()).collect::<Vec<_>>()),
            TweaksRequest::Sessions { game, limit } => json!(tw::sessions(&data(), &game, limit.unwrap_or(20))),
        })
    }

    /// The plan of a game's next launch (no files touched). Steam games started by Steam
    /// get no tweaks yet; the plan says so.
    fn preview(&self, cfg: &launcher::Config, game: &GameRef, g: Option<&launcher::Game>, profile: Option<&str>) -> Result<TweaksPlan> {
        let (env, proton) = preview_env(cfg, g, profile);
        let route = if g.is_some() { "quiver" } else { "steam" };
        let build = (route == "quiver").then(|| build_dir(cfg, proton.as_deref()));
        if tw::display().is_none() {
            refresh_display();
        }
        let caps = tw::capabilities(&data(), build.as_deref(), route);
        let ctx = match g {
            Some(g) => quiver_core::options::context(&store_for(g), profile.filter(|p| *p != VANILLA)).ok(),
            None => None,
        };
        let mut c = tw::plan_for(&data(), game, ctx.as_ref(), caps, &env, None)?;
        if g.is_none() {
            c.plan.notes.push("this game starts through its store's client: Quiver Tweaks apply to games Quiver launches (and quiver steam-run) for now".into());
        }
        Ok(c.plan)
    }

    /// Tweak ops at a scope (the Options request's entry for the platform surface).
    pub(crate) async fn tweaks_set_ops(self: &Arc<Self>, source: &str, scope: Scope, ops: Vec<quiver_api::options::OptionOp>, message: Option<String>, confirmed: bool) -> Result<OptionsChange> {
        self.tweaks_set(source, scope, Change::Ops(ops), message, confirmed).await
    }

    /// The platform surface at a profile's scope, for the Options view's surface list.
    pub(crate) fn tweaks_profile_state(&self, product_id: u32, ctx: &quiver_core::options::Ctx, level: Option<quiver_api::options::Level>) -> Result<quiver_api::options::SurfaceState> {
        let game = GameRef::ubisoft(product_id);
        let scopes = Scopes::load(&data(), Some(&game), Some((&ctx.store, &ctx.profile_id)))?;
        let plan = installed(product_id).and_then(|g| self.preview(&self.cfg, &game, Some(&g), Some(&ctx.profile_id)).ok());
        let last = tw::last_session(&data(), &game);
        Ok(tw::state(tw::schema(), &scopes, ScopeKind::Profile, level, plan.as_ref(), last.as_ref()))
    }

    async fn tweaks_set(self: &Arc<Self>, source: &str, scope: Scope, change: Change, message: Option<String>, confirmed: bool) -> Result<OptionsChange> {
        if scope == Scope::Global && !matches!(change, Change::Restore(_)) {
            ask(source, confirmed, "changing tweaks for every game (global scope)")?;
        }
        let src = source.to_owned();
        let sc = scope.clone();
        let (rev, applied) = with_store_lock(move || -> Result<(Option<OptionsRevision>, tw::Applied)> {
            let r = resolve_scope(&sc)?;
            let store = r.installed.as_ref().map(store_for);
            let d = data();
            let at = r.at(&d, &store);
            let cur = at.current()?;
            let a = match &change {
                Change::Ops(ops) => tw::apply_ops(tw::schema(), r.kind, &cur, &r.inherited(), ops),
                Change::Prefer(id, p) => tw::apply_prefer(tw::schema(), r.kind, &cur, id, Some(*p))?,
                Change::Restore(n) => {
                    let hist = at.history()?;
                    let target = if *n == 0 { OptionsRevision::default() } else { hist.iter().find(|x| x.revision == *n).cloned().with_context(|| format!("no revision r{n} in this scope"))? };
                    let mut a = tw::Applied { values: target.values.get(quiver_api::tweaks::SURFACE_KEY).cloned().unwrap_or_default(), prefer: target.values.get(quiver_api::tweaks::PREFER_KEY).cloned().unwrap_or_default(), ..Default::default() };
                    if a.values != cur.values.get(quiver_api::tweaks::SURFACE_KEY).cloned().unwrap_or_default() || a.prefer != cur.values.get(quiver_api::tweaks::PREFER_KEY).cloned().unwrap_or_default() {
                        a.changes.push(format!("{}: restored r{n}", r.kind.label()));
                    }
                    a
                }
            };
            if !a.dangers.is_empty() && !confirmed {
                bail!("{} needs your confirmation: {}", a.dangers.join("; "), "pass confirmed");
            }
            let message = message.or_else(|| matches!(change, Change::Restore(_)).then(|| "restore".into()));
            let rev = if a.changes.is_empty() { None } else { Some(at.append(a.values.clone(), a.prefer.clone(), &src, message, a.changes.clone())?) };
            Ok((rev, a))
        })
        .await?;
        let running = match scope.game() {
            Some(g) => self.tweaks.running(&g),
            None => !self.tweaks.sessions.lock().unwrap().is_empty(),
        };
        let outcomes = applied
            .outcomes
            .iter()
            .map(|(id, o, before, after)| OpOutcome {
                surface: quiver_api::tweaks::SURFACE_KEY.into(),
                id: id.clone(),
                outcome: o.clone().unwrap_or(Outcome::SavedRestart { running }),
                before: before.clone(),
                after: after.clone(),
            })
            .collect();
        let (product_id, profile) = match &scope {
            Scope::Global => (0, String::new()),
            Scope::Game { game } => (game.as_ubisoft().unwrap_or(0), String::new()),
            Scope::Profile { game, profile } => (game.as_ubisoft().unwrap_or(0), profile.clone().unwrap_or_default()),
        };
        let revision = rev.as_ref().map_or(0, |r| r.revision);
        if let Some(r) = &rev {
            self.emit(Event::TweaksChanged { scope: scope.clone(), revision, source: source.to_owned(), changes: r.changes.clone() });
            self.log_activity_game(source, format!("tweaks r{revision}: {}", r.changes.join("; ")), scope.game());
        }
        let mut warnings = Vec::new();
        if running && rev.is_some() {
            warnings.push("tweaks apply at the next launch: the running game keeps what it started with".into());
        }
        Ok(OptionsChange { product_id, profile, revision, outcomes, changes: applied.changes, live: Vec::new(), warnings })
    }

    // -- launches -------------------------------------------------------------------------

    /// Before a launch (after options and perf): compile the tweaks into the launch's
    /// environment, copy NVIDIA's updater if asked for, start the session's record. Returns
    /// the job lines and the revisions the session runs.
    pub(crate) fn tweaks_launch(self: &Arc<Self>, game: &launcher::Game, plan: &mut quiver_core::mods::Plan, session: &str) -> (Vec<String>, Option<TweakRevisions>) {
        if std::env::var("QUIVER_TWEAKS").is_ok_and(|v| v == "0") {
            return (Vec::new(), None);
        }
        let gref = GameRef::ubisoft(game.product_id);
        let d = data();
        let dir = tw::session_dir(&d, &gref, session);
        let mut user: BTreeMap<String, String> = self.cfg.env.iter().cloned().collect();
        user.extend(plan.api.settings.env.clone());
        let build = build_dir(&self.cfg, plan.api.settings.proton.as_deref());
        refresh_display();
        let store = store_for(game);
        let profile = plan.api.profile.as_ref().map(|p| p.id.clone());
        let result = locked(|| -> Result<tw::compile::Compiled> {
            let ctx = quiver_core::options::context(&store, profile.as_deref()).ok();
            let caps = tw::capabilities(&d, Some(&build), "quiver");
            tw::plan_for(&d, &gref, ctx.as_ref(), caps, &user, Some(&dir))
        });
        let c = match result {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("tweaks for the launch: {e:#}");
                return (vec![format!("tweaks not applied: {e:#}")], None);
            }
        };
        let revisions = c.plan.revisions;
        if c.plan.entries.is_empty() {
            return (c.plan.warnings.iter().map(|w| format!("tweaks: {w}")).collect(), Some(revisions));
        }
        let mut lines = Vec::new();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            lines.push(format!("tweaks: no session log dir ({e}); effects won't be confirmed"));
        }
        if let Some(src) = &c.copy_updater {
            let dst = game.prefix.join("drive_c/windows/system32/nvngx_update.exe");
            match std::fs::create_dir_all(dst.parent().unwrap()).and_then(|_| std::fs::copy(src, &dst)) {
                Ok(_) => lines.push(format!("tweaks: copied {} into the prefix", src.display())),
                Err(e) => lines.push(format!("tweaks: couldn't copy {} into the prefix: {e}", src.display())),
            }
        }
        let mut st = tw::load_prefix_state(&d, &gref);
        if c.indicator_on != st.indicator_on {
            st.indicator_on = c.indicator_on;
            let _ = tw::save_prefix_state(&d, &gref, &st);
        }
        plan.api.settings.env.extend(c.plan.env.clone());
        for e in &c.plan.entries {
            if e.planned != Planned::Default {
                lines.push(format!("tweaks: {} {} — {}", e.id, e.display, e.planned.describe()));
            }
        }
        lines.extend(c.plan.warnings.iter().map(|w| format!("tweaks: warning: {w}")));
        lines.extend(c.plan.notes.iter().map(|n| format!("tweaks: {n}")));
        let record = SessionTweaks {
            session: session.to_owned(),
            game: gref,
            profile: c.plan.profile.clone().unwrap_or_else(|| VANILLA.into()),
            route: "quiver".into(),
            started: chrono::Utc::now(),
            ended: None,
            revisions,
            log_dir: Some(dir.clone()),
            effects: tw::initial_effects(&c.plan),
            env: c.plan.env.clone(),
            warnings: c.plan.warnings.clone(),
        };
        let _ = tw::save_session(&dir, &record);
        tw::prune_sessions(&d, &gref, 30);
        self.tweaks.sessions.lock().unwrap().insert(session.to_owned(), Live { record, plan: c.plan, dir, prefix: Some(game.prefix.clone()) });
        self.tweaks_metrics(session);
        (lines, Some(revisions))
    }

    /// While a session runs: look for evidence every 3 s; report what got confirmed.
    pub(crate) fn tweaks_watch(self: &Arc<Self>, session: String) {
        if self.tweaks.get(&session).is_none() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let Some(mut l) = this.tweaks.get(&session) else { return };
                if l.record.effects.iter().all(|e| e.effect.is_final()) {
                    return;
                }
                let dir = l.dir.clone();
                let Ok(logs) = tokio::task::spawn_blocking(move || tw::evidence::Logs::read(&dir)).await else { continue };
                let found = tw::evidence::scan(&l.plan, &mut l.record.effects, &logs);
                if found.is_empty() {
                    continue;
                }
                let _ = tw::save_session(&l.dir, &l.record);
                for id in &found {
                    if let Some(e) = l.record.effect(id) {
                        this.emit(Event::TweaksEffect { session: session.clone(), game: l.record.game, effect: e.clone() });
                    }
                }
                this.tweaks.sessions.lock().unwrap().insert(session.clone(), l);
                this.tweaks_metrics(&session);
            }
        });
    }

    /// After a session (before its perf session ends): remember the API the Sight layer saw
    /// (the next launch's `requires`), settle every effect (unconfirmed, with why) and record
    /// them.
    pub(crate) fn tweaks_session_end(&self, session: &str, product_id: u32) {
        let now = quiver_perf::clock::mono_raw_ns();
        let engine = self.perf.with_session(Some(session), product_id, |s| s.layer_info(now).map(|l| l.engine)).flatten();
        if let Some(api) = engine.as_deref().and_then(tw::api_of_engine)
            && let Err(e) = tw::save_api(&data(), &GameRef::ubisoft(product_id), api)
        {
            tracing::warn!("remembering the game's API: {e:#}");
        }
        let Some(mut l) = self.tweaks.sessions.lock().unwrap().remove(session) else { return };
        let logs = tw::evidence::Logs::read(&l.dir);
        let before: Vec<Effect> = l.record.effects.iter().map(|e| e.effect.clone()).collect();
        tw::evidence::finish(&l.plan, &mut l.record.effects, &logs, l.prefix.as_deref());
        l.record.ended = Some(chrono::Utc::now());
        if let Err(e) = tw::save_session(&l.dir, &l.record) {
            tracing::warn!("recording the session's tweaks: {e:#}");
        }
        for (e, was) in l.record.effects.iter().zip(before) {
            if e.effect != was {
                self.emit(Event::TweaksEffect { session: session.to_owned(), game: l.record.game, effect: e.clone() });
            }
        }
    }

    /// The `tweaks` producer (TWEAKS §6): per active tweak a `text` metric `tweaks.<id>` and a
    /// `state` metric `tweaks.<id>.effect`, plus `tweaks.summary` (the Full preset's chips).
    fn tweaks_metrics(&self, session: &str) {
        let Some(l) = self.tweaks.get(session) else { return };
        let product_id = l.record.game.as_ubisoft().unwrap_or(0);
        let now = quiver_perf::clock::mono_raw_ns();
        let added = self.perf.with_session(Some(session), product_id, |s| {
            let fresh = s.store.producer_mut(crate::perf::catalog::PRODUCER_TWEAKS).is_none();
            if fresh {
                s.store.producer(crate::perf::catalog::PRODUCER_TWEAKS, "Quiver Tweaks", true);
                s.store.declare(crate::perf::catalog::PRODUCER_TWEAKS, crate::perf::catalog::tweaks_summary());
            }
            for e in &l.record.effects {
                let (text_id, state_id) = (format!("tweaks.{}", e.id), format!("tweaks.{}.effect", e.id));
                if !s.store.has_metric(&text_id) && crate::perf::PerfHub::session_has_room(s) {
                    for d in crate::perf::catalog::tweak_metrics(&e.id, &e.label) {
                        s.store.declare(crate::perf::catalog::PRODUCER_TWEAKS, d);
                    }
                }
                s.store.push_text(&text_id, now, tweak_text(e));
                s.store.push_state_value(&state_id, now, e.effect.name());
            }
            s.store.push_text("tweaks.summary", now, summary(&l.record.effects));
            fresh
        });
        if added == Some(true) {
            self.perf.catalog_changed(session, product_id);
        }
    }
}

enum Change {
    Ops(Vec<quiver_api::options::OptionOp>),
    Prefer(String, quiver_api::tweaks::Prefer),
    Restore(u32),
}

/// `160 · DXVK`, `K · override read`, `by DLSS`.
fn tweak_text(e: &TweakEffect) -> String {
    let how = match &e.effect {
        Effect::Confirmed { .. } => format!("{} ✓", e.mech.as_deref().unwrap_or("")),
        Effect::Provided { by, .. } => format!("by {by}"),
        Effect::Unavailable { .. } => "unavailable".into(),
        Effect::Superseded { .. } => "yours".into(),
        _ => e.mech.clone().unwrap_or_default(),
    };
    format!("{} · {how}", e.value)
}

/// The Full preset's chip row: `CAP 160 · DLSS K · LL on · VSYNC off` (unconfirmed ones with
/// a `?`, unavailable ones left out).
pub fn summary(effects: &[TweakEffect]) -> String {
    let short = |id: &str| match id {
        "fps.cap" => "CAP",
        "sync.vsync" => "VSYNC",
        "sync.max_latency" => "FIF",
        "latency.mode" => "LL",
        "dlss.sr.model" => "DLSS",
        "dlss.sr.scale" => "RES",
        "dlss.rr.model" => "RR",
        "dlss.fg.model" => "FG",
        "dlss.fg.frames" => "MFG",
        "fg.smooth_motion" => "SMOOTH",
        "tex.anisotropy" => "AF",
        "tex.lod_bias" => "LOD",
        "hdr" => "HDR",
        "dlss.runtime" => "NGX",
        "dlss.indicator" => "IND",
        _ => "",
    };
    effects
        .iter()
        .filter(|e| !matches!(e.effect, Effect::Unavailable { .. } | Effect::Superseded { .. }) && !short(&e.id).is_empty())
        .map(|e| {
            let v = chip_value(&e.value);
            let mark = if matches!(e.effect, Effect::Confirmed { .. } | Effect::Provided { .. } | Effect::Measured { .. }) { "" } else { "?" };
            format!("{} {v}{mark}", short(&e.id))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A value as short as a chip: `refresh rate − 3 (162 fps)` → `162`, `K: DLSS 4 transformer` → `K`.
fn chip_value(v: &str) -> String {
    if v.starts_with("refresh rate")
        && let Some(inner) = v.split('(').nth(1)
    {
        return inner.trim_end_matches(')').replace(" fps", "");
    }
    v.split(':').next().unwrap_or(v).split(" (").next().unwrap_or(v).replace(" fps", "")
}

/// Tweaks for a `quiver steam-run` launch: the compiled environment (Steam picks the Proton
/// build, so build-dependent bindings are assumed to hold and the notes say so).
pub(crate) fn steam_plan(cfg: &launcher::Config, plan: &mut quiver_api::steamlib::SteamLaunchPlan) {
    if std::env::var("QUIVER_TWEAKS").is_ok_and(|v| v == "0") {
        return;
    }
    let game = GameRef::ubisoft(plan.product_id);
    let mut user: BTreeMap<String, String> = cfg.env.iter().cloned().collect();
    user.extend(plan.env.clone());
    refresh_display();
    let d = data();
    let caps = tw::capabilities(&d, None, "steam");
    let ctx = installed(plan.product_id).and_then(|g| quiver_core::options::context(&store_for(&g), None).ok());
    match locked(|| tw::plan_for(&d, &game, ctx.as_ref(), caps, &user, None)) {
        Ok(c) => {
            plan.env.extend(c.plan.env.clone());
            for e in c.plan.entries.iter().filter(|e| e.planned != Planned::Default) {
                plan.perf_notes.push(format!("tweaks: {} {} — {}", e.id, e.display, e.planned.describe()));
            }
            plan.perf_notes.extend(c.plan.warnings.iter().map(|w| format!("tweaks: warning: {w}")));
            plan.perf_notes.extend(c.plan.notes.iter().map(|n| format!("tweaks: {n}")));
        }
        Err(e) => plan.perf_notes.push(format!("tweaks not applied: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chip_row_says_only_what_applies() {
        let e = |id: &str, value: &str, effect: Effect| TweakEffect { id: id.into(), label: id.into(), value: value.into(), mech: Some("dxvk".into()), effect };
        let row = summary(&[
            e("fps.cap", "160 fps", Effect::Confirmed { evidence: "x".into(), at: chrono::Utc::now() }),
            e("dlss.sr.model", "K: DLSS 4 transformer", Effect::Applied),
            e("latency.mode", "on", Effect::Provided { by: "DLSS".into(), value: None }),
            e("dlss.fg.frames", "4×", Effect::Unavailable { reason: "x".into() }),
        ]);
        assert_eq!(row, "CAP 160 · DLSS K? · LL on");
        assert_eq!(chip_value("refresh rate − 3 (162 fps)"), "162");
        assert_eq!(tweak_text(&e("fps.cap", "160 fps", Effect::Provided { by: "DLSS".into(), value: None })), "160 fps · by DLSS");
    }
}
