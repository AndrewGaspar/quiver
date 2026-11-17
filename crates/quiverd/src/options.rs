//! `Request::Options`: mod options (design/MOD-CONFIG.md; `quiver_core::options` does the file
//! work). Sessions quiverd launched are tracked here with what the running game has, so
//! every change reports truthfully what reached it (C3).
//!
//! **Live channels.** A surface's `reload` files are rewritten through the running game's
//! namespace (`/proc/<pid>/root`); a mod connected over the SDK gets values pushed instead,
//! through [`SdkLink`], which the SDK pipe registers with [`Options::set_sdk`]. Values a mod
//! reports itself come in through [`Daemon::options_from_mod`].
//!
//! **Agent policy** (steer 13): setting values on a profile is allowed; live changes to a
//! running game, restores, session reverts and `danger` options are "Ask" (refused without
//! `confirmed`). `danger` options need `confirmed` from every source (front ends ask first).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use quiver_api::mods::{VANILLA, is_human_source};
use quiver_api::options::{
    CrashNote, InvokeResult, KeysReport, Live, LiveChange, LiveChannel, LintReport, OpOutcome, OptionDiff, OptionOp, OptionsChange, OptionsEvent,
    OptionsRequest, Outcome, SchemaInfo, SessionDetail, SessionOptions, Values,
};
use quiver_core::launcher::Games;
use quiver_core::mods::ModStore;
use quiver_core::options::{self as opt, Ctx, RunInfo};

use crate::Daemon;
use crate::mods::{launch_profile, store_for, with_store_lock};

/// A value pushed to a mod over the SDK: `text` as the file would hold it, `json` typed.
#[derive(Debug, Clone, PartialEq)]
pub struct SdkValue {
    pub id: String,
    pub text: String,
    pub json: Value,
}

/// How a mod answered a pushed batch (built by the SDK pipe).
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum SdkAck {
    /// Applied (`ack` without rejections).
    Applied,
    /// Applied except these (id → the mod's reason).
    Rejected(BTreeMap<String, String>),
    /// No `ack` in time.
    TimedOut,
    /// The channel isn't there (any more).
    Gone,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The SDK mod channel, as the options code uses it (implemented by the SDK pipe in the
/// bridge relay; see design/MOD-CONFIG.md §5 "SDK"). Not registered: every surface uses the
/// file channel or none.
pub trait SdkLink: Send + Sync {
    /// Whether a mod is connected for `surface` (its schema's `surface` id) in `session`.
    fn connected(&self, session: &str, surface: &str) -> bool;
    /// Send a `set` with these values and wait for the mod's `ack`.
    fn push(&self, session: &str, surface: &str, values: Vec<SdkValue>) -> BoxFuture<SdkAck>;
    /// Send an `action`; `Ok(detail)` once the mod took it.
    fn invoke(&self, session: &str, surface: &str, action: &str, arg: Option<String>) -> BoxFuture<Result<Option<String>>>;
}

/// A session quiverd launched, as options see it.
#[derive(Debug, Clone)]
struct LiveSession {
    record: SessionOptions,
    /// What the running program has: surface key → id → value.
    has: Values,
    /// Values set for this session only (`Live::Only`).
    session_values: Values,
    pids: Vec<u32>,
    /// The SDK said `bye` (a clean exit).
    clean_bye: bool,
    /// A mod talked to Quiver over the SDK in this session.
    sdk_seen: bool,
    /// Test sessions: the "process" root to write through (instead of `/proc/<pid>/root`).
    root: Option<PathBuf>,
}

/// Options state in the daemon.
pub struct Options {
    sessions: Mutex<HashMap<String, LiveSession>>,
    sdk: Mutex<Option<Arc<dyn SdkLink>>>,
    pub(crate) events: broadcast::Sender<OptionsEvent>,
}

impl Default for Options {
    fn default() -> Self {
        Self { sessions: Mutex::default(), sdk: Mutex::default(), events: broadcast::channel(256).0 }
    }
}

impl Options {
    /// Register the SDK mod channel (the SDK pipe calls this once at startup).
    #[allow(dead_code)]
    pub fn set_sdk(&self, link: Arc<dyn SdkLink>) {
        *self.sdk.lock().unwrap() = Some(link);
    }

    fn sdk(&self) -> Option<Arc<dyn SdkLink>> {
        self.sdk.lock().unwrap().clone()
    }

    fn emit(&self, e: OptionsEvent) {
        let _ = self.events.send(e);
    }

    fn session_for(&self, product_id: u32, profile: &str) -> Option<LiveSession> {
        self.sessions.lock().unwrap().values().find(|s| s.record.product_id == product_id && s.record.profile == profile).cloned()
    }

    /// The options revision a running session launched with.
    pub(crate) fn launched_revision(&self, session: &str) -> Option<u32> {
        self.session(session).map(|s| s.record.launched_revision)
    }

    fn session(&self, id: &str) -> Option<LiveSession> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut LiveSession)) {
        if let Some(s) = self.sessions.lock().unwrap().get_mut(id) {
            f(s);
        }
    }

    /// The addressing fields for a game being played (C1).
    pub fn detail(&self, mut s: quiver_api::ActiveSession) -> SessionDetail {
        let found = self.sessions.lock().unwrap().values().find(|l| l.record.product_id == s.product_id).cloned();
        if let Some(l) = found {
            s.session = Some(l.record.session.clone());
            s.profile = Some(l.record.profile.clone());
            s.options_revision = Some(l.record.launched_revision);
            s.pids = l.pids;
        }
        s
    }
}

/// A fresh session id (`s-` and 6 base-36 characters).
pub fn new_session_id() -> String {
    quiver_core::modsdk::new_session_id()
}

fn game(product_id: u32) -> Result<quiver_core::launcher::Game> {
    Games::load()?.get(product_id).cloned().context("not installed (options need an installed game)")
}

/// The store and profile id a request means (`None`: the game's default profile).
fn resolve(product_id: u32, profile: Option<&str>) -> Result<(ModStore, Option<String>)> {
    let store = store_for(&game(product_id)?);
    let (p, _) = launch_profile(&store, product_id, profile)?;
    // A name to the profile's id (values are stored by id).
    let p = match p {
        Some(q) if !q.eq_ignore_ascii_case(VANILLA) => Some(store.profile(&q)?.0.id),
        _ => None,
    };
    Ok((store, p))
}

fn ctx_for(product_id: u32, profile: Option<&str>) -> Result<Ctx> {
    let (store, p) = resolve(product_id, profile)?;
    opt::context(&store, p.as_deref())
}

fn ask(source: &str, confirmed: bool, what: &str) -> Result<()> {
    if is_human_source(source) || confirmed {
        return Ok(());
    }
    bail!("{what} needs your confirmation when requested by '{source}' (agent policy: Ask). Run it in a terminal, where quiver asks, or without --source")
}

/// Test hook: `QUIVER_TEST_SESSION=<product>:<profile>:<root>` registers a running session
/// whose "process" root is `<root>` (`/` for a plain process), so integration tests can
/// exercise live changes without a game. Only read at startup.
pub(crate) fn test_session(d: &Daemon) {
    let Ok(spec) = std::env::var("QUIVER_TEST_SESSION") else { return };
    let mut parts = spec.splitn(3, ':');
    let (Some(p), Some(profile), Some(root)) = (parts.next().and_then(|p| p.parse::<u32>().ok()), parts.next(), parts.next()) else {
        tracing::warn!("QUIVER_TEST_SESSION: expected <product>:<profile>:<root>");
        return;
    };
    let result = (|| -> Result<String> {
        let (store, prof) = resolve(p, Some(profile))?;
        let ctx = opt::context(&store, prof.as_deref())?;
        let rev = opt::current(&store, &ctx.profile_id)?;
        let has = ctx.surfaces.iter().map(|s| (s.key.clone(), opt::eval(&ctx, s, &rev.values, &rev.raw, None).options.into_iter().map(|(id, e)| (id, e.effective())).collect())).collect();
        let session = new_session_id();
        d.options.register(&ctx, &session, rev.revision, has, vec![std::process::id()], Some(PathBuf::from(root)));
        Ok(session)
    })();
    match result {
        Ok(s) => tracing::info!("test session {s} for {p} ({profile}) through {root}"),
        Err(e) => tracing::warn!("QUIVER_TEST_SESSION: {e:#}"),
    }
}

impl Options {
    fn register(&self, ctx: &Ctx, session: &str, revision: u32, has: Values, pids: Vec<u32>, root: Option<PathBuf>) {
        let record = SessionOptions {
            session: session.to_owned(),
            product_id: ctx.store.product_id(),
            profile: ctx.profile_id.clone(),
            launched_revision: revision,
            started: chrono::Utc::now(),
            ..Default::default()
        };
        let _ = opt::save_session(&ctx.store, &record);
        self.sessions.lock().unwrap().insert(session.to_owned(), LiveSession { record, has, session_values: Values::new(), pids, clean_bye: false, sdk_seen: false, root });
    }
}

impl Daemon {
    pub(crate) async fn handle_options(self: &Arc<Self>, source: &str, req: OptionsRequest) -> Result<Value> {
        let src = source.to_owned();
        Ok(match req {
            OptionsRequest::Sessions => json!(self.sessions.list().into_iter().map(|s| self.options.detail(s)).collect::<Vec<_>>()),
            OptionsRequest::List { product_id, profile, level } => {
                let this = self.clone();
                json!(
                    with_store_lock(move || {
                        let ctx = ctx_for(product_id, profile.as_deref())?;
                        let rev = opt::current(&ctx.store, &ctx.profile_id)?;
                        let live = this.options.session_for(product_id, &ctx.profile_id);
                        let run = this.run_info(&ctx, live.as_ref());
                        let mut out = opt::states(&ctx, &rev, &run, level);
                        if !ctx.problems.is_empty()
                            && let Some(first) = out.first_mut()
                        {
                            first.lint.extend(ctx.problems.iter().cloned());
                        }
                        // Quiver Tweaks, at the profile scope, last on the wire (indexes stay as
                        // they were; the app lists it first, design/TWEAKS.md §8.3).
                        out.push(this.tweaks_profile_state(product_id, &ctx, level)?);
                        Ok(out)
                    })
                    .await?
                )
            }
            OptionsRequest::Set { product_id, profile, ops, live, message, confirmed } => {
                // Ops on Quiver Tweaks go to the profile scope of the tweaks store.
                let (tweak_ops, ops): (Vec<OptionOp>, Vec<OptionOp>) = ops.into_iter().partition(|o| quiver_api::tweaks::is_platform_surface(op_surface(o)));
                let mut out = if ops.is_empty() && !tweak_ops.is_empty() {
                    None
                } else {
                    Some(self.options_set(&src, product_id, profile.clone(), ops, live, message.clone(), confirmed, None).await?)
                };
                if !tweak_ops.is_empty() {
                    let scope = quiver_api::tweaks::Scope::Profile { game: quiver_api::GameRef::ubisoft(product_id), profile: profile.clone() };
                    let t = self.tweaks_set_ops(&src, scope, tweak_ops, message, confirmed).await?;
                    out = Some(match out {
                        None => t,
                        Some(mut o) => {
                            o.outcomes.extend(t.outcomes);
                            o.changes.extend(t.changes);
                            o.warnings.extend(t.warnings);
                            if o.revision == 0 {
                                o.revision = t.revision;
                            }
                            o
                        }
                    });
                }
                json!(out.unwrap_or_default())
            }
            OptionsRequest::History { product_id, profile } => {
                json!(
                    with_store_lock(move || {
                        let (store, p) = resolve(product_id, profile.as_deref())?;
                        opt::history(&store, p.as_deref().unwrap_or(VANILLA))
                    })
                    .await?
                )
            }
            OptionsRequest::Restore { product_id, profile, revision, confirmed } => {
                ask(source, confirmed, "restoring an options revision")?;
                let (ops, msg) = with_store_lock(move || {
                    let ctx = ctx_for(product_id, profile.as_deref())?;
                    let hist = opt::history(&ctx.store, &ctx.profile_id)?;
                    let target = match revision {
                        0 => quiver_api::options::OptionsRevision::default(),
                        r => hist.iter().find(|x| x.revision == r).cloned().with_context(|| format!("no options revision r{r} (latest: r{})", hist.last().map_or(0, |x| x.revision)))?,
                    };
                    let cur = hist.last().cloned().unwrap_or_default();
                    Ok((ops_towards(&cur, &target), (ctx.profile_id.clone(), format!("restored r{revision}"))))
                })
                .await?;
                let (pid, m) = msg;
                json!(self.options_set(&src, product_id, Some(pid), ops, Live::IfRunning, Some(m), true, None).await?)
            }
            OptionsRequest::RevertSession { product_id, session, confirmed } => {
                ask(source, confirmed, "reverting a session's option changes")?;
                let live = self.options.session(&session);
                let (ops, pid) = with_store_lock(move || {
                    let store = store_for(&game(product_id)?);
                    let rec = match &live {
                        Some(l) => l.record.clone(),
                        None => opt::load_session(&store, &session)?,
                    };
                    let hist = opt::history(&store, &rec.profile)?;
                    let target = hist.iter().find(|x| x.revision == rec.launched_revision).cloned().unwrap_or_default();
                    let cur = hist.last().cloned().unwrap_or_default();
                    Ok((ops_towards(&cur, &target), (rec.profile, rec.session, rec.launched_revision)))
                })
                .await?;
                let (profile, session, rev) = pid;
                self.options.update(&session, |s| s.session_values.clear());
                json!(
                    self.options_set(&src, product_id, Some(profile), ops, Live::IfRunning, Some(format!("reverted session {session} (back to r{rev})")), true, Some(session))
                        .await?
                )
            }
            OptionsRequest::Diff { product_id, profile, against } => {
                json!(with_store_lock(move || diff(product_id, profile.as_deref(), against.as_deref())).await?)
            }
            OptionsRequest::Keys { product_id, profile } => {
                json!(
                    with_store_lock(move || {
                        let ctx = ctx_for(product_id, profile.as_deref())?;
                        let rev = opt::current(&ctx.store, &ctx.profile_id)?;
                        let bindings = opt::key_bindings(&ctx, &rev);
                        let conflicts = opt::keys::conflicts(&bindings);
                        Ok(KeysReport { product_id, profile: ctx.profile_id.clone(), bindings, conflicts })
                    })
                    .await?
                )
            }
            OptionsRequest::Invoke { product_id, surface, action, arg, session } => {
                let ctx = with_store_lock(move || {
                    let store = store_for(&game(product_id)?);
                    Ok(store)
                })
                .await?;
                let live = match session {
                    Some(s) => self.options.session(&s).context("no such running session")?,
                    None => self.options.sessions.lock().unwrap().values().find(|s| s.record.product_id == product_id).cloned().context("the game isn't running (actions need a running session)")?,
                };
                let c = opt::context(&ctx, Some(&live.record.profile))?;
                let s = opt::surface(&c, &surface)?;
                if !s.schema().actions.contains_key(&action) {
                    bail!("{} has no action {action} (actions: {})", s.title(), s.schema().actions.keys().cloned().collect::<Vec<_>>().join(", "));
                }
                let sdk = self.options.sdk().filter(|l| l.connected(&live.record.session, &s.schema().surface));
                let Some(sdk) = sdk else {
                    bail!("{} isn't connected over the mod SDK in this session; actions need it (its own hotkey still works)", s.title());
                };
                let detail = sdk.invoke(&live.record.session, &s.schema().surface, &action, arg).await?;
                self.log_activity(source, format!("ran {} action {action} in {}", s.title(), self.product_name(product_id)), Some(product_id));
                json!(InvokeResult { session: live.record.session, surface: s.key.clone(), action, ok: true, detail })
            }
            OptionsRequest::Schema { product_id, mod_id, version } => json!(with_store_lock(move || schema_info(product_id, &mod_id, version.as_deref())).await?),
            OptionsRequest::SchemaSet { product_id, mod_id, toml, message } => {
                let s2 = src.clone();
                let info = with_store_lock(move || {
                    let store = store_for(&game(product_id)?);
                    let rec = store.find_mod(&mod_id)?;
                    opt::set_local_schema(&store, &rec.id, &toml, &s2, message)?;
                    schema_info(product_id, &rec.id, None)
                })
                .await?;
                self.log_activity(source, format!("local options schema for '{}' of {}", info.mod_id, self.product_name(product_id)), Some(product_id));
                json!(info)
            }
            OptionsRequest::Infer { product_id, mod_id, file } => {
                json!(
                    with_store_lock(move || {
                        let store = store_for(&game(product_id)?);
                        let rec = store.find_mod(&mod_id)?;
                        let rel = file.trim_start_matches('/');
                        let path = store.layer_dir(&rec.current).join(rel);
                        let text = std::fs::read_to_string(&path).with_context(|| format!("{} has no file {rel}", rec.name))?;
                        let surface: String = rec.name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
                        Ok(opt::infer::to_toml(&opt::infer::infer_schema(surface.trim_matches('-'), &rec.name, rel, &text)))
                    })
                    .await?
                )
            }
            OptionsRequest::Lint { path } => json!(tokio::task::spawn_blocking(move || lint_folder(&path)).await??),
            OptionsRequest::Files { product_id, mod_id } => {
                json!(
                    with_store_lock(move || {
                        let store = store_for(&game(product_id)?);
                        let rec = store.find_mod(&mod_id)?;
                        Ok(opt::config_files(&store.layer_dir(&rec.current), 200))
                    })
                    .await?
                )
            }
            OptionsRequest::SessionRecords { product_id, profile, limit } => {
                json!(
                    with_store_lock(move || {
                        let (store, p) = match &profile {
                            Some(_) => resolve(product_id, profile.as_deref())?,
                            None => (store_for(&game(product_id)?), None),
                        };
                        let want = profile.as_ref().map(|_| p.unwrap_or_else(|| VANILLA.into()));
                        let mut list: Vec<SessionOptions> = opt::sessions(&store).into_iter().filter(|s| want.as_ref().is_none_or(|w| &s.profile == w)).collect();
                        list.truncate(limit.unwrap_or(20).max(1));
                        Ok(list)
                    })
                    .await?
                )
            }
        })
    }

    fn run_info<'a>(&self, ctx: &Ctx, live: Option<&'a LiveSession>) -> RunInfo<'a> {
        let sdk = self.options.sdk();
        RunInfo {
            session: live.map(|l| l.record.session.clone()),
            running: live.is_some_and(|l| !l.pids.is_empty() || l.root.is_some()),
            sdk: match (&sdk, live) {
                (Some(link), Some(l)) => ctx.surfaces.iter().filter(|s| link.connected(&l.record.session, &s.schema().surface)).map(|s| s.key.clone()).collect(),
                _ => Default::default(),
            },
            has: live.map(|l| &l.has),
            session_values: live.map(|l| &l.session_values),
        }
    }

    /// `OptionsSet` (and restores, reverts and imports through it): validate, save, deliver
    /// live, report one outcome per op.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn options_set(
        self: &Arc<Self>,
        source: &str,
        product_id: u32,
        profile: Option<String>,
        ops: Vec<OptionOp>,
        live: Live,
        message: Option<String>,
        confirmed: bool,
        session_hint: Option<String>,
    ) -> Result<OptionsChange> {
        let ctx = {
            let p = profile.clone();
            with_store_lock(move || ctx_for(product_id, p.as_deref())).await?
        };
        let running = self.options.session_for(product_id, &ctx.profile_id);
        let rev = opt::current(&ctx.store, &ctx.profile_id)?;
        // Session-only changes apply on top of what the session already has.
        let (base_values, base_raw) = match (live, &running) {
            (Live::Only, Some(s)) => (merge(&rev.values, &s.session_values), rev.raw.clone()),
            _ => (rev.values.clone(), rev.raw.clone()),
        };
        let mut applied = opt::apply_ops(&ctx, &base_values, &base_raw, &ops)?;
        let changed = applied.outcomes.iter().any(|o| o.2.is_none());
        if live == Live::Only && running.is_none() {
            for o in applied.outcomes.iter_mut().filter(|o| o.2.is_none()) {
                o.2 = Some(Outcome::Rejected { reason: format!("{} isn't running with this profile; nothing was saved (`live: only` sets values for a running session)", self.product_name(product_id)) });
            }
        }
        if changed && !applied.dangers.is_empty() && !confirmed {
            bail!("needs confirmation (dangerous option): {}. Send it again with confirmation (the CLI asks; agents: confirmed)", applied.dangers.join("; "));
        }
        if changed && running.is_some() && live != Live::No && !is_human_source(source) && !confirmed {
            bail!(
                "{} is running: changing it live needs your confirmation when requested by '{source}' (agent policy: Ask). Save without touching the running game with --no-live, or confirm",
                self.product_name(product_id)
            );
        }
        let delivering = running.is_some() && live != Live::No && applied.outcomes.iter().any(|o| o.2.is_none());
        let mut out = OptionsChange { product_id, profile: ctx.profile_id.clone(), ..Default::default() };
        out.changes = applied.changes.clone();
        // Save (everything but session-only changes).
        let session_id = running.as_ref().map(|s| s.record.session.clone()).or(session_hint);
        let (values, raw) = if live == Live::Only {
            (rev.values.clone(), rev.raw.clone())
        } else {
            (applied.values.clone(), applied.raw.clone())
        };
        let mut session_values = running.as_ref().map(|s| s.session_values.clone()).unwrap_or_default();
        if live == Live::Only && running.is_some() {
            for (key, id, done, _, _) in applied.outcomes.iter().filter(|o| o.2.is_none()) {
                let m = session_values.entry(key.clone()).or_default();
                match applied.values.get(key).and_then(|v| v.get(id)) {
                    Some(v) => {
                        m.insert(id.clone(), v.clone());
                    }
                    None => {
                        m.remove(id);
                    }
                }
                let _ = done;
            }
        } else if running.is_some() {
            // A saved change replaces a session-only one.
            for (key, id, done, _, _) in &applied.outcomes {
                if done.is_none()
                    && let Some(m) = session_values.get_mut(key)
                {
                    m.remove(id);
                }
            }
        }
        if changed && live != Live::Only {
            let (store, pid, src, sess, msg, ch) = (ctx.store.clone(), ctx.profile_id.clone(), source.to_owned(), session_id.clone(), message.clone(), applied.changes.clone());
            let r = with_store_lock(move || opt::append(&store, &pid, values, raw, &src, sess, msg, ch)).await?;
            out.revision = r.revision;
        }
        // Deliver live: per surface, the SDK if connected, else the file channel.
        let mut delivered: BTreeMap<String, Result<LiveChannel, String>> = BTreeMap::new();
        let mut sdk_rejects: BTreeMap<(String, String), Outcome> = BTreeMap::new();
        if delivering && let Some(s) = &running {
            let touched: Vec<String> = applied.outcomes.iter().filter(|o| o.2.is_none()).map(|o| o.0.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            let saved = opt::current(&ctx.store, &ctx.profile_id)?;
            for key in touched {
                let Some(surf) = ctx.surfaces.iter().find(|x| x.key == key) else { continue };
                let sdk = self.options.sdk().filter(|l| l.connected(&s.record.session, &surf.schema().surface));
                if let Some(link) = sdk {
                    let ev = opt::eval(&ctx, surf, &saved.values, &saved.raw, Some(&session_values));
                    let vals: Vec<SdkValue> = applied
                        .outcomes
                        .iter()
                        .filter(|o| o.0 == key && o.2.is_none())
                        .filter_map(|o| ev.options.get(&o.1).map(|e| SdkValue { id: o.1.clone(), text: e.def.to_text(&e.effective()), json: e.effective() }))
                        .collect();
                    match link.push(&s.record.session, &surf.schema().surface, vals).await {
                        SdkAck::Applied => {
                            delivered.insert(key.clone(), Ok(LiveChannel::Sdk));
                        }
                        SdkAck::Rejected(why) => {
                            delivered.insert(key.clone(), Ok(LiveChannel::Sdk));
                            for (id, r) in why {
                                sdk_rejects.insert((key.clone(), id), Outcome::Rejected { reason: format!("the mod refused it: {r} (saved; applies next launch)") });
                            }
                        }
                        SdkAck::TimedOut => {
                            for o in applied.outcomes.iter().filter(|o| o.0 == key && o.2.is_none()) {
                                sdk_rejects.insert((key.clone(), o.1.clone()), Outcome::TimedOut);
                            }
                        }
                        SdkAck::Gone => {
                            delivered.insert(key.clone(), Err("the mod's SDK channel closed".into()));
                        }
                    }
                    continue;
                }
                if !opt::has_live_file(surf) {
                    continue;
                }
                let result = match (&s.root, s.pids.first()) {
                    (Some(root), _) => opt::write_live_at(&ctx, surf, &saved.values, &saved.raw, Some(&session_values), root),
                    (None, Some(&pid)) => opt::write_live(&ctx, surf, &saved.values, &saved.raw, Some(&session_values), pid),
                    (None, None) => Err(anyhow::anyhow!("the game's process isn't known yet")),
                };
                match result {
                    Ok(files) => {
                        out.live.push(format!("{} is running ({}): rewrote {}", self.product_name(product_id), surf.title(), files.join(", ")));
                        delivered.insert(key.clone(), Ok(LiveChannel::File));
                    }
                    Err(e) => {
                        out.warnings.push(format!("live: {e:#}"));
                        delivered.insert(key.clone(), Err(format!("rewriting the file in the running game failed: {e:#}; applies next launch")));
                    }
                }
            }
        }
        let mut outcomes: Vec<OpOutcome> = opt::outcomes(&ctx, &applied, running.is_some() && live != Live::No, &delivered);
        for o in outcomes.iter_mut() {
            if let Some(x) = sdk_rejects.remove(&(o.surface.clone(), o.id.clone())) {
                o.outcome = x;
            }
        }
        if live == Live::No && running.is_some() {
            for o in outcomes.iter_mut().filter(|o| matches!(o.outcome, Outcome::SavedRestart { .. })) {
                let apply = ctx.surfaces.iter().find(|s| s.key == o.surface).and_then(|s| s.schema().options.get(&o.id)).map(|d| d.apply);
                o.outcome = match apply {
                    Some(quiver_api::options::Apply::Live | quiver_api::options::Apply::At) => {
                        Outcome::SavedPending { detail: "not sent to the running game (live: no); applies next launch".into() }
                    }
                    _ => Outcome::SavedRestart { running: true },
                };
            }
        }
        out.outcomes = outcomes;
        // What the running game has now, and its live changes (crash correlation).
        if let Some(s) = &running {
            let now = chrono::Utc::now();
            let mut has = s.has.clone();
            let mut changes = Vec::new();
            for o in &out.outcomes {
                if let Outcome::AppliedLive { .. } = o.outcome {
                    if let Some(v) = &o.after {
                        has.entry(o.surface.clone()).or_default().insert(o.id.clone(), v.clone());
                    }
                    changes.push(LiveChange { at: now, surface: o.surface.clone(), id: o.id.clone(), source: source.to_owned(), value: o.after.clone().unwrap_or_default() });
                }
            }
            let sid = s.record.session.clone();
            let sv = session_values.clone();
            self.options.update(&sid, |l| {
                l.has = has;
                l.session_values = sv;
                l.record.live_changes.extend(changes);
            });
            if let Some(l) = self.options.session(&sid) {
                let _ = opt::save_session(&ctx.store, &l.record);
            }
            let mut by_surface: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
            for o in out.outcomes.iter().filter(|o| matches!(o.outcome, Outcome::AppliedLive { .. } | Outcome::SavedRestart { .. } | Outcome::SavedAt { .. } | Outcome::SavedPending { .. })) {
                by_surface.entry(o.surface.clone()).or_default().insert(o.id.clone(), o.after.clone().unwrap_or_default());
            }
            for (surface, values) in by_surface {
                self.options.emit(OptionsEvent::Live { session: sid.clone(), product_id, surface, values, by: source.to_owned(), saved: live != Live::Only });
            }
            // Sight's toast lane: every outcome, rejections included.
            let outcomes: Vec<OpOutcome> = out.outcomes.iter().filter(|o| o.outcome != Outcome::Unchanged).cloned().collect();
            if !outcomes.is_empty() {
                self.options.emit(OptionsEvent::Outcome { session: sid, product_id, profile: ctx.profile_id.clone(), by: source.to_owned(), outcomes, changes: out.changes.clone() });
            }
        }
        if out.revision > 0 {
            self.options.emit(OptionsEvent::Changed { product_id, profile: ctx.profile_id.clone(), revision: out.revision, source: source.to_owned(), changes: out.changes.clone() });
            let live_note = if out.live.is_empty() { String::new() } else { " (live)".into() };
            self.log_activity(
                source,
                format!("options r{} for {} ({}){live_note}: {}", out.revision, self.product_name(product_id), ctx.profile_id, out.changes.join("; ")),
                Some(product_id),
            );
        }
        Ok(out)
    }

    // -- launches ---------------------------------------------------------------------------

    /// Before a launch: import outside edits, render and patch the profile's option files
    /// (into `plan`), and start tracking the session. Returns the job lines.
    pub(crate) fn options_launch(&self, game: &quiver_core::launcher::Game, plan: &mut quiver_core::mods::Plan, session: &str) -> Vec<String> {
        let store = store_for(game);
        let profile = plan.api.profile.as_ref().map(|p| p.id.clone());
        let result = crate::mods::locked(|| -> Result<(Ctx, opt::LaunchOptions)> {
            let ctx = opt::context(&store, profile.as_deref())?;
            let lo = opt::prepare_launch(&ctx, plan)?;
            Ok((ctx, lo))
        });
        match result {
            Ok((ctx, lo)) => {
                if let Some(r) = &lo.imported {
                    self.options.emit(OptionsEvent::Changed { product_id: game.product_id, profile: ctx.profile_id.clone(), revision: r.revision, source: r.source.clone(), changes: r.changes.clone() });
                }
                self.options.register(&ctx, session, lo.revision, lo.has, Vec::new(), None);
                let mut lines = lo.lines;
                lines.extend(ctx.problems.iter().map(|p| format!("options: {p}")));
                lines
            }
            Err(e) => {
                tracing::warn!("options for the launch: {e:#}");
                vec![format!("options not applied: {e:#}")]
            }
        }
    }

    pub(crate) fn options_pids(&self, session: &str, pids: Vec<u32>) {
        self.options.update(session, |s| s.pids = pids);
    }

    /// While a session runs and anyone is subscribed, look for changes the game or a person
    /// made to its files every 2 s (through its namespace) and record them (§4).
    pub(crate) fn options_watch(self: &Arc<Self>, session: String) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let Some(s) = this.options.session(&session) else { return };
                if this.options.events.receiver_count() == 0 {
                    continue;
                }
                let Some(pid) = s.pids.first().copied() else { continue };
                let (product_id, profile) = (s.record.product_id, s.record.profile.clone());
                let found = with_store_lock(move || {
                    let ctx = ctx_for(product_id, Some(&profile))?;
                    opt::import_external(&ctx, Some(&PathBuf::from(format!("/proc/{pid}/root"))), "while the game ran")
                })
                .await;
                if let Ok(Some(r)) = found {
                    this.options_imported(&s, r);
                }
            }
        });
    }

    fn options_imported(&self, s: &LiveSession, r: quiver_api::options::OptionsRevision) {
        let product_id = s.record.product_id;
        self.options.emit(OptionsEvent::Changed { product_id, profile: s.record.profile.clone(), revision: r.revision, source: r.source.clone(), changes: r.changes.clone() });
        let cur = s.has.clone();
        let mut has = cur.clone();
        for (key, vals) in &r.values {
            let mut changed = BTreeMap::new();
            for (id, v) in vals {
                if cur.get(key).and_then(|m| m.get(id)) != Some(v) {
                    changed.insert(id.clone(), v.clone());
                    has.entry(key.clone()).or_default().insert(id.clone(), v.clone());
                }
            }
            if !changed.is_empty() {
                self.options.emit(OptionsEvent::Live { session: s.record.session.clone(), product_id, surface: key.clone(), values: changed, by: r.source.clone(), saved: true });
            }
        }
        self.options.update(&s.record.session, |l| l.has = has);
        self.log_activity(&r.source, format!("options r{} for {}: {}", r.revision, self.product_name(product_id), r.changes.join("; ")), Some(product_id));
    }

    /// After a session: import outside edits, and if the game exited abnormally within 60 s
    /// of a live change, offer a revert (never automatic, §8).
    pub(crate) async fn options_session_end(self: &Arc<Self>, session: &str, exit_code: Option<i32>) {
        let Some(s) = self.options.sessions.lock().unwrap().remove(session) else { return };
        let (product_id, profile) = (s.record.product_id, s.record.profile.clone());
        let p2 = profile.clone();
        if let Ok(Some(r)) = with_store_lock(move || {
            let ctx = ctx_for(product_id, Some(&p2))?;
            opt::import_external(&ctx, None, "after the session")
        })
        .await
        {
            self.options_imported(&s, r);
        }
        let mut record = s.record.clone();
        record.ended = Some(chrono::Utc::now());
        // Abnormal: a non-zero exit, or an SDK mod that never said `bye`.
        let abnormal = exit_code.is_some_and(|c| c != 0) || (s.sdk_seen && !s.clean_bye);
        if abnormal && let Some(last) = record.live_changes.last() {
            let secs = (record.ended.unwrap() - last.at).num_seconds();
            if secs <= 60 {
                let name = self.product_name(product_id);
                let message = format!("{name} crashed {secs} s after {} {} was set. Revert this session's in-game changes?", last.surface, last.id);
                record.crash = Some(CrashNote { seconds_after: secs, change: format!("{} {}", last.surface, last.id), exit_code, message: message.clone() });
                self.log_activity("quiverd", format!("{message} (quiver mods {product_id} options revert --session {session})"), Some(product_id));
                self.options.emit(OptionsEvent::Crash { product_id, profile: profile.clone(), note: record.clone() });
            }
        }
        if let Ok(g) = game(product_id) {
            let _ = opt::save_session(&store_for(&g), &record);
        }
    }

    /// A mod reported values over the SDK (`changed`): its own menu (`persist`, saved to the
    /// profile as `mod:<surface>`) or runtime state such as a hotkey flip (shown, not saved).
    /// Values are validated; one the mod says it runs with is recorded as fact.
    #[allow(dead_code)]
    pub(crate) async fn options_from_mod(self: &Arc<Self>, session: &str, surface: &str, values: BTreeMap<String, String>, persist: bool) -> Result<OptionsChange> {
        let s = self.options.session(session).context("no such session")?;
        self.options.update(session, |l| l.sdk_seen = true);
        let source = format!("mod:{surface}");
        let ops = values.iter().map(|(id, text)| OptionOp::Set { surface: surface.to_owned(), id: id.clone(), value: Value::String(text.clone()) }).collect();
        if !persist {
            // Runtime state: what the program has now; shown, not saved.
            let ctx = ctx_for(s.record.product_id, Some(&s.record.profile))?;
            let surf = opt::surface(&ctx, surface)?;
            let key = surf.key.clone();
            let mut typed = BTreeMap::new();
            for (id, text) in &values {
                if let Some(d) = surf.schema().options.get(id) {
                    typed.insert(id.clone(), d.parse_text(text).unwrap_or(Value::String(text.clone())));
                }
            }
            self.options.update(session, |l| {
                l.has.entry(key.clone()).or_default().extend(typed.clone());
            });
            self.options.emit(OptionsEvent::Live { session: session.to_owned(), product_id: s.record.product_id, surface: key, values: typed, by: source, saved: false });
            return Ok(OptionsChange { product_id: s.record.product_id, profile: s.record.profile, ..Default::default() });
        }
        self.options_set(&source, s.record.product_id, Some(s.record.profile.clone()), ops, Live::No, Some("Changed in the mod's own menu".into()), true, Some(session.to_owned())).await
    }

    /// The SDK said `bye` for a session (a clean exit).
    #[allow(dead_code)]
    pub(crate) fn options_sdk_bye(&self, session: &str) {
        self.options.update(session, |s| {
            s.clean_bye = true;
            s.sdk_seen = true;
        });
    }
}

fn op_surface(o: &OptionOp) -> &str {
    match o {
        OptionOp::Set { surface, .. } | OptionOp::Reset { surface, .. } | OptionOp::Preset { surface, .. } | OptionOp::Raw { surface, .. } | OptionOp::ResetAll { surface } => surface,
    }
}

fn merge(a: &Values, b: &Values) -> Values {
    let mut out = a.clone();
    for (k, m) in b {
        out.entry(k.clone()).or_default().extend(m.iter().map(|(i, v)| (i.clone(), v.clone())));
    }
    out
}

/// The ops that turn `cur` into `target` (sets, resets, raw keys).
fn ops_towards(cur: &quiver_api::options::OptionsRevision, target: &quiver_api::options::OptionsRevision) -> Vec<OptionOp> {
    let mut ops = Vec::new();
    let keys: std::collections::BTreeSet<&String> = cur.values.keys().chain(target.values.keys()).collect();
    for key in keys {
        let (a, b) = (cur.values.get(key).cloned().unwrap_or_default(), target.values.get(key).cloned().unwrap_or_default());
        for (id, v) in &b {
            if a.get(id) != Some(v) {
                ops.push(OptionOp::Set { surface: key.clone(), id: id.clone(), value: v.clone() });
            }
        }
        for id in a.keys().filter(|id| !b.contains_key(*id)) {
            ops.push(OptionOp::Reset { surface: key.clone(), id: id.clone() });
        }
    }
    let keys: std::collections::BTreeSet<&String> = cur.raw.keys().chain(target.raw.keys()).collect();
    for key in keys {
        let (a, b) = (cur.raw.get(key).cloned().unwrap_or_default(), target.raw.get(key).cloned().unwrap_or_default());
        for (bind, v) in &b {
            if a.get(bind) != Some(v) {
                ops.push(OptionOp::Raw { surface: key.clone(), binding: bind.clone(), value: Some(v.clone()) });
            }
        }
        for bind in a.keys().filter(|k| !b.contains_key(*k)) {
            ops.push(OptionOp::Raw { surface: key.clone(), binding: bind.clone(), value: None });
        }
    }
    ops
}

fn diff(product_id: u32, profile: Option<&str>, against: Option<&str>) -> Result<Vec<OptionDiff>> {
    let ctx = ctx_for(product_id, profile)?;
    let rev = opt::current(&ctx.store, &ctx.profile_id)?;
    let against = against.unwrap_or("default");
    // The other side's values, evaluated with this profile's schemas.
    let (other_values, other_raw, label): (Option<Values>, _, String) = if against.eq_ignore_ascii_case("default") {
        (None, Default::default(), "default".into())
    } else if let Some(r) = against.strip_prefix(['r', 'R']).and_then(|n| n.parse::<u32>().ok()) {
        let hist = opt::history(&ctx.store, &ctx.profile_id)?;
        let t = if r == 0 { Default::default() } else { hist.into_iter().find(|x| x.revision == r).with_context(|| format!("no options revision r{r}"))? };
        (Some(t.values), t.raw, format!("r{r}"))
    } else {
        let (store, p) = resolve(product_id, Some(against))?;
        let id = p.unwrap_or_else(|| VANILLA.into());
        let t = opt::current(&store, &id)?;
        (Some(t.values), t.raw, against.to_owned())
    };
    let mut out = Vec::new();
    for s in &ctx.surfaces {
        let mine = opt::eval(&ctx, s, &rev.values, &rev.raw, None);
        let theirs = other_values.as_ref().map(|v| opt::eval(&ctx, s, v, &other_raw, None));
        for (id, e) in &mine.options {
            let a = e.effective();
            let b = match &theirs {
                Some(t) => t.options.get(id).map(|x| x.effective()).unwrap_or(Value::Null),
                None => e.base(),
            };
            if !quiver_api::options::same_value(&a, &b) {
                out.push(OptionDiff {
                    surface: s.key.clone(),
                    title: s.title(),
                    id: id.clone(),
                    label: e.def.label.clone(),
                    value: e.def.display(&a),
                    against: e.def.display(&b),
                    value_json: a,
                    against_json: b,
                });
            }
        }
        let theirs_raw = theirs.map(|t| t.raw).unwrap_or_default();
        for (k, v) in mine.raw.iter() {
            if theirs_raw.get(k) != Some(v) {
                out.push(OptionDiff { surface: s.key.clone(), title: s.title(), id: k.clone(), value: v.clone(), against: theirs_raw.get(k).cloned().unwrap_or_else(|| "(file's)".into()), ..Default::default() });
            }
        }
        let _ = &label;
    }
    Ok(out)
}

fn schema_info(product_id: u32, mod_id: &str, version: Option<&str>) -> Result<SchemaInfo> {
    let store = store_for(&game(product_id)?);
    let rec = store.find_mod(mod_id)?;
    let vid = match version {
        Some(v) => rec.version(v).map(|v| v.id.clone()).with_context(|| format!("{} has no version {v}", rec.name))?,
        None => rec.current.clone(),
    };
    let layer = store.layers()?.into_iter().find(|l| l.id == vid).context("version missing from the store")?;
    let r = opt::resolve_schema(&store, &rec, &layer).with_context(|| {
        format!("{} {} has no options schema (none shipped, none in Quiver's catalog, no local one); guess one: options infer {} FILE", rec.name, rec.label_of(&vid), rec.name)
    })??;
    let dir = store.layer_dir(&vid);
    let texts = r
        .schema
        .files
        .iter()
        .filter(|(_, f)| f.root == quiver_api::options::FileRoot::Install)
        .map(|(id, f)| (id.clone(), std::fs::read_to_string(dir.join(&f.path)).ok()))
        .collect();
    let mut lint = opt::schema::lint(&r.schema);
    lint.extend(opt::schema::lint_files(&r.schema, &texts).1);
    Ok(SchemaInfo {
        key: format!("{}/{}", rec.id, r.schema.surface),
        mod_id: rec.id.clone(),
        version: vid.clone(),
        version_label: rec.label_of(&vid),
        source: r.source,
        note: r.note,
        schema: r.schema,
        text: r.text,
        lint,
        revision: r.revision,
    })
}

/// Lint a package or plain folder before `version add` (`options lint DIR`).
pub fn lint_folder(path: &std::path::Path) -> Result<LintReport> {
    let path = path.canonicalize().with_context(|| format!("{} not found", path.display()))?;
    let file = if path.is_file() { Some(path.clone()) } else { opt::schema_in(&path) };
    let file = file.with_context(|| format!("no {} in {}", quiver_api::options::SCHEMA_FILE, path.display()))?;
    let dir = file.parent().unwrap_or(&path).to_owned();
    // A package keeps its files in `files/` (or its manifest's `files.dir`).
    let files_dir = match quiver_core::modpkg::read_package(&dir) {
        Ok(Some((_, Some(f)))) => f,
        _ => dir.clone(),
    };
    let text = std::fs::read_to_string(&file)?;
    let schema = opt::schema::parse(&text, file.extension().is_some_and(|e| e == "json")).with_context(|| file.display().to_string())?;
    let mut findings = opt::schema::lint(&schema);
    let texts = schema
        .files
        .iter()
        .filter(|(_, f)| f.root == quiver_api::options::FileRoot::Install)
        .map(|(id, f)| (id.clone(), std::fs::read_to_string(files_dir.join(&f.path)).ok()))
        .collect();
    let (undescribed, more) = opt::schema::lint_files(&schema, &texts);
    findings.extend(more);
    Ok(LintReport { schema: Some(file), surface: schema.surface.clone(), options: schema.options.len(), undescribed, findings })
}
