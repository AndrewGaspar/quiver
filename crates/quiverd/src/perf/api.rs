//! `Request::Perf` and the launch hooks, on the daemon.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use quiver_api::perf::{HudScopeRef, PerfRequest, SessionSummary};
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
            PerfRequest::Catalog { session, product_id } => json!(hub.catalog(session.as_deref(), product_id)?),
            PerfRequest::Subscribe { session, product_id, select, every_ms, window_ms, stats } => {
                json!(hub.subscribe(&need_conn()?, session.as_deref(), product_id, select, every_ms, window_ms, stats)?)
            }
            PerfRequest::Unsubscribe { sub } => {
                hub.unsubscribe(need_conn()?.id, sub)?;
                Value::Null
            }
            PerfRequest::HudGet { scope } => json!(hub.hud.view(&self.perf_scope(scope)?)?),
            PerfRequest::HudSet { scope, set, unset, message } => {
                let scope = self.perf_scope(scope)?;
                if let Some(l) = &set.layout {
                    super::layout::load(hub.hud.dir(), l).map_err(|e| anyhow::anyhow!(e))?;
                }
                let v = hub.hud.apply(&scope, &set, &unset, source, message.as_deref())?;
                let what = serde_json::to_string(&set).unwrap_or_default();
                let unset_s = if unset.is_empty() { String::new() } else { format!(", unset {}", unset.join(",")) };
                self.log_activity(source, format!("HUD settings r{} ({}): {what}{unset_s}", v.revision, scope.label()), scope.product_id);
                hub.broadcast.send(quiver_api::perf::PerfEvent::HudChanged { product_id: scope.product_id, session: None }).ok();
                json!(v)
            }
            PerfRequest::HudToggle { session, product_id } => json!(hub.toggle(session.as_deref(), product_id)?),
            PerfRequest::HudNext { session, product_id } => json!(hub.next(session.as_deref(), product_id)?),
            PerfRequest::HudAttach { session } => json!(hub.attach(&need_conn()?, &session)?),
            PerfRequest::HudStatus { session, product_id } => json!(hub.status(session.as_deref(), product_id)?),
            PerfRequest::HudLayouts => json!(super::layout::list(hub.hud.dir())),
            PerfRequest::HudLayout { id } => json!(super::layout::load(hub.hud.dir(), &id).map_err(|e| anyhow::anyhow!(e))?.to_api()),
            PerfRequest::HudKeybinds => json!(hub.keybinds()),
            PerfRequest::LaunchEnv { product_id, profile } => {
                let profile = match profile {
                    Some(p) => Some(crate::mods::resolve_default(product_id, &p)?).filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case(quiver_api::mods::VANILLA)),
                    None => None,
                };
                let cfg = hub.hud.for_launch(product_id, profile.as_deref());
                // A dry run: the session dir a launch would get, without creating it.
                let dir = super::shm_root().join(format!("quiver-{}", quiver_perf::session::current_uid())).join("s-example");
                let env = [("QUIVER_SESSION", "s-example".to_owned()), ("QUIVER_PERF_DIR", dir.display().to_string())];
                let s = quiver_core::settings::current(product_id).map(|(s, _)| s).unwrap_or_default();
                json!(super::launch::plan(&cfg, &env, &s.env, &s.wrappers, super::launch::installed_layer_dir(hub.data_dir()).as_deref()))
            }
            PerfRequest::Summaries { product_id, limit } => json!(summaries(product_id, limit)),
            PerfRequest::DevSession { product_id, stop } => {
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
                    let began = hub.begin(&id, product_id, None, None, &Default::default(), &[], true)?;
                    let mut info = hub.sessions().into_iter().find(|s| s.session == id).context("session vanished")?;
                    info.dev = true;
                    self.log_activity(source, format!("perf dev session {id} for {product_id}"), Some(product_id));
                    json!({ "info": info, "env": began.plan.env })
                }
            }
        })
    }

    /// A scope with its profile name resolved to an id.
    fn perf_scope(&self, mut scope: HudScopeRef) -> Result<HudScopeRef> {
        if let (Some(p), Some(prof)) = (scope.product_id, &scope.profile) {
            let id = crate::mods::resolve_default(p, prof)?;
            if id.is_empty() || id.eq_ignore_ascii_case(quiver_api::mods::VANILLA) {
                scope.profile = None;
            } else {
                scope.profile = Some(id);
            }
        }
        Ok(scope)
    }

    /// The perf part of a launch: the session's registry and, with the HUD on for the game,
    /// its session dir and the launch's environment (added to `env`). Returns lines for
    /// the job's detail. A failure leaves the launch as it was.
    pub(crate) fn perf_launch(&self, session_id: &str, game: &launcher::Game, profile: Option<&str>, settings: &mut quiver_api::GameSettings) -> Vec<String> {
        if std::env::var("QUIVER_PERF").is_ok_and(|v| v == "0") {
            return Vec::new();
        }
        match self.perf.begin(session_id, game.product_id, profile, Some(&game.prefix), &settings.env, &settings.wrappers, false) {
            Ok(b) => {
                let enabled = b.plan.hud.as_ref().is_some_and(|h| h.enabled);
                settings.env.extend(b.plan.env);
                if enabled { b.plan.notes.into_iter().map(|n| format!("hud: {n}")).collect() } else { Vec::new() }
            }
            Err(e) => {
                tracing::warn!("perf: {e:#}");
                vec![format!("hud: not set up ({e:#})")]
            }
        }
    }
}

/// A game's recorded sessions that carry a perf summary, newest first.
pub fn summaries(product_id: u32, limit: usize) -> Vec<SessionSummary> {
    let path = launcher::data_dir().join("games").join(product_id.to_string()).join("playtime.jsonl");
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
